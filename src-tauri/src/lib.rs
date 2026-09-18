mod audio_capture;
mod audio_codec;
mod audio_preview;
mod capture;
mod encoding;
mod hw_encoding;
mod quality;
mod rtc;
mod session;
mod signaling_client;
mod video_preview;

use std::sync::Arc;

// Leading `::` needed here: this is the crate root, where `mod rtc;` below
// also declares a *local* module of the same name (`crate::rtc`, our own
// WebRTC engine wrapper) — without it, `rtc::...` would be ambiguous
// between that and the external `rtc` (webrtc-rs) crate this actually
// means.
use ::rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;

/// Native OS notification — matters now that the app can run minimized to
/// the tray (see `setup_tray_and_background_close`), where the in-app
/// status text alone wouldn't be seen. Failure is silent (worst case: no
/// notification), same as every other best-effort UI touch in this file.
fn notify(app: &tauri::AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Holds in-progress/active signaling sessions between Tauri command calls
/// (each call is a separate invocation, so the state that spans
/// "start hosting" -> "wait for peer" — or a joined session — has to live
/// somewhere outside any single command).
#[derive(Default)]
struct SessionState {
    hosting: tokio::sync::Mutex<Option<session::HostingSession>>,
    active: tokio::sync::Mutex<Option<session::Session>>,
    /// The currently-live broadcast, if hosting — `None` when not
    /// broadcasting. `session::Broadcast` owns its shared capture/encode
    /// pipeline(s) and every connected viewer internally (including
    /// juggling old/new hardware encoders across "Aplicar" and fanning out
    /// to any number of simultaneous viewers), so this crate doesn't need
    /// to track pipeline handles/stop flags itself anymore — see
    /// CLAUDE_SESSIONS.md's "Multi-espectador" section.
    active_broadcast: tokio::sync::Mutex<Option<session::Broadcast>>,
    /// Whether the embedded signaling server (see
    /// [`session::spawn_local_signaling_server`]) has already been started
    /// in this process — it only needs to happen once per app run, not once
    /// per broadcast.
    signaling_server_started: tokio::sync::Mutex<bool>,
    /// The watcher's own local MJPEG server (see [`video_preview::attach_video_sink`]),
    /// kept alive only while actually watching — dropping it (on
    /// [`stop_watching`] or when the session ends) frees its local port
    /// instead of leaking it for the rest of the process.
    watch_server: tokio::sync::Mutex<Option<Arc<video_preview::MjpegServer>>>,
    /// Same idea as `watch_server`, for the watcher's local audio WebSocket
    /// server (see [`audio_preview::attach_audio_sink`]) — `None` whenever
    /// the broadcaster isn't sending audio.
    watch_audio_server: tokio::sync::Mutex<Option<Arc<audio_preview::AudioWsServer>>>,
}

/// Waits for `session::Session::ended` to fire (the peer left, or the
/// signaling connection dropped) and, when it does, clears the watch-side
/// state and tells the frontend via a Tauri event — so the broadcaster
/// leaving resets the watcher's UI instead of the app only finding out the
/// next time someone happens to call a command. See the "sair da live" fix
/// in CLAUDE_SESSIONS.md. Guest-side only now — the host side has its own
/// equivalent, [`watch_for_broadcast_end`], since it watches a
/// `session::Broadcast` instead of a `session::Session`.
async fn watch_for_watch_session_end(app: tauri::AppHandle, mut ended: tokio::sync::watch::Receiver<bool>) {
    if ended.wait_for(|v| *v).await.is_err() {
        return;
    }
    let state = app.state::<SessionState>();
    state.active.lock().await.take();
    state.watch_server.lock().await.take();
    state.watch_audio_server.lock().await.take();
    notify(&app, "Screen Streaming", "A transmissão foi encerrada pelo transmissor.");
    let _ = app.emit("watch-ended", ());
}

/// Waits for `session::Broadcast::ended` to fire (explicit "Parar
/// transmissão", or the host's own signaling connection dropping) and
/// clears the broadcast state, telling the frontend via a Tauri event.
/// Does **not** fire just because one of several viewers left — see
/// `session::run_viewer_connection`'s doc comment for why that's a
/// deliberate behavior change from the old 1:1 design.
async fn watch_for_broadcast_end(app: tauri::AppHandle, mut ended: tokio::sync::watch::Receiver<bool>) {
    if ended.wait_for(|v| *v).await.is_err() {
        return;
    }
    let state = app.state::<SessionState>();
    state.active_broadcast.lock().await.take();
    set_tray_live(&app, false);
    let _ = app.emit("broadcast-ended", ());
}

/// Pushes a `"viewer-count-changed"` Tauri event (payload: the new count)
/// every time the number of connected viewers changes, including once
/// right away with whatever it is when this starts (so the UI doesn't have
/// to wait for the *next* change to learn the first viewer already
/// connected). Runs until the underlying `watch::Sender` (owned by the
/// broadcast's controller task) is dropped, i.e. until the broadcast ends.
/// Also fires a native notification on every join/leave *after* the first
/// report (which is just the starting count, not really an "event") —
/// matters now that the window can be minimized to the tray, where the
/// in-app "N pessoas assistindo" text alone wouldn't be seen.
async fn watch_viewer_count(app: tauri::AppHandle, mut count: tokio::sync::watch::Receiver<u32>) {
    let mut previous = *count.borrow();
    let _ = app.emit("viewer-count-changed", previous);
    while count.changed().await.is_ok() {
        let current = *count.borrow();
        let _ = app.emit("viewer-count-changed", current);
        if current > previous {
            notify(&app, "Screen Streaming", &format!("Alguém entrou — {current} assistindo agora."));
        } else if current < previous {
            notify(&app, "Screen Streaming", &format!("Um espectador saiu — {current} assistindo agora."));
        }
        previous = current;
    }
}

/// What [`start_hosting_session`] hands back: the pairing code, plus every
/// address (one per network interface) the embedded signaling server is
/// reachable at, for the UI to display/copy.
#[derive(serde::Serialize)]
struct HostingInfo {
    code: String,
    addresses: Vec<session::NetworkAddress>,
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

/// Lists open windows that can be captured, for the "Uma janela específica"
/// source picker.
#[tauri::command]
async fn list_capturable_windows() -> Result<Vec<capture::CapturableWindow>, String> {
    tauri::async_runtime::spawn_blocking(capture::list_capturable_windows)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Lists playback devices system audio can be captured from, for the
/// device picker next to "Transmitir áudio do sistema".
#[tauri::command]
async fn list_audio_devices() -> Result<Vec<audio_capture::AudioDeviceInfo>, String> {
    tauri::async_runtime::spawn_blocking(audio_capture::list_playback_devices)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Starts (or reuses) this machine's embedded signaling server, connects to
/// it, and requests a pairing code — the broadcaster never has to run
/// `signaling-server` separately or know any address themselves, only
/// share the code and one of the returned addresses with whoever is
/// joining. `max_viewers` caps how many people can join with the resulting
/// code at once (`null`/omitted means unlimited). Call [`wait_for_peer`]
/// next to block until someone joins with that code.
#[tauri::command]
async fn start_hosting_session(
    state: tauri::State<'_, SessionState>,
    max_viewers: Option<u32>,
) -> Result<HostingInfo, String> {
    ensure_local_signaling_server(&state).await?;
    let addresses = session::local_signaling_urls();

    let self_addr = format!("ws://127.0.0.1:{}", session::SIGNALING_PORT);
    let (code, hosting) = session::start_hosting(&self_addr, max_viewers)
        .await
        .map_err(|e| e.to_string())?;
    *state.hosting.lock().await = Some(hosting);
    Ok(HostingInfo { code, addresses })
}

/// Blocks until the first viewer joins the session started by
/// [`start_hosting_session`], then completes the WebRTC handshake with them
/// and starts capturing + encoding + sending the chosen source at the
/// chosen quality. Any further viewers (up to whatever `max_viewers` was
/// passed to [`start_hosting_session`]) are accepted in the background from
/// then on, all sharing the same capture/encode pipeline — see
/// `session::Broadcast`.
///
/// `window_title` selects a specific window (matched by a substring of its
/// title, same as [`list_capturable_windows`]) instead of the whole primary
/// monitor when non-empty.
#[tauri::command]
async fn wait_for_peer(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    window_title: Option<String>,
    resolution_height: u32,
    fps: u32,
    audio: bool,
    audio_device_id: Option<String>,
    boost_performance: bool,
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
    let quality = quality::StreamQuality { resolution_height, fps, audio, audio_device_id, boost_performance };

    let broadcast = hosting.wait_for_peer(source, quality).await.map_err(|e| e.to_string())?;
    let ended = broadcast.ended.clone();
    let viewer_count = broadcast.watch_viewer_count();
    *state.active_broadcast.lock().await = Some(broadcast);
    set_tray_live(&app, true);
    tauri::async_runtime::spawn(watch_for_broadcast_end(app.clone(), ended));
    tauri::async_runtime::spawn(watch_viewer_count(app, viewer_count));
    Ok(())
}

/// Restarts the shared capture/encode pipeline with new source/quality
/// settings while already broadcasting — the "Aplicar alterações" button.
/// Every currently connected viewer keeps streaming on their
/// already-negotiated track (see `session::Broadcast::apply`), no
/// reconnection/re-pairing needed on their end. Gated behind an explicit
/// button (rather than applying on every settings change) so adjusting
/// several settings in a row doesn't restart the pipeline once per click.
#[tauri::command]
async fn apply_broadcast_settings(
    state: tauri::State<'_, SessionState>,
    window_title: Option<String>,
    resolution_height: u32,
    fps: u32,
    audio: bool,
    audio_device_id: Option<String>,
    boost_performance: bool,
) -> Result<(), String> {
    let guard = state.active_broadcast.lock().await;
    let broadcast = guard.as_ref().ok_or("not broadcasting")?;

    let source = match window_title {
        Some(title) if !title.trim().is_empty() => capture::CaptureSource::Window(title),
        _ => capture::CaptureSource::Monitor,
    };
    let quality = quality::StreamQuality { resolution_height, fps, audio, audio_device_id, boost_performance };
    broadcast.apply(source, quality);
    Ok(())
}

/// Stops the broadcast: ends every connected viewer's session and the
/// shared capture/encode pipeline(s) started by [`wait_for_peer`]. See
/// `session::Broadcast::stop`.
#[tauri::command]
async fn stop_broadcast(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    if let Some(broadcast) = state.active_broadcast.lock().await.take() {
        broadcast.stop();
    }
    Ok(())
}

/// Connects to the signaling server, joins the room for `code`, and
/// completes the WebRTC handshake with whoever is hosting it.
#[tauri::command]
async fn join_signaling_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    signaling_addr: String,
    code: String,
) -> Result<(), String> {
    let session = session::join_session(&signaling_addr, code)
        .await
        .map_err(|e| e.to_string())?;
    let ended = session.ended.clone();
    *state.active.lock().await = Some(session);
    tauri::async_runtime::spawn(watch_for_watch_session_end(app, ended));
    Ok(())
}

/// What [`start_watching`] hands back — the video URL is always present,
/// the audio one only when the broadcaster had "Transmitir áudio do
/// sistema" on.
#[derive(serde::Serialize)]
struct WatchUrls {
    video: String,
    audio: Option<String>,
}

/// Waits for the track(s) from the session joined by
/// [`join_signaling_session`], starts decoding them, and returns their
/// local URLs — video as an MJPEG HTTP stream the "Assistir" screen points
/// an `<img>` at (see `video_preview.rs` for why plain HTTP instead of
/// frame data pushed through Tauri's event bridge), audio (if present) as
/// a WebSocket of raw PCM chunks (see `audio_preview.rs`).
///
/// A session carries at most two tracks, negotiated together in the same
/// offer — order isn't guaranteed, so this reads from `incoming_tracks`
/// until it has the (required) video one, then gives the (optional) audio
/// one a short grace period to show up too, rather than either assuming a
/// fixed order or blocking forever when the broadcaster isn't sending audio
/// at all.
#[tauri::command]
async fn start_watching(state: tauri::State<'_, SessionState>) -> Result<WatchUrls, String> {
    let mut guard = state.active.lock().await;
    let session = guard
        .as_mut()
        .ok_or("no active session — call join_signaling_session first")?;

    let mut video_track = None;
    let mut audio_track = None;
    while video_track.is_none() {
        let track = session
            .incoming_tracks
            .recv()
            .await
            .ok_or("connection closed before a video track arrived")?;
        match track.kind().await {
            RtpCodecKind::Video => video_track = Some(track),
            RtpCodecKind::Audio => audio_track = Some(track),
            RtpCodecKind::Unspecified => {}
        }
    }
    if audio_track.is_none() {
        if let Ok(Some(track)) = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            session.incoming_tracks.recv(),
        )
        .await
        {
            if matches!(track.kind().await, RtpCodecKind::Audio) {
                audio_track = Some(track);
            }
        }
    }
    drop(guard);

    let (video_url, video_server) = video_preview::attach_video_sink(video_track.expect("checked above"))
        .await
        .map_err(|e| e.to_string())?;
    *state.watch_server.lock().await = Some(video_server);

    let audio_url = match audio_track {
        Some(track) => match audio_preview::attach_audio_sink(track).await {
            Ok((url, server)) => {
                *state.watch_audio_server.lock().await = Some(server);
                Some(url)
            }
            Err(e) => {
                eprintln!("[lib] audio track arrived but failed to start decoding it: {e}");
                None
            }
        },
        None => None,
    };

    Ok(WatchUrls { video: video_url, audio: audio_url })
}

/// Ends an in-progress "Assistir" session on purpose (the "Sair"/"Parar de
/// assistir" button) — tells the broadcaster right away (see
/// [`session::Session::notify_leaving`]) and tears down this side's local
/// preview server, instead of the only way to stop watching being to close
/// the whole app.
#[tauri::command]
async fn stop_watching(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    if let Some(session) = state.active.lock().await.take() {
        session.notify_leaving();
        let _ = session.peer_connection.close().await;
    }
    state.watch_server.lock().await.take();
    state.watch_audio_server.lock().await.take();
    Ok(())
}

/// Id the tray icon is built with — needed to look it up again later (via
/// `AppHandle::tray_by_id`) from wherever a broadcast starts/stops, to swap
/// its icon between idle and "live" (see [`set_tray_live`]).
const TRAY_ICON_ID: &str = "main-tray";

/// Badges `icon` with a small solid red circle in the bottom-right corner —
/// drawn directly into the RGBA buffer rather than shipped as a second
/// static asset file, so there's exactly one real icon file to keep in
/// sync with the app's branding. Used for the tray icon while a broadcast
/// is actually live, so closing the main window (see
/// [`setup_tray_and_background_close`]) can never make it easy to forget a
/// transmission is still running in the background.
fn with_live_badge(icon: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let (width, height) = (icon.width(), icon.height());
    let mut rgba = icon.rgba().to_vec();

    let radius = (width.min(height) as f32 * 0.32).max(3.0);
    let (cx, cy) = (width as f32 - radius - 1.0, height as f32 - radius - 1.0);
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            if dx * dx + dy * dy <= radius * radius {
                let i = ((y * width + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[235, 60, 70, 255]); // opaque red
            }
        }
    }

    tauri::image::Image::new_owned(rgba, width, height)
}

/// Swaps the tray icon between idle and "live" (red-badged) — called when
/// a broadcast actually starts streaming ([`wait_for_peer`]) and when it
/// ends ([`watch_for_broadcast_end`]). A no-op if the tray icon somehow
/// isn't there (shouldn't happen outside of tests, which don't build one).
fn set_tray_live(app: &tauri::AppHandle, live: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ICON_ID) else { return };
    let Some(default_icon) = app.default_window_icon().cloned() else { return };
    let icon = if live { with_live_badge(&default_icon) } else { default_icon };
    let _ = tray.set_icon(Some(icon));
    let _ = tray.set_tooltip(Some(if live { "Screen Streaming — transmitindo" } else { "Screen Streaming" }));
}

