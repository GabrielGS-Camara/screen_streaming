//! Minimal WebRTC signaling relay.
//!
//! Pairs two clients by a short numeric code and relays whatever JSON
//! payload they send each other (SDP offer/answer, ICE candidates) —
//! it never inspects or logs the payload, and never sees the actual video
//! (that's always peer-to-peer over WebRTC once signaling is done).
//!
//! Protocol (JSON text frames over a plain WebSocket):
//! - Client -> Server `{"type":"host"}` — start a new room.
//! - Server -> Client `{"type":"hosting","code":"482193"}` — the code to
//!   share with the other person.
//! - Client -> Server `{"type":"join","code":"482193"}` — join an existing
//!   room.
//! - Server -> both clients `{"type":"paired"}` — once joined, both sides
//!   get this.
//! - Client -> Server `{"type":"relay","payload":<anything>}` — forwarded
//!   verbatim to the other side as `{"type":"relay","payload":<anything>}`.
//! - Server -> Client `{"type":"peer_left"}` — the other side disconnected.
//! - Server -> Client `{"type":"error","message":"..."}` — bad code, room
//!   full, etc.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Host,
    Join { code: String },
    Relay { payload: serde_json::Value },
    /// Sent when a client wants to end the session on purpose (e.g. a
    /// "parar transmissão"/"sair" button), without needing its own way to
    /// force-close the underlying socket (the client-side WebSocket split
    /// into separate read/write halves has no such handle — see
    /// `signaling_client.rs`). Treated exactly like an actual disconnect:
    /// the other side gets `PeerLeft`, the room is torn down, and this
    /// connection itself ends right after.
    Leave,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    Hosting { code: &'a str },
    Paired,
    PeerLeft,
    Relay { payload: serde_json::Value },
    Error { message: &'a str },
}

impl ServerMessage<'_> {
    fn into_ws(&self) -> WsMessage {
        WsMessage::Text(serde_json::to_string(self).expect("ServerMessage always serializes"))
    }
}

type PeerTx = mpsc::UnboundedSender<WsMessage>;

struct Room {
    host: PeerTx,
    guest: Option<PeerTx>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Host,
    Guest,
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

/// Binds `addr` and serves the signaling relay forever.
pub async fn run(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve(listener).await
}

/// Serves the signaling relay on an already-bound listener. Split out from
/// [`run`] so tests can bind to port 0 (an OS-assigned free port) and read
/// back the real address instead of racing for a fixed port.
pub async fn serve(listener: TcpListener) -> std::io::Result<()> {
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let (stream, _) = listener.accept().await?;
        let rooms = rooms.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, rooms).await {
                eprintln!("[signaling] connection error: {e}");
            }
        });
    }
}

async fn generate_unique_code(rooms: &Rooms) -> String {
    loop {
        let code: u32 = rand::rng().random_range(100_000..1_000_000);
        let code = code.to_string();
        if !rooms.lock().await.contains_key(&code) {
            return code;
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    rooms: Rooms,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut stream) = ws.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut membership: Option<(String, Role)> = None;

    'read_loop: while let Some(Ok(msg)) = stream.next().await {
        let WsMessage::Text(text) = msg else {
            continue;
        };
        let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) else {
            let _ = tx.send(ServerMessage::Error { message: "mensagem inválida" }.into_ws());
            continue;
        };

        match client_msg {
            ClientMessage::Host => {
                if membership.is_some() {
                    let _ = tx.send(
                        ServerMessage::Error { message: "já está em uma sala" }.into_ws(),
                    );
                    continue;
                }
                let code = generate_unique_code(&rooms).await;
                rooms.lock().await.insert(
                    code.clone(),
                    Room { host: tx.clone(), guest: None },
                );
                let _ = tx.send(ServerMessage::Hosting { code: &code }.into_ws());
                membership = Some((code, Role::Host));
            }

            ClientMessage::Join { code } => {
                if membership.is_some() {
                    let _ = tx.send(
                        ServerMessage::Error { message: "já está em uma sala" }.into_ws(),
                    );
                    continue;
                }
                let mut rooms_guard = rooms.lock().await;
                match rooms_guard.get_mut(&code) {
                    None => {
                        let _ = tx.send(
                            ServerMessage::Error { message: "código não encontrado" }.into_ws(),
                        );
                    }
                    Some(room) if room.guest.is_some() => {
                        let _ =
                            tx.send(ServerMessage::Error { message: "sala já está cheia" }.into_ws());
                    }
                    Some(room) => {
                        room.guest = Some(tx.clone());
                        let _ = room.host.send(ServerMessage::Paired.into_ws());
                        let _ = tx.send(ServerMessage::Paired.into_ws());
                        membership = Some((code, Role::Guest));
                    }
                }
            }

            ClientMessage::Relay { payload } => {
                let Some((code, role)) = &membership else {
                    let _ = tx.send(
                        ServerMessage::Error { message: "entre numa sala primeiro" }.into_ws(),
                    );
                    continue;
                };
                let rooms_guard = rooms.lock().await;
                if let Some(room) = rooms_guard.get(code) {
                    let peer = match role {
                        Role::Host => room.guest.as_ref(),
                        Role::Guest => Some(&room.host),
                    };
                    if let Some(peer_tx) = peer {
                        let _ = peer_tx.send(ServerMessage::Relay { payload }.into_ws());
                    }
                }
            }

            ClientMessage::Leave => {
                if let Some((code, role)) = membership.take() {
                    notify_peer_left_and_close_room(&rooms, &code, role).await;
                }
                break 'read_loop;
            }
        }
    }

    if let Some((code, role)) = membership {
        notify_peer_left_and_close_room(&rooms, &code, role).await;
    }

    writer.abort();
    Ok(())
}

