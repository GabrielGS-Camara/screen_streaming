//! WebRTC transport, built on the `webrtc` crate.
//!
//! Note on the crate's shape: `webrtc` 0.20 is built as an async layer over
//! a Sans-I/O core (the `rtc` crate) and is *event-handler*, not
//! *callback*, based — instead of registering `on_xxx(Box::new(...))`
//! closures on the connection, you implement [`PeerConnectionEventHandler`]
//! once and hand it to the builder. Data/track events are pulled by polling
//! (`DataChannel::poll()` / `TrackRemote::poll()`) rather than pushed via a
//! callback. Codec setup (`MediaEngine`, codec registration,
//! `register_default_interceptors`) lives in the lower-level `rtc` crate,
//! re-exported only partially from `webrtc` — see the `rtc::` imports below.

use std::sync::Arc;
use std::time::Duration;

use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfiguration,
    RTCIceCandidateInit, RTCIceConnectionState, RTCIceGatheringState, RTCPeerConnectionIceEvent,
    RTCPeerConnectionState,
};

use crate::capture;
use crate::encoding::H264Encoder;

/// Forwards this peer's locally-gathered ICE candidates, and any incoming
/// data channel or media track, out through channels, so the code driving
/// the connection (outside the handler) can react to them.
struct Handler {
    ice_candidates: mpsc::UnboundedSender<RTCIceCandidateInit>,
    incoming_data_channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
    incoming_tracks: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
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

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.incoming_tracks.send(track);
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        eprintln!("[rtc] ICE connection state: {state:?}");
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        eprintln!("[rtc] peer connection state: {state:?}");
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        eprintln!("[rtc] ICE gathering state: {state:?}");
    }
}

/// `udp_addrs` controls which network interface(s) ICE gathers host
/// candidates from: `vec!["127.0.0.1:0"]` for the in-process tests below
/// (both peers are in this same process, loopback is enough); for real
/// sessions, pass every *usable* local address explicitly (see
/// `session::usable_local_addrs`) rather than the tempting `"0.0.0.0:0"`
/// wildcard — this crate has no way to filter out link-local/VPN
/// interfaces during gathering (the setting exists in the underlying `rtc`
/// crate but is commented out as a TODO), and at least one of those can
/// hang gathering forever instead of just failing that one candidate.
/// `config` is where a STUN server goes — irrelevant for the loopback
/// tests, required for real cross-machine connections.
pub(crate) async fn build_peer_connection(
    media_engine: MediaEngine,
    config: RTCConfiguration,
    udp_addrs: Vec<String>,
    ice_candidates: mpsc::UnboundedSender<RTCIceCandidateInit>,
    incoming_data_channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
    incoming_tracks: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
) -> Result<Arc<dyn PeerConnection>, Box<dyn std::error::Error + Send + Sync>> {
    let mut media_engine = media_engine;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            ice_candidates,
            incoming_data_channels,
            incoming_tracks,
        }))
        .with_udp_addrs(udp_addrs)
        .build()
        .await?;

    Ok(Arc::new(pc))
}

/// Wires the two peers' trickle ICE together: whatever local candidate one
/// side gathers gets handed to the other side's `add_ice_candidate`.
fn forward_ice_candidates_between(
    offerer: Arc<dyn PeerConnection>,
    mut offerer_ice_rx: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    answerer: Arc<dyn PeerConnection>,
    mut answerer_ice_rx: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
) {
    let answerer_for_ice = answerer.clone();
    tokio::spawn(async move {
        while let Some(candidate) = offerer_ice_rx.recv().await {
            let _ = answerer_for_ice.add_ice_candidate(candidate).await;
        }
    });
    tokio::spawn(async move {
        while let Some(candidate) = answerer_ice_rx.recv().await {
            let _ = offerer.add_ice_candidate(candidate).await;
        }
    });
}

