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
use windows_capture::window::Window;

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
///
/// Note: this isn't generic over the capture source (monitor vs. window)
/// because `windows-capture`'s internal `GraphicsCaptureItemType` — what a
/// shared helper's generic parameter would need to convert into — isn't
/// public, so callers can't name that bound. Each source gets its own thin
/// function below instead.
pub fn benchmark_primary_monitor_capture(
    duration_secs: u64,
) -> Result<CaptureStats, Box<dyn std::error::Error + Send + Sync>> {
    let count = Arc::new(AtomicU32::new(0));
    let first_frame_info = Arc::new(Mutex::new((0u32, 0u32, 0usize)));

    let settings = Settings::new(
        Monitor::primary()?,
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

#[derive(Debug, serde::Serialize)]
pub struct CapturableWindow {
    pub title: String,
    pub process_name: String,
    pub width: u32,
    pub height: u32,
}

/// Lists open windows that Windows Graphics Capture can actually target
/// (visible, not a tool window, not a child window), for a future "pick a
/// window to share" UI.
pub fn list_capturable_windows(
) -> Result<Vec<CapturableWindow>, Box<dyn std::error::Error + Send + Sync>> {
    let windows = Window::enumerate()?;

    let mut result = Vec::new();
    for window in windows {
        if !window.is_valid() {
            continue;
        }

        let title = window.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }

        result.push(CapturableWindow {
            title,
            process_name: window.process_name().unwrap_or_default(),
            width: window.width().unwrap_or(0).max(0) as u32,
            height: window.height().unwrap_or(0).max(0) as u32,
        });
    }

    Ok(result)
}

/// Captures a specific window (matched by a substring of its title, same as
/// what a "pick a window" dropdown fed by [`list_capturable_windows`] would
/// pass) for `duration_secs` seconds and reports the effective frame rate.
pub fn benchmark_window_capture(
    title_contains: &str,
    duration_secs: u64,
) -> Result<CaptureStats, Box<dyn std::error::Error + Send + Sync>> {
    run_window_benchmark(Window::from_contains_name(title_contains)?, duration_secs)
}

/// Captures whatever window is currently in the foreground. Only used by
/// the manual smoke test below (doesn't depend on any particular app being
/// open by title, unlike `benchmark_window_capture`).
#[cfg(test)]
fn benchmark_foreground_window_capture(
    duration_secs: u64,
) -> Result<CaptureStats, Box<dyn std::error::Error + Send + Sync>> {
    run_window_benchmark(Window::foreground()?, duration_secs)
}

/// Shared by both window-based benchmarks above (unlike the primary-monitor
/// one, this can be a plain helper since `Window` is a single concrete
/// type — no generic bound needed).
fn run_window_benchmark(
    window: Window,
    duration_secs: u64,
) -> Result<CaptureStats, Box<dyn std::error::Error + Send + Sync>> {
    let count = Arc::new(AtomicU32::new(0));
    let first_frame_info = Arc::new(Mutex::new((0u32, 0u32, 0usize)));

    let settings = Settings::new(
        window,
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

/// One captured frame: dimensions plus a tightly-packed RGBA8 buffer (row
/// padding already stripped — see [`windows_capture::frame::FrameBuffer::as_nopadding_buffer`]).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct FrameSink {
    tx: std::sync::mpsc::Sender<CapturedFrame>,
    start: Instant,
    duration_secs: u64,
}

impl GraphicsCaptureApiHandler for FrameSink {
    type Flags = (std::sync::mpsc::Sender<CapturedFrame>, u64);
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (tx, duration_secs) = ctx.flags;
        Ok(Self {
            tx,
            start: Instant::now(),
            duration_secs,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (width, height) = (frame.width(), frame.height());
        let buffer = frame.buffer()?;
        let mut packed = Vec::new();
        let rgba = buffer.as_nopadding_buffer(&mut packed).to_vec();

        // The receiving end may have stopped listening (e.g. it only wanted
        // the first few frames) — that's not a capture error, just stop.
        if self
            .tx
            .send(CapturedFrame {
                width,
                height,
                rgba,
            })
            .is_err()
        {
            capture_control.stop();
            return Ok(());
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

/// Captures the primary monitor for up to `duration_secs` seconds, sending
/// each frame's dimensions and tightly-packed RGBA8 pixels through `tx` as
/// they arrive. Runs synchronously (blocks the calling thread until the
/// capture session ends) — same as the other capture functions in this
/// module, so call it from a dedicated thread when driving it from async
/// code (e.g. to feed a video encoder without blocking the WebRTC runtime).
pub fn capture_primary_monitor_frames(
    duration_secs: u64,
    tx: std::sync::mpsc::Sender<CapturedFrame>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let settings = Settings::new(
        Monitor::primary()?,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        (tx, duration_secs),
    );

    FrameSink::start(settings)?;
    Ok(())
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

    /// Not run in CI (no GPU/display) — run manually with
    /// `cargo test -- --ignored --nocapture` on a real machine, with some
    /// window in the foreground.
    #[test]
    #[ignore]
    fn captures_foreground_window_at_a_reasonable_rate() {
        let stats = benchmark_foreground_window_capture(3).expect("capture failed");
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

    #[test]
    #[ignore]
    fn lists_at_least_one_capturable_window() {
        let windows = list_capturable_windows().expect("failed to list windows");
        for w in &windows {
            println!(
                "{:>6}x{:<6} {:<24} {}",
                w.width, w.height, w.process_name, w.title
            );
        }
        assert!(!windows.is_empty(), "expected at least one open window");
    }
}
