//! Sharer side: sends an H.264 track (and optionally Opus) to one viewer.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::peer_connection::{PeerConnection, PeerConnectionEventHandler, RTCPeerConnectionState};
use webrtc::rtp_transceiver::RtpSender;

use crate::common::{self, BaseHandler, CandidateBuffer};
use crate::pacer::AudioFirstPacer;
use crate::{OutgoingEvents, SignalData, TransportConfig, forwarder, lossy};

/// Bounds for the bandwidth estimate, in bits per second. `initial` and `min`
/// apply as far as `max` allows.
#[derive(Debug, Clone, Copy)]
pub struct BitrateLimits {
    pub initial: u32,
    pub min: u32,
    pub max: u32,
}

/// Publishes the estimator's target so the app can read it; the estimator
/// itself is owned by the interceptor chain. (Pattern from webrtc-rs's
/// `bandwidth-estimation-from-disk` example.)
struct ReportingEstimator {
    inner: Gcc,
    limits: BitrateLimits,
    /// What `limits.max` should become.
    requested_max: Arc<AtomicU32>,
    target: Arc<AtomicU64>,
}

impl ReportingEstimator {
    fn new(limits: BitrateLimits, requested_max: Arc<AtomicU32>, target: Arc<AtomicU64>) -> Self {
        let inner = gcc(limits, limits.initial as f64);
        Self { inner, limits, requested_max, target }
    }

    fn publish(&self) {
        self.target.store(self.inner.target_bitrate() as u64, Ordering::Relaxed);
    }

    /// `Gcc` can't change its bounds, so a new limit means a new estimator. It
    /// rarely happens (the frame size changed), so the lost history is cheap.
    fn apply_requested_max(&mut self) {
        let max = self.requested_max.load(Ordering::Relaxed);
        if max == self.limits.max {
            return;
        }
        let current = self.inner.target_bitrate();
        // An estimate pinned at the old limit says nothing about the link, so
        // start where a new viewer would. One below it was measured: keep it.
        let start = match current >= self.limits.max as f64 {
            true => current.max(self.limits.initial as f64),
            false => current,
        };
        self.limits.max = max;
        self.inner = gcc(self.limits, start);
    }
}

fn gcc(limits: BitrateLimits, start: f64) -> Gcc {
    let max = limits.max as f64;
    Gcc::new(start.min(max), (limits.min as f64).min(max), max)
}

impl BandwidthEstimator for ReportingEstimator {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.apply_requested_max();
        self.inner.on_reports(now, reports);
        self.publish();
    }

    fn target_bitrate(&self) -> f64 {
        self.inner.target_bitrate()
    }

    fn handle_timeout(&mut self, now: Instant) {
        self.apply_requested_max();
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

/// One sending track and what it needs to write samples.
struct SendTrack {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    /// Known once the viewer answered.
    payload_type: Mutex<Option<PayloadType>>,
}

impl SendTrack {
    async fn add(pc: &Arc<dyn PeerConnection>, kind: RtpCodecKind, id: &str, codec: RTCRtpCodec) -> Result<Self> {
        let ssrc = rand_ssrc();
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "niscord".into(),
                id.into(),
                id.into(),
                kind,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                    codec,
                    ..Default::default()
                }],
            ),
        )?);
        let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        Ok(Self { track, sender, ssrc, payload_type: Mutex::new(None) })
    }

    async fn negotiated(&self) -> Result<()> {
        let parameters = self.sender.get_parameters().await?;
        let pt = parameters.rtp_parameters.codecs.first().map(|c| c.payload_type);
        *self.payload_type.lock().unwrap() = Some(pt.context("viewer accepted no codec")?);
        Ok(())
    }

    async fn send(&self, data: Bytes, duration: Duration) -> Result<()> {
        let Some(pt) = *self.payload_type.lock().unwrap() else { return Ok(()) };
        let sample = Sample { data, duration, ..Sample::new(Instant::now()) };
        self.track.sample_writer(self.ssrc, pt).write_sample(&sample).await?;
        Ok(())
    }
}

pub struct OutgoingPeer {
    pc: Arc<dyn PeerConnection>,
    video: SendTrack,
    audio: Option<SendTrack>,
    connected: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU64>,
    max_bitrate: Arc<AtomicU32>,
    candidates: CandidateBuffer,
}

