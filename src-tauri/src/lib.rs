mod capture;
mod encoding;
mod hw_encoding;
mod rtc;
mod session;
mod signaling_client;

/// Holds in-progress/active signaling sessions between Tauri command calls
/// (each call is a separate invocation, so the state that spans
/// "start hosting" -> "wait for peer" — or a joined session — has to live
/// somewhere outside any single command).
#[derive(Default)]
struct SessionState {
    hosting: tokio::sync::Mutex<Option<session::HostingSession>>,
    active: tokio::sync::Mutex<Option<session::Session>>,
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

/// Connects to the signaling server and requests a pairing code. Call
/// [`wait_for_peer`] next to block until someone joins with that code.
#[tauri::command]
async fn start_hosting_session(
    state: tauri::State<'_, SessionState>,
    signaling_addr: String,
) -> Result<String, String> {
    let (code, hosting) = session::start_hosting(&signaling_addr)
        .await
        .map_err(|e| e.to_string())?;
    *state.hosting.lock().await = Some(hosting);
    Ok(code)
}

/// Blocks until someone joins the session started by [`start_hosting_session`],
/// then completes the WebRTC handshake with them.
#[tauri::command]
async fn wait_for_peer(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    let hosting = state
        .hosting
        .lock()
        .await
        .take()
        .ok_or("no hosting session in progress — call start_hosting_session first")?;
    let session = hosting.wait_for_peer().await.map_err(|e| e.to_string())?;
    *state.active.lock().await = Some(session);
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
            join_signaling_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
