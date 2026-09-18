//! Hardware-accelerated H.264 encoding via FFmpeg, through the
//! `ffmpeg-next` crate. This is what [`crate::encoding`] (software,
//! `openh264`) was a stepping stone towards — the GPU does the encoding
//! instead of the CPU, which is the whole point of the project (must not
//! weigh the machine down while a game is running alongside the stream).
//!
//! FFmpeg picks the concrete implementation (NVENC/AMD AMF/Intel
//! QuickSync/Media Foundation) per the encoder name — we just try each
//! candidate name in turn and use whichever one actually opens on this
//! machine's GPU.
//!
//! Build/runtime requirements: the FFmpeg SDK must be available at build
//! time (`FFMPEG_DIR` env var pointing at a shared dev build) and its DLLs
//! on `PATH` at runtime. See CLAUDE_SESSIONS.md for exactly how this
//! machine is set up.

use ffmpeg_next as ffmpeg;
use ffmpeg::Rational;
use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::codec::encoder::video::Encoder as VideoEncoder;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::software::scaling::flag::Flags as ScalingFlags;
use ffmpeg::util::frame::video::Video as VideoFrame;

/// Names FFmpeg's encoder registry uses for hardware H.264 encoders, tried
/// in this order. The first one that both exists in this FFmpeg build and
/// successfully opens (i.e. the matching GPU/driver is actually present)
/// wins — same idea as `ff-encode`'s `HardwareEncoder::Auto`.
const CANDIDATE_ENCODERS: &[&str] = &["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf"];

/// Codec-specific options tuned for real-time throughput over compression
/// efficiency: fastest/lowest-latency preset, no lookahead buffering. The
/// generic `AVCodecContext` setters on [`ffmpeg::codec::encoder::video::Video`]
/// (width, bitrate, GOP, B-frames, ...) don't cover these — each hardware
/// backend only understands its own private option names, passed as a
/// string dictionary to `open_as_with`. Per-frame encode time, not just
/// whether the encoder opens, is what determines the sustainable frame
/// rate once real motion (not just a static desktop) is on screen — a slow
/// "quality-first" preset can easily fail to keep up in real time, which
/// shows up as low, choppy fps despite capture itself running fine. An
/// unrecognized option key is silently ignored by FFmpeg rather than
/// treated as an error, so it's safe to only fill in what each specific
/// backend actually understands here.
fn low_latency_options(name: &str) -> ffmpeg::Dictionary<'static> {
    let mut options = ffmpeg::Dictionary::new();
    match name {
        "h264_nvenc" => {
            options.set("preset", "p1");
            options.set("tune", "ll");
            options.set("rc", "cbr");
            options.set("rc-lookahead", "0");
        }
        "h264_qsv" => {
            options.set("preset", "veryfast");
            options.set("look_ahead", "0");
        }
        "h264_amf" => {
            options.set("quality", "speed");
            options.set("usage", "lowlatency");
        }
        "h264_mf" => {
            options.set("scenario", "display_remoting");
            options.set("rate_control", "cbr");
        }
        _ => {}
    }
    options
}

pub struct HardwareH264Encoder {
    encoder: VideoEncoder,
    scaler: ScalingContext,
    codec_name: &'static str,
    /// Dimensions of the RGBA frames fed to [`Self::encode_rgba`] (the raw
    /// capture size).
    capture_width: u32,
    capture_height: u32,
    /// Dimensions actually encoded (after quality downscale — see
    /// [`crate::quality::StreamQuality::target_dimensions`]). Equal to the
    /// capture size when no downscale is requested.
    output_width: u32,
    output_height: u32,
    next_pts: i64,
}

impl HardwareH264Encoder {
    /// Tries each candidate hardware encoder in turn and keeps the first
    /// one that opens successfully. `output_width`/`output_height` may be
    /// smaller than `capture_width`/`capture_height` — the scaler
    /// downsamples RGBA -> NV12 in the same pass that does the color
    /// conversion, so downscaling costs nothing extra beyond what the
    /// pixel-format conversion already does.
    pub fn new(
        capture_width: u32,
        capture_height: u32,
        output_width: u32,
        output_height: u32,
        bitrate_bps: usize,
        fps: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        ffmpeg::init()?;

        let mut attempts = Vec::new();
        for &name in CANDIDATE_ENCODERS {
            match Self::try_open(name, output_width, output_height, bitrate_bps, fps) {
                Ok(encoder) => {
                    let scaler = ScalingContext::get(
                        Pixel::RGBA,
                        capture_width,
                        capture_height,
                        Pixel::NV12,
                        output_width,
                        output_height,
                        ScalingFlags::BILINEAR,
                    )?;
                    return Ok(Self {
                        encoder,
                        scaler,
                        codec_name: name,
                        capture_width,
                        capture_height,
                        output_width,
                        output_height,
                        next_pts: 0,
                    });
                }
                Err(e) => attempts.push(format!("{name}: {e}")),
            }
        }

        Err(format!(
            "no hardware H.264 encoder available on this machine. Tried:\n{}",
            attempts.join("\n")
        )
        .into())
    }

