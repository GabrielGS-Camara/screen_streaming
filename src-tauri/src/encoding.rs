//! Software H.264 encoding via `openh264` (Cisco's OpenH264, BSD-2-Clause).
//!
//! This is a stepping stone, not the final answer: it proves real captured
//! frames can become real H.264 bytes that a WebRTC video track can carry.
//! Hardware encoding (Media Foundation / NVENC / AMF / QuickSync) is the
//! follow-up — software x264-style encoding burns CPU, which conflicts with
//! the project's whole point of not weighing down a game running alongside
//! the stream.

use openh264::OpenH264API;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer};

pub struct H264Encoder {
    encoder: Encoder,
}

impl H264Encoder {
    pub fn new(
        bitrate_bps: u32,
        max_fps: f32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(max_fps));

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
        Ok(Self { encoder })
    }

    /// Encodes one tightly-packed RGBA8 frame (no row padding — see
    /// [`windows_capture::frame::FrameBuffer::as_nopadding_buffer`]) into
    /// H.264 Annex-B bytes (one or more NAL units).
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let rgba_source = RgbaSliceU8::new(rgba, (width, height));
        let yuv = YUVBuffer::from_rgb_source(rgba_source);
        let bitstream = self.encoder.encode(&yuv)?;
        Ok(bitstream.to_vec())
    }
}
