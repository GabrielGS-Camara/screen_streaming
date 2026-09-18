//! WebSocket client for the `signaling-server` relay (see
//! `signaling-server/src/lib.rs` for the protocol this speaks).
//!
//! Deliberately its own tiny protocol layer, separate from `rtc.rs`: this
//! module knows nothing about WebRTC, it just gets JSON payloads to/from
//! the paired peer(s). `session.rs` is what interprets those payloads as
//! SDP/ICE and drives the actual `PeerConnection`(s) — including, on the
//! host side, telling multiple guests' relayed messages apart by
//! `guest_id` when more than one person is watching the same broadcast.

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Messages this client can send to the signaling server.
pub enum ClientMessage {
    /// `max_viewers: None` means no cap — any number of guests can join
    /// with the resulting code.
    Host { max_viewers: Option<u32> },
    Join(String),
    /// `to` is only meaningful (and required to actually reach anyone) when
    /// sent by a host with more than one potential guest — see
    /// `signaling-server`'s protocol doc comment. A guest always leaves it
    /// `None`: it only ever has one peer, the host.
    Relay { payload: serde_json::Value, to: Option<u32> },
    /// Ends the session on purpose (e.g. "Parar transmissão"/"Sair") —
    /// tells the server to notify the peer immediately, same as an actual
    /// disconnect, without needing to actually drop this connection.
    Leave,
}

/// Messages/events the signaling server sends back.
#[derive(Debug)]
pub enum SignalingEvent {
    Hosting(String),
    /// For the host, `Some(id)` — a new guest joined, and this is how its
    /// future relayed messages (and eventual departure) will be tagged.
    /// For a guest, always `None` (it only ever has one peer).
    Paired { guest_id: Option<u32> },
    /// For the host, `Some(id)` — that one specific guest disconnected (the
    /// broadcast itself keeps going for everyone else still watching). For
    /// a guest, always `None` — its one peer, the host, is gone, so the
    /// whole thing ended.
    PeerLeft { guest_id: Option<u32> },
    /// For the host, tagged with which guest this came from. Irrelevant
    /// for a guest (always from the host, the only peer it has).
    Relay { guest_id: Option<u32>, payload: serde_json::Value },
    Error(String),
    /// The connection to the signaling server itself dropped (not a
    /// `PeerLeft` — that's a *peer* leaving the room).
    Disconnected,
}

/// Connects to the signaling server at `addr` (e.g. `"ws://host:9876"`)
/// and returns a sender for outgoing messages plus a receiver for events
/// from the server. Both run on background tasks for the lifetime of the
/// connection.
pub async fn connect(
    addr: &str,
) -> Result<
    (mpsc::UnboundedSender<ClientMessage>, mpsc::UnboundedReceiver<SignalingEvent>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let (ws, _) = tokio_tungstenite::connect_async(addr).await?;
    let (mut sink, mut stream) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let payload = match msg {
                ClientMessage::Host { max_viewers } => json!({"type": "host", "max_viewers": max_viewers}),
                ClientMessage::Join(code) => json!({"type": "join", "code": code}),
                ClientMessage::Relay { payload, to } => {
                    json!({"type": "relay", "payload": payload, "to": to})
                }
                ClientMessage::Leave => json!({"type": "leave"}),
            };
            if sink.send(WsMessage::Text(payload.to_string())).await.is_err() {
                break;
            }
        }
    });

    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SignalingEvent>();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let WsMessage::Text(text) = msg else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            let guest_id = || value["guest_id"].as_u64().map(|n| n as u32);
            let event = match value["type"].as_str() {
                Some("hosting") => {
                    SignalingEvent::Hosting(value["code"].as_str().unwrap_or_default().to_owned())
                }
                Some("paired") => SignalingEvent::Paired { guest_id: guest_id() },
                Some("peer_left") => SignalingEvent::PeerLeft { guest_id: guest_id() },
                Some("relay") => {
                    SignalingEvent::Relay { guest_id: guest_id(), payload: value["payload"].clone() }
                }
                Some("error") => {
                    SignalingEvent::Error(value["message"].as_str().unwrap_or_default().to_owned())
                }
                _ => continue,
            };
            if evt_tx.send(event).is_err() {
                break;
            }
        }
        let _ = evt_tx.send(SignalingEvent::Disconnected);
    });

    Ok((out_tx, evt_rx))
}
