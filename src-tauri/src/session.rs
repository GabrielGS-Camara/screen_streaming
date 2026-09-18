//! Bridges [`crate::signaling_client`] to a real [`PeerConnection`]
//! (built via [`crate::rtc::build_peer_connection`]): host or join a
//! pairing-code session on the signaling server, exchange offer/answer/ICE
//! through it — real network relay, not the in-process forwarding
//! `rtc.rs`'s own tests use — and hand back a negotiating/connected
//! `PeerConnection` the rest of the app can attach tracks/data channels to.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
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
use crate::video_preview;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
/// background for the connection's lifetime.
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
    /// `Some` only on the host side — the video track this session is
    /// broadcasting on, kept around so "Aplicar" can restart the
    /// capture/encode pipeline on it (see [`spawn_capture_pipeline`])
    /// without renegotiating. Always `None` for a guest's `Session` (they
    /// don't send a video track of their own).
    pub video_track: Option<VideoTrackHandle>,
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
}

/// Connects to the signaling server and requests a new pairing code.
/// Returns as soon as the code is issued — call
/// [`HostingSession::wait_for_peer`] to block until someone joins and
/// finish the WebRTC handshake.
pub async fn start_hosting(signaling_addr: &str) -> Result<(String, HostingSession), BoxError> {
    let (signaling_tx, mut signaling_rx) = signaling_client::connect(signaling_addr).await?;
    signaling_tx
        .send(ClientMessage::Host)
        .map_err(|_| "signaling channel closed")?;

    let code = match signaling_rx.recv().await {
        Some(SignalingEvent::Hosting(code)) => code,
        Some(SignalingEvent::Error(message)) => return Err(format!("signaling error: {message}").into()),
        other => return Err(format!("unexpected signaling response: {other:?}").into()),
    };

    Ok((code, HostingSession { signaling_tx, signaling_rx }))
}

