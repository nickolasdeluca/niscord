//! Viewer side: receives one H.264 track from one sharer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{PeerConnection, PeerConnectionEventHandler, RTCPeerConnectionState};

use crate::assembler::FrameAssembler;
use crate::common::{self, BaseHandler, CandidateBuffer};
use crate::{IncomingEvents, SignalData, TransportConfig, is_keyframe};

/// How often to check for frames stuck behind lost packets.
const TICK: Duration = Duration::from_millis(50);
/// Don't ask for keyframes more often than this; one takes a while to arrive.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

/// The remote track and its SSRC, once negotiation produced it.
type RemoteTrack = Arc<Mutex<Option<(Arc<dyn TrackRemote>, u32)>>>;

struct Handler {
    base: BaseHandler,
    events: Arc<dyn IncomingEvents>,
    track: RemoteTrack,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: webrtc::peer_connection::RTCPeerConnectionIceEvent) {
        self.base.ice_candidate(&event.candidate);
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        self.base.connection_state(state);
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let Some(ssrc) = track.ssrcs().await.first().copied() else {
            tracing::warn!("remote track has no SSRC");
            return;
        };
        *self.track.lock().unwrap() = Some((track.clone(), ssrc));
        tokio::spawn(receive_loop(track, ssrc, self.events.clone()));
    }
}

async fn receive_loop(track: Arc<dyn TrackRemote>, ssrc: u32, events: Arc<dyn IncomingEvents>) {
    let mut assembler = FrameAssembler::default();
    let mut last_request: Option<Instant> = None;
    // Nothing decodes before the first keyframe; ask right away.
    request_keyframe(&track, ssrc, &mut last_request).await;

    loop {
        let event = match tokio::time::timeout(TICK, track.poll()).await {
            Ok(Some(event)) => Some(event),
            Ok(None) => break,
            Err(_) => None,
        };
        let now = Instant::now();
        match event {
            Some(TrackRemoteEvent::OnRtpPacket(packet)) => assembler.push(now, packet),
            Some(TrackRemoteEvent::OnEnded) => break,
            Some(_) => continue,
            None => {}
        }
        for frame in assembler.pop(now) {
            if frame.after_loss {
                tracing::debug!("lost packets, asking for keyframe");
                request_keyframe(&track, ssrc, &mut last_request).await;
            }
            let keyframe = is_keyframe(&frame.data);
            events.frame(frame.data, keyframe);
        }
        // A gap with nothing decodable after it (e.g. the last packets of
        // the newest frame were lost and the screen is static).
        if assembler.stalled(now) {
            request_keyframe(&track, ssrc, &mut last_request).await;
        }
    }
}

async fn request_keyframe(track: &Arc<dyn TrackRemote>, ssrc: u32, last: &mut Option<Instant>) {
    if last.is_some_and(|t| t.elapsed() < KEYFRAME_REQUEST_INTERVAL) {
        return;
    }
    *last = Some(Instant::now());
    let pli = PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc };
    if let Err(err) = track.write_rtcp(vec![Box::new(pli)]).await {
        tracing::debug!("could not request keyframe: {err}");
    }
}

pub struct IncomingPeer {
    pc: Arc<dyn PeerConnection>,
    events: Arc<dyn IncomingEvents>,
    track: RemoteTrack,
    last_request: tokio::sync::Mutex<Option<Instant>>,
    candidates: CandidateBuffer,
}

impl IncomingPeer {
    /// Create the connection; it waits for the sharer's offer.
    pub async fn start(config: &TransportConfig, events: Arc<dyn IncomingEvents>) -> Result<Self> {
        let mut engine = common::media_engine()?;
        // Defaults include the NACK generator and TWCC feedback the sharer's
        // bandwidth estimator relies on.
        let registry = register_default_interceptors(Registry::new(), &mut engine)?;
        let track = RemoteTrack::default();
        let handler = Arc::new(Handler {
            base: BaseHandler { events: events.clone() },
            events: events.clone(),
            track: track.clone(),
        });
        let pc = common::build_peer_connection(config, engine, registry, handler).await?;
        Ok(Self {
            pc,
            events,
            track,
            last_request: tokio::sync::Mutex::new(None),
            candidates: CandidateBuffer::default(),
        })
    }

    /// Apply the sharer's offer (answering it) or one of its ICE candidates.
    pub async fn handle_signal(&self, data: SignalData) -> Result<()> {
        match data {
            SignalData::Offer { sdp } => {
                self.pc.set_remote_description(RTCSessionDescription::offer(sdp)?).await?;
                let answer = self.pc.create_answer(None).await?;
                self.pc.set_local_description(answer.clone()).await?;
                self.events.signal(common::answer_signal(&answer));
                common::add_candidates(&self.pc, self.candidates.release()).await;
            }
            SignalData::Candidate { candidate } => {
                if let Some(c) = self.candidates.offer(common::parse_candidate(&candidate)?) {
                    common::add_candidates(&self.pc, vec![c]).await;
                }
            }
            SignalData::Answer { .. } => bail!("unexpected answer from a sharer"),
        }
        Ok(())
    }

    /// Ask the sharer for a keyframe (e.g. the decoder hit an error).
    pub async fn request_keyframe(&self) {
        let track = self.track.lock().unwrap().clone();
        if let Some((track, ssrc)) = track {
            request_keyframe(&track, ssrc, &mut *self.last_request.lock().await).await;
        }
    }

    pub async fn close(&self) {
        if let Err(err) = self.pc.close().await {
            tracing::debug!("closing incoming peer: {err}");
        }
    }
}
