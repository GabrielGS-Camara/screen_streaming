//! System audio loopback capture via WASAPI (crate `wasapi`) — captures
//! whatever this machine is currently playing (the default output device's
//! own mix), the audio-side equivalent of `capture.rs`'s screen capture.
//! Used when the user enables "Transmitir áudio do sistema".
//!
//! Loopback here means the *whole* system mix (every app's output, cursor
//! sounds included). Windows also supports capturing just one process's
//! audio tree — `wasapi::AudioClient::new_application_loopback_client`,
//! already available in this same crate — which is what a future "exclude
//! just Discord" feature (asked about, confirmed viable, not implemented —
//! see CLAUDE_SESSIONS.md) would build on instead of this whole-system
//! capture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20ms per chunk — a standard Opus frame size (Opus only accepts 2.5/5/10/
/// 20/40/60ms), and short enough not to add noticeable extra latency next
/// to the video pipeline's own low-latency tuning (see `hw_encoding.rs`).
pub const FRAME_SAMPLES_PER_CHANNEL: usize = 960;
const FRAME_SAMPLES_TOTAL: usize = FRAME_SAMPLES_PER_CHANNEL * CHANNELS;

/// One fixed-size chunk of interleaved i16 PCM — exactly one Opus frame's
/// worth (`FRAME_SAMPLES_PER_CHANNEL` samples per channel).
pub struct CapturedAudio {
    pub samples: Vec<i16>,
}

/// Captures system audio indefinitely, sending fixed-size PCM chunks
/// through `tx` until `stop` is set or the receiving end goes away. Runs
/// synchronously (blocks the calling thread) — call from a dedicated
/// thread, same convention as `capture::capture_frames_until_stopped`.
///
/// `tx` is bounded for the same reason the screen-capture channel is (see
/// `capture.rs`'s `FrameStream`): a consumer (the Opus encoder) that falls
/// behind should skip old audio instead of building a growing backlog —
/// a dropped chunk is a brief click, a backlog is audio that keeps sliding
/// further out of sync and never recovers.
pub fn capture_system_audio_until_stopped(
    stop: Arc<AtomicBool>,
    tx: std::sync::mpsc::SyncSender<CapturedAudio>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    wasapi::initialize_mta().ok()?;

    let enumerator = DeviceEnumerator::new()?;
    // `Direction::Render` (not `Capture`) on the *default playback* device
    // is what actually puts WASAPI into loopback mode below — capturing
    // what's being played, not what a microphone hears. This non-obvious
    // pairing is documented in the `wasapi` crate's own `record.rs`
    // example ("use `Direction::Render` for loopback mode"), not something
    // guessed here.
    let device = enumerator.get_default_device(&Direction::Render)?;
    let mut audio_client = device.get_iaudioclient()?;

    let desired_format =
        WaveFormat::new(16, 16, &SampleType::Int, SAMPLE_RATE as usize, CHANNELS, None);
    let blockalign = desired_format.get_blockalign() as usize;
    let chunk_bytes = FRAME_SAMPLES_PER_CHANNEL * blockalign;

    let (_default_period, min_period) = audio_client.get_device_period()?;
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: min_period };
    audio_client.initialize_client(&desired_format, &Direction::Render, &mode)?;

    let h_event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    let mut byte_queue: VecDeque<u8> = VecDeque::new();

    audio_client.start_stream()?;

    while !stop.load(Ordering::Relaxed) {
        capture_client.read_from_device_to_deque(&mut byte_queue)?;

        while byte_queue.len() >= chunk_bytes {
            let bytes: Vec<u8> = byte_queue.drain(..chunk_bytes).collect();
            let samples: Vec<i16> = bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            debug_assert_eq!(samples.len(), FRAME_SAMPLES_TOTAL);

            match tx.try_send(CapturedAudio { samples }) {
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    let _ = audio_client.stop_stream();
                    return Ok(());
                }
            }
        }

        // 1s timeout: generous compared to the 10-20ms device period, just
        // enough to notice a stalled device instead of blocking forever.
        if h_event.wait_for_event(1000).is_err() {
            break;
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not run in CI (no real audio device) — run manually with
    /// `cargo test -- --ignored --nocapture` on a real machine. Doesn't
    /// require anything to actually be playing: even silence produces
    /// real (all-zero) chunks at the right rate, which is enough to prove
    /// the WASAPI loopback pipeline itself works end-to-end.
    #[test]
    #[ignore]
    fn captures_real_system_audio_chunks() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let stop_for_thread = stop.clone();
        let capture_thread =
            std::thread::spawn(move || capture_system_audio_until_stopped(stop_for_thread, tx));

        let mut chunks = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                assert_eq!(chunk.samples.len(), FRAME_SAMPLES_TOTAL);
                chunks += 1;
            }
        }

        stop.store(true, Ordering::Relaxed);
        drop(rx);
        capture_thread
            .join()
            .expect("capture thread panicked")
            .expect("capture failed");

        println!("captured {chunks} chunk(s) of {FRAME_SAMPLES_PER_CHANNEL} samples/channel in ~2s");
        assert!(chunks > 0, "expected at least one real audio chunk from WASAPI loopback");
    }
}
