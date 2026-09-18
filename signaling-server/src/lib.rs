//! Minimal WebRTC signaling relay.
//!
//! Pairs a host with one or more guests by a short numeric code and relays
//! whatever JSON payload they send each other (SDP offer/answer, ICE
//! candidates) — it never inspects or logs the payload, and never sees the
//! actual video (that's always peer-to-peer over WebRTC once signaling is
//! done, and stays peer-to-peer *per guest* even with several watching the
//! same code — see `session.rs` for how the host fans one shared
//! capture/encode out to N separate `PeerConnection`s).
//!
//! Protocol (JSON text frames over a plain WebSocket):
//! - Client -> Server `{"type":"host","max_viewers":<number|null>}` — start
//!   a new room. `max_viewers: null` (or the field omitted) means no cap.
//! - Server -> Client `{"type":"hosting","code":"482193"}` — the code to
//!   share with whoever's going to watch.
//! - Client -> Server `{"type":"join","code":"482193"}` — join an existing
//!   room (fails with an `error` if it's already at `max_viewers`).
//! - Server -> host `{"type":"paired","guest_id":<number>}` — a new viewer
//!   joined; `guest_id` is how the host tells this specific viewer's
//!   relayed messages apart from any others already connected.
//! - Server -> guest `{"type":"paired"}` — no `guest_id`: a guest only ever
//!   has one peer (the host), so it never needs to address anything.
//! - Client (host) -> Server `{"type":"relay","to":<guest_id>,"payload":<anything>}`
//!   — forwarded to that one guest as `{"type":"relay","payload":<anything>}`.
//! - Client (guest) -> Server `{"type":"relay","payload":<anything>}` —
//!   forwarded to the host as `{"type":"relay","guest_id":<id>,"payload":<anything>}`,
//!   tagged with the sending guest's id so the host knows which of its
//!   (possibly several) in-progress handshakes/connections it's for.
//! - Server -> Client `{"type":"peer_left","guest_id":<number|null>}` — for
//!   a guest, always `null` (their one peer, the host, is gone — the whole
//!   broadcast ended). For the host, the id of whichever specific viewer
//!   disconnected (the broadcast itself keeps going for everyone else).
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

/// Identifies one guest within a room — scoped to that room only (not
/// globally unique), assigned in join order starting at 0.
type GuestId = u32;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Host {
        /// `None`/omitted means no limit — any number of guests can join
        /// with the code.
        #[serde(default)]
        max_viewers: Option<u32>,
    },
    Join {
        code: String,
    },
    Relay {
        payload: serde_json::Value,
        /// Only meaningful (and required to actually go anywhere) when the
        /// sender is the host — which of its guests this is for. A guest
        /// never sets this: it only ever has one peer, the host.
        #[serde(default)]
        to: Option<GuestId>,
    },
    /// Sent when a client wants to end the session on purpose (e.g. a
    /// "parar transmissão"/"sair" button), without needing its own way to
    /// force-close the underlying socket (the client-side WebSocket split
    /// into separate read/write halves has no such handle — see
    /// `signaling_client.rs`). Treated exactly like an actual disconnect.
    Leave,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    Hosting {
        code: &'a str,
    },
    Paired {
        #[serde(skip_serializing_if = "Option::is_none")]
        guest_id: Option<GuestId>,
    },
    PeerLeft {
        #[serde(skip_serializing_if = "Option::is_none")]
        guest_id: Option<GuestId>,
    },
    Relay {
        #[serde(skip_serializing_if = "Option::is_none")]
        guest_id: Option<GuestId>,
        payload: serde_json::Value,
    },
    Error {
        message: &'a str,
    },
}

impl ServerMessage<'_> {
    fn into_ws(&self) -> WsMessage {
        WsMessage::Text(serde_json::to_string(self).expect("ServerMessage always serializes"))
    }
}

type PeerTx = mpsc::UnboundedSender<WsMessage>;

struct Room {
    host: PeerTx,
    guests: HashMap<GuestId, PeerTx>,
    max_viewers: Option<u32>,
    next_guest_id: GuestId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Host,
    Guest(GuestId),
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
            ClientMessage::Host { max_viewers } => {
                if membership.is_some() {
                    let _ = tx.send(
                        ServerMessage::Error { message: "já está em uma sala" }.into_ws(),
                    );
                    continue;
                }
                let code = generate_unique_code(&rooms).await;
                rooms.lock().await.insert(
                    code.clone(),
                    Room { host: tx.clone(), guests: HashMap::new(), max_viewers, next_guest_id: 0 },
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
                    Some(room)
                        if room.max_viewers.is_some_and(|max| room.guests.len() as u32 >= max) =>
                    {
                        let _ =
                            tx.send(ServerMessage::Error { message: "sala já está cheia" }.into_ws());
                    }
                    Some(room) => {
                        let guest_id = room.next_guest_id;
                        room.next_guest_id += 1;
                        room.guests.insert(guest_id, tx.clone());
                        let _ = room
                            .host
                            .send(ServerMessage::Paired { guest_id: Some(guest_id) }.into_ws());
                        let _ = tx.send(ServerMessage::Paired { guest_id: None }.into_ws());
                        membership = Some((code, Role::Guest(guest_id)));
                    }
                }
            }