impl HostingSession {
    /// Blocks until someone joins with the pairing code, then completes the
    /// offer/answer/ICE handshake over the signaling channel. `source` and
    /// `quality` control what gets captured/encoded and at what
    /// resolution/fps ceiling; `stop` lets the caller end the capture
    /// pipeline later (e.g. a "stop transmission" button) without tearing
    /// down the whole process. `preview` lets the caller turn the
    /// broadcaster's own live preview on/off on demand later (send
    /// `Some(server)`/`None`) without restarting the broadcast — see
    /// `attach_video_source`.
    pub async fn wait_for_peer(
        mut self,
        source: CaptureSource,
        quality: StreamQuality,
        stop: Arc<AtomicBool>,
        preview: watch::Receiver<Option<Arc<video_preview::MjpegServer>>>,
    ) -> Result<Session, BoxError> {
        match self.signaling_rx.recv().await {
            Some(SignalingEvent::Paired) => {}
            Some(SignalingEvent::PeerLeft) => return Err("the other side left before joining".into()),
            Some(SignalingEvent::Error(message)) => {
                return Err(format!("signaling error: {message}").into());
            }
            other => return Err(format!("unexpected signaling response while waiting: {other:?}").into()),
        }
        eprintln!("[session/host] paired");

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
        eprintln!("[session/host] peer connection built");

        forward_local_ice(ice_rx, self.signaling_tx.clone());

        // Adding the video track before create_offer() is what gives the
        // offer real media sections (and therefore ICE credentials) — see
        // CLAUDE_SESSIONS.md's debugging journey for why an offer built
        // with no track/data channel at all silently breaks the handshake.
        let video_track = attach_video_source(&pc, source, quality, stop.clone(), preview).await?;

        // Audio (if the user turned "Transmitir áudio do sistema" on) has
        // to be added before create_offer() too, for the same reason —
        // otherwise it would need a full renegotiation round trip to add
        // later. Toggling audio back on/off later via "Aplicar" isn't
        // supported (unlike resolution/fps) precisely because it would
        // need exactly that renegotiation — see `lib.rs`'s
        // `apply_broadcast_settings`. Failure here is logged, not fatal:
        // losing audio (e.g. no default playback device on this machine)
        // shouldn't cost the whole broadcast.
        if quality.audio {
            if let Err(e) = attach_audio_source(&pc, stop.clone()).await {
                eprintln!("[session] audio requested but failed to start: {e}");
            }
        }

        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;
        send_relay(&self.signaling_tx, RelayPayload::from_description(&offer))?;
        eprintln!("[session/host] offer sent, waiting for answer");

        let (answer, buffered_candidates) = wait_for_remote_sdp(&mut self.signaling_rx).await?;
        eprintln!(
            "[session/host] answer received, applying ({} buffered candidate(s))",
            buffered_candidates.len()
        );
        pc.set_remote_description(answer).await?;
        for candidate in buffered_candidates {
            let _ = pc.add_ice_candidate(candidate).await;
        }

        let (ended_tx, ended_rx) = watch::channel(false);
        let signaling_tx = self.signaling_tx.clone();
        pump_remaining_signaling(pc.clone(), self.signaling_rx, Some(stop), ended_tx);

        Ok(Session {
            peer_connection: pc,
            incoming_tracks: track_rx,
            ended: ended_rx,
            signaling_tx,
            video_track: Some(video_track),
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
        Some(SignalingEvent::Paired) => {}
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

    forward_local_ice(ice_rx, signaling_tx.clone());

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
    send_relay(&signaling_tx, RelayPayload::from_description(&answer))?;
    eprintln!("[session/guest] answer sent");

    let (ended_tx, ended_rx) = watch::channel(false);
    let signaling_tx_for_session = signaling_tx.clone();
    pump_remaining_signaling(pc.clone(), signaling_rx, None, ended_tx);

    Ok(Session {
        peer_connection: pc,
        incoming_tracks: track_rx,
        ended: ended_rx,
        signaling_tx: signaling_tx_for_session,
        video_track: None,
    })
}

/// How often the broadcaster's own live preview gets a fresh frame — much
/// lower than the real stream's fps, since it's only ever a quick look at
/// what's being sent, not something that needs to be smooth, and every
/// preview frame costs an extra JPEG encode on the sending machine.
const PREVIEW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// A video track already added to the peer connection and negotiated (has a
/// real SSRC/payload type) — returned separately from the capture/encode
/// pipeline that feeds it (see [`spawn_capture_pipeline`]) so "Aplicar"
/// (changing quality/source mid-broadcast — see `lib.rs`'s
/// `apply_broadcast_settings`) can restart just the pipeline on the same
/// already-negotiated track instead of renegotiating a new one over the
/// signaling channel, which the guest would need a fresh offer/answer round
/// trip for.
pub struct VideoTrackHandle {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
}

/// Adds a real video track to `pc` and negotiates it (no frames flow yet —
/// call [`spawn_capture_pipeline`] to actually start capturing/encoding).
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

/// Adds a real video track to `pc` and starts the capture -> hardware
/// encode -> RTP pipeline feeding it — see [`create_video_track`] +
/// [`spawn_capture_pipeline`], which this just calls in sequence for the
/// common case (starting a broadcast from scratch).
async fn attach_video_source(
    pc: &Arc<dyn PeerConnection>,
    source: CaptureSource,
    quality: StreamQuality,
    stop: Arc<AtomicBool>,
    preview: watch::Receiver<Option<Arc<video_preview::MjpegServer>>>,
) -> Result<VideoTrackHandle, BoxError> {
    let handle = create_video_track(pc).await?;
    spawn_capture_pipeline(&handle, source, quality, stop, preview);
    Ok(handle)
}

/// Starts the capture -> hardware encode -> RTP pipeline feeding `handle`'s
/// already-negotiated video track, running indefinitely until `stop` is set
/// or the capture/encode threads give up. Mirrors
/// `rtc::video_track_smoke_test`'s 3-stage threading shape (blocking
/// capture thread -> blocking encode thread -> async sample-writer task),
/// but indefinite, quality-aware, and using the hardware encoder instead of
/// the software one used there to validate the pipeline originally.
///
/// Can be called again later, with a fresh `stop`/`source`/`quality`, on the
/// *same* `handle` — that's exactly what "Aplicar" does. The old pipeline's
/// threads wind down shortly after their `stop` flips (checked once per
/// frame), slightly overlapping with the new pipeline's — an acceptable,
/// brief transition glitch rather than a full renegotiation, which for a
/// personal-use tool isn't worth the added complexity/risk.
///
/// `preview` is watched on every captured frame (cheaply — a `watch`
/// receiver's `borrow()` is synchronous): whenever it holds `Some(server)`,
/// a throttled copy of the raw RGBA frame is JPEG-encoded straight (no
/// H.264 round trip) and published to that server, so the broadcaster can
/// look at their own preview on demand without it costing anything when
/// nobody asked for it.
pub(crate) fn spawn_capture_pipeline(
    handle: &VideoTrackHandle,
    source: CaptureSource,
    quality: StreamQuality,
    stop: Arc<AtomicBool>,
    preview: watch::Receiver<Option<Arc<video_preview::MjpegServer>>>,
) {
    let video_track = handle.track.clone();
    let ssrc = handle.ssrc;
    let payload_type = handle.payload_type;

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
    std::thread::spawn(move || {
        if let Err(e) = capture::capture_frames_until_stopped(&source, capture_stop, raw_frame_tx) {
            eprintln!("[session] capture error: {e}");
        }
    });

    let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        // The encoder needs real capture dimensions to open, which we only
        // know once the first frame arrives — so it's built lazily here
        // rather than passed in.
        let mut encoder: Option<HardwareH264Encoder> = None;
        let frame_interval = quality.frame_interval();
        let mut last_encoded_at = std::time::Instant::now() - frame_interval;

        let mut preview_encoder: Option<video_preview::RgbaPreviewEncoder> = None;
        let mut last_preview_at = std::time::Instant::now() - PREVIEW_INTERVAL;

        for frame in raw_frame_rx {
            if let Some(server) = preview.borrow().clone() {
                if last_preview_at.elapsed() >= PREVIEW_INTERVAL {
                    last_preview_at = std::time::Instant::now();
                    if preview_encoder.is_none() {
                        preview_encoder = video_preview::RgbaPreviewEncoder::new()
                            .inspect_err(|e| eprintln!("[session] failed to start preview encoder: {e}"))
                            .ok();
                    }
                    if let Some(pe) = preview_encoder.as_mut() {
                        match pe.encode(&frame.rgba, frame.width, frame.height) {
                            Ok(jpegs) => {
                                for jpeg in jpegs {
                                    server.publish(jpeg);
                                }
                            }
                            Err(e) => eprintln!("[session] preview encode error: {e}"),
                        }
                    }
                }
            }

            if encoder.is_none() {
                let (output_width, output_height) =
                    quality.target_dimensions(frame.width, frame.height);
                match HardwareH264Encoder::new(
                    frame.width,
                    frame.height,
                    output_width,
                    output_height,
                    quality.bitrate_bps(),
                    quality.fps,
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
                            if encoded_tx.send(bytes).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => eprintln!("[session] hardware encode error: {e}"),
                }
            }
        }

        if let Some(enc) = encoder.as_mut() {
            if let Ok(packets) = enc.flush() {
                for bytes in packets {
                    let _ = encoded_tx.send(bytes);
                }
            }
        }
    });

    let frame_duration = quality.frame_interval();
    tokio::spawn(async move {
        while let Some(data) = encoded_rx.recv().await {
            let sample = rtc::media::Sample { data: data.into(), duration: frame_duration, ..Default::default() };
            let _ = video_track.sample_writer(ssrc, payload_type).write_sample(&sample).await;
        }
    });
}

/// Duration of one audio sample handed to the track — fixed, unlike
/// video's `quality.frame_interval()`, since it's tied directly to
/// `audio_capture::FRAME_SAMPLES_PER_CHANNEL` (20ms @ 48kHz), not a
/// user-chosen fps.
const AUDIO_FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(20);

/// Adds a real Opus audio track to `pc` and starts the WASAPI loopback
/// capture -> Opus encode -> RTP pipeline feeding it — the audio
/// equivalent of [`attach_video_source`], minus the quality/downscale and
/// on-demand preview handling video needs (system audio is always sent at
/// its native quality, and there's no separate "preview your own audio"
/// feature). Runs until `stop` is set, same convention as the video
/// pipeline.
async fn attach_audio_source(
    pc: &Arc<dyn PeerConnection>,
    stop: Arc<AtomicBool>,
) -> Result<(), BoxError> {
    let audio_codec = opus_codec_parameters();
    let ssrc = rand::random::<u32>();
    let audio_track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
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

    let sender = pc.add_track(audio_track.clone() as Arc<dyn TrackLocal>).await?;
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or("sender has no negotiated audio codec")?;

    // Bounded to a few chunks (not 1, like video's raw-frame channel): a
    // dropped audio chunk is an audible click, so a little slack (~80ms)
    // to absorb momentary scheduling jitter is worth it — while staying
    // bounded so a genuinely slow encoder still can't build an
    // ever-growing backlog (see `capture.rs`'s reasoning, same idea).
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<audio_capture::CapturedAudio>(4);
    let capture_stop = stop.clone();
    std::thread::spawn(move || {
        if let Err(e) = audio_capture::capture_system_audio_until_stopped(capture_stop, raw_tx) {
            eprintln!("[session] audio capture error: {e}");
        }
    });

    let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let encoder = match audio_codec::AudioEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[session] failed to start Opus encoder: {e}");
                return;
            }
        };
        for chunk in raw_rx {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match encoder.encode(&chunk.samples) {
                Ok(bytes) => {
                    if encoded_tx.send(bytes).is_err() {
                        return;
                    }
                }
                Err(e) => eprintln!("[session] Opus encode error: {e}"),
            }
        }
    });

    tokio::spawn(async move {
        while let Some(data) = encoded_rx.recv().await {
            let sample =
                rtc::media::Sample { data: data.into(), duration: AUDIO_FRAME_DURATION, ..Default::default() };
            let _ = audio_track.sample_writer(ssrc, payload_type).write_sample(&sample).await;
        }
    });

    Ok(())
}

