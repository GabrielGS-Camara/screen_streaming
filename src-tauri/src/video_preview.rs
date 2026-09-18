//! Turns video frames into something the plain HTML/JS frontend can
//! actually display — used both for an incoming WebRTC H.264 track (the
//! "Assistir" tab) and for a broadcaster's own on-demand live preview of
//! their outgoing capture (the "Transmitir" tab).
//!
//! The frontend has no native WebRTC of its own — the whole `PeerConnection`
//! lives in this Rust backend (see `session.rs`/`rtc.rs`), not in the
//! WebView — so received RTP video packets need to become pixels the
//! WebView can render before the "Assistir" tab is useful. Piping raw or
//! JPEG frames through Tauri's JSON-based event bridge would be far too
//! slow for video (JSON-encoding a multi-megabyte array dozens of times a
//! second), so instead this module runs a tiny local-only HTTP server that
//! serves the frames as an MJPEG stream (`multipart/x-mixed-replace`) —
//! the frontend just points a plain `<img src="http://127.0.0.1:PORT/stream">`
//! at it and the browser engine (WebView2) handles the rest natively. The
//! same [`MjpegServer`] is reused for both use cases.
//!
//! Watch-side pipeline: RTP packets -> [`AccessUnitAssembler`] (depacketize
//! + reframe into Annex-B access units) -> [`H264ToJpeg`] (software H.264
//! decode + [`JpegEncoder`], via `ffmpeg-next`) -> [`MjpegServer`].
//!
//! Broadcast-preview pipeline: raw RGBA capture frames ->
//! [`RgbaPreviewEncoder`] ([`JpegEncoder`] directly, no H.264 round trip at
//! all) -> [`MjpegServer`].
//!
//! Software decode, unlike the outgoing (encode) side, is intentionally not
//! GPU-accelerated: decoding a single incoming 720p/1080p stream is cheap
//! (a small fraction of the cost of encoding the same stream), so it
//! doesn't threaten the project's "must not weigh down a game" requirement
//! even in software. That requirement is about the machine doing the
//! *capturing*, which this module never runs on.

use std::sync::Arc;

use bytes::Bytes;
use ffmpeg_next as ffmpeg;
use ffmpeg::Rational;
use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::codec::decoder::video::Video as VideoDecoder;
use ffmpeg::codec::encoder::video::Encoder as VideoEncoder;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::software::scaling::flag::Flags as ScalingFlags;
use ffmpeg::util::frame::video::Video as VideoFrame;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Reassembles depacketized H.264 NAL units into complete Annex-B access
/// units (one per video frame). The RTP marker bit is set on a video
/// track's last packet of each frame (RFC 6184) — that's the only signal
/// used to decide a frame is complete, so frames can be handed to the
/// decoder as soon as they're whole instead of waiting on a timer.
struct AccessUnitAssembler {
    depacketizer: H264Packet,
    buffer: Vec<u8>,
}

impl AccessUnitAssembler {
    fn new() -> Self {
        Self { depacketizer: H264Packet::default(), buffer: Vec::new() }
    }

    /// Feeds one RTP packet's payload. Returns the completed Annex-B access
    /// unit once the frame's last packet (marker bit set) arrives.
    fn push(&mut self, payload: &Bytes, marker: bool) -> Option<Vec<u8>> {
        match self.depacketizer.depacketize(payload) {
            Ok(nal) => self.buffer.extend_from_slice(&nal),
            Err(e) => eprintln!("[video_preview] H.264 depacketize error: {e}"),
        }

        if !marker || self.buffer.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.buffer))
    }
}

/// Software MJPEG encode of one video frame at a time, converting from
/// whatever pixel format the frame arrives in. Shared by both preview
/// pipelines in this module: the watch side (frames come from H.264
/// decode) and the broadcaster's own live preview (frames come straight
/// from the RGBA capture — see [`RgbaPreviewEncoder`] — no H.264 involved
/// at all, since it's only ever shown locally). Encoder/scaler are created
/// lazily, once the first frame's real dimensions/format are known.
struct JpegEncoder {
    encoder: Option<VideoEncoder>,
    scaler: Option<ScalingContext>,
    next_pts: i64,
}

impl JpegEncoder {
    fn new() -> Self {
        Self { encoder: None, scaler: None, next_pts: 0 }
    }

    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<Vec<u8>>, BoxError> {
        let (width, height) = (frame.width(), frame.height());

        if self.encoder.is_none() {
            let codec = ffmpeg::encoder::find_by_name("mjpeg")
                .ok_or("mjpeg encoder not available in this FFmpeg build")?;
            let context = CodecContext::new_with_codec(codec);
            let mut video = context.encoder().video()?;
            video.set_width(width);
            video.set_height(height);
            video.set_format(Pixel::YUVJ420P);
            video.set_time_base(Rational(1, 90));
            video.set_bit_rate(6_000_000);
            self.encoder = Some(video.open_as(codec)?);
            self.scaler = Some(ScalingContext::get(
                frame.format(),
                width,
                height,
                Pixel::YUVJ420P,
                width,
                height,
                ScalingFlags::BILINEAR,
            )?);
        }

        let mut converted = VideoFrame::new(Pixel::YUVJ420P, width, height);
        self.scaler.as_mut().unwrap().run(frame, &mut converted)?;
        converted.set_pts(Some(self.next_pts));
        self.next_pts += 1;

        let encoder = self.encoder.as_mut().unwrap();
        encoder.send_frame(&converted)?;

        let mut jpegs = Vec::new();
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        jpegs.push(data.to_vec());
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::ffi::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => {
                    eprintln!("[video_preview] mjpeg encode error: {e}");
                    break;
                }
            }
        }
        Ok(jpegs)
    }
}