/// Creates two peer connections in-process, connects them over ICE, opens a
/// data channel and sends one message end-to-end. Used to validate that the
/// WebRTC engine itself works on this machine before building a real video
/// track or a real (cross-process) signaling transport on top of it.
pub async fn loopback_data_channel_smoke_test(
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let (offerer_ice_tx, offerer_ice_rx) = mpsc::unbounded_channel();
    let (offerer_dc_tx, _offerer_dc_rx) = mpsc::unbounded_channel();
    let (offerer_track_tx, _offerer_track_rx) = mpsc::unbounded_channel();
    let offerer = build_peer_connection(
        MediaEngine::default(),
        RTCConfiguration::default(),
        vec!["127.0.0.1:0".to_owned()],
        offerer_ice_tx,
        offerer_dc_tx,
        offerer_track_tx,
    )
    .await?;

    let (answerer_ice_tx, answerer_ice_rx) = mpsc::unbounded_channel();
    let (answerer_dc_tx, mut answerer_dc_rx) = mpsc::unbounded_channel();
    let (answerer_track_tx, _answerer_track_rx) = mpsc::unbounded_channel();
    let answerer = build_peer_connection(
        MediaEngine::default(),
        RTCConfiguration::default(),
        vec!["127.0.0.1:0".to_owned()],
        answerer_ice_tx,
        answerer_dc_tx,
        answerer_track_tx,
    )
    .await?;

    forward_ice_candidates_between(
        offerer.clone(),
        offerer_ice_rx,
        answerer.clone(),
        answerer_ice_rx,
    );

    let (result_tx, mut result_rx) = mpsc::channel::<String>(1);

    let offerer_channel = offerer.create_data_channel("smoke-test", None).await?;
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

/// H.264 codec definition shared by both peers — they must agree on the
/// exact same payload type / fmtp line to negotiate the codec during
/// offer/answer.
pub(crate) fn h264_codec_parameters() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    }
}

/// Opus codec definition shared by both peers, same idea as
/// [`h264_codec_parameters`] — standard payload type 111, stereo, 48kHz
/// (Opus's only real-time-quality sample rate; it internally resamples
/// narrower content up to this).
pub(crate) fn opus_codec_parameters() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    }
}

#[derive(Debug)]
pub struct VideoTrackStats {
    pub frames_captured: u32,
    pub rtp_packets_received: u32,
    pub rtp_bytes_received: usize,
}