            ClientMessage::Relay { payload, to } => {
                let Some((code, role)) = &membership else {
                    let _ = tx.send(
                        ServerMessage::Error { message: "entre numa sala primeiro" }.into_ws(),
                    );
                    continue;
                };
                let rooms_guard = rooms.lock().await;
                let Some(room) = rooms_guard.get(code) else { continue };
                match role {
                    Role::Host => {
                        // The host is talking to potentially several
                        // guests at once — `to` says which one this is
                        // for. No `to` (or an id nobody recognizes) means
                        // there's nowhere to actually send it.
                        if let Some(guest_tx) = to.and_then(|id| room.guests.get(&id)) {
                            let _ = guest_tx
                                .send(ServerMessage::Relay { guest_id: None, payload }.into_ws());
                        }
                    }
                    Role::Guest(id) => {
                        let _ = room.host.send(
                            ServerMessage::Relay { guest_id: Some(*id), payload }.into_ws(),
                        );
                    }
                }
            }

            ClientMessage::Leave => {
                if let Some((code, role)) = membership.take() {
                    handle_departure(&rooms, &code, role).await;
                }
                break 'read_loop;
            }
        }
    }

    if let Some((code, role)) = membership {
        handle_departure(&rooms, &code, role).await;
    }

    writer.abort();
    Ok(())
}

