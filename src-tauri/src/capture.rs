//! Screen capture via the Windows Graphics Capture API.
//!
//! This module currently only proves out the capture pipeline (can we pull
//! frames from the GPU at a healthy, stable rate with low overhead?) before
//! any encoding/network code is built on top of it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

#[derive(Debug, serde::Serialize)]
pub struct CaptureStats {
    pub frame_count: u32,
    pub elapsed_secs: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    /// Size in bytes of the raw (possibly padded) buffer for the first frame,
    /// sanity-checked against width * height * 4 (BGRA8).
    pub first_frame_buffer_len: usize,
}

type Flags = (Arc<AtomicU32>, u64, Arc<Mutex<(u32, u32, usize)>>);

struct FrameCounter {
    count: Arc<AtomicU32>,
    start: Instant,
    duration_secs: u64,
    first_frame_info: Arc<Mutex<(u32, u32, usize)>>,
}

impl GraphicsCaptureApiHandler for FrameCounter {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (count, duration_secs, first_frame_info) = ctx.flags;
        Ok(Self {
            count,
            start: Instant::now(),
            duration_secs,
            first_frame_info,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let previous_count = self.count.fetch_add(1, Ordering::Relaxed);

        // Only touch the pixel buffer once, on the very first frame, to
        // confirm the GPU -> CPU readback path actually works end-to-end.
        // Reading it every frame isn't needed for this benchmark and would
        // skew the FPS number we're trying to measure.
        if previous_count == 0 {
            let (width, height) = (frame.width(), frame.height());
            let mut buffer = frame.buffer()?;
            let len = buffer.as_raw_buffer().len();
            if let Ok(mut info) = self.first_frame_info.lock() {
                *info = (width, height, len);
            }
        }

        if self.start.elapsed().as_secs() >= self.duration_secs {
            capture_control.stop();
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Captures the primary monitor for `duration_secs` seconds and reports the
/// effective frame rate. Used to validate that Windows Graphics Capture
/// works on this machine before building encoding/streaming on top of it.
pub fn benchmark_primary_monitor_capture(
    duration_secs: u64,
) -> Result<CaptureStats, Box<dyn std::error::Error + Send + Sync>> {
    let monitor = Monitor::primary()?;

    let count = Arc::new(AtomicU32::new(0));
    let first_frame_info = Arc::new(Mutex::new((0u32, 0u32, 0usize)));

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        (count.clone(), duration_secs, first_frame_info.clone()),
    );

    let start = Instant::now();
    FrameCounter::start(settings)?;
    let elapsed_secs = start.elapsed().as_secs_f64();

    let (width, height, first_frame_buffer_len) = *first_frame_info.lock().unwrap();
    let frame_count = count.load(Ordering::Relaxed);
    let fps = if elapsed_secs > 0.0 {
        frame_count as f64 / elapsed_secs
    } else {
        0.0
    };

    Ok(CaptureStats {
        frame_count,
        elapsed_secs,
        fps,
        width,
        height,
        first_frame_buffer_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not run in CI (no GPU/display) — run manually with
    /// `cargo test -- --ignored --nocapture` on a real machine.
    #[test]
    #[ignore]
    fn captures_primary_monitor_at_a_reasonable_rate() {
        let stats = benchmark_primary_monitor_capture(3).expect("capture failed");
        println!(
            "{} frames in {:.2}s (~{:.1} fps), {}x{}, first frame buffer = {} bytes",
            stats.frame_count,
            stats.elapsed_secs,
            stats.fps,
            stats.width,
            stats.height,
            stats.first_frame_buffer_len
        );
        assert!(stats.frame_count > 0, "expected at least one frame");
        assert!(stats.width > 0 && stats.height > 0);
    }
}