/// Lets the main window be closed without ending whatever's running — a
/// broadcast (or a watch session) has no reason to stop just because
/// nobody's looking at the window right now. Closing the window hides it
/// instead of exiting the process; a tray icon is what brings it back (or
/// actually quits the app) since otherwise there'd be no way to reach it
/// again short of killing the process from Task Manager.
fn setup_tray_and_background_close<R: tauri::Runtime>(
    app: &tauri::App<R>,
) -> Result<(), Box<dyn std::error::Error>> {
    let show_item = MenuItem::with_id(app, "show", "Abrir", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Sair", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    fn show_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }

    TrayIconBuilder::with_id(TRAY_ICON_ID)
        .icon(app.default_window_icon().cloned().expect("app icon configured in tauri.conf.json"))
        .menu(&menu)
        .tooltip("Screen Streaming")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } =
                event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    if let Some(window) = app.get_webview_window("main") {
        let window_to_hide = window.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window_to_hide.hide();
            }
        });
    }

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .manage(SessionState::default())
        .setup(|app| setup_tray_and_background_close(app))
        .invoke_handler(tauri::generate_handler![
            greet,
            list_capturable_windows,
            list_audio_devices,
            start_hosting_session,
            wait_for_peer,
            apply_broadcast_settings,
            stop_broadcast,
            join_signaling_session,
            start_watching,
            stop_watching
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
