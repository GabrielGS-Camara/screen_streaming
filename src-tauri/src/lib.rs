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
use tauri::{Emitter, Manager};

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
    let _ = app.emit("broadcast-ended", ());
}

/// Pushes a `"viewer-count-changed"` Tauri event (payload: the new count)
/// every time the number of connected viewers changes, including once
/// right away with whatever it is when this starts (so the UI doesn't have
/// to wait for the *next* change to learn the first viewer already
/// connected). Runs until the underlying `watch::Sender` (owned by the
/// broadcast's controller task) is dropped, i.e. until the broadcast ends.
async fn watch_viewer_count(app: tauri::AppHandle, mut count: tokio::sync::watch::Receiver<u32>) {
    let _ = app.emit("viewer-count-changed", *count.borrow());
    while count.changed().await.is_ok() {
        let _ = app.emit("viewer-count-changed", *count.borrow());
    }
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

/// Lists open windows that can be captured, for the "Uma janela específica"
/// source picker.
#[tauri::command]
async fn list_capturable_windows() -> Result<Vec<capture::CapturableWindow>, String> {
    tauri::async_runtime::spawn_blocking(capture::list_capturable_windows)
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
    let quality = quality::StreamQuality { resolution_height, fps, audio, boost_performance };

    let broadcast = hosting.wait_for_peer(source, quality).await.map_err(|e| e.to_string())?;
    let ended = broadcast.ended.clone();
    let viewer_count = broadcast.watch_viewer_count();
    *state.active_broadcast.lock().await = Some(broadcast);
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
    boost_performance: bool,
) -> Result<(), String> {
    let guard = state.active_broadcast.lock().await;
    let broadcast = guard.as_ref().ok_or("not broadcasting")?;

    let source = match window_title {
        Some(title) if !title.trim().is_empty() => capture::CaptureSource::Window(title),
        _ => capture::CaptureSource::Monitor,
    };
    let quality = quality::StreamQuality { resolution_height, fps, audio, boost_performance };
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

/// The watcher's current video URL (see [`start_watching`]) — read by
/// `pip.html`, the little always-on-top window [`open_pip_window`] opens,
/// so it knows what to point its own `<img>` at.
#[tauri::command]
async fn get_watch_video_url(state: tauri::State<'_, SessionState>) -> Result<String, String> {
    state
        .watch_server
        .lock()
        .await
        .as_ref()
        .map(|server| server.url.clone())
        .ok_or_else(|| "not watching anything right now".to_owned())
}

/// Opens (or, if one's already open, replaces) a small always-on-top native
/// window showing the live stream — the "Picture-in-picture" button on
/// "Assistir". A real OS-level window rather than the browser's Document
/// Picture-in-Picture API: that API silently broke the video the first
/// time it was tried (moving the `<img>` into its own separate browsing
/// context killed the in-flight `multipart/x-mixed-replace` connection —
/// see CLAUDE_SESSIONS.md), and even after working around that, real
/// WebView2 testing showed it's just not reliable there. A plain Tauri
/// window sidesteps all of that — it's just another ordinary webview
/// loading `pip.html`, no special browser API involved, so it works the
/// same on every WebView2 version.
#[tauri::command]
async fn open_pip_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window("pip") {
        let _ = existing.close();
    }
    tauri::WebviewWindowBuilder::new(&app, "pip", tauri::WebviewUrl::App("pip.html".into()))
        .title("Screen Streaming — PiP")
        .inner_size(480.0, 270.0)
        .resizable(true)
        .always_on_top(true)
        .decorations(true)
        .skip_taskbar(true)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(SessionState::default())
        .invoke_handler(tauri::generate_handler![
            greet,
            list_capturable_windows,
            start_hosting_session,
            wait_for_peer,
            apply_broadcast_settings,
            stop_broadcast,
            join_signaling_session,
            start_watching,
            stop_watching,
            get_watch_video_url,
            open_pip_window
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
