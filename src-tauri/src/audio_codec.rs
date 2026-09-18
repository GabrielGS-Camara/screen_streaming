//! Opus encode/decode, via the `audiopus` crate — a safe wrapper over the
//! real reference `libopus` C library (`audiopus_sys`, built with the
//! `static` feature: compiled from source at build time and linked
//! directly into the exe, no `opus.dll`/etc. to distribute — see
//! Cargo.toml's comment for why, and CLAUDE_SESSIONS.md for the FFmpeg DLL
//! problem this deliberately avoids repeating for audio).

use audiopus::coder::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use audiopus::{Application, Channels, SampleRate};

use crate::audio_capture::{CHANNELS, FRAME_SAMPLES_PER_CHANNEL};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Encodes fixed-size PCM chunks (see `audio_capture::FRAME_SAMPLES_PER_CHANNEL`)
/// into Opus packets, one packet per chunk.
pub struct AudioEncoder {
    encoder: OpusEncoder,
}

impl AudioEncoder {
    pub fn new() -> Result<Self, BoxError> {
        let encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)?;
        Ok(Self { encoder })
    }

    /// Encodes one `FRAME_SAMPLES_PER_CHANNEL`-per-channel chunk of
    /// interleaved i16 PCM into one Opus packet.
    pub fn encode(&self, pcm: &[i16]) -> Result<Vec<u8>, BoxError> {
        // Opus packets are always well under 4000 bytes in practice (the
        // reference encoder itself won't produce more for any valid
        // input/bitrate combination).
        let mut output = [0u8; 4000];
        let len = self.encoder.encode(pcm, &mut output)?;
        Ok(output[..len].to_vec())
    }
}

/// Decodes Opus packets back into interleaved i16 PCM — the watch side of
/// `audio_preview.rs`'s pipeline.
pub struct AudioDecoder {
    decoder: OpusDecoder,
}

impl AudioDecoder {
    pub fn new() -> Result<Self, BoxError> {
        let decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo)?;
        Ok(Self { decoder })
    }

    /// Decodes one Opus packet into interleaved i16 PCM. Sized generously
    /// above `FRAME_SAMPLES_PER_CHANNEL` — Opus itself decides how many
    /// samples come out based on what's actually in the packet, this just
    /// has to be large enough to never truncate a real one.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, BoxError> {
        let mut output = vec![0i16; FRAME_SAMPLES_PER_CHANNEL * CHANNELS * 6];
        let samples_per_channel = self.decoder.decode(Some(packet), output.as_mut_slice(), false)?;
        output.truncate(samples_per_channel * CHANNELS);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real encode + decode round trip (no mocking) — proves the
    /// statically-linked libopus actually works end-to-end on this
    /// machine, independent of any audio hardware.
    #[test]
    fn encodes_and_decodes_a_real_tone_round_trip() {
        let samples_total = FRAME_SAMPLES_PER_CHANNEL * CHANNELS;
        // A real (not silent) synthetic tone — all-zero input is too easy a
        // case, Opus could special-case silence.
        let pcm: Vec<i16> = (0..samples_total)
            .map(|i| ((i as f32 * 0.05).sin() * 10_000.0) as i16)
            .collect();

        let encoder = AudioEncoder::new().expect("failed to create Opus encoder");
        let packet = encoder.encode(&pcm).expect("encode failed");
        assert!(!packet.is_empty(), "expected a non-empty Opus packet");

        let mut decoder = AudioDecoder::new().expect("failed to create Opus decoder");
        let decoded = decoder.decode(&packet).expect("decode failed");
        assert_eq!(
            decoded.len(),
            samples_total,
            "expected the decoded frame to have the same sample count as the input"
        );
    }
}
