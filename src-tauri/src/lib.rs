mod capture;
mod encoding;
mod hw_encoding;
mod quality;
mod rtc;
mod session;
mod signaling_client;
mod video_preview;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Holds in-progress/active signaling sessions between Tauri command calls
/// (each call is a separate invocation, so the state that spans
/// "start hosting" -> "wait for peer" — or a joined session — has to live
/// somewhere outside any single command).
#[derive(Default)]
struct SessionState {
    hosting: tokio::sync::Mutex<Option<session::HostingSession>>,
    active: tokio::sync::Mutex<Option<session::Session>>,
    /// Set to stop the host-side capture pipeline started by
    /// [`wait_for_peer`] — e.g. from a future "stop transmission" button.
    broadcast_stop: tokio::sync::Mutex<Option<Arc<AtomicBool>>>,
    /// Turns the broadcaster's own live preview on/off — `Some(server)`
    /// while the "Ver prévia" toggle is on, `None` otherwise. Read by the
    /// capture/encode pipeline on every frame (see
    /// `session::attach_video_source`), written by
    /// [`start_broadcast_preview`]/[`stop_broadcast_preview`].
    broadcast_preview: tokio::sync::Mutex<
        Option<tokio::sync::watch::Sender<Option<Arc<video_preview::MjpegServer>>>>,
    >,
    /// Whether the embedded signaling server (see
    /// [`session::spawn_local_signaling_server`]) has already been started
    /// in this process — it only needs to happen once per app run, not once
    /// per broadcast.
    signaling_server_started: tokio::sync::Mutex<bool>,
}

/// What [`start_hosting_session`] hands back: the pairing code, plus every
/// address (one per network interface) the embedded signaling server is
/// reachable at, for the UI to display/copy.
#[derive(serde::Serialize)]
struct HostingInfo {
    code: String,
    addresses: Vec<String>,
}

