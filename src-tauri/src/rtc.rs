//! WebRTC transport, built on the `webrtc` crate.
//!
//! This module currently only proves out the signaling/connection engine
//! (can two peers exchange an offer/answer, connect over ICE, and open a
//! data channel?) before any video track or real signaling transport is
//! built on top of it.
//!
//! Note on the crate's shape: `webrtc` 0.20 is built as an async layer over
//! a Sans-I/O core (the `rtc` crate) and is *event-handler*, not
//! *callback*, based — instead of registering `on_xxx(Box::new(...))`
//! closures on the connection, you implement [`PeerConnectionEventHandler`]
//! once and hand it to the builder. Data channel events are pulled by
//! polling (`DataChannel::poll().await`) rather than pushed via a callback.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceCandidateInit,
    RTCPeerConnectionIceEvent,
};

/// Forwards this peer's locally-gathered ICE candidates and any incoming
/// data channel out through channels, so the code driving the connection
/// (outside the handler) can react to them.
struct Handler {
    ice_candidates: mpsc::UnboundedSender<RTCIceCandidateInit>,
    incoming_data_channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.ice_candidates.send(init);
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.incoming_data_channels.send(data_channel);
    }
}

async fn build_peer_connection(
    ice_candidates: mpsc::UnboundedSender<RTCIceCandidateInit>,
    incoming_data_channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
) -> Result<Arc<dyn PeerConnection>, Box<dyn std::error::Error + Send + Sync>> {
    // No STUN server here on purpose: both peers are in this same process,
    // so plain host candidates (loopback/LAN) are enough to connect. STUN
    // only matters once real peers on different networks are involved.
    let pc = PeerConnectionBuilder::new()
        .with_handler(Arc::new(Handler {
            ice_candidates,
            incoming_data_channels,
        }))
        .with_udp_addrs(vec!["127.0.0.1:0"])
        .build()
        .await?;

    Ok(Arc::new(pc))
}

/// Creates two peer connections in-process, connects them over ICE, opens a
/// data channel and sends one message end-to-end. Used to validate that the
/// WebRTC engine itself works on this machine before building a real video
/// track or a real (cross-process) signaling transport on top of it.
pub async fn loopback_data_channel_smoke_test(
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let (offerer_ice_tx, mut offerer_ice_rx) = mpsc::unbounded_channel();
    let (offerer_dc_tx, _offerer_dc_rx) = mpsc::unbounded_channel();
    let offerer = build_peer_connection(offerer_ice_tx, offerer_dc_tx).await?;

    let (answerer_ice_tx, mut answerer_ice_rx) = mpsc::unbounded_channel();
    let (answerer_dc_tx, mut answerer_dc_rx) = mpsc::unbounded_channel();
    let answerer = build_peer_connection(answerer_ice_tx, answerer_dc_tx).await?;

    // Trickle ICE: forward each side's local candidates to the other side
    // directly (in-process) as they're discovered.
    let answerer_for_ice = answerer.clone();
    tokio::spawn(async move {
        while let Some(candidate) = offerer_ice_rx.recv().await {
            let _ = answerer_for_ice.add_ice_candidate(candidate).await;
        }
    });
    let offerer_for_ice = offerer.clone();
    tokio::spawn(async move {
        while let Some(candidate) = answerer_ice_rx.recv().await {
            let _ = offerer_for_ice.add_ice_candidate(candidate).await;
        }
    });

    let (result_tx, mut result_rx) = mpsc::channel::<String>(1);

    let offerer_channel = offerer
        .create_data_channel("smoke-test", None)
        .await?;
    tokio::spawn(async move {
        while let Some(event) = offerer_channel.poll().await {
            if let DataChannelEvent::OnOpen = event {
                let _ = offerer_channel
                    .send_text("hello from screen_streaming")
                    .await;
            }
        }
    });

    let result_tx_for_answer = result_tx.clone();
    tokio::spawn(async move {
        if let Some(answerer_channel) = answerer_dc_rx.recv().await {
            while let Some(event) = answerer_channel.poll().await {
                if let DataChannelEvent::OnMessage(msg) = event {
                    let text = String::from_utf8_lossy(&msg.data).into_owned();
                    let _ = result_tx_for_answer.send(text).await;
                }
            }
        }
    });

    let offer = offerer.create_offer(None).await?;
    offerer.set_local_description(offer.clone()).await?;
    answerer.set_remote_description(offer).await?;

    let answer = answerer.create_answer(None).await?;
    answerer.set_local_description(answer.clone()).await?;
    offerer.set_remote_description(answer).await?;

    let received = tokio::time::timeout(Duration::from_secs(10), result_rx.recv())
        .await
        .map_err(|_| "timed out waiting for the data channel message")?
        .ok_or("data channel closed before sending a message")?;

    offerer.close().await?;
    answerer.close().await?;

    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_two_peers_and_exchanges_a_data_channel_message() {
        let received = loopback_data_channel_smoke_test()
            .await
            .expect("loopback smoke test failed");
        assert_eq!(received, "hello from screen_streaming");
    }
}
