//! Viewer side: receives the H.264 track (and Opus, if any) from one sharer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
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
        if track.kind().await == RtpCodecKind::Audio {
            tokio::spawn(audio_loop(track, self.events.clone()));
            return;
        }
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
    let mut last_keyframe: Option<Instant> = None;
    // Nothing decodes before the first keyframe; ask right away.
    request_keyframe(&track, ssrc, &mut last_request, "joined").await;

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
                request_keyframe(&track, ssrc, &mut last_request, "lost packets").await;
            }
            let keyframe = is_keyframe(&frame.data);
            if keyframe {
                let since_last_ms = last_keyframe.map(|t| now.duration_since(t).as_millis());
                tracing::debug!(bytes = frame.data.len(), ?since_last_ms, "keyframe received");
                last_keyframe = Some(now);
            }
            events.frame(frame.data, keyframe);
        }
        // A gap with nothing decodable after it (e.g. the last packets of
        // the newest frame were lost and the screen is static).
        if assembler.stalled(now) {
            request_keyframe(&track, ssrc, &mut last_request, "frame stuck").await;
        }
    }
}

/// Hands Opus packets on in order, counting the ones lost before each.
async fn audio_loop(track: Arc<dyn TrackRemote>, events: Arc<dyn IncomingEvents>) {
    let mut sequence = AudioSequence::default();
    while let Some(event) = track.poll().await {
        match event {
            TrackRemoteEvent::OnRtpPacket(packet) => {
                if let Some(lost) = sequence.accept(packet.header.sequence_number) {
                    events.audio(packet.payload, lost);
                }
            }
            TrackRemoteEvent::OnEnded => break,
            _ => {}
        }
    }
}

/// RTP sequence tracking for audio, where late packets are useless: the
/// decoder already concealed them.
#[derive(Default)]
struct AudioSequence {
    next: Option<u16>,
}

/// A jump bigger than this is a restart, not loss.
const MAX_AUDIO_GAP: u16 = 50;

impl AudioSequence {
    /// How many packets were lost before this one, or `None` to drop it
    /// (duplicate or arrived too late).
    fn accept(&mut self, seq: u16) -> Option<usize> {
        let lost = match self.next {
            None => 0,
            Some(next) => {
                let ahead = seq.wrapping_sub(next);
                if ahead >= u16::MAX / 2 {
                    return None; // behind: late or duplicate
                }
                if ahead > MAX_AUDIO_GAP { 0 } else { ahead as usize }
            }
        };
        self.next = Some(seq.wrapping_add(1));
        Some(lost)
    }
}

async fn request_keyframe(track: &Arc<dyn TrackRemote>, ssrc: u32, last: &mut Option<Instant>, reason: &str) {
    if last.is_some_and(|t| t.elapsed() < KEYFRAME_REQUEST_INTERVAL) {
        return;
    }
    *last = Some(Instant::now());
    tracing::debug!(reason, "asking for keyframe");
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
            request_keyframe(&track, ssrc, &mut *self.last_request.lock().await, "can't decode").await;
        }
    }

    pub async fn close(&self) {
        if let Err(err) = self.pc.close().await {
            tracing::debug!("closing incoming peer: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AudioSequence;

    #[test]
    fn audio_sequence_counts_loss_and_drops_late_packets() {
        let mut seq = AudioSequence::default();
        assert_eq!(seq.accept(65_534), Some(0));
        assert_eq!(seq.accept(65_535), Some(0));
        assert_eq!(seq.accept(1), Some(1), "0 lost, across the wrap");
        assert_eq!(seq.accept(0), None, "late");
        assert_eq!(seq.accept(1), None, "duplicate");
        assert_eq!(seq.accept(4), Some(2));
        assert_eq!(seq.accept(1_000), Some(0), "restart, not loss");
        assert_eq!(seq.accept(1_001), Some(0));
    }
}
