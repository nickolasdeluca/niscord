//! Pieces shared by both directions: codec setup, connection building,
//! signaling (de)serialization and ICE candidate buffering.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{RTCIceCandidate, RTCIceCandidateInit, RTCIceServer};
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use webrtc::peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler};
use webrtc::runtime::TokioRuntime;

use crate::{PeerEvents, PeerState, SignalData, TransportConfig};

pub const H264_PAYLOAD_TYPE: u8 = 102;
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// Constrained Baseline, level 5.1 (enough for 4K30 / 1080p120); both ends
/// are Niscord with OpenH264, which ignores the level anyway.
pub fn video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: VIDEO_CLOCK_RATE,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e033".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: H264_PAYLOAD_TYPE,
    }
}

pub fn media_engine() -> Result<MediaEngine> {
    let mut engine = MediaEngine::default();
    engine.register_codec(video_codec(), RtpCodecKind::Video)?;
    Ok(engine)
}

pub async fn build_peer_connection(
    config: &TransportConfig,
    engine: MediaEngine,
    registry: Registry,
    handler: Arc<dyn PeerConnectionEventHandler>,
) -> Result<Arc<dyn PeerConnection>> {
    let ice_servers = config
        .ice_servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone().unwrap_or_default(),
            credential: s.credential.clone().unwrap_or_default(),
        })
        .collect();
    let rtc_config = RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build();
    let pc = PeerConnectionBuilder::new()
        .with_configuration(rtc_config)
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(Arc::new(TokioRuntime))
        .with_udp_addrs(vec![config.udp_addr.clone()])
        .build()
        .await
        .context("creating peer connection")?;
    Ok(Arc::new(pc))
}

pub fn offer_signal(desc: &RTCSessionDescription) -> SignalData {
    SignalData::Offer { sdp: desc.sdp.clone() }
}

pub fn answer_signal(desc: &RTCSessionDescription) -> SignalData {
    SignalData::Answer { sdp: desc.sdp.clone() }
}

pub fn candidate_signal(candidate: &RTCIceCandidate) -> Option<SignalData> {
    let init = candidate.to_json().ok()?;
    Some(SignalData::Candidate { candidate: serde_json::to_string(&init).ok()? })
}

pub fn parse_candidate(json: &str) -> Result<RTCIceCandidateInit> {
    serde_json::from_str(json).context("malformed ICE candidate")
}

/// Map webrtc-rs connection states onto ours.
pub fn map_state(state: webrtc::peer_connection::RTCPeerConnectionState) -> Option<PeerState> {
    use webrtc::peer_connection::RTCPeerConnectionState as S;
    Some(match state {
        S::New | S::Connecting => PeerState::Connecting,
        S::Connected => PeerState::Connected,
        // Disconnected can recover by itself (ICE keeps checking); only report
        // it as connecting so the UI shows a spinner rather than an error.
        S::Disconnected => PeerState::Connecting,
        S::Failed => PeerState::Failed,
        S::Closed => PeerState::Closed,
        _ => return None,
    })
}

/// ICE candidates can arrive before the remote description is set, which
/// webrtc rejects; hold them until it is.
#[derive(Default)]
pub struct CandidateBuffer {
    inner: Mutex<(bool, Vec<RTCIceCandidateInit>)>,
}

impl CandidateBuffer {
    /// Returns the candidate back if it can be added right away.
    pub fn offer(&self, candidate: RTCIceCandidateInit) -> Option<RTCIceCandidateInit> {
        let mut inner = self.inner.lock().unwrap();
        if inner.0 {
            Some(candidate)
        } else {
            inner.1.push(candidate);
            None
        }
    }

    /// Mark the remote description as set and take what was buffered.
    pub fn release(&self) -> Vec<RTCIceCandidateInit> {
        let mut inner = self.inner.lock().unwrap();
        inner.0 = true;
        std::mem::take(&mut inner.1)
    }
}

pub async fn add_candidates(pc: &Arc<dyn PeerConnection>, candidates: Vec<RTCIceCandidateInit>) {
    for candidate in candidates {
        if let Err(err) = pc.add_ice_candidate(candidate).await {
            tracing::debug!("ignoring ICE candidate: {err}");
        }
    }
}

/// Event handler parts common to both directions.
pub struct BaseHandler {
    pub events: Arc<dyn PeerEvents>,
}

impl BaseHandler {
    pub fn ice_candidate(&self, candidate: &RTCIceCandidate) {
        if let Some(signal) = candidate_signal(candidate) {
            self.events.signal(signal);
        }
    }

    pub fn connection_state(&self, state: webrtc::peer_connection::RTCPeerConnectionState) {
        tracing::debug!(%state, "peer connection state");
        if let Some(state) = map_state(state) {
            self.events.state(state);
        }
    }
}
