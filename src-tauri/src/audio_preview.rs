//! Delivers decoded audio to the frontend — the audio-side equivalent of
//! `video_preview.rs`'s MJPEG server. WebView2 has no built-in way to
//! receive raw PCM through an `<img>`-style HTTP endpoint the way video
//! does, so this instead runs a tiny local WebSocket server (via
//! `tokio-tungstenite`, already used for signaling) that pushes binary
//! frames of interleaved i16 PCM (little-endian,
//! `audio_capture::SAMPLE_RATE` Hz, `audio_capture::CHANNELS` channels) to
//! whoever connects. The frontend feeds those straight into a Web Audio
//! `AudioWorklet` for playback with per-viewer volume control (a
//! `GainNode` — see `main.js`), simpler and more reliably real-time than
//! routing raw PCM through an `<audio>` element, which wants a container
//! format rather than bare samples.
//!
//! Watch-side pipeline: RTP Opus packets ->
//! [`rtc::rtp::codec::opus::OpusPacket`] (depacketize — a pass-through,
//! RFC 7587 puts exactly one Opus frame per RTP packet, never fragmented)
//! -> [`crate::audio_codec::AudioDecoder`] -> [`AudioWsServer`].

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rtc::rtp::codec::opus::OpusPacket;
use rtc::rtp::packetizer::Depacketizer;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use crate::audio_codec::AudioDecoder;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Serves the most recently decoded PCM chunk to any number of local
/// WebSocket clients — same "publish latest, slow clients just miss
/// chunks" shape as `video_preview::MjpegServer`, over a WebSocket instead
/// of HTTP multipart since raw audio has no equivalent MIME stream format.
pub struct AudioWsServer {
    pub url: String,
    chunk_tx: watch::Sender<Option<Arc<Vec<u8>>>>,
    /// Stops the accept loop when the last `Arc<AudioWsServer>` is dropped
    /// — see `video_preview::MjpegServer`'s identical `Drop` impl for why.
    accept_task: tokio::task::AbortHandle,
}

impl AudioWsServer {
    pub async fn start() -> Result<Self, BoxError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (chunk_tx, chunk_rx) = watch::channel(None);

        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(serve_audio_client(stream, chunk_rx.clone()));
                    }
                    Err(e) => {
                        eprintln!("[audio_preview] WS server accept error: {e}");
                        break;
                    }
                }
            }
        })
        .abort_handle();

        Ok(Self { url: format!("ws://{addr}/audio"), chunk_tx, accept_task })
    }

    /// Publishes one chunk of interleaved i16 PCM as raw little-endian bytes.
    pub fn publish(&self, pcm: &[i16]) {
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let _ = self.chunk_tx.send(Some(Arc::new(bytes)));
    }
}

impl Drop for AudioWsServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

async fn serve_audio_client(stream: TcpStream, mut chunk_rx: watch::Receiver<Option<Arc<Vec<u8>>>>) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[audio_preview] WS handshake failed: {e}");
            return;
        }
    };
    let (mut sink, _stream) = ws.split();

    loop {
        if chunk_rx.changed().await.is_err() {
            return;
        }
        let Some(chunk) = chunk_rx.borrow_and_update().clone() else {
            continue;
        };
        if sink.send(WsMessage::Binary((*chunk).clone())).await.is_err() {
            return;
        }
    }
}

/// Wires an incoming Opus audio track to a fresh [`AudioWsServer`] and
/// returns its URL plus the server itself — the caller (`lib.rs`) holds
/// onto the returned `Arc` for as long as the watcher is actually
/// watching, same convention as `video_preview::attach_video_sink`.
pub async fn attach_audio_sink(
    track: Arc<dyn TrackRemote>,
) -> Result<(String, Arc<AudioWsServer>), BoxError> {
    let server = Arc::new(AudioWsServer::start().await?);
    let url = server.url.clone();

    let (packet_tx, packet_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
    let server_for_thread = server.clone();
    std::thread::spawn(move || {
        let mut decoder = match AudioDecoder::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[audio_preview] failed to start Opus decoder: {e}");
                return;
            }
        };
        let mut depacketizer = OpusPacket;
        for payload in packet_rx {
            match depacketizer.depacketize(&payload) {
                Ok(opus_packet) => match decoder.decode(&opus_packet) {
                    Ok(pcm) => server_for_thread.publish(&pcm),
                    Err(e) => eprintln!("[audio_preview] Opus decode error: {e}"),
                },
                Err(e) => eprintln!("[audio_preview] Opus depacketize error: {e}"),
            }
        }
    });

    tokio::spawn(async move {
        while let Some(event) = track.poll().await {
            if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                if packet_tx.send(packet.payload.clone()).is_err() {
                    break;
                }
            }
        }
    });

    Ok((url, server))
}
