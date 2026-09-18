//! The user-chosen streaming quality (resolution ceiling, fps ceiling,
//! whether to send system audio) — see the "Qualidade configurável" row in
//! CLAUDE_SESSIONS.md's architecture table. Resolution/fps here are ceilings,
//! not guarantees: capture itself always runs at the source's native
//! resolution/refresh rate (Windows Graphics Capture doesn't let us ask for
//! anything else), so downscaling and frame-rate limiting are applied on the
//! encode side, in [`crate::hw_encoding`] and [`crate::session`].

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct StreamQuality {
    /// Target output height in pixels (180/240/360/480/720/1080). Width is
    /// derived to keep the source's aspect ratio.
    pub resolution_height: u32,
    /// Target frames per second ceiling (15/30/60/120). Frames arriving
    /// faster than this are dropped before encoding; slower sources (a
    /// static desktop, or a 60Hz monitor with "120" selected) just can't
    /// fill it.
    pub fps: u32,
    /// Whether to also capture and send system audio.
    /// **Not implemented yet** — captured here so the UI/commands have
    /// somewhere to put the user's choice, but the streaming pipeline
    /// currently ignores it. See CLAUDE_SESSIONS.md pendências.
    pub audio: bool,
}

impl StreamQuality {
    /// Scales `(source_width, source_height)` down to fit within
    /// `resolution_height`, preserving aspect ratio. Never scales up (a
    /// 480p capture with "1080p" selected stays 480p — there's no extra
    /// detail to invent). Both dimensions are rounded to the nearest even
    /// number, since NV12/H.264 need even width/height (chroma
    /// subsampling).
    pub fn target_dimensions(&self, source_width: u32, source_height: u32) -> (u32, u32) {
        if source_width == 0 || source_height == 0 {
            return (source_width, source_height);
        }

        let target_height = self.resolution_height.min(source_height).max(2);
        let target_width = ((source_width as u64 * target_height as u64) / source_height as u64)
            .max(2) as u32;

        let even = |v: u32| if v % 2 == 0 { v } else { v + 1 };
        (even(target_width), even(target_height))
    }

    /// Minimum time between two frames handed to the encoder, derived from
    /// `fps`. Frames arriving sooner than this (screen changing faster than
    /// the chosen ceiling) are dropped before ever reaching the encoder.
    pub fn frame_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(1.0 / self.fps.max(1) as f64)
    }

    /// A reasonable H.264 bitrate for the chosen resolution. Rough,
    /// hand-picked steps rather than a formula — good enough until real
    /// network conditions call for adaptive bitrate.
    pub fn bitrate_bps(&self) -> usize {
        match self.resolution_height {
            0..=180 => 400_000,
            181..=240 => 700_000,
            241..=360 => 1_200_000,
            361..=480 => 2_000_000,
            481..=720 => 3_500_000,
            _ => 6_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscales_keeping_aspect_ratio_and_evenness() {
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false };
        let (w, h) = quality.target_dimensions(1920, 1080);
        assert_eq!(h, 480);
        // 1920 * 480 / 1080 = 853.33 -> 853 -> rounded up to even.
        assert_eq!(w, 854);
        assert_eq!(w % 2, 0);
    }

    #[test]
    fn never_scales_up_past_the_source() {
        let quality = StreamQuality { resolution_height: 1080, fps: 30, audio: false };
        let (w, h) = quality.target_dimensions(640, 480);
        assert_eq!((w, h), (640, 480));
    }
}
