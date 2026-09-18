//! WebSocket client for the `signaling-server` relay (see
//! `signaling-server/src/lib.rs` for the protocol this speaks).
//!
//! Deliberately its own tiny protocol layer, separate from `rtc.rs`: this
//! module knows nothing about WebRTC, it just gets JSON payloads to/from
//! the paired peer. `session.rs` is what interprets those payloads as
//! SDP/ICE and drives the actual `PeerConnection`.

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Messages this client can send to the signaling server.
pub enum ClientMessage {
    Host,
    Join(String),
    Relay(serde_json::Value),
    /// Ends the session on purpose (e.g. "Parar transmissão"/"Sair") —
    /// tells the server to notify the peer immediately, same as an actual
    /// disconnect, without needing to actually drop this connection.
    Leave,
}

/// Messages/events the signaling server sends back.
#[derive(Debug)]
pub enum SignalingEvent {
    Hosting(String),
    Paired,
    PeerLeft,
    Relay(serde_json::Value),
    Error(String),
    /// The connection to the signaling server itself dropped (not a
    /// `PeerLeft` — that's the *other client* leaving the room).
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
                ClientMessage::Host => json!({"type": "host"}),
                ClientMessage::Join(code) => json!({"type": "join", "code": code}),
                ClientMessage::Relay(payload) => json!({"type": "relay", "payload": payload}),
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
            let event = match value["type"].as_str() {
                Some("hosting") => {
                    SignalingEvent::Hosting(value["code"].as_str().unwrap_or_default().to_owned())
                }
                Some("paired") => SignalingEvent::Paired,
                Some("peer_left") => SignalingEvent::PeerLeft,
                Some("relay") => SignalingEvent::Relay(value["payload"].clone()),
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