impl OutgoingPeer {
    /// Create the connection and send the offer through `events`. With
    /// `audio`, an Opus track goes along with the video.
    pub async fn start(
        config: &TransportConfig,
        limits: BitrateLimits,
        audio: bool,
        events: Arc<dyn OutgoingEvents>,
    ) -> Result<Self> {
        let mut engine = common::media_engine()?;
        let initial = limits.initial.min(limits.max);
        let target_bitrate = Arc::new(AtomicU64::new(initial as u64));
        let max_bitrate = Arc::new(AtomicU32::new(limits.max));
        let estimator = ReportingEstimator::new(limits, max_bitrate.clone(), target_bitrate.clone());
        let registry = configure_congestion_control(Registry::new(), estimator, CongestionFeedback::Twcc, &mut engine)?
            .with(Slot::Pacer, AudioFirstPacer::new(initial as f64));
        let registry = register_default_interceptors(registry, &mut engine)?;
        let mut registry = registry.with(Slot::from(forwarder::SLOT), forwarder::KeyframeRequestForwarder::default());
        if let Some(loss) = &config.simulated_loss {
            registry = registry.with(Slot::from(lossy::SLOT), lossy::LossyLink::new(loss.clone()));
        }

        let connected = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(Handler {
            base: BaseHandler { events: events.clone() },
            connected: connected.clone(),
            events: events.clone(),
        });
        let pc = common::build_peer_connection(config, engine, registry, handler).await?;

        let video = SendTrack::add(&pc, RtpCodecKind::Video, "screen", common::video_codec().rtp_codec).await?;
        let audio = match audio {
            true => Some(SendTrack::add(&pc, RtpCodecKind::Audio, "audio", common::audio_codec().rtp_codec).await?),
            false => None,
        };

        // Keyframe requests (PLI/FIR) from the viewer.
        {
            let track = video.track.clone();
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

        Ok(Self { pc, video, audio, connected, target_bitrate, max_bitrate, candidates: CandidateBuffer::default() })
    }

    /// Apply the viewer's answer or one of its ICE candidates.
    pub async fn handle_signal(&self, data: SignalData) -> Result<()> {
        match data {
            SignalData::Answer { sdp } => {
                self.pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
                self.video.negotiated().await?;
                if let Some(audio) = &self.audio {
                    audio.negotiated().await?;
                }
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
        self.video.send(data, duration).await
    }

    /// Send one Opus packet. Ignored without an audio track or connection.
    pub async fn send_audio(&self, packet: Bytes, duration: Duration) -> Result<()> {
        match &self.audio {
            Some(audio) if self.is_connected() => audio.send(packet, duration).await,
            _ => Ok(()),
        }
    }

    /// Current bandwidth estimate towards this viewer, in bits per second.
    pub fn target_bitrate(&self) -> u32 {
        self.target_bitrate.load(Ordering::Relaxed).min(u32::MAX as u64) as u32
    }

    /// Change the ceiling of the bandwidth estimate; it applies with the next
    /// feedback from the viewer.
    pub fn set_max_bitrate(&self, bps: u32) {
        self.max_bitrate.store(bps, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn estimator(initial: u32, max: u32) -> (ReportingEstimator, Arc<AtomicU32>, Arc<AtomicU64>) {
        let limits = BitrateLimits { initial, min: 300_000, max };
        let requested = Arc::new(AtomicU32::new(max));
        let target = Arc::new(AtomicU64::new(0));
        (ReportingEstimator::new(limits, requested.clone(), target.clone()), requested, target)
    }

    #[test]
    fn raising_the_limit_frees_an_estimate_pinned_at_the_old_one() {
        let (mut estimator, requested, target) = estimator(2_500_000, 1_000_000);
        assert_eq!(estimator.target_bitrate(), 1_000_000.0);

        requested.store(4_000_000, Ordering::Relaxed);
        estimator.handle_timeout(Instant::now());
        // Starts where a new viewer would, not creeping up from the old limit.
        assert_eq!(target.load(Ordering::Relaxed), 2_500_000);
    }

    #[test]
    fn raising_the_limit_keeps_an_estimate_measured_below_it() {
        let (mut estimator, requested, target) = estimator(500_000, 1_000_000);

        requested.store(4_000_000, Ordering::Relaxed);
        estimator.handle_timeout(Instant::now());
        assert_eq!(target.load(Ordering::Relaxed), 500_000);
    }

    #[test]
    fn lowering_the_limit_lowers_the_estimate() {
        let (mut estimator, requested, target) = estimator(2_500_000, 4_000_000);

        requested.store(1_000_000, Ordering::Relaxed);
        estimator.handle_timeout(Instant::now());
        assert_eq!(target.load(Ordering::Relaxed), 1_000_000);
    }
}
