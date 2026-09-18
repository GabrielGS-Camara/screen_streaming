//! Bridges [`crate::signaling_client`] to real [`PeerConnection`]s (built via
//! [`crate::rtc::build_peer_connection`]): host or join a pairing-code
//! session on the signaling server, exchange offer/answer/ICE through it —
//! real network relay, not the in-process forwarding `rtc.rs`'s own tests
//! use — and hand back a negotiating/connected `PeerConnection` the rest of
//! the app can attach tracks/data channels to.
//!
//! On the host side, a room can now have any number of simultaneous
//! viewers (see `signaling-server`'s protocol): capture/encoding stays a
//! single shared pipeline (never duplicated per viewer, or every extra
//! person watching would cost another GPU encode pass) whose packets fan
//! out over a `tokio::sync::broadcast` channel to one independent
//! `PeerConnection` per viewer. See [`Broadcast`], the type `lib.rs` gets
//! back once the first viewer connects.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bytes::Bytes;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, RTCConfiguration, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCSdpType, RTCSessionDescription,
};

use crate::audio_capture;
use crate::audio_codec;
use crate::capture::{self, CaptureSource};
use crate::hw_encoding::HardwareH264Encoder;
use crate::quality::StreamQuality;
use crate::rtc::{build_peer_connection, h264_codec_parameters, opus_codec_parameters};
use crate::signaling_client::{self, ClientMessage, SignalingEvent};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Identifies one viewer within a broadcast — scoped to that broadcast only
/// (assigned by `signaling-server` in join order), not globally unique.
type GuestId = u32;

/// One relayed signaling message: either the SDP offer/answer or a single
/// ICE candidate. Tagged so the receiving side can tell which apart from
/// connection phase (early candidates can arrive before the SDP does).
///
/// The `Sdp` variant carries just `sdp_type` + `sdp` (not a whole
/// `RTCSessionDescription`) on purpose: that type has a `parsed` field
/// cache that's `#[serde(skip)]`, only ever populated by its own
/// `RTCSessionDescription::offer`/`::answer` constructors (which parse the
/// SDP text via `unmarshal()`). Round-tripping the *whole* struct through
/// serde on the receiving end silently produces one with `parsed: None`,
/// and `set_remote_description` on such a value hangs forever instead of
/// erroring — cost real time to track down, see CLAUDE_SESSIONS.md.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RelayPayload {
    Sdp {
        #[serde(rename = "type")]
        sdp_type: RTCSdpType,
        sdp: String,
    },
    IceCandidate(RTCIceCandidateInit),
}

impl RelayPayload {
    fn from_description(desc: &RTCSessionDescription) -> Self {
        RelayPayload::Sdp { sdp_type: desc.sdp_type, sdp: desc.sdp.clone() }
    }
}

/// Reconstructs a proper `RTCSessionDescription` from relayed
/// `sdp_type`/`sdp`, via the type's own constructors so its internal
/// `parsed` cache actually gets populated — see [`RelayPayload`]'s doc
/// comment for why that matters.
fn session_description_from_parts(sdp_type: RTCSdpType, sdp: String) -> Result<RTCSessionDescription, BoxError> {
    Ok(match sdp_type {
        RTCSdpType::Offer => RTCSessionDescription::offer(sdp)?,
        RTCSdpType::Answer => RTCSessionDescription::answer(sdp)?,
        RTCSdpType::Pranswer => RTCSessionDescription::pranswer(sdp)?,
        RTCSdpType::Rollback => RTCSessionDescription::rollback(Some(sdp))?,
        RTCSdpType::Unspecified => return Err("relayed SDP with an unspecified type".into()),
    })
}

fn stun_config() -> RTCConfiguration {
    RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }])
        .build()
}

/// Local IPv4 addresses worth gathering ICE candidates from — every real
/// interface except link-local (169.254.0.0/16, the address Windows hands
/// an adapter when DHCP fails, e.g. a disconnected virtual adapter). Used
/// instead of binding the "any interface" wildcard `0.0.0.0:0`: this
/// crate has no way to filter out a bad interface *during* gathering (see
/// [`crate::rtc::build_peer_connection`]'s doc comment), and on a
/// real machine — VPN clients, Docker, Hyper-V, WSL all add virtual
/// adapters — one dead interface in the wildcard scan can hang gathering
/// forever instead of just not producing a candidate for that interface.
pub(crate) fn usable_local_addrs() -> Vec<String> {
    let interfaces = local_ip_address::list_afinet_netifas().unwrap_or_default();
    let mut addrs: Vec<String> = interfaces
        .into_iter()
        .filter_map(|(_, ip)| match ip {
            std::net::IpAddr::V4(v4) if !v4.is_link_local() => Some(format!("{v4}:0")),
            _ => None,
        })
        .collect();
    addrs.sort();
    addrs.dedup();
    if addrs.is_empty() {
        // Better to try the wildcard than to gather from nothing at all.
        addrs.push("0.0.0.0:0".to_owned());
    }
    eprintln!("[session] usable local addrs: {addrs:?}");
    addrs
}

/// Port the embedded signaling server listens on — same default the
/// standalone `signaling-server` binary uses (`SIGNALING_ADDR` env var
/// default), so a manually-run instance and the embedded one are
/// interchangeable from a client's point of view.
pub const SIGNALING_PORT: u16 = 9876;

/// Starts the signaling server **embedded in this process**, listening on
/// every interface (`0.0.0.0`). A broadcaster runs this automatically when
/// they start hosting — see [`crate::lib`]'s `start_hosting_session`
/// command — so nobody ever has to separately run the `signaling-server`
/// binary in a terminal to use the app, matching the product requirement
/// of not needing to "create a server" the way Discord does.
pub async fn spawn_local_signaling_server() -> Result<(), BoxError> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", SIGNALING_PORT)).await?;
    tokio::spawn(signaling_server::serve(listener));
    Ok(())
}

/// Every `ws://` address (one per real, non-loopback network interface)
/// this machine's embedded signaling server is reachable at — what a
/// broadcaster shows the person joining so they know what to paste into
/// "Assistir". Multiple entries are normal (Wi-Fi + Ethernet + a VPN
/// adapter, etc.); the user picks whichever one the other computer can
/// actually reach.
pub fn local_signaling_urls() -> Vec<String> {
    let interfaces = local_ip_address::list_afinet_netifas().unwrap_or_default();
    let mut urls: Vec<String> = interfaces
        .into_iter()
        .filter_map(|(_, ip)| match ip {
            std::net::IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_loopback() => {
                Some(format!("ws://{v4}:{SIGNALING_PORT}"))
            }
            _ => None,
        })
        .collect();
    urls.sort();
    urls.dedup();
    urls
}

fn new_media_engine() -> Result<MediaEngine, BoxError> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(h264_codec_parameters(), RtpCodecKind::Video)?;
    // Registered unconditionally on both sides, same as the video codec —
    // whether an audio *track* actually shows up in a given session's SDP
    // depends only on the host's "Transmitir áudio do sistema" toggle (see
    // `HostingSession::wait_for_peer`), not on what codecs either side
    // merely knows how to speak.
    media_engine.register_codec(opus_codec_parameters(), RtpCodecKind::Audio)?;
    Ok(media_engine)
}

