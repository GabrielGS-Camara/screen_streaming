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

use crate::capture::{self, CaptureSource};
use crate::hw_encoding::HardwareH264Encoder;
use crate::quality::StreamQuality;
use crate::rtc::{build_peer_connection, h264_codec_parameters};
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
    Ok(media_engine)
}

/// A live WebRTC session paired through the signaling server: offer/answer
/// and ICE already exchanged, ICE candidates continuing to flow in the
/// background for the connection's lifetime.
pub struct Session {
    pub peer_connection: Arc<dyn PeerConnection>,
    pub incoming_tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
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
        attach_video_source(&pc, source, quality, stop, preview).await?;

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

        pump_remaining_signaling(pc.clone(), self.signaling_rx);

        Ok(Session { peer_connection: pc, incoming_tracks: track_rx })
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

    pump_remaining_signaling(pc.clone(), signaling_rx);

    Ok(Session { peer_connection: pc, incoming_tracks: track_rx })
}

/// How often the broadcaster's own live preview gets a fresh frame — much
/// lower than the real stream's fps, since it's only ever a quick look at
/// what's being sent, not something that needs to be smooth, and every
/// preview frame costs an extra JPEG encode on the sending machine.
const PREVIEW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Adds a real video track to `pc` and starts the capture -> hardware
/// encode -> RTP pipeline feeding it, running indefinitely until `stop` is
/// set or the capture/encode threads give up. Mirrors
/// `rtc::video_track_smoke_test`'s 3-stage threading shape (blocking
/// capture thread -> blocking encode thread -> async sample-writer task),
/// but indefinite, quality-aware, and using the hardware encoder instead of
/// the software one used there to validate the pipeline originally.
///
/// `preview` is watched on every captured frame (cheaply — a `watch`
/// receiver's `borrow()` is synchronous): whenever it holds `Some(server)`,
/// a throttled copy of the raw RGBA frame is JPEG-encoded straight (no
/// H.264 round trip) and published to that server, so the broadcaster can
/// look at their own preview on demand without it costing anything when
/// nobody asked for it.
async fn attach_video_source(
    pc: &Arc<dyn PeerConnection>,
    source: CaptureSource,
    quality: StreamQuality,
    stop: Arc<AtomicBool>,
    preview: watch::Receiver<Option<Arc<video_preview::MjpegServer>>>,
) -> Result<(), BoxError> {
    let video_codec = h264_codec_parameters();
    let ssrc = rand::random::<u32>();
    let video_track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
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

    let sender = pc.add_track(video_track.clone() as Arc<dyn TrackLocal>).await?;
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or("sender has no negotiated codec")?;

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
/// signaling connection itself drops.
fn pump_remaining_signaling(
    pc: Arc<dyn PeerConnection>,
    mut signaling_rx: mpsc::UnboundedReceiver<SignalingEvent>,
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
                    let _ = pc.close().await;
                    break;
                }
                _ => {}
            }
        }
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
}