/// Tells whoever is still in `code`'s room (if anyone) that their peer is
/// gone, then removes the room — shared by an explicit `Leave` message and
/// an actual socket disconnect, since both mean the same thing to the
/// remaining side. Whoever left, the room can no longer pair a fresh 1:1
/// session — simplest correct behavior for this personal-use scale is to
/// just drop it and let the remaining side reconnect/host again.
async fn notify_peer_left_and_close_room(rooms: &Rooms, code: &str, role: Role) {
    let mut rooms_guard = rooms.lock().await;
    if let Some(room) = rooms_guard.get(code) {
        let peer = match role {
            Role::Host => room.guest.as_ref(),
            Role::Guest => Some(&room.host),
        };
        if let Some(peer_tx) = peer {
            let _ = peer_tx.send(ServerMessage::PeerLeft.into_ws());
        }
    }
    rooms_guard.remove(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::connect_async;

    async fn recv_json(
        ws: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
    ) -> serde_json::Value {
        loop {
            match ws.next().await.expect("stream ended unexpectedly").expect("ws error") {
                WsMessage::Text(text) => return serde_json::from_str(&text).expect("valid JSON"),
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn pairs_two_clients_and_relays_messages_between_them() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));

        let url = format!("ws://{addr}");
        let (mut host_ws, _) = connect_async(&url).await.expect("host connects");
        let (mut guest_ws, _) = connect_async(&url).await.expect("guest connects");

        host_ws
            .send(WsMessage::Text(json!({"type": "host"}).to_string()))
            .await
            .unwrap();
        let hosting = recv_json(&mut host_ws).await;
        assert_eq!(hosting["type"], "hosting");
        let code = hosting["code"].as_str().unwrap().to_owned();
        assert_eq!(code.len(), 6, "expected a 6-digit pairing code, got {code:?}");

        guest_ws
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();

        assert_eq!(recv_json(&mut guest_ws).await["type"], "paired");
        assert_eq!(recv_json(&mut host_ws).await["type"], "paired");

        // Host -> guest relay.
        host_ws
            .send(WsMessage::Text(
                json!({"type": "relay", "payload": {"kind": "offer", "sdp": "v=0..."}}).to_string(),
            ))
            .await
            .unwrap();
        let relayed = recv_json(&mut guest_ws).await;
        assert_eq!(relayed["type"], "relay");
        assert_eq!(relayed["payload"]["kind"], "offer");
        assert_eq!(relayed["payload"]["sdp"], "v=0...");

        // Guest -> host relay, the other direction.
        guest_ws
            .send(WsMessage::Text(
                json!({"type": "relay", "payload": {"kind": "answer"}}).to_string(),
            ))
            .await
            .unwrap();
        let relayed_back = recv_json(&mut host_ws).await;
        assert_eq!(relayed_back["type"], "relay");
        assert_eq!(relayed_back["payload"]["kind"], "answer");

        // Guest disconnects; host should be told.
        drop(guest_ws);
        let peer_left = recv_json(&mut host_ws).await;
        assert_eq!(peer_left["type"], "peer_left");
    }

    #[tokio::test]
    async fn leave_notifies_the_peer_without_closing_the_sender_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));

        let url = format!("ws://{addr}");
        let (mut host_ws, _) = connect_async(&url).await.expect("host connects");
        let (mut guest_ws, _) = connect_async(&url).await.expect("guest connects");

        host_ws
            .send(WsMessage::Text(json!({"type": "host"}).to_string()))
            .await
            .unwrap();
        let code = recv_json(&mut host_ws).await["code"].as_str().unwrap().to_owned();

        guest_ws
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut guest_ws).await["type"], "paired");
        assert_eq!(recv_json(&mut host_ws).await["type"], "paired");

        // Guest leaves on purpose (e.g. "Sair"/"Parar de assistir") — the
        // host should find out immediately, same as an actual disconnect.
        guest_ws
            .send(WsMessage::Text(json!({"type": "leave"}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut host_ws).await["type"], "peer_left");

        // Starting a fresh room afterwards still works normally — the
        // "leave" cleanup didn't wedge shared room state for later clients.
        let (mut next_ws, _) = connect_async(&url).await.expect("new client connects");
        next_ws
            .send(WsMessage::Text(json!({"type": "host"}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut next_ws).await["type"], "hosting");
    }

    #[tokio::test]
    async fn rejects_an_unknown_pairing_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));

        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        ws.send(WsMessage::Text(json!({"type": "join", "code": "000000"}).to_string()))
            .await
            .unwrap();

        let response = recv_json(&mut ws).await;
        assert_eq!(response["type"], "error");
    }
}
