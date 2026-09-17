mod capture;
mod rtc;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            benchmark_capture,
            list_capturable_windows,
            benchmark_window_capture
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