/// Handles a client leaving (on purpose via `Leave`, or an actual socket
/// disconnect — both end up here). What happens depends on *who* left:
/// - The **host** leaving ends the whole broadcast — every connected guest
///   gets `PeerLeft` and the room is removed outright, since there's
///   nothing left to watch.
/// - A **guest** leaving only affects that one guest — the host gets
///   `PeerLeft` with that guest's id so it can tear down just that
///   viewer's `PeerConnection`, but the room stays open (same code still
///   works) for whoever else is watching, and for new joins if there's
///   still room under `max_viewers`.
async fn handle_departure(rooms: &Rooms, code: &str, role: Role) {
    let mut rooms_guard = rooms.lock().await;
    match role {
        Role::Host => {
            if let Some(room) = rooms_guard.remove(code) {
                for guest_tx in room.guests.values() {
                    let _ = guest_tx.send(ServerMessage::PeerLeft { guest_id: None }.into_ws());
                }
            }
        }
        Role::Guest(id) => {
            if let Some(room) = rooms_guard.get_mut(code) {
                room.guests.remove(&id);
                let _ = room.host.send(ServerMessage::PeerLeft { guest_id: Some(id) }.into_ws());
            }
        }
    }
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
            .send(WsMessage::Text(json!({"type": "host", "max_viewers": null}).to_string()))
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
        let host_paired = recv_json(&mut host_ws).await;
        assert_eq!(host_paired["type"], "paired");
        let guest_id = host_paired["guest_id"].as_u64().expect("host should learn the guest's id");

        // Host -> guest relay (addressed by guest_id).
        host_ws
            .send(WsMessage::Text(
                json!({"type": "relay", "to": guest_id, "payload": {"kind": "offer", "sdp": "v=0..."}})
                    .to_string(),
            ))
            .await
            .unwrap();
        let relayed = recv_json(&mut guest_ws).await;
        assert_eq!(relayed["type"], "relay");
        assert_eq!(relayed["payload"]["kind"], "offer");
        assert_eq!(relayed["payload"]["sdp"], "v=0...");

        // Guest -> host relay, the other direction — tagged with the
        // guest's id so the host can tell it apart from other viewers.
        guest_ws
            .send(WsMessage::Text(
                json!({"type": "relay", "payload": {"kind": "answer"}}).to_string(),
            ))
            .await
            .unwrap();
        let relayed_back = recv_json(&mut host_ws).await;
        assert_eq!(relayed_back["type"], "relay");
        assert_eq!(relayed_back["guest_id"], guest_id);
        assert_eq!(relayed_back["payload"]["kind"], "answer");

        // Guest disconnects; host should be told which one left, but stays
        // in the room itself (a solo guest leaving doesn't end a broadcast
        // that could still have other viewers).
        drop(guest_ws);
        let peer_left = recv_json(&mut host_ws).await;
        assert_eq!(peer_left["type"], "peer_left");
        assert_eq!(peer_left["guest_id"], guest_id);
    }

    #[tokio::test]
    async fn multiple_guests_can_join_the_same_unlimited_room() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));
        let url = format!("ws://{addr}");

        let (mut host_ws, _) = connect_async(&url).await.expect("host connects");
        host_ws
            .send(WsMessage::Text(json!({"type": "host", "max_viewers": null}).to_string()))
            .await
            .unwrap();
        let code = recv_json(&mut host_ws).await["code"].as_str().unwrap().to_owned();

        let (mut guest_a, _) = connect_async(&url).await.expect("guest a connects");
        guest_a
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut guest_a).await["type"], "paired");
        let paired_a = recv_json(&mut host_ws).await;
        let guest_a_id = paired_a["guest_id"].as_u64().unwrap();

        let (mut guest_b, _) = connect_async(&url).await.expect("guest b connects");
        guest_b
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut guest_b).await["type"], "paired");
        let paired_b = recv_json(&mut host_ws).await;
        let guest_b_id = paired_b["guest_id"].as_u64().unwrap();

        assert_ne!(guest_a_id, guest_b_id, "each guest should get a distinct id");

        // Host addresses guest B specifically — guest A must not see it.
        host_ws
            .send(WsMessage::Text(
                json!({"type": "relay", "to": guest_b_id, "payload": {"for": "b"}}).to_string(),
            ))
            .await
            .unwrap();
        let received_by_b = recv_json(&mut guest_b).await;
        assert_eq!(received_by_b["payload"]["for"], "b");

        // Guest A leaving doesn't kick guest B or close the room.
        drop(guest_a);
        let left = recv_json(&mut host_ws).await;
        assert_eq!(left["type"], "peer_left");
        assert_eq!(left["guest_id"], guest_a_id);

        host_ws
            .send(WsMessage::Text(
                json!({"type": "relay", "to": guest_b_id, "payload": {"still": "here"}}).to_string(),
            ))
            .await
            .unwrap();
        let still_there = recv_json(&mut guest_b).await;
        assert_eq!(still_there["payload"]["still"], "here");
    }

    #[tokio::test]
    async fn rejects_joins_past_max_viewers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));
        let url = format!("ws://{addr}");

        let (mut host_ws, _) = connect_async(&url).await.expect("host connects");
        host_ws
            .send(WsMessage::Text(json!({"type": "host", "max_viewers": 1}).to_string()))
            .await
            .unwrap();
        let code = recv_json(&mut host_ws).await["code"].as_str().unwrap().to_owned();

        let (mut guest_a, _) = connect_async(&url).await.expect("guest a connects");
        guest_a
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut guest_a).await["type"], "paired");
        let _ = recv_json(&mut host_ws).await; // host's own "paired" for guest a

        let (mut guest_b, _) = connect_async(&url).await.expect("guest b connects");
        guest_b
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        let rejection = recv_json(&mut guest_b).await;
        assert_eq!(rejection["type"], "error");
    }

    #[tokio::test]
    async fn host_leaving_notifies_every_guest_and_closes_the_room() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));
        let url = format!("ws://{addr}");

        let (mut host_ws, _) = connect_async(&url).await.expect("host connects");
        host_ws
            .send(WsMessage::Text(json!({"type": "host", "max_viewers": null}).to_string()))
            .await
            .unwrap();
        let code = recv_json(&mut host_ws).await["code"].as_str().unwrap().to_owned();

        let (mut guest_a, _) = connect_async(&url).await.expect("guest a connects");
        guest_a
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut guest_a).await["type"], "paired");
        let _ = recv_json(&mut host_ws).await;

        let (mut guest_b, _) = connect_async(&url).await.expect("guest b connects");
        guest_b
            .send(WsMessage::Text(json!({"type": "join", "code": code}).to_string()))
            .await
            .unwrap();
        assert_eq!(recv_json(&mut guest_b).await["type"], "paired");
        let _ = recv_json(&mut host_ws).await;

        host_ws
            .send(WsMessage::Text(json!({"type": "leave"}).to_string()))
            .await
            .unwrap();

        assert_eq!(recv_json(&mut guest_a).await["type"], "peer_left");
        assert_eq!(recv_json(&mut guest_b).await["type"], "peer_left");
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
            .send(WsMessage::Text(json!({"type": "host", "max_viewers": null}).to_string()))
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
            .send(WsMessage::Text(json!({"type": "host", "max_viewers": null}).to_string()))
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