/// Software H.264 decode + [`JpegEncoder`] of one access unit at a time.
/// The decoder is created lazily on first use.
struct H264ToJpeg {
    decoder: VideoDecoder,
    jpeg: JpegEncoder,
}

impl H264ToJpeg {
    fn new() -> Result<Self, BoxError> {
        ffmpeg::init()?;
        let codec = ffmpeg::decoder::find_by_name("h264")
            .ok_or("h264 decoder not available in this FFmpeg build")?;
        let decoder = CodecContext::new_with_codec(codec).decoder().open_as(codec)?.video()?;
        Ok(Self { decoder, jpeg: JpegEncoder::new() })
    }

    /// Decodes one Annex-B access unit, returning zero or more JPEG frames
    /// (the decoder can buffer internally, same as the hardware encoder
    /// does on the sending side).
    fn push_access_unit(&mut self, annexb: &[u8]) -> Result<Vec<Vec<u8>>, BoxError> {
        let packet = ffmpeg::Packet::copy(annexb);
        self.decoder.send_packet(&packet)?;

        let mut jpegs = Vec::new();
        let mut decoded = VideoFrame::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            jpegs.extend(self.jpeg.encode(&decoded)?);
        }
        Ok(jpegs)
    }
}

/// Builds an ffmpeg RGBA video frame from a tightly-packed RGBA8 buffer
/// (same layout `capture::CapturedFrame` uses), handling the row-stride
/// padding ffmpeg's own buffers can have — same technique as
/// `hw_encoding::HardwareH264Encoder::encode_rgba`.
fn rgba_frame(rgba: &[u8], width: u32, height: u32) -> VideoFrame {
    let mut frame = VideoFrame::new(Pixel::RGBA, width, height);
    let stride = frame.stride(0);
    let row_bytes = (width * 4) as usize;
    let dst = frame.data_mut(0);
    for y in 0..height as usize {
        let src = &rgba[y * row_bytes..(y + 1) * row_bytes];
        dst[y * stride..y * stride + row_bytes].copy_from_slice(src);
    }
    frame
}

/// Encodes raw RGBA capture frames straight to JPEG — no H.264 involved at
/// all. Used for the broadcaster's own on-demand live preview (see
/// `session::attach_video_source`'s `preview` parameter): much cheaper
/// than the watch side's pipeline since there's no encode-then-decode
/// round trip, which matters because this runs on the *sending* machine —
/// the one the project's "don't weigh down a game" requirement is about.
pub struct RgbaPreviewEncoder {
    jpeg: JpegEncoder,
}

impl RgbaPreviewEncoder {
    pub fn new() -> Result<Self, BoxError> {
        ffmpeg::init()?;
        Ok(Self { jpeg: JpegEncoder::new() })
    }

    pub fn encode(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<Vec<Vec<u8>>, BoxError> {
        self.jpeg.encode(&rgba_frame(rgba, width, height))
    }
}

/// Serves the most recently published JPEG frame to any number of local
/// HTTP clients as an MJPEG stream. Each connected client gets a background
/// task that blocks on `frame_rx.changed()` and writes out whatever's
/// current — a slow/absent client just doesn't advance, it never holds up
/// publishing new frames.
pub struct MjpegServer {
    pub url: String,
    frame_tx: watch::Sender<Option<Arc<Vec<u8>>>>,
    /// Stops the accept loop below when the last `Arc<MjpegServer>` holding
    /// this is dropped (see the `Drop` impl) — without this, the listener
    /// (and its bound local port) would stay alive for the rest of the
    /// process even after nothing references this server anymore, e.g.
    /// after `stop_broadcast`/`stop_watching` drop their copy.
    accept_task: tokio::task::AbortHandle,
}

impl MjpegServer {
    pub async fn start() -> Result<Self, BoxError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (frame_tx, frame_rx) = watch::channel(None);

        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(serve_mjpeg_client(stream, frame_rx.clone()));
                    }
                    Err(e) => {
                        eprintln!("[video_preview] MJPEG server accept error: {e}");
                        break;
                    }
                }
            }
        })
        .abort_handle();

        Ok(Self { url: format!("http://{addr}/stream"), frame_tx, accept_task })
    }

    pub fn publish(&self, jpeg: Vec<u8>) {
        let _ = self.frame_tx.send(Some(Arc::new(jpeg)));
    }
}