/// A live WebRTC session paired through the signaling server: offer/answer
/// and ICE already exchanged, ICE candidates continuing to flow in the
/// background for the connection's lifetime. Used only on the **guest**
/// side now — the host side hands back a [`Broadcast`] instead, since it
/// may have any number of these live at once (one per viewer), all fed by
/// one shared capture/encode pipeline.
pub struct Session {
    pub peer_connection: Arc<dyn PeerConnection>,
    pub incoming_tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
    /// Flips to `true` once the other side leaves (signaling reports
    /// `PeerLeft`) or the signaling connection itself drops — see
    /// [`pump_remaining_signaling`]. `lib.rs` watches this to know when to
    /// reset the UI/state on its own, instead of only reacting to a button
    /// click — see the "sair da live" fix in CLAUDE_SESSIONS.md.
    pub ended: watch::Receiver<bool>,
    signaling_tx: mpsc::UnboundedSender<ClientMessage>,
}

impl Session {
    /// Tells the other side this session is ending on purpose (see
    /// [`ClientMessage::Leave`]) — call before/alongside closing
    /// `peer_connection`, from a "parar transmissão" or "sair"/"parar de
    /// assistir" action, so the other side finds out immediately instead of
    /// only once its own connection times out or the whole app closes. The
    /// signaling server ends this side's own signaling connection right
    /// after relaying it (see `signaling-server`'s `ClientMessage::Leave`),
    /// which is fine — nothing here reuses it afterwards.
    pub fn notify_leaving(&self) {
        let _ = self.signaling_tx.send(ClientMessage::Leave);
    }
}

/// Returned by [`start_hosting`] once a pairing code exists, before anyone
/// has joined. Split into two steps (rather than one function that blocks
/// until a peer shows up) so the caller can display the code immediately
/// — the real UI needs exactly this: show the code, *then* wait.
pub struct HostingSession {
    signaling_tx: mpsc::UnboundedSender<ClientMessage>,
    signaling_rx: mpsc::UnboundedReceiver<SignalingEvent>,
    max_viewers: Option<u32>,
}

/// Connects to the signaling server and requests a new pairing code.
/// `max_viewers` caps how many guests can join with the resulting code at
/// once — `None` means no cap. Returns as soon as the code is issued —
/// call [`HostingSession::wait_for_peer`] to block until someone joins and
/// finish the WebRTC handshake.
pub async fn start_hosting(
    signaling_addr: &str,
    max_viewers: Option<u32>,
) -> Result<(String, HostingSession), BoxError> {
    let (signaling_tx, mut signaling_rx) = signaling_client::connect(signaling_addr).await?;
    signaling_tx
        .send(ClientMessage::Host { max_viewers })
        .map_err(|_| "signaling channel closed")?;

    let code = match signaling_rx.recv().await {
        Some(SignalingEvent::Hosting(code)) => code,
        Some(SignalingEvent::Error(message)) => return Err(format!("signaling error: {message}").into()),
        other => return Err(format!("unexpected signaling response: {other:?}").into()),
    };

    Ok((code, HostingSession { signaling_tx, signaling_rx, max_viewers }))
}

impl HostingSession {
    /// Blocks until the **first** viewer joins with the pairing code and
    /// completes the offer/answer/ICE handshake — same UX as before, the
    /// UI only says "Conectado — transmitindo." once this returns. From
    /// then on, any further viewers (up to `max_viewers`, enforced by
    /// `signaling-server` itself) are accepted in the background for as
    /// long as the returned [`Broadcast`] lives, all sharing the same
    /// capture/encode pipeline this starts. `source`/`quality` control
    /// what gets captured/encoded and at what resolution/fps ceiling.
    pub async fn wait_for_peer(
        mut self,
        source: CaptureSource,
        quality: StreamQuality,
    ) -> Result<Broadcast, BoxError> {
        let first_guest_id = loop {
            match self.signaling_rx.recv().await {
                Some(SignalingEvent::Paired { guest_id: Some(id) }) => break id,
                Some(SignalingEvent::Error(message)) => {
                    return Err(format!("signaling error: {message}").into());
                }
                Some(SignalingEvent::Disconnected) => {
                    return Err("lost connection to the signaling server".into());
                }
                other => return Err(format!("unexpected signaling response while waiting: {other:?}").into()),
            }
        };
        eprintln!("[session/host] first viewer paired (guest {first_guest_id})");

        let pipeline = spawn_shared_video_pipeline(source, quality);
        // Audio (if the user turned "Transmitir áudio do sistema" on) is
        // its own shared pipeline, same fan-out idea — a viewer just
        // doesn't get an audio track/forwarder if this is `None` (see
        // `establish_viewer`). Toggling audio on/off later via "Aplicar"
        // isn't supported (unlike resolution/fps) precisely because it
        // would need a full renegotiation round trip per viewer — see
        // `lib.rs`'s `apply_broadcast_settings`.
        let audio_pipeline = if quality.audio { Some(spawn_shared_audio_pipeline()) } else { None };

        let state: SharedState = Arc::new(tokio::sync::Mutex::new(Shared {
            pipeline: Some(pipeline),
            audio_pipeline,
            guest_events: HashMap::new(),
            viewers: HashMap::new(),
        }));

        let (viewer_count_tx, viewer_count_rx) = watch::channel(0u32);
        let (ended_tx, ended_rx) = watch::channel(false);
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();

        let (guest_tx, mut guest_rx) = mpsc::unbounded_channel();
        state.lock().await.guest_events.insert(first_guest_id, guest_tx);

        let signaling_tx = self.signaling_tx.clone();
        // Owns `self.signaling_rx` from here on — routes each guest's
        // relayed messages to its own channel, spawns a task per new
        // viewer, and handles `apply`/`stop` commands. Started *before*
        // the first viewer's handshake finishes, since that handshake's
        // own SDP answer/ICE candidates only reach it once this loop is
        // routing them.
        tokio::spawn(run_broadcast_controller(
            state.clone(),
            self.signaling_rx,
            signaling_tx.clone(),
            commands_rx,
            viewer_count_tx.clone(),
            ended_tx,
        ));

        let pc = match establish_viewer(first_guest_id, &state, &signaling_tx, &mut guest_rx, &viewer_count_tx).await
        {
            Ok(pc) => pc,
            Err(e) => {
                // Nobody will ever hold the `Broadcast` this would have
                // returned, so nothing else would ever stop the
                // controller/pipeline just started above — tell it to
                // shut down itself instead of leaking a capture/encode
                // pipeline that runs forever for nobody.
                let _ = commands_tx.send(Command::Stop);
                return Err(e);
            }
        };
        tokio::spawn(run_viewer_connection(first_guest_id, pc, state, guest_rx, viewer_count_tx.clone()));

        Ok(Broadcast {
            max_viewers: self.max_viewers,
            ended: ended_rx,
            viewer_count: viewer_count_rx,
            commands: commands_tx,
        })
    }
}