    fn try_open(
        name: &str,
        width: u32,
        height: u32,
        bitrate_bps: usize,
        fps: u32,
    ) -> Result<VideoEncoder, Box<dyn std::error::Error + Send + Sync>> {
        let codec = ffmpeg::encoder::find_by_name(name)
            .ok_or("not compiled into this FFmpeg build")?;
        let context = CodecContext::new_with_codec(codec);
        let mut video = context.encoder().video()?;

        // NV12 is what every one of these hardware encoders wants natively.
        video.set_width(width);
        video.set_height(height);
        video.set_format(Pixel::NV12);
        video.set_time_base(Rational(1, fps as i32));
        video.set_frame_rate(Some(Rational(fps as i32, 1)));
        video.set_bit_rate(bitrate_bps);
        video.set_max_bit_rate(bitrate_bps);
        video.set_gop(fps.max(1));
        // B-frames trade latency and encode speed for a bit of compression
        // efficiency — not a trade worth making for a live screen share,
        // where sustaining real-time throughput under motion matters far
        // more than a few percent of bitrate.
        video.set_max_b_frames(0);

        Ok(video.open_as_with(codec, low_latency_options(name))?)
    }

    /// Encodes one tightly-packed RGBA8 frame (no row padding), returning
    /// zero or more H.264 packets — encoders commonly buffer internally, so
    /// a given call may return nothing (the bytes come out on a later
    /// call).
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
    ) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        let (width, height) = (self.capture_width, self.capture_height);

        let mut input = VideoFrame::new(Pixel::RGBA, width, height);
        let stride = input.stride(0);
        let row_bytes = (width * 4) as usize;
        {
            let dst = input.data_mut(0);
            for y in 0..height as usize {
                let src = &rgba[y * row_bytes..(y + 1) * row_bytes];
                dst[y * stride..y * stride + row_bytes].copy_from_slice(src);
            }
        }

        let mut converted = VideoFrame::new(Pixel::NV12, self.output_width, self.output_height);
        self.scaler.run(&input, &mut converted)?;
        converted.set_pts(Some(self.next_pts));
        self.next_pts += 1;

        self.encoder.send_frame(&converted)?;
        Ok(self.drain_packets())
    }

    /// Signals end-of-stream and drains whatever packets the encoder had
    /// buffered internally. Call once when done encoding.
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        self.encoder.send_eof()?;
        Ok(self.drain_packets())
    }

    fn drain_packets(&mut self) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        packets.push(data.to_vec());
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::ffi::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => {
                    eprintln!("[{}] receive_packet error: {e}", self.codec_name);
                    break;
                }
            }
        }
        packets
    }

    pub fn codec_name(&self) -> &'static str {
        self.codec_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture;

    /// Not run in CI (no GPU/display) — run manually with
    /// `cargo test -- --ignored --nocapture` on a real machine.
    #[test]
    #[ignore]
    fn opens_a_hardware_encoder_and_encodes_a_real_frame() {
        let (tx, rx) = std::sync::mpsc::channel();
        let capture_thread =
            std::thread::spawn(move || capture::capture_primary_monitor_frames(3, tx));

        let frame = rx
            .recv()
            .expect("expected at least one captured frame within 3s");
        drop(rx);
        let _ = capture_thread.join();

        let mut encoder =
            HardwareH264Encoder::new(frame.width, frame.height, frame.width, frame.height, 4_000_000, 30)
                .expect("no hardware H.264 encoder available on this machine");
        println!("using hardware encoder: {}", encoder.codec_name());

        let packets = encoder
            .encode_rgba(&frame.rgba)
            .expect("hardware encode failed");
        println!("first call produced {} packet(s)", packets.len());

        // Some hardware encoders buffer several frames (B-frame lookahead
        // etc.) before emitting anything; feed a full second's worth of
        // identical frames, then explicitly flush, before giving up.
        let mut total_bytes: usize = packets.iter().map(Vec::len).sum();
        for i in 0..30 {
            let more = encoder.encode_rgba(&frame.rgba).expect("hardware encode failed");
            if !more.is_empty() {
                println!("frame {i}: got {} packet(s)", more.len());
            }
            total_bytes += more.iter().map(Vec::len).sum::<usize>();
        }

        let flushed = encoder.flush().expect("flush failed");
        println!(
            "flush produced {} packet(s), {} bytes",
            flushed.len(),
            flushed.iter().map(Vec::len).sum::<usize>()
        );
        total_bytes += flushed.iter().map(Vec::len).sum::<usize>();

        assert!(total_bytes > 0, "hardware encoder never produced any bytes");
    }
}