/// Starts the embedded signaling server the first time any broadcast is
/// started in this process; a no-op on later calls.
async fn ensure_local_signaling_server(state: &SessionState) -> Result<(), String> {
    let mut started = state.signaling_server_started.lock().await;
    if *started {
        return Ok(());
    }
    session::spawn_local_signaling_server()
        .await
        .map_err(|e| e.to_string())?;
    *started = true;
    Ok(())
}

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// Temporary diagnostic command: captures the primary monitor for a few
/// seconds and reports the achieved frame rate. Used to validate the
/// Windows Graphics Capture pipeline on the current machine; will be
/// replaced once real capture/streaming controls exist in the UI.
#[tauri::command]
async fn benchmark_capture() -> Result<capture::CaptureStats, String> {
    tauri::async_runtime::spawn_blocking(|| capture::benchmark_primary_monitor_capture(3))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Temporary diagnostic command: lists open windows that can be captured,
/// for the future "share a specific app" source picker.
#[tauri::command]
async fn list_capturable_windows() -> Result<Vec<capture::CapturableWindow>, String> {
    tauri::async_runtime::spawn_blocking(capture::list_capturable_windows)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Temporary diagnostic command: captures a specific window (matched by a
/// substring of its title) for a few seconds and reports the achieved
/// frame rate.
#[tauri::command]
async fn benchmark_window_capture(title_contains: String) -> Result<capture::CaptureStats, String> {
    tauri::async_runtime::spawn_blocking(move || {
        capture::benchmark_window_capture(&title_contains, 3)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Starts (or reuses) this machine's embedded signaling server, connects to
/// it, and requests a pairing code — the broadcaster never has to run
/// `signaling-server` separately or know any address themselves, only
/// share the code and one of the returned addresses with whoever is
/// joining. Call [`wait_for_peer`] next to block until someone joins with
/// that code.
#[tauri::command]
async fn start_hosting_session(state: tauri::State<'_, SessionState>) -> Result<HostingInfo, String> {
    ensure_local_signaling_server(&state).await?;
    let addresses = session::local_signaling_urls();

    let self_addr = format!("ws://127.0.0.1:{}", session::SIGNALING_PORT);
    let (code, hosting) = session::start_hosting(&self_addr)
        .await
        .map_err(|e| e.to_string())?;
    *state.hosting.lock().await = Some(hosting);
    Ok(HostingInfo { code, addresses })
}

/// Blocks until someone joins the session started by [`start_hosting_session`],
/// then completes the WebRTC handshake with them and starts capturing +
/// encoding + sending the chosen source at the chosen quality.
///
/// `window_title` selects a specific window (matched by a substring of its
/// title, same as [`list_capturable_windows`]/[`benchmark_window_capture`])
/// instead of the whole primary monitor when non-empty.
#[tauri::command]
async fn wait_for_peer(
    state: tauri::State<'_, SessionState>,
    window_title: Option<String>,
    resolution_height: u32,
    fps: u32,
    audio: bool,
) -> Result<(), String> {
    let hosting = state
        .hosting
        .lock()
        .await
        .take()
        .ok_or("no hosting session in progress — call start_hosting_session first")?;

    let source = match window_title {
        Some(title) if !title.trim().is_empty() => capture::CaptureSource::Window(title),
        _ => capture::CaptureSource::Monitor,
    };
    let quality = quality::StreamQuality { resolution_height, fps, audio };
    let stop = Arc::new(AtomicBool::new(false));
    *state.broadcast_stop.lock().await = Some(stop.clone());

    let (preview_tx, preview_rx) = tokio::sync::watch::channel(None);
    *state.broadcast_preview.lock().await = Some(preview_tx);

    let session = hosting
        .wait_for_peer(source, quality, stop, preview_rx)
        .await
        .map_err(|e| e.to_string())?;
    *state.active.lock().await = Some(session);
    Ok(())
}

/// Turns on the broadcaster's own live preview (of the outgoing capture,
/// not the encoded/decoded round trip) and returns its MJPEG URL. Only
/// valid while broadcasting — i.e. after [`wait_for_peer`] has succeeded.
#[tauri::command]
async fn start_broadcast_preview(state: tauri::State<'_, SessionState>) -> Result<String, String> {
    let guard = state.broadcast_preview.lock().await;
    let preview_tx = guard
        .as_ref()
        .ok_or("not broadcasting — start a transmission first")?;
    let server = Arc::new(
        video_preview::MjpegServer::start()
            .await
            .map_err(|e| e.to_string())?,
    );
    let url = server.url.clone();
    let _ = preview_tx.send(Some(server));
    Ok(url)
}

/// Turns the broadcaster's own live preview back off.
#[tauri::command]
async fn stop_broadcast_preview(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    if let Some(preview_tx) = state.broadcast_preview.lock().await.as_ref() {
        let _ = preview_tx.send(None);
    }
    Ok(())
}

/// Stops the host-side capture/encode pipeline started by [`wait_for_peer`]
/// and closes the peer connection.
#[tauri::command]
async fn stop_broadcast(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    if let Some(stop) = state.broadcast_stop.lock().await.take() {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    state.broadcast_preview.lock().await.take();
    if let Some(session) = state.active.lock().await.take() {
        let _ = session.peer_connection.close().await;
    }
    Ok(())
}

/// Connects to the signaling server, joins the room for `code`, and
/// completes the WebRTC handshake with whoever is hosting it.
#[tauri::command]
async fn join_signaling_session(
    state: tauri::State<'_, SessionState>,
    signaling_addr: String,
    code: String,
) -> Result<(), String> {
    let session = session::join_session(&signaling_addr, code)
        .await
        .map_err(|e| e.to_string())?;
    *state.active.lock().await = Some(session);
    Ok(())
}

/// Waits for the video track from the session joined by
/// [`join_signaling_session`], starts decoding it, and returns the local
/// URL of the MJPEG preview stream the "Assistir" screen points an `<img>`
/// at — see `video_preview.rs` for why the frontend gets a plain HTTP URL
/// instead of frame data pushed through Tauri's event bridge.
#[tauri::command]
async fn start_watching(state: tauri::State<'_, SessionState>) -> Result<String, String> {
    let mut guard = state.active.lock().await;
    let session = guard
        .as_mut()
        .ok_or("no active session — call join_signaling_session first")?;
    let track = session
        .incoming_tracks
        .recv()
        .await
        .ok_or("connection closed before a video track arrived")?;
    video_preview::attach_video_sink(track)
        .await
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(SessionState::default())
        .invoke_handler(tauri::generate_handler![
            greet,
            benchmark_capture,
            list_capturable_windows,
            benchmark_window_capture,
            start_hosting_session,
            wait_for_peer,
            stop_broadcast,
            start_broadcast_preview,
            stop_broadcast_preview,
            join_signaling_session,
            start_watching
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