/// Connects to the signaling server, joins the room for `code`, and
/// completes the offer/answer/ICE handshake over the signaling channel.
pub async fn join_session(signaling_addr: &str, code: String) -> Result<Session, BoxError> {
    let (signaling_tx, mut signaling_rx) = signaling_client::connect(signaling_addr).await?;
    signaling_tx
        .send(ClientMessage::Join(code))
        .map_err(|_| "signaling channel closed")?;

    match signaling_rx.recv().await {
        Some(SignalingEvent::Paired { .. }) => {}
        Some(SignalingEvent::Error(message)) => return Err(format!("signaling error: {message}").into()),
        other => return Err(format!("unexpected signaling response while joining: {other:?}").into()),
    }
    eprintln!("[session/guest] paired");

    let (ice_tx, ice_rx) = mpsc::unbounded_channel();
    let (data_channel_tx, _data_channel_rx) = mpsc::unbounded_channel();
    let (track_tx, track_rx) = mpsc::unbounded_channel();
    let pc = build_peer_connection(
        new_media_engine()?,
        stun_config(),
        usable_local_addrs(),
        ice_tx,
        data_channel_tx,
        track_tx,
    )
    .await?;
    eprintln!("[session/guest] peer connection built, waiting for offer");

    forward_local_ice(ice_rx, signaling_tx.clone(), None);

    let (offer, buffered_candidates) = wait_for_remote_sdp(&mut signaling_rx).await?;
    eprintln!(
        "[session/guest] offer received, applying ({} buffered candidate(s))",
        buffered_candidates.len()
    );
    pc.set_remote_description(offer).await?;
    eprintln!("[session/guest] remote description set");
    for candidate in buffered_candidates {
        let _ = pc.add_ice_candidate(candidate).await;
    }
    eprintln!("[session/guest] buffered candidates applied, creating answer");

    let answer = pc.create_answer(None).await?;
    eprintln!("[session/guest] answer created, setting local description");
    pc.set_local_description(answer.clone()).await?;
    eprintln!("[session/guest] local description set, relaying answer");
    send_relay(&signaling_tx, None, RelayPayload::from_description(&answer))?;
    eprintln!("[session/guest] answer sent");

    let (ended_tx, ended_rx) = watch::channel(false);
    let signaling_tx_for_session = signaling_tx.clone();
    pump_remaining_signaling(pc.clone(), signaling_rx, ended_tx);

    Ok(Session {
        peer_connection: pc,
        incoming_tracks: track_rx,
        ended: ended_rx,
        signaling_tx: signaling_tx_for_session,
    })
}

/// A video track already added to a viewer's peer connection and
/// negotiated (has a real SSRC/payload type) — the actual bytes come
/// later, from whichever [`VideoPipeline`] this viewer is currently
/// subscribed to (see [`spawn_video_forwarder`]).
struct VideoTrackHandle {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
}

/// Adds a real video track to `pc` and negotiates it — no frames flow yet,
/// a caller still has to subscribe it to a [`VideoPipeline`] (see
/// [`spawn_video_forwarder`]).
async fn create_video_track(pc: &Arc<dyn PeerConnection>) -> Result<VideoTrackHandle, BoxError> {
    let video_codec = h264_codec_parameters();
    let ssrc = rand::random::<u32>();
    let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        "screen-streaming-stream".to_owned(),
        "screen-streaming-video".to_owned(),
        "screen".to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?);

    let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or("sender has no negotiated codec")?;

    Ok(VideoTrackHandle { track, ssrc, payload_type })
}

/// Same idea as [`VideoTrackHandle`]/[`create_video_track`], for audio.
struct AudioTrackHandle {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
}

async fn create_audio_track(pc: &Arc<dyn PeerConnection>) -> Result<AudioTrackHandle, BoxError> {
    let audio_codec = opus_codec_parameters();
    let ssrc = rand::random::<u32>();
    let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        "screen-streaming-stream".to_owned(),
        "screen-streaming-audio".to_owned(),
        "audio".to_owned(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
            codec: audio_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?);

    let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or("sender has no negotiated audio codec")?;

    Ok(AudioTrackHandle { track, ssrc, payload_type })
}

/// Raises the *calling* thread's OS scheduling priority one step
/// (`THREAD_PRIORITY_ABOVE_NORMAL`) — only called when the user explicitly
/// opted into `StreamQuality::boost_performance`, since this trades away
/// part of the project's own "must not weigh down a game running
/// alongside" requirement for smoother capture under CPU contention; it's
/// the user's call to make, not the app's. Failure is logged, not fatal —
/// worst case the thread just keeps running at its normal priority.
fn boost_current_thread_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    unsafe {
        if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) {
            eprintln!("[session] failed to raise thread priority: {e}");
        }
    }
}

/// One encoded, ready-to-send-as-is chunk from the shared video pipeline —
/// `data` is `Bytes` (not `Vec<u8>`) specifically so fanning it out to N
/// viewers' [`spawn_video_forwarder`] tasks is a cheap refcount bump per
/// viewer, not a real copy.
struct VideoPacket {
    data: Bytes,
    duration: std::time::Duration,
}

/// Same idea as [`VideoPacket`], for Opus audio chunks.
struct AudioPacket {
    data: Bytes,
    duration: std::time::Duration,
}

/// Join handles for one pipeline's dedicated OS threads (capture + encode).
/// Exists so a caller can wait for a pipeline to **fully** stop (including
/// the hardware encoder actually being dropped/closed) before starting a
/// replacement — see [`VideoPipeline::stop_and_join`]. Without this,
/// "Aplicar" restarting the pipeline while the old encoder was still
/// mid-shutdown could fail to open the new hardware encoder (most GPU
/// encoders don't love two overlapping sessions) and silently stop
/// encoding entirely — a real bug hit in practice ("fica travado até
/// voltar as configurações e aplicar de novo" — by the time of the *second*
/// click, the old thread had finally finished on its own).
struct PipelineThreads {
    capture: std::thread::JoinHandle<()>,
    encode: std::thread::JoinHandle<()>,
}

async fn join_pipeline_threads(threads: PipelineThreads) {
    let _ = tokio::task::spawn_blocking(move || {
        let _ = threads.capture.join();
        let _ = threads.encode.join();
    })
    .await;
}

/// The shared capture -> hardware encode pipeline feeding every current
/// (and future) viewer of a broadcast — there is always exactly one of
/// these alive at a time per broadcast, never one per viewer, or every
/// extra person watching would cost another GPU encode pass. Each viewer
/// gets its own [`spawn_video_forwarder`] task subscribed to `packets`.
struct VideoPipeline {
    packets: broadcast::Sender<Arc<VideoPacket>>,
    stop: Arc<AtomicBool>,
    threads: PipelineThreads,
}

impl VideoPipeline {
    fn subscribe(&self) -> broadcast::Receiver<Arc<VideoPacket>> {
        self.packets.subscribe()
    }

    /// Signals `stop` and blocks (on a blocking-safe executor thread, not
    /// the async caller's own) until both the capture and encode threads
    /// have fully exited.
    async fn stop_and_join(self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        join_pipeline_threads(self.threads).await;
    }
}