/// End-to-end proof that real screen content can flow over WebRTC: captures
/// a handful of real frames from the primary monitor, encodes each to H.264
/// (software encoder — see [`crate::encoding`]), sends them through a real
/// `TrackLocalStaticSample`, and confirms the other peer actually receives
/// RTP video packets for them.
pub async fn video_track_smoke_test(
    capture_duration_secs: u64,
) -> Result<VideoTrackStats, Box<dyn std::error::Error + Send + Sync>> {
    let video_codec = h264_codec_parameters();

    let mut offerer_media_engine = MediaEngine::default();
    offerer_media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
    let mut answerer_media_engine = MediaEngine::default();
    answerer_media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;

    let (offerer_ice_tx, offerer_ice_rx) = mpsc::unbounded_channel();
    let (offerer_dc_tx, _offerer_dc_rx) = mpsc::unbounded_channel();
    let (offerer_track_tx, _offerer_track_rx) = mpsc::unbounded_channel();
    let offerer = build_peer_connection(
        offerer_media_engine,
        RTCConfiguration::default(),
        vec!["127.0.0.1:0".to_owned()],
        offerer_ice_tx,
        offerer_dc_tx,
        offerer_track_tx,
    )
    .await?;

    let (answerer_ice_tx, answerer_ice_rx) = mpsc::unbounded_channel();
    let (answerer_dc_tx, _answerer_dc_rx) = mpsc::unbounded_channel();
    let (answerer_track_tx, mut answerer_track_rx) = mpsc::unbounded_channel();
    let answerer = build_peer_connection(
        answerer_media_engine,
        RTCConfiguration::default(),
        vec!["127.0.0.1:0".to_owned()],
        answerer_ice_tx,
        answerer_dc_tx,
        answerer_track_tx,
    )
    .await?;

    forward_ice_candidates_between(
        offerer.clone(),
        offerer_ice_rx,
        answerer.clone(),
        answerer_ice_rx,
    );

    let ssrc = rand::random::<u32>();
    let video_track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        "screen-streaming-stream".to_owned(),
        "screen-streaming-video".to_owned(),
        "screen".to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?);

    let sender = offerer
        .add_track(video_track.clone() as Arc<dyn TrackLocal>)
        .await?;
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or("sender has no negotiated codec")?;

    let offer = offerer.create_offer(None).await?;
    offerer.set_local_description(offer.clone()).await?;
    answerer.set_remote_description(offer).await?;

    let answer = answerer.create_answer(None).await?;
    answerer.set_local_description(answer.clone()).await?;
    offerer.set_remote_description(answer).await?;

    // Capture real frames on a dedicated OS thread (windows-capture's own
    // loop blocks the calling thread), encode each on a second dedicated
    // thread (CPU-bound, shouldn't run on a tokio worker), and forward the
    // resulting H.264 bytes to an async task that writes them to the track.
    let (raw_frame_tx, raw_frame_rx) = std::sync::mpsc::channel::<capture::CapturedFrame>();
    let capture_thread = std::thread::spawn(move || {
        capture::capture_primary_monitor_frames(capture_duration_secs, raw_frame_tx)
    });

    let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let encoder_thread = std::thread::spawn(move || -> Result<u32, String> {
        let mut encoder = H264Encoder::new(2_000_000, 30.0).map_err(|e| e.to_string())?;
        let mut frames_captured = 0u32;
        for frame in raw_frame_rx {
            frames_captured += 1;
            match encoder.encode_rgba(&frame.rgba, frame.width as usize, frame.height as usize) {
                Ok(bytes) if !bytes.is_empty() => {
                    if encoded_tx.send(bytes).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("H.264 encode error: {e}"),
            }
        }
        Ok(frames_captured)
    });

    let sample_writer_task = tokio::spawn(async move {
        let mut sent = 0u32;
        while let Some(data) = encoded_rx.recv().await {
            let sample = rtc::media::Sample {
                data: data.into(),
                duration: Duration::from_millis(33),
                ..Default::default()
            };
            if video_track
                .sample_writer(ssrc, payload_type)
                .write_sample(&sample)
                .await
                .is_ok()
            {
                sent += 1;
            }
        }
        sent
    });

    let (stats_tx, mut stats_rx) = mpsc::channel::<(u32, usize)>(1);
    tokio::spawn(async move {
        if let Some(track) = answerer_track_rx.recv().await {
            let mut packets = 0u32;
            let mut bytes = 0usize;
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    packets += 1;
                    bytes += packet.payload.len();
                    if packets >= 3 {
                        break;
                    }
                }
            }
            let _ = stats_tx.send((packets, bytes)).await;
        }
    });

    let (rtp_packets_received, rtp_bytes_received) =
        tokio::time::timeout(Duration::from_secs(capture_duration_secs + 15), stats_rx.recv())
            .await
            .map_err(|_| "timed out waiting for the answerer to receive RTP video packets")?
            .ok_or("no video track / RTP packets received")?;

    capture_thread
        .join()
        .map_err(|_| "capture thread panicked")??;
    let frames_captured = encoder_thread
        .join()
        .map_err(|_| "encoder thread panicked")??;
    let _ = sample_writer_task.await;

    offerer.close().await?;
    answerer.close().await?;

    Ok(VideoTrackStats {
        frames_captured,
        rtp_packets_received,
        rtp_bytes_received,
    })
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

    /// Not run in CI (no GPU/display) — run manually with
    /// `cargo test -- --ignored --nocapture` on a real machine.
    #[tokio::test]
    #[ignore]
    async fn streams_real_screen_capture_over_a_video_track() {
        let stats = video_track_smoke_test(3)
            .await
            .expect("video track smoke test failed");
        println!("{stats:?}");
        assert!(stats.rtp_packets_received > 0);
        assert!(stats.rtp_bytes_received > 0);
    }
}