fn send_relay(
    signaling_tx: &mpsc::UnboundedSender<ClientMessage>,
    payload: RelayPayload,
) -> Result<(), BoxError> {
    let value = serde_json::to_value(payload)?;
    signaling_tx
        .send(ClientMessage::Relay(value))
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
            Some(SignalingEvent::Relay(payload)) => match serde_json::from_value(payload) {
                Ok(RelayPayload::Sdp { sdp_type, sdp }) => {
                    let desc = session_description_from_parts(sdp_type, sdp)?;
                    return Ok((desc, buffered_candidates));
                }
                Ok(RelayPayload::IceCandidate(candidate)) => buffered_candidates.push(candidate),
                Err(_) => {}
            },
            Some(SignalingEvent::PeerLeft) => {
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
) {
    tokio::spawn(async move {
        while let Some(candidate) = local_ice_rx.recv().await {
            let _ = send_relay(&signaling_tx, RelayPayload::IceCandidate(candidate));
        }
    });
}

/// Keeps applying ICE candidates that arrive after the initial handshake,
/// and closes the peer connection if the other side leaves or the
/// signaling connection itself drops. `stop` is `Some` only on the host
/// side — when the peer leaves, it's not enough to just close the
/// `PeerConnection` (writes to a closed connection just silently no-op):
/// the dedicated capture/encode threads started by `attach_video_source`
/// have no idea the connection ended and would otherwise keep capturing
/// and hardware-encoding the screen forever for nobody, which is exactly
/// the "broadcaster leaves the stream running" bug this fixes — see
/// CLAUDE_SESSIONS.md. `ended_tx` is set on both sides, so `lib.rs` can
/// react (reset state, tell the UI) without polling.
fn pump_remaining_signaling(
    pc: Arc<dyn PeerConnection>,
    mut signaling_rx: mpsc::UnboundedReceiver<SignalingEvent>,
    stop: Option<Arc<AtomicBool>>,
    ended_tx: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        while let Some(event) = signaling_rx.recv().await {
            match event {
                SignalingEvent::Relay(payload) => {
                    if let Ok(RelayPayload::IceCandidate(candidate)) = serde_json::from_value(payload) {
                        let _ = pc.add_ice_candidate(candidate).await;
                    }
                }
                SignalingEvent::PeerLeft | SignalingEvent::Disconnected => {
                    if let Some(stop) = &stop {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
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
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use webrtc::media_stream::track_remote::TrackRemoteEvent;

    #[test]
    fn prints_usable_local_addrs() {
        println!("{:?}", usable_local_addrs());
    }

    #[test]
    fn prints_local_signaling_urls() {
        println!("{:?}", local_signaling_urls());
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
        tx.send(ClientMessage::Host).expect("signaling channel closed");

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

        let (code, hosting) = start_hosting(&signaling_addr).await.expect("start_hosting failed");
        println!("pairing code: {code}");

        let quality = StreamQuality { resolution_height: 720, fps: 30, audio: false };
        let stop = Arc::new(AtomicBool::new(false));
        let (_preview_tx, preview_rx) = watch::channel(None);

        // try_join, not join: if one side errors out fast (e.g. a bad
        // offer), the other would otherwise hang waiting for a reply that
        // will never come, and the test would only fail once the 20s
        // timeout below expired instead of with the real error.
        let (host_session, mut guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality, stop.clone(), preview_rx),
                join_session(&signaling_addr, code),
            ),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        // Prove real video actually flows end-to-end (not just that
        // signaling/ICE completed): the guest side should receive the
        // host's video track, and real RTP packets over it.
        let track = tokio::time::timeout(Duration::from_secs(15), guest_session.incoming_tracks.recv())
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

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = guest_session.peer_connection.close().await;
        let _ = host_session.peer_connection.close().await;
    }

    /// Not run in CI (needs a real display/GPU) — run manually with
    /// `cargo test -- --ignored --nocapture`. Confirms the broadcaster's
    /// own on-demand live preview produces real JPEG frames straight from
    /// the RGBA capture — no H.264 encode/decode round trip involved, and
    /// entirely separate from the actual outgoing WebRTC stream.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn broadcaster_can_preview_their_own_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false };
        let stop = Arc::new(AtomicBool::new(false));
        let (preview_tx, preview_rx) = watch::channel(None);

        let (host_session, guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality, stop.clone(), preview_rx),
                join_session(&signaling_addr, code),
            ),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        // Turn the preview on, same as the "Ver prévia" button does.
        let server = Arc::new(
            video_preview::MjpegServer::start()
                .await
                .expect("failed to start the preview server"),
        );
        let preview_url = server.url.clone();
        preview_tx.send(Some(server)).expect("preview channel closed unexpectedly");

        let host_port = preview_url.trim_start_matches("http://").trim_end_matches("/stream");
        let mut client = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(host_port))
            .await
            .expect("connect timed out")
            .expect("failed to connect to the preview server");

        let mut received = Vec::new();
        let mut buf = [0u8; 8192];
        let found_jpeg = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let n = client.read(&mut buf).await.expect("read failed");
                assert!(n > 0, "preview server closed the connection early");
                received.extend_from_slice(&buf[..n]);
                if received.windows(2).any(|w| w == [0xFF, 0xD8]) {
                    break;
                }
            }
        })
        .await
        .is_ok();

        assert!(found_jpeg, "expected a real JPEG frame from the broadcaster's own preview");

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = guest_session.peer_connection.close().await;
        let _ = host_session.peer_connection.close().await;
    }

    /// Not run in CI (needs a real display/GPU) — run manually with
    /// `cargo test -- --ignored --nocapture`. Proves the actual bug a
    /// friend hit: without this, a watcher leaving didn't stop the
    /// broadcaster's capture/encode pipeline at all — it just kept running
    /// forever with nobody watching. Confirms both halves of the fix: the
    /// guest's `notify_leaving()` reaches the host as a real `PeerLeft`
    /// over the real (embedded) signaling server, and the host reacts by
    /// setting its `stop` flag (what actually ends the capture/encode
    /// threads in `attach_video_source`) and flipping `ended`, not just
    /// closing the `PeerConnection`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn guest_leaving_stops_the_hosts_capture_pipeline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false };
        let stop = Arc::new(AtomicBool::new(false));
        let (_preview_tx, preview_rx) = watch::channel(None);

        let (mut host_session, guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality, stop.clone(), preview_rx),
                join_session(&signaling_addr, code),
            ),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        assert!(!stop.load(std::sync::atomic::Ordering::Relaxed), "stop shouldn't be set yet");

        // Same call the "Sair"/"Parar de assistir" button makes.
        guest_session.notify_leaving();

        tokio::time::timeout(Duration::from_secs(10), host_session.ended.wait_for(|ended| *ended))
            .await
            .expect("host's `ended` never fired after the guest left")
            .expect("host's `ended` watch channel closed unexpectedly");

        assert!(
            stop.load(std::sync::atomic::Ordering::Relaxed),
            "the host's capture/encode pipeline should have been told to stop"
        );

        let _ = guest_session.peer_connection.close().await;
        let _ = host_session.peer_connection.close().await;
    }

    /// Not run in CI (needs a real display/GPU) — run manually with
    /// `cargo test -- --ignored --nocapture`. Proves "Aplicar" (changing
    /// quality/source mid-broadcast) actually works: restarting the
    /// capture/encode pipeline on the *same* video track (what
    /// `lib.rs`'s `apply_broadcast_settings` does) still delivers real RTP
    /// video to the guest afterwards, with no renegotiation — the guest
    /// never has to reconnect or receive a new track.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn applying_new_settings_keeps_streaming_on_the_same_track() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr).await.expect("start_hosting failed");
        let quality = StreamQuality { resolution_height: 480, fps: 30, audio: false };
        let stop = Arc::new(AtomicBool::new(false));
        let (preview_tx, preview_rx) = watch::channel(None);

        let (host_session, mut guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(
                hosting.wait_for_peer(CaptureSource::Monitor, quality, stop.clone(), preview_rx),
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

        // "Aplicar": stop the original pipeline and start a new one, with
        // different settings, on the *same* negotiated track — same
        // sequence `apply_broadcast_settings` runs.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let new_quality = StreamQuality { resolution_height: 240, fps: 15, audio: false };
        let new_stop = Arc::new(AtomicBool::new(false));
        let handle = host_session.video_track.as_ref().expect("host session should have a video track");
        spawn_capture_pipeline(
            handle,
            CaptureSource::Monitor,
            new_quality,
            new_stop.clone(),
            preview_tx.subscribe(),
        );

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

        new_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = guest_session.peer_connection.close().await;
        let _ = host_session.peer_connection.close().await;
    }
}
