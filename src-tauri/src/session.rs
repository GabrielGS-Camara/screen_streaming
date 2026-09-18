//! Bridges [`crate::signaling_client`] to a real [`PeerConnection`]
//! (built via [`crate::rtc::build_peer_connection`]): host or join a
//! pairing-code session on the signaling server, exchange offer/answer/ICE
//! through it — real network relay, not the in-process forwarding
//! `rtc.rs`'s own tests use — and hand back a negotiating/connected
//! `PeerConnection` the rest of the app can attach tracks/data channels to.

use std::sync::Arc;

use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, RTCConfiguration, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCSdpType, RTCSessionDescription,
};

use crate::rtc::{build_peer_connection, h264_codec_parameters};
use crate::signaling_client::{self, ClientMessage, SignalingEvent};

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
    /// offer/answer/ICE handshake over the signaling channel.
    pub async fn wait_for_peer(mut self) -> Result<Session, BoxError> {
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

        // An offer with no media section at all (no track, no data
        // channel) comes out with no ICE credentials — the peer can't do
        // anything with it. A placeholder data channel is enough to get a
        // real, connectable offer; once real tracks get added here later
        // this stops being the only thing keeping the session negotiable.
        let _control_channel = pc.create_data_channel("screen-streaming-control", None).await?;

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
    use tokio::net::TcpListener;
    use webrtc::data_channel::DataChannelEvent;

    #[test]
    fn prints_usable_local_addrs() {
        println!("{:?}", usable_local_addrs());
    }

    /// Not run in CI (no real network/GPU needed here, but it does open
    /// real UDP sockets and hits a public STUN server) — run manually with
    /// `cargo test -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn hosts_and_joins_a_session_and_opens_a_data_channel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(signaling_server::serve(listener));
        let signaling_addr = format!("ws://{addr}");

        let (code, hosting) = start_hosting(&signaling_addr).await.expect("start_hosting failed");
        println!("pairing code: {code}");

        // try_join, not join: if one side errors out fast (e.g. a bad
        // offer), the other would otherwise hang waiting for a reply that
        // will never come, and the test would only fail once the 20s
        // timeout below expired instead of with the real error.
        let (host_session, guest_session) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::try_join(hosting.wait_for_peer(), join_session(&signaling_addr, code)),
        )
        .await
        .expect("handshake did not complete within 20s")
        .expect("host or guest side failed to connect");

        // Prove the underlying PeerConnection actually works, not just that
        // signaling completed: open a real data channel over it.
        let dc = host_session
            .peer_connection
            .create_data_channel("session-smoke-test", None)
            .await
            .expect("create_data_channel failed");

        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);

        // A data channel's OnOpen only fires once the underlying
        // ICE/DTLS/SCTP handshake with the peer actually completes, so
        // this alone proves the two sides really connected over the
        // network — we don't need the guest side to do anything with the
        // channel it receives for that.
        tokio::spawn(async move {
            while let Some(event) = dc.poll().await {
                if let DataChannelEvent::OnOpen = event {
                    let _ = tx.send("open".to_owned()).await;
                }
            }
        });

        let opened = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await;
        assert_eq!(opened, Ok(Some("open".to_owned())));

        let _ = guest_session.peer_connection.close().await;
        let _ = host_session.peer_connection.close().await;
    }
}
