//! Sharer side: sends one H.264 track to one viewer.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use rtc::interceptor::{BandwidthEstimator, EstimatorStats, Gcc, PacketReport, Registry, Slot};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    CongestionFeedback, configure_congestion_control, register_default_interceptors,
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::PayloadType;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::peer_connection::{PeerConnection, PeerConnectionEventHandler, RTCPeerConnectionState};
use webrtc::rtp_transceiver::RtpSender;

use crate::common::{self, BaseHandler, CandidateBuffer};
use crate::{OutgoingEvents, SignalData, TransportConfig, forwarder};

/// Bounds for the bandwidth estimate, in bits per second.
#[derive(Debug, Clone, Copy)]
pub struct BitrateLimits {
    pub initial: u32,
    pub min: u32,
    pub max: u32,
}

/// Publishes the estimator's target so the app can read it; the estimator
/// itself is owned by the interceptor chain. (Pattern from webrtc-rs's
/// `bandwidth-estimation-from-disk` example.)
struct ReportingEstimator<E> {
    inner: E,
    target: Arc<AtomicU64>,
}

impl<E: BandwidthEstimator> ReportingEstimator<E> {
    fn publish(&self) {
        self.target.store(self.inner.target_bitrate() as u64, Ordering::Relaxed);
    }
}

impl<E: BandwidthEstimator> BandwidthEstimator for ReportingEstimator<E> {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.inner.on_reports(now, reports);
        self.publish();
    }

    fn target_bitrate(&self) -> f64 {
        self.inner.target_bitrate()
    }

    fn handle_timeout(&mut self, now: Instant) {
        self.inner.handle_timeout(now);
        self.publish();
    }

    fn poll_timeout(&self) -> Option<Instant> {
        self.inner.poll_timeout()
    }

    fn stats(&self) -> EstimatorStats {
        self.inner.stats()
    }
}

struct Handler {
    base: BaseHandler,
    connected: Arc<AtomicBool>,
    events: Arc<dyn OutgoingEvents>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: webrtc::peer_connection::RTCPeerConnectionIceEvent) {
        self.base.ice_candidate(&event.candidate);
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let connected = state == RTCPeerConnectionState::Connected;
        let was = self.connected.swap(connected, Ordering::Relaxed);
        self.base.connection_state(state);
        // A viewer can only start decoding at a keyframe.
        if connected && !was {
            self.events.keyframe_requested();
        }
    }
}

pub struct OutgoingPeer {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    payload_type: Mutex<Option<PayloadType>>,
    connected: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU64>,
    candidates: CandidateBuffer,
}

impl OutgoingPeer {
    /// Create the connection and send the offer through `events`.
    pub async fn start(
        config: &TransportConfig,
        limits: BitrateLimits,
        events: Arc<dyn OutgoingEvents>,
    ) -> Result<Self> {
        let mut engine = common::media_engine()?;
        let target_bitrate = Arc::new(AtomicU64::new(limits.initial as u64));
        let estimator = ReportingEstimator {
            inner: Gcc::new(limits.initial as f64, limits.min as f64, limits.max as f64),
            target: target_bitrate.clone(),
        };
        let registry = configure_congestion_control(Registry::new(), estimator, CongestionFeedback::Twcc, &mut engine)?;
        let registry = register_default_interceptors(registry, &mut engine)?;
        let registry = registry.with(Slot::from(forwarder::SLOT), forwarder::KeyframeRequestForwarder::default());

        let connected = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(Handler {
            base: BaseHandler { events: events.clone() },
            connected: connected.clone(),
            events: events.clone(),
        });
        let pc = common::build_peer_connection(config, engine, registry, handler).await?;

        let ssrc = rand_ssrc();
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "niscord".into(),
                "screen".into(),
                "screen".into(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                    codec: common::video_codec().rtp_codec,
                    ..Default::default()
                }],
            ),
        )?);
        let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await?;

        // Keyframe requests (PLI/FIR) from the viewer.
        {
            let track = track.clone();
            let events = events.clone();
            tokio::spawn(async move {
                while let Some(event) = track.poll().await {
                    if let TrackLocalEvent::OnRtcpPacket(_) = event {
                        events.keyframe_requested();
                    }
                }
            });
        }

        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;
        events.signal(common::offer_signal(&offer));

        Ok(Self {
            pc,
            track,
            sender,
            ssrc,
            payload_type: Mutex::new(None),
            connected,
            target_bitrate,
            candidates: CandidateBuffer::default(),
        })
    }

    /// Apply the viewer's answer or one of its ICE candidates.
    pub async fn handle_signal(&self, data: SignalData) -> Result<()> {
        match data {
            SignalData::Answer { sdp } => {
                self.pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
                let parameters = self.sender.get_parameters().await?;
                let pt = parameters.rtp_parameters.codecs.first().map(|c| c.payload_type);
                *self.payload_type.lock().unwrap() = Some(pt.context("viewer accepted no codec")?);
                common::add_candidates(&self.pc, self.candidates.release()).await;
            }
            SignalData::Candidate { candidate } => {
                if let Some(c) = self.candidates.offer(common::parse_candidate(&candidate)?) {
                    common::add_candidates(&self.pc, vec![c]).await;
                }
            }
            SignalData::Offer { .. } => bail!("unexpected offer from a viewer"),
        }
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Send one encoded access unit. Dropped silently until connected.
    pub async fn send_frame(&self, data: Bytes, duration: Duration) -> Result<()> {
        if !self.is_connected() {
            return Ok(());
        }
        let Some(pt) = *self.payload_type.lock().unwrap() else { return Ok(()) };
        let sample = Sample { data, duration, ..Sample::new(Instant::now()) };
        self.track.sample_writer(self.ssrc, pt).write_sample(&sample).await?;
        Ok(())
    }

    /// Current bandwidth estimate towards this viewer, in bits per second.
    pub fn target_bitrate(&self) -> u32 {
        self.target_bitrate.load(Ordering::Relaxed).min(u32::MAX as u64) as u32
    }

    pub async fn close(&self) {
        if let Err(err) = self.pc.close().await {
            tracing::debug!("closing outgoing peer: {err}");
        }
    }
}

fn rand_ssrc() -> u32 {
    // Uniqueness per connection is all that matters; avoid a rand dependency.
    use std::hash::{BuildHasher, RandomState};
    (RandomState::new().hash_one(Instant::now()) as u32) | 1
}