/// Same idea as [`VideoPipeline`], for the WASAPI loopback capture -> Opus
/// encode pipeline. Audio has no per-broadcaster "quality" setting, unlike
/// video (always sent at its native quality), so [`spawn_shared_audio_pipeline`]
/// takes no parameters.
struct AudioPipeline {
    packets: broadcast::Sender<Arc<AudioPacket>>,
    stop: Arc<AtomicBool>,
    threads: PipelineThreads,
}

impl AudioPipeline {
    fn subscribe(&self) -> broadcast::Receiver<Arc<AudioPacket>> {
        self.packets.subscribe()
    }

    async fn stop_and_join(self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        join_pipeline_threads(self.threads).await;
    }
}

/// Starts the capture -> hardware encode pipeline and returns it publishing
/// to a fresh `broadcast` channel — no viewer subscribed yet, that's up to
/// the caller (see [`spawn_video_forwarder`]). Mirrors
/// `rtc::video_track_smoke_test`'s 3-stage threading shape (blocking
/// capture thread -> blocking encode thread), but indefinite,
/// quality-aware, and using the hardware encoder instead of the software
/// one used there to validate the pipeline originally — and, unlike a
/// single-viewer design, writes each encoded chunk straight to the
/// broadcast channel instead of a specific track's sample writer, so any
/// number of viewers (including zero, momentarily, between one leaving and
/// the next joining) can consume the same encoded bytes.
fn spawn_shared_video_pipeline(source: CaptureSource, quality: StreamQuality) -> VideoPipeline {
    let stop = Arc::new(AtomicBool::new(false));
    let (packets_tx, _) = broadcast::channel::<Arc<VideoPacket>>(32);

    // Capture on a dedicated OS thread (windows-capture's own loop blocks
    // the calling thread) and encode on a second one (CPU/GPU-bound, must
    // not run on a tokio worker thread) — same reasoning as the smoke test.
    //
    // Bounded to 1: if the encoder is still busy with the previous frame
    // (e.g. real screen motion makes each frame more expensive to encode),
    // capture just drops new ones instead of queuing them up. An unbounded
    // channel here was the real cause of low/choppy real-world frame rates
    // — the pipeline kept dutifully encoding an ever-growing backlog of
    // increasingly stale frames instead of always working with the latest
    // one, which gets worse exactly when there's more on-screen motion
    // (frames arrive faster and each one costs more to encode, compounding
    // the backlog). See CLAUDE_SESSIONS.md.
    let (raw_frame_tx, raw_frame_rx) = std::sync::mpsc::sync_channel::<capture::CapturedFrame>(1);
    let capture_stop = stop.clone();
    let boost_performance = quality.boost_performance;
    let capture_thread = std::thread::spawn(move || {
        if boost_performance {
            boost_current_thread_priority();
        }
        if let Err(e) = capture::capture_frames_until_stopped(&source, capture_stop, raw_frame_tx) {
            eprintln!("[session] capture error: {e}");
        }
    });

    // The RTP timestamp each viewer's sample gets is derived from
    // `duration` (the sample writer advances its own running clock by it,
    // independent of the encoder's internal PTS) — using the *real*
    // elapsed time between packets here, instead of a fixed nominal value,
    // keeps that clock honest regardless of fps mode. It has to be
    // measured this way rather than assumed: under `UNLIMITED_FPS` there's
    // no fixed interval to assume in the first place, and even at a capped
    // fps, real frames never arrive at a perfectly exact cadence. Computed
    // once here (not per-viewer) since it describes the pipeline's own
    // output cadence, the same for everyone subscribed to it.
    let fallback_duration = std::time::Duration::from_secs_f64(1.0 / quality.nominal_fps() as f64);
    let packets_tx_encode = packets_tx.clone();
    let encode_thread = std::thread::spawn(move || {
        if boost_performance {
            boost_current_thread_priority();
        }
        // The encoder needs real capture dimensions to open, which we only
        // know once the first frame arrives — so it's built lazily here
        // rather than passed in.
        let mut encoder: Option<HardwareH264Encoder> = None;
        let frame_interval = quality.frame_interval();
        let mut last_encoded_at = std::time::Instant::now() - frame_interval;
        let mut last_sent_at: Option<std::time::Instant> = None;

        let mut publish = |bytes: Vec<u8>, last_sent_at: &mut Option<std::time::Instant>| {
            let now = std::time::Instant::now();
            let duration = match *last_sent_at {
                Some(prev) => now.duration_since(prev),
                None => fallback_duration,
            };
            *last_sent_at = Some(now);
            let _ = packets_tx_encode.send(Arc::new(VideoPacket { data: bytes.into(), duration }));
        };

        for frame in raw_frame_rx {
            if encoder.is_none() {
                let (output_width, output_height) =
                    quality.target_dimensions(frame.width, frame.height);
                match HardwareH264Encoder::new(
                    frame.width,
                    frame.height,
                    output_width,
                    output_height,
                    quality.bitrate_bps(),
                    quality.nominal_fps(),
                ) {
                    Ok(e) => {
                        eprintln!(
                            "[session] hardware encoder: {} ({}x{} -> {}x{})",
                            e.codec_name(),
                            frame.width,
                            frame.height,
                            output_width,
                            output_height
                        );
                        encoder = Some(e);
                    }
                    Err(e) => {
                        eprintln!("[session] no hardware encoder available: {e}");
                        break;
                    }
                }
            }

            // FPS ceiling: drop frames arriving faster than the chosen
            // limit instead of encoding (and sending) all of them.
            if last_encoded_at.elapsed() < frame_interval {
                continue;
            }
            last_encoded_at = std::time::Instant::now();

            if let Some(enc) = encoder.as_mut() {
                match enc.encode_rgba(&frame.rgba) {
                    Ok(packets) => {
                        for bytes in packets {
                            publish(bytes, &mut last_sent_at);
                        }
                    }
                    Err(e) => eprintln!("[session] hardware encode error: {e}"),
                }
            }
        }

        if let Some(enc) = encoder.as_mut() {
            if let Ok(packets) = enc.flush() {
                for bytes in packets {
                    publish(bytes, &mut last_sent_at);
                }
            }
        }
    });

    VideoPipeline { packets: packets_tx, stop, threads: PipelineThreads { capture: capture_thread, encode: encode_thread } }
}

/// Duration of one audio sample handed to a viewer's track — fixed,
/// unlike video's `quality.frame_interval()`, since it's tied directly to
/// `audio_capture::FRAME_SAMPLES_PER_CHANNEL` (20ms @ 48kHz), not a
/// user-chosen fps.
const AUDIO_FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(20);