impl Drop for MjpegServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Writes the MJPEG multipart response for one connected client. Doesn't
/// bother parsing the client's request line — every connection gets the
/// same stream, and an HTTP GET request is small enough to sit in the
/// kernel's receive buffer without ever being read, so skipping the read
/// entirely doesn't risk stalling the client.
async fn serve_mjpeg_client(
    mut stream: TcpStream,
    mut frame_rx: watch::Receiver<Option<Arc<Vec<u8>>>>,
) {
    const HEADER: &str = "HTTP/1.1 200 OK\r\n\
        Content-Type: multipart/x-mixed-replace; boundary=frame\r\n\
        Cache-Control: no-cache\r\n\
        Connection: close\r\n\r\n";
    if stream.write_all(HEADER.as_bytes()).await.is_err() {
        return;
    }

    loop {
        if frame_rx.changed().await.is_err() {
            return;
        }
        let Some(jpeg) = frame_rx.borrow_and_update().clone() else {
            continue;
        };
        let part_header =
            format!("--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len());
        if stream.write_all(part_header.as_bytes()).await.is_err()
            || stream.write_all(&jpeg).await.is_err()
            || stream.write_all(b"\r\n").await.is_err()
        {
            return;
        }
    }
}

/// Wires an incoming video track to a fresh [`MjpegServer`] and returns its
/// URL plus the server itself — the caller (`lib.rs`) holds onto the
/// returned `Arc` for as long as the watcher is actually watching, and
/// drops it on `stop_watching`/disconnect to free the local port (see
/// [`MjpegServer`]'s `Drop` impl) instead of leaking it for the rest of the
/// process. Runs the network side (polling the track) as an async task and
/// the CPU-bound decode/encode side on a dedicated OS thread — same split
/// as the outgoing capture/encode pipeline in `session.rs`, for the same
/// reason: CPU-bound work shouldn't run on a tokio worker thread.
pub async fn attach_video_sink(track: Arc<dyn TrackRemote>) -> Result<(String, Arc<MjpegServer>), BoxError> {
    let server = Arc::new(MjpegServer::start().await?);
    let url = server.url.clone();

    let (au_tx, au_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let server_for_thread = server.clone();
    std::thread::spawn(move || {
        let mut pipeline = match H264ToJpeg::new() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[video_preview] failed to start H.264 decoder: {e}");
                return;
            }
        };
        for access_unit in au_rx {
            match pipeline.push_access_unit(&access_unit) {
                Ok(jpegs) => {
                    for jpeg in jpegs {
                        server_for_thread.publish(jpeg);
                    }
                }
                Err(e) => eprintln!("[video_preview] decode error: {e}"),
            }
        }
    });

    tokio::spawn(async move {
        let mut assembler = AccessUnitAssembler::new();
        while let Some(event) = track.poll().await {
            if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                if let Some(access_unit) = assembler.push(&packet.payload, packet.header.marker) {
                    if au_tx.send(access_unit).is_err() {
                        break;
                    }
                }
            }
        }
    });

    Ok((url, server))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    use crate::capture::CaptureSource;
    use crate::quality::StreamQuality;
    use crate::session::{join_session, start_hosting};

    /// Not run in CI (needs a real display/GPU to capture and hardware-encode
    /// a real stream) — run manually with `cargo test -- --ignored --nocapture`.
    /// Exercises the whole receiving pipeline end-to-end: real screen capture
    /// -> hardware H.264 encode -> real WebRTC video track -> RTP
    /// depacketization -> software H.264 decode -> MJPEG encode -> served
    /// over the local HTTP preview server, then confirms a client connecting
    /// to that server actually receives JPEG bytes (a SOI marker).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn decodes_a_real_stream_into_an_mjpeg_preview() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false };
        let stop = Arc::new(AtomicBool::new(false));
        let (_preview_tx, preview_rx) = watch::channel(None);

        let (host_session, mut guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality, stop.clone(), preview_rx),
                join_session(&signaling_addr, code),
            ),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        let track = tokio::time::timeout(Duration::from_secs(15), guest_session.incoming_tracks.recv())
            .await
            .expect("timed out waiting for the incoming video track")
            .expect("incoming_tracks closed without ever receiving a track");

        let (preview_url, _server) = attach_video_sink(track).await.expect("attach_video_sink failed");
        println!("preview url: {preview_url}");

        let host_port = preview_url.trim_start_matches("http://").trim_end_matches("/stream");
        let mut client = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(host_port))
            .await
            .expect("connect timed out")
            .expect("failed to connect to the MJPEG preview server");

        let mut received = Vec::new();
        let mut buf = [0u8; 8192];
        let found_jpeg = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let n = client.read(&mut buf).await.expect("read failed");
                assert!(n > 0, "MJPEG server closed the connection early");
                received.extend_from_slice(&buf[..n]);
                if received.windows(2).any(|w| w == [0xFF, 0xD8]) {
                    break;
                }
            }
        })
        .await
        .is_ok();

        assert!(found_jpeg, "expected a JPEG frame (SOI marker) from the MJPEG preview stream");

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = guest_session.peer_connection.close().await;
        let _ = host_session.peer_connection.close().await;
    }
}
