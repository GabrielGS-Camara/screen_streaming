//! The user-chosen streaming quality (resolution ceiling, fps ceiling,
//! whether to send system audio) — see the "Qualidade configurável" row in
//! CLAUDE_SESSIONS.md's architecture table. Resolution/fps here are ceilings,
//! not guarantees: capture itself always runs at the source's native
//! resolution/refresh rate (Windows Graphics Capture doesn't let us ask for
//! anything else), so downscaling and frame-rate limiting are applied on the
//! encode side, in [`crate::hw_encoding`] and [`crate::session`].

/// Sentinel `fps` value meaning "no ceiling" — encode every captured frame
/// instead of dropping ones that arrive faster than a chosen rate. Exposed
/// as "Ilimitado" in the UI, for machines fast enough to keep up with
/// whatever the source monitor's real refresh rate is.
pub const UNLIMITED_FPS: u32 = 0;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamQuality {
    /// Target output height in pixels (144/240/360/480/720/1080/1440/2160).
    /// Width is derived to keep the source's aspect ratio.
    pub resolution_height: u32,
    /// Target frames per second ceiling (15/30/60/120/...), or
    /// [`UNLIMITED_FPS`]. Frames arriving faster than this are dropped
    /// before encoding; slower sources (a static desktop, or a 60Hz
    /// monitor with a higher fps selected) just can't fill it.
    pub fps: u32,
    /// Whether to also capture and send system audio.
    pub audio: bool,
    /// Which playback device to capture system audio from (an id from
    /// `audio_capture::list_playback_devices`) — `None` keeps using
    /// whatever the system's current default output device is.
    pub audio_device_id: Option<String>,
    /// Opt-in: raises the capture/encode threads' OS scheduling priority
    /// (`THREAD_PRIORITY_ABOVE_NORMAL`) to squeeze out a bit more real-world
    /// fps under CPU contention. Off by default and only ever on by
    /// explicit user choice — it trades away part of the project's own
    /// "must not weigh down a game running alongside" requirement for
    /// smoother capture, so the user needs to be the one deciding that
    /// trade is worth it, not the app deciding it for them.
    pub boost_performance: bool,
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
    /// [`UNLIMITED_FPS`] means zero — nothing ever gets dropped on account
    /// of arriving "too soon".
    pub fn frame_interval(&self) -> std::time::Duration {
        if self.fps == UNLIMITED_FPS {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_secs_f64(1.0 / self.fps.max(1) as f64)
    }

    /// A concrete fps figure for the encoder's own internal timing
    /// configuration (time base, keyframe/GOP interval) — always a real
    /// number, even when `fps` itself is [`UNLIMITED_FPS`]. This doesn't
    /// need to be exact: it only controls how finely PTS is quantized and
    /// how often a keyframe gets forced, not how many frames actually get
    /// encoded (that's governed by [`frame_interval`](Self::frame_interval)
    /// and, when unlimited, by however fast the source really is). A fixed
    /// nominal value covering most high-refresh monitors is a reasonable
    /// default when there's no real ceiling to reuse.
    pub fn nominal_fps(&self) -> u32 {
        if self.fps == UNLIMITED_FPS { 120 } else { self.fps }
    }

    /// A reasonable H.264 bitrate for the chosen resolution. Rough,
    /// hand-picked steps rather than a formula — good enough until real
    /// network conditions call for adaptive bitrate.
    ///
    /// Higher than a natural-video bitrate chart would suggest for the same
    /// resolution: screen content (sharp text/UI edges everywhere) is
    /// substantially harder to compress cleanly than camera footage, which
    /// is mostly soft gradients. Pushed up twice now after real tests still
    /// looked blocky at 1080p+ (4K worst of all) — this second pass favors
    /// quality about as far as it reasonably goes; the UI no longer offers
    /// anything below 720p (see `index.html`), so the brackets under that
    /// are unreachable through it and only exist so this function stays
    /// total over any `resolution_height`. The real ceiling from here on is
    /// upload bandwidth, not encoder settings — going further would need
    /// per-connection adaptive bitrate, not another hand-picked bump.
    pub fn bitrate_bps(&self) -> usize {
        match self.resolution_height {
            0..=144 => 300_000,
            145..=240 => 700_000,
            241..=360 => 1_200_000,
            361..=480 => 2_500_000,
            481..=720 => 6_000_000,
            721..=1080 => 12_000_000,
            1081..=1440 => 24_000_000,
            _ => 50_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscales_keeping_aspect_ratio_and_evenness() {
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false, audio_device_id: None, boost_performance: false };
        let (w, h) = quality.target_dimensions(1920, 1080);
        assert_eq!(h, 480);
        // 1920 * 480 / 1080 = 853.33 -> 853 -> rounded up to even.
        assert_eq!(w, 854);
        assert_eq!(w % 2, 0);
    }

    #[test]
    fn never_scales_up_past_the_source() {
        let quality = StreamQuality { resolution_height: 1080, fps: 30, audio: false, audio_device_id: None, boost_performance: false };
        let (w, h) = quality.target_dimensions(640, 480);
        assert_eq!((w, h), (640, 480));
    }
}