/// Starts the WASAPI loopback capture -> Opus encode pipeline and returns
/// it publishing to a fresh `broadcast` channel — the audio equivalent of
/// [`spawn_shared_video_pipeline`]. System audio is always sent at its
/// native quality (no downscale-equivalent setting), so this takes no
/// quality parameter.
fn spawn_shared_audio_pipeline() -> AudioPipeline {
    let stop = Arc::new(AtomicBool::new(false));
    let (packets_tx, _) = broadcast::channel::<Arc<AudioPacket>>(32);

    // Bounded to a few chunks (not 1, like video's raw-frame channel): a
    // dropped audio chunk is an audible click, so a little slack (~80ms)
    // to absorb momentary scheduling jitter is worth it — while staying
    // bounded so a genuinely slow encoder still can't build an
    // ever-growing backlog (see `capture.rs`'s reasoning, same idea).
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<audio_capture::CapturedAudio>(4);
    let capture_stop = stop.clone();
    let capture_thread = std::thread::spawn(move || {
        if let Err(e) = audio_capture::capture_system_audio_until_stopped(capture_stop, raw_tx) {
            eprintln!("[session] audio capture error: {e}");
        }
    });

    let encode_stop = stop.clone();
    let packets_tx_encode = packets_tx.clone();
    let encode_thread = std::thread::spawn(move || {
        let encoder = match audio_codec::AudioEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[session] failed to start Opus encoder: {e}");
                return;
            }
        };
        for chunk in raw_rx {
            if encode_stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match encoder.encode(&chunk.samples) {
                Ok(bytes) => {
                    let _ = packets_tx_encode
                        .send(Arc::new(AudioPacket { data: bytes.into(), duration: AUDIO_FRAME_DURATION }));
                }
                Err(e) => eprintln!("[session] Opus encode error: {e}"),
            }
        }
    });

    AudioPipeline { packets: packets_tx, stop, threads: PipelineThreads { capture: capture_thread, encode: encode_thread } }
}

/// Feeds one viewer's video track from a [`VideoPipeline`]'s broadcast
/// channel until told to stop (via the returned [`tokio::task::AbortHandle`])
/// or the pipeline itself ends. A lagging viewer (`RecvError::Lagged`) just
/// skips forward to the latest packets instead of stopping — falling behind
/// briefly shouldn't kill the connection, it should just look like a
/// dropped frame, same as the existing FPS-ceiling frame dropping.
fn spawn_video_forwarder(
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    mut rx: broadcast::Receiver<Arc<VideoPacket>>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(packet) => {
                    let sample =
                        rtc::media::Sample { data: packet.data.clone(), duration: packet.duration, ..Default::default() };
                    let _ = track.sample_writer(ssrc, payload_type).write_sample(&sample).await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .abort_handle()
}

/// Same idea as [`spawn_video_forwarder`], for a viewer's audio track.
fn spawn_audio_forwarder(
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    mut rx: broadcast::Receiver<Arc<AudioPacket>>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(packet) => {
                    let sample =
                        rtc::media::Sample { data: packet.data.clone(), duration: packet.duration, ..Default::default() };
                    let _ = track.sample_writer(ssrc, payload_type).write_sample(&sample).await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .abort_handle()
}

/// Everything about one already-connected viewer needed to re-subscribe it
/// to a new pipeline (see `apply_new_settings`) or tear it down (see
/// `run_viewer_connection`'s cleanup). Doesn't need the viewer's own `pc`
/// — the task that inserts this (`establish_viewer`) hands its `pc` straight
/// to `run_viewer_connection`, which closes it directly when the viewer
/// leaves.
struct ViewerHandle {
    video_track: Arc<TrackLocalStaticSample>,
    video_ssrc: u32,
    video_payload_type: u8,
    video_forwarder: tokio::task::AbortHandle,
    audio_forwarder: Option<tokio::task::AbortHandle>,
}

/// State shared between the broadcast's controller task and every
/// in-progress/connected viewer task — an `Arc<tokio::sync::Mutex<..>>`
/// rather than a stricter actor/message-passing design, which would be
/// more ceremony than a personal-scale hobby app's host-side viewer
/// registry actually needs.
struct Shared {
    /// Always `Some` while the broadcast is running — `Option` only so
    /// `apply_new_settings`/final cleanup can `.take()` it out to await
    /// `stop_and_join()` without holding the lock across that await.
    pipeline: Option<VideoPipeline>,
    audio_pipeline: Option<AudioPipeline>,
    /// Routes a guest's relayed SDP/ICE/`PeerLeft` events (which all
    /// arrive tagged by `guest_id` on the host's one signaling connection,
    /// see `run_broadcast_controller`) to that specific viewer's own task.
    guest_events: HashMap<GuestId, mpsc::UnboundedSender<SignalingEvent>>,
    /// Only viewers that have *finished* the handshake — used by
    /// `apply_new_settings` to re-subscribe everyone to a new pipeline,
    /// and to report `viewer_count()`.
    viewers: HashMap<GuestId, ViewerHandle>,
}

type SharedState = Arc<tokio::sync::Mutex<Shared>>;

/// Commands [`Broadcast::apply`]/[`Broadcast::stop`] send to the broadcast's
/// controller task (see `run_broadcast_controller`) — both are synchronous
/// from the caller's point of view, the actual work happens in the
/// background.
enum Command {
    Apply { source: CaptureSource, quality: StreamQuality },
    Stop,
}

/// Completes the offer/answer/ICE handshake for one viewer: builds its
/// `PeerConnection`, adds a video track (and an audio one, if the
/// broadcast has an audio pipeline) subscribed to the broadcast's current
/// pipeline(s), sends the offer addressed to `guest_id`, and waits for the
/// answer on `guest_rx` (fed by `run_broadcast_controller`'s routing).
/// Registers the viewer in `state.viewers` (and updates `viewer_count_tx`)
/// only once the handshake actually succeeds. Used both for the very first
/// viewer (awaited inline by `HostingSession::wait_for_peer`) and for every
/// later one (awaited by `spawn_viewer`, in the background) — the only
/// difference is who's waiting on it and how a failure is handled.
async fn establish_viewer(
    guest_id: GuestId,
    state: &SharedState,
    signaling_tx: &mpsc::UnboundedSender<ClientMessage>,
    guest_rx: &mut mpsc::UnboundedReceiver<SignalingEvent>,
    viewer_count_tx: &watch::Sender<u32>,
) -> Result<Arc<dyn PeerConnection>, BoxError> {
    let (ice_tx, ice_rx) = mpsc::unbounded_channel();
    let (data_channel_tx, _data_channel_rx) = mpsc::unbounded_channel();
    let (track_tx, _track_rx) = mpsc::unbounded_channel();
    let pc = build_peer_connection(
        new_media_engine()?,
        stun_config(),
        usable_local_addrs(),
        ice_tx,
        data_channel_tx,
        track_tx,
    )
    .await?;

    forward_local_ice(ice_rx, signaling_tx.clone(), Some(guest_id));

    // Adding tracks before create_offer() is what gives the offer real
    // media sections (and therefore ICE credentials) — see
    // CLAUDE_SESSIONS.md's debugging journey for why an offer built with
    // no track/data channel at all silently breaks the handshake.
    let video_handle = create_video_track(&pc).await?;
    let video_rx = state
        .lock()
        .await
        .pipeline
        .as_ref()
        .ok_or("broadcast pipeline missing")?
        .subscribe();
    let video_forwarder =
        spawn_video_forwarder(video_handle.track.clone(), video_handle.ssrc, video_handle.payload_type, video_rx);

    let has_audio = state.lock().await.audio_pipeline.is_some();
    let audio_forwarder = if has_audio {
        let audio_handle = create_audio_track(&pc).await?;
        let audio_rx = state
            .lock()
            .await
            .audio_pipeline
            .as_ref()
            .ok_or("broadcast audio pipeline missing")?
            .subscribe();
        Some(spawn_audio_forwarder(
            audio_handle.track,
            audio_handle.ssrc,
            audio_handle.payload_type,
            audio_rx,
        ))
    } else {
        None
    };

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer.clone()).await?;
    send_relay(signaling_tx, Some(guest_id), RelayPayload::from_description(&offer))?;

    let (answer, buffered_candidates) = wait_for_remote_sdp(guest_rx).await?;
    pc.set_remote_description(answer).await?;
    for candidate in buffered_candidates {
        let _ = pc.add_ice_candidate(candidate).await;
    }

    let mut s = state.lock().await;
    s.viewers.insert(
        guest_id,
        ViewerHandle {
            video_track: video_handle.track,
            video_ssrc: video_handle.ssrc,
            video_payload_type: video_handle.payload_type,
            video_forwarder,
            audio_forwarder,
        },
    );
    let count = s.viewers.len() as u32;
    drop(s);
    let _ = viewer_count_tx.send(count);

    Ok(pc)
}

/// Keeps applying ICE candidates that arrive after one viewer's initial
/// handshake, and cleans that viewer up (closes its `PeerConnection`,
/// aborts its forwarders, removes it from `state.viewers`/`guest_events`,
/// updates `viewer_count`) once it leaves — whether that's an explicit
/// `PeerLeft`, the whole signaling connection dropping, or this task's own
/// `guest_rx` just closing (e.g. the broadcast itself stopped and
/// `run_broadcast_controller`'s cleanup dropped every `guest_events`
/// sender). Deliberately does **not** touch the shared pipeline — one
/// viewer leaving must never stop the broadcast for everyone else, unlike
/// the old single-viewer behavior. See CLAUDE_SESSIONS.md's
/// "Multi-espectador" section for why that changed.
async fn run_viewer_connection(
    guest_id: GuestId,
    pc: Arc<dyn PeerConnection>,
    state: SharedState,
    mut guest_rx: mpsc::UnboundedReceiver<SignalingEvent>,
    viewer_count_tx: watch::Sender<u32>,
) {
    while let Some(event) = guest_rx.recv().await {
        match event {
            SignalingEvent::Relay { payload, .. } => {
                if let Ok(RelayPayload::IceCandidate(candidate)) = serde_json::from_value(payload) {
                    let _ = pc.add_ice_candidate(candidate).await;
                }
            }
            SignalingEvent::PeerLeft { .. } | SignalingEvent::Disconnected => break,
            _ => {}
        }
    }

    let _ = pc.close().await;
    let mut s = state.lock().await;
    if let Some(v) = s.viewers.remove(&guest_id) {
        v.video_forwarder.abort();
        if let Some(h) = v.audio_forwarder {
            h.abort();
        }
    }
    s.guest_events.remove(&guest_id);
    let count = s.viewers.len() as u32;
    drop(s);
    let _ = viewer_count_tx.send(count);
}

/// Handles a viewer that joins *after* the first one — spawned by
/// `run_broadcast_controller` in reaction to a `Paired{guest_id}` event.
/// Unlike the first viewer (whose failure to connect fails the whole
/// `wait_for_peer` call), a later viewer failing to connect just logs and
/// gives up on that one viewer — the broadcast keeps going for everyone
/// else.
fn spawn_viewer(
    guest_id: GuestId,
    state: SharedState,
    signaling_tx: mpsc::UnboundedSender<ClientMessage>,
    mut guest_rx: mpsc::UnboundedReceiver<SignalingEvent>,
    viewer_count_tx: watch::Sender<u32>,
) {
    tokio::spawn(async move {
        match establish_viewer(guest_id, &state, &signaling_tx, &mut guest_rx, &viewer_count_tx).await {
            Ok(pc) => run_viewer_connection(guest_id, pc, state, guest_rx, viewer_count_tx).await,
            Err(e) => {
                eprintln!("[session/host] viewer {guest_id} failed to connect: {e}");
                state.lock().await.guest_events.remove(&guest_id);
            }
        }
    });
}

/// Forwards one already-routed event to whichever viewer task is currently
/// registered for `guest_id` in `state.guest_events` — a no-op if that
/// viewer already finished cleaning up (a late-arriving message racing its
/// own departure).
async fn route_to_guest(state: &SharedState, guest_id: GuestId, event: SignalingEvent) {
    let s = state.lock().await;
    if let Some(tx) = s.guest_events.get(&guest_id) {
        let _ = tx.send(event);
    }
}

/// Restarts the shared video pipeline with new source/quality settings —
/// the "Aplicar alterações" button, now viewer-count-agnostic: every
/// currently connected viewer gets re-subscribed (old forwarder aborted, a
/// new one started against the new pipeline) on its *same* already
/// negotiated track, no renegotiation, nobody has to reconnect. Only after
/// every viewer is moved over does the old pipeline actually get told to
/// stop (and awaited via `stop_and_join`) — same ordering the old
/// single-viewer code used to avoid two hardware encoders overlapping.
/// Audio is untouched (same limitation as before: changing it would need a
/// real SDP renegotiation per viewer).
async fn apply_new_settings(state: &SharedState, source: CaptureSource, quality: StreamQuality) {
    let new_pipeline = spawn_shared_video_pipeline(source, quality);
    let mut s = state.lock().await;
    let old_pipeline = s.pipeline.replace(new_pipeline);
    let guest_ids: Vec<GuestId> = s.viewers.keys().copied().collect();
    for id in &guest_ids {
        let rx = s.pipeline.as_ref().expect("just set above").subscribe();
        if let Some(v) = s.viewers.get_mut(id) {
            v.video_forwarder.abort();
            v.video_forwarder = spawn_video_forwarder(v.video_track.clone(), v.video_ssrc, v.video_payload_type, rx);
        }
    }
    drop(s);
    if let Some(old) = old_pipeline {
        old.stop_and_join().await;
    }
}

/// Owns the host's signaling connection for the whole lifetime of a
/// broadcast: routes each guest's relayed messages to its own task (see
/// `route_to_guest`), spawns a task per new viewer (see `spawn_viewer`),
/// and reacts to `apply`/`stop` commands from the [`Broadcast`] handle the
/// app actually holds. Ends when the host's signaling connection itself
/// drops, or a `Command::Stop` arrives — either way, every viewer gets
/// cleaned up (by dropping `guest_events`, which closes each viewer task's
/// `guest_rx` and lets its own cleanup run) and both shared pipelines are
/// stopped before `ended` fires.
async fn run_broadcast_controller(
    state: SharedState,
    mut signaling_rx: mpsc::UnboundedReceiver<SignalingEvent>,
    signaling_tx: mpsc::UnboundedSender<ClientMessage>,
    mut commands_rx: mpsc::UnboundedReceiver<Command>,
    viewer_count_tx: watch::Sender<u32>,
    ended_tx: watch::Sender<bool>,
) {
    loop {
        tokio::select! {
            event = signaling_rx.recv() => {
                match event {
                    Some(SignalingEvent::Paired { guest_id: Some(id) }) => {
                        let (guest_tx, guest_rx) = mpsc::unbounded_channel();
                        state.lock().await.guest_events.insert(id, guest_tx);
                        spawn_viewer(id, state.clone(), signaling_tx.clone(), guest_rx, viewer_count_tx.clone());
                    }
                    Some(event @ SignalingEvent::Relay { guest_id: Some(id), .. }) => {
                        route_to_guest(&state, id, event).await;
                    }
                    Some(event @ SignalingEvent::PeerLeft { guest_id: Some(id) }) => {
                        route_to_guest(&state, id, event).await;
                    }
                    Some(SignalingEvent::Disconnected) | None => break,
                    Some(_) => {}
                }
            }
            cmd = commands_rx.recv() => {
                match cmd {
                    Some(Command::Apply { source, quality }) => {
                        apply_new_settings(&state, source, quality).await;
                    }
                    Some(Command::Stop) | None => {
                        let _ = signaling_tx.send(ClientMessage::Leave);
                        break;
                    }
                }
            }
        }
    }

    let mut s = state.lock().await;
    // Dropping every guest's sender closes its `guest_rx`, so each
    // still-running `run_viewer_connection` task's `while let Some(..)`
    // loop ends on its own and runs its own cleanup (close its
    // `PeerConnection`, abort its forwarders) — no need to duplicate that
    // here.
    s.guest_events.clear();
    let pipeline = s.pipeline.take();
    let audio_pipeline = s.audio_pipeline.take();
    drop(s);
    if let Some(p) = pipeline {
        p.stop_and_join().await;
    }
    if let Some(p) = audio_pipeline {
        p.stop_and_join().await;
    }
    let _ = ended_tx.send(true);
}

/// A broadcast with any number of simultaneous viewers (up to
/// `max_viewers`, enforced by `signaling-server`), all fed by one shared
/// capture/encode pipeline. Returned by [`HostingSession::wait_for_peer`]
/// once the first viewer connects; `lib.rs` holds onto this for as long as
/// "Iniciar transmissão" stays active.
pub struct Broadcast {
    pub max_viewers: Option<u32>,
    /// Fires once when the broadcast ends — either `stop()` was called, or
    /// the host's own signaling connection dropped. A single viewer
    /// leaving does **not** fire this (the broadcast keeps running for
    /// everyone else) — see `run_viewer_connection`.
    pub ended: watch::Receiver<bool>,
    viewer_count: watch::Receiver<u32>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Broadcast {
    /// How many viewers are connected right now — for the UI's live "N
    /// pessoas assistindo" display.
    pub fn viewer_count(&self) -> u32 {
        *self.viewer_count.borrow()
    }

    /// A receiver that resolves every time the viewer count changes.
    /// `lib.rs` spawns a small task watching this to push a Tauri event to
    /// the frontend live, the same pattern `watch_for_session_end` already
    /// uses for `ended`.
    pub fn watch_viewer_count(&self) -> watch::Receiver<u32> {
        self.viewer_count.clone()
    }

    /// Restarts the shared capture/encode pipeline with new source/quality
    /// settings — every currently connected viewer is re-subscribed to the
    /// new pipeline on their already-negotiated track, no renegotiation, no
    /// dropped viewers. Synchronous: just queues the command, the actual
    /// work happens on the broadcast's own background controller task.
    pub fn apply(&self, source: CaptureSource, quality: StreamQuality) {
        let _ = self.commands.send(Command::Apply { source, quality });
    }

    /// Ends the broadcast: tells the signaling server, closes every
    /// viewer's `PeerConnection`, and stops the shared capture/encode
    /// pipeline(s). Synchronous, same as `apply`.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

fn send_relay(
    signaling_tx: &mpsc::UnboundedSender<ClientMessage>,
    to: Option<GuestId>,
    payload: RelayPayload,
) -> Result<(), BoxError> {
    let value = serde_json::to_value(payload)?;
    signaling_tx
        .send(ClientMessage::Relay { payload: value, to })
        .map_err(|_| "signaling channel closed".into())
}

/// Waits for the relayed SDP (the offer, if joining; the answer, if
/// hosting). Any ICE candidates that arrive first — real races over a real
/// network — are buffered and returned alongside it, since applying them
/// before a remote description exists doesn't make sense.
async fn wait_for_remote_sdp(
    signaling_rx: &mut mpsc::UnboundedReceiver<SignalingEvent>,
) -> Result<(RTCSessionDescription, Vec<RTCIceCandidateInit>), BoxError> {
    let mut buffered_candidates = Vec::new();
    loop {
        match signaling_rx.recv().await {
            Some(SignalingEvent::Relay { payload, .. }) => match serde_json::from_value(payload) {
                Ok(RelayPayload::Sdp { sdp_type, sdp }) => {
                    let desc = session_description_from_parts(sdp_type, sdp)?;
                    return Ok((desc, buffered_candidates));
                }
                Ok(RelayPayload::IceCandidate(candidate)) => buffered_candidates.push(candidate),
                Err(_) => {}
            },
            Some(SignalingEvent::PeerLeft { .. }) => {
                return Err("the other side left before completing the handshake".into());
            }
            Some(SignalingEvent::Disconnected) => {
                return Err("lost connection to the signaling server".into());
            }
            Some(SignalingEvent::Error(message)) => return Err(format!("signaling error: {message}").into()),
            Some(_) => {}
            None => return Err("signaling channel closed unexpectedly".into()),
        }
    }
}

fn forward_local_ice(
    mut local_ice_rx: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    signaling_tx: mpsc::UnboundedSender<ClientMessage>,
    to: Option<GuestId>,
) {
    tokio::spawn(async move {
        while let Some(candidate) = local_ice_rx.recv().await {
            let _ = send_relay(&signaling_tx, to, RelayPayload::IceCandidate(candidate));
        }
    });
}

/// Guest-side only now (the host side has its own per-viewer equivalent,
/// [`run_viewer_connection`]): keeps applying ICE candidates that arrive
/// after the initial handshake, and closes the peer connection once the
/// host leaves or the signaling connection itself drops. `ended_tx` lets
/// `lib.rs` react (reset state, tell the UI) without polling.
fn pump_remaining_signaling(
    pc: Arc<dyn PeerConnection>,
    mut signaling_rx: mpsc::UnboundedReceiver<SignalingEvent>,
    ended_tx: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        while let Some(event) = signaling_rx.recv().await {
            match event {
                SignalingEvent::Relay { payload, .. } => {
                    if let Ok(RelayPayload::IceCandidate(candidate)) = serde_json::from_value(payload) {
                        let _ = pc.add_ice_candidate(candidate).await;
                    }
                }
                SignalingEvent::PeerLeft { .. } | SignalingEvent::Disconnected => {
                    let _ = pc.close().await;
                    let _ = ended_tx.send(true);
                    break;
                }
                _ => {}
            }
        }
        // The channel closing without an explicit PeerLeft/Disconnected
        // event (e.g. the caller dropped its `signaling_tx`/`Session` to
        // stop deliberately) still means this session is over.
        let _ = ended_tx.send(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use webrtc::media_stream::track_remote::TrackRemoteEvent;

    #[test]
    fn prints_usable_local_addrs() {
        println!("{:?}", usable_local_addrs());
    }

    #[test]
    fn prints_local_signaling_urls() {
        println!("{:?}", local_signaling_urls());
    }

    async fn expect_real_video_packet(session: &mut Session) {
        let track = tokio::time::timeout(Duration::from_secs(15), session.incoming_tracks.recv())
            .await
            .expect("timed out waiting for the incoming video track")
            .expect("incoming_tracks closed without ever receiving a track");

        let mut got_packet = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_secs(15), track.poll()).await {
                Ok(Some(TrackRemoteEvent::OnRtpPacket(_))) => {
                    got_packet = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(got_packet, "expected at least one real RTP video packet from the host");
    }

    async fn wait_for_viewer_count(broadcast: &Broadcast, target: u32) {
        let mut rx = broadcast.watch_viewer_count();
        tokio::time::timeout(Duration::from_secs(10), async {
            while *rx.borrow() != target {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("viewer count never reached {target}"));
    }

    /// Not run in CI (binds the real, fixed `SIGNALING_PORT` on every
    /// interface — could conflict with another instance already running)
    /// — run manually with `cargo test -- --ignored --nocapture`. Proves
    /// the embedded signaling server (what a broadcaster's "Iniciar
    /// transmissão" now starts automatically, see `lib.rs`) actually
    /// accepts a real client connection and issues a real pairing code.
    #[tokio::test]
    #[ignore]
    async fn embedded_signaling_server_accepts_a_real_connection() {
        spawn_local_signaling_server()
            .await
            .expect("failed to bind the embedded signaling server");

        let addr = format!("ws://127.0.0.1:{SIGNALING_PORT}");
        let (tx, mut rx) = signaling_client::connect(&addr)
            .await
            .expect("failed to connect to the embedded signaling server");
        tx.send(ClientMessage::Host { max_viewers: None }).expect("signaling channel closed");

        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(SignalingEvent::Hosting(code))) => assert_eq!(code.len(), 6),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    /// Not run in CI (no real network/GPU/display here, but it does open
    /// real UDP sockets, hits a public STUN server, and captures the real
    /// screen) — run manually with `cargo test -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn hosts_and_joins_a_session_and_streams_real_video() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr, None).await.expect("start_hosting failed");
        println!("pairing code: {code}");

        let quality = StreamQuality { resolution_height: 720, fps: 30, audio: false, boost_performance: false };

        // try_join, not join: if one side errors out fast (e.g. a bad
        // offer), the other would otherwise hang waiting for a reply that
        // will never come, and the test would only fail once the 20s
        // timeout below expired instead of with the real error.
        let (broadcast, mut guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality),
                join_session(&signaling_addr, code),
            ),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        assert_eq!(broadcast.viewer_count(), 1);

        // Prove real video actually flows end-to-end (not just that
        // signaling/ICE completed).
        expect_real_video_packet(&mut guest_session).await;

        broadcast.stop();
        let _ = guest_session.peer_connection.close().await;
    }

    /// Not run in CI (needs a real display/GPU) — run manually with
    /// `cargo test -- --ignored --nocapture`. Proves the fan-out actually
    /// works: two viewers on the *same* pairing code both get real RTP
    /// video from the one shared capture/encode pipeline, and the host
    /// correctly tells them apart (a message addressed to one doesn't
    /// reach the other).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn multiple_viewers_receive_the_same_broadcast() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr, None).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false, boost_performance: false };

        let (broadcast, mut guest_a) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality),
                join_session(&signaling_addr, code.clone()),
            ),
        )
        .await
        .expect("first handshake did not complete within 20s")
        .expect("host or first guest failed to connect");

        let mut guest_b = tokio::time::timeout(Duration::from_secs(20), join_session(&signaling_addr, code))
            .await
            .expect("second guest did not connect within 20s")
            .expect("second guest failed to connect");

        wait_for_viewer_count(&broadcast, 2).await;

        expect_real_video_packet(&mut guest_a).await;
        expect_real_video_packet(&mut guest_b).await;

        broadcast.stop();
        let _ = guest_a.peer_connection.close().await;
        let _ = guest_b.peer_connection.close().await;
    }

    /// Not run in CI (needs a real display/GPU) — run manually with
    /// `cargo test -- --ignored --nocapture`. Proves the deliberate
    /// behavior change documented in CLAUDE_SESSIONS.md's
    /// "Multi-espectador" section: with several viewers connected, one of
    /// them leaving must **not** stop the broadcast for the others (unlike
    /// the old single-viewer behavior, where the one viewer leaving always
    /// ended everything) — only `broadcast.stop()` (or the host's own
    /// signaling connection dropping) really ends it.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn one_viewer_leaving_does_not_stop_the_broadcast_for_others() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr, None).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false, boost_performance: false };

        let (broadcast, guest_a) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality),
                join_session(&signaling_addr, code.clone()),
            ),
        )
        .await
        .expect("first handshake did not complete within 20s")
        .expect("host or first guest failed to connect");

        let mut guest_b = tokio::time::timeout(Duration::from_secs(20), join_session(&signaling_addr, code))
            .await
            .expect("second guest did not connect within 20s")
            .expect("second guest failed to connect");

        wait_for_viewer_count(&broadcast, 2).await;

        // Same call the "Sair"/"Parar de assistir" button makes.
        guest_a.notify_leaving();
        wait_for_viewer_count(&broadcast, 1).await;

        assert!(
            !*broadcast.ended.borrow(),
            "broadcast shouldn't end just because one of several viewers left"
        );

        // The remaining viewer should keep receiving real video.
        expect_real_video_packet(&mut guest_b).await;

        broadcast.stop();
        let _ = guest_b.peer_connection.close().await;
    }

    /// Not run in CI (needs a real display/GPU) — run manually with
    /// `cargo test -- --ignored --nocapture`. Proves "Aplicar" (changing
    /// quality/source mid-broadcast) still works under the new shared
    /// pipeline design: `broadcast.apply(...)` restarts the pipeline and
    /// the guest keeps receiving real RTP video on the *same* track
    /// afterwards, with no renegotiation.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn applying_new_settings_keeps_streaming_on_the_same_track() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr, None).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false, boost_performance: false };

        let (broadcast, mut guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality),
                join_session(&signaling_addr, code),
            ),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        let track = tokio::time::timeout(Duration::from_secs(15), guest_session.incoming_tracks.recv())
            .await
            .expect("timed out waiting for the incoming video track")
            .expect("incoming_tracks closed without ever receiving a track");

        let new_quality = StreamQuality { resolution_height: 240, fps: 15, audio: false, boost_performance: false };
        broadcast.apply(CaptureSource::Monitor, new_quality);

        let mut got_packet_after_apply = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_secs(15), track.poll()).await {
                Ok(Some(TrackRemoteEvent::OnRtpPacket(_))) => {
                    got_packet_after_apply = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            got_packet_after_apply,
            "expected the guest to keep receiving real RTP video after applying new settings"
        );

        broadcast.stop();
        let _ = guest_session.peer_connection.close().await;
    }
}
