//! Pacer that lets audio go ahead of queued video.
//!
//! webrtc-rs's `PacerInterceptor` keeps one FIFO for every stream, so each
//! Opus packet waited behind whatever video was queued. A keyframe takes a
//! good part of a second to drain at the estimated rate, so audio reached
//! viewers in bunches and their playout ran dry in between (2-3 dropouts a
//! second on a CS2 stream, with the sharer's CPU and estimate both fine).
//! This meters the same budget but always releases audio first; it is a
//! twentieth of the bitrate, so video barely notices.

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Pacer, Packet, StreamInfo, TaggedPacket};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use rtc::shared::marshal::MarshalSize;

/// Packets held before new ones are refused, as upstream.
const QUEUE_LIMIT: usize = 4096;

pub struct AudioFirstPacer {
    pacer: Pacer,
    audio_ssrcs: HashSet<u32>,
    audio: VecDeque<TaggedPacket>,
    video: VecDeque<TaggedPacket>,
    released: VecDeque<TaggedPacket>,
    read_queue: VecDeque<TaggedPacket>,
    /// RTCP, which isn't paced: feedback is only useful while it's fresh.
    write_queue: VecDeque<TaggedPacket>,
}

impl AudioFirstPacer {
    /// Paced at `bits_per_second` until the first estimate arrives.
    pub fn new(bits_per_second: f64) -> Self {
        Self {
            pacer: Pacer::new(bits_per_second),
            audio_ssrcs: HashSet::new(),
            audio: VecDeque::new(),
            video: VecDeque::new(),
            released: VecDeque::new(),
            read_queue: VecDeque::new(),
            write_queue: VecDeque::new(),
        }
    }

    fn bits_of(packet: &TaggedPacket) -> f64 {
        match &packet.message.packet {
            Packet::Rtp(rtp) => (rtp.marshal_size() * 8) as f64,
            _ => 0.0,
        }
    }

    fn head(&self) -> Option<&TaggedPacket> {
        self.audio.front().or_else(|| self.video.front())
    }
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for AudioFirstPacer {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        // The congestion controller sits wire-ward and attaches its estimate
        // to inbound feedback on the way up.
        if let Some(Attribute::TargetBitrateChanged { bits_per_second }) =
            msg.message.get(&Attribute::TargetBitrateChanged { bits_per_second: 0.0 })
        {
            self.pacer.set_target_bitrate(*bits_per_second);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        let Packet::Rtp(rtp) = &msg.message.packet else {
            self.write_queue.push_back(msg);
            return Ok(());
        };
        self.pacer.refill(msg.now);
        if self.audio.len() + self.video.len() >= QUEUE_LIMIT {
            return Ok(());
        }
        match self.audio_ssrcs.contains(&rtp.header.ssrc) {
            true => self.audio.push_back(msg),
            false => self.video.push_back(msg),
        }
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front().or_else(|| self.released.pop_front())
    }

    fn handle_timeout(&mut self, now: Instant) -> Result<(), Error> {
        self.pacer.refill(now);
        while let Some(head) = self.head() {
            let bits = Self::bits_of(head);
            if !self.pacer.can_release(bits) {
                break;
            }
            let queue = if self.audio.is_empty() { &mut self.video } else { &mut self.audio };
            let mut packet = queue.pop_front().expect("head just checked");
            self.pacer.consume(bits);
            // Interceptors below (the send history) must see when it left.
            packet.now = now;
            self.released.push_back(packet);
        }
        Ok(())
    }

    fn poll_timeout(&mut self) -> Option<Instant> {
        self.pacer.releasable_at(Self::bits_of(self.head()?))
    }
}

impl Interceptor for AudioFirstPacer {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        if info.mime_type.to_ascii_lowercase().starts_with("audio/") {
            self.audio_ssrcs.insert(info.ssrc);
        }
    }

    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        self.audio_ssrcs.remove(&info.ssrc);
    }

    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}

    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use rtc::interceptor::AttributedPacket;
    use rtc::rtp::header::Header;
    use rtc::shared::TransportContext;

    use super::*;

    const AUDIO: u32 = 1;
    const VIDEO: u32 = 2;

    fn rtp(now: Instant, ssrc: u32, sequence_number: u16, size: usize) -> TaggedPacket {
        let header = Header { version: 2, ssrc, sequence_number, ..Default::default() };
        let packet = rtc::rtp::Packet { header, payload: Bytes::from(vec![0; size]) };
        TaggedPacket {
            now,
            transport: TransportContext::default(),
            message: AttributedPacket::new(Packet::Rtp(packet)),
        }
    }

    /// Run the pacer until it's empty; the SSRC and arrival of every packet.
    fn drain(pacer: &mut AudioFirstPacer, start: Instant) -> Vec<(u32, Duration)> {
        let mut out = Vec::new();
        while let Some(at) = pacer.poll_timeout() {
            pacer.handle_timeout(at).unwrap();
            while let Some(p) = pacer.poll_write() {
                let Packet::Rtp(rtp) = &p.message.packet else { continue };
                out.push((rtp.header.ssrc, p.now - start));
            }
        }
        out
    }

    #[test]
    fn audio_skips_ahead_of_a_queued_keyframe() {
        let t0 = Instant::now();
        let mut pacer = AudioFirstPacer::new(1_000_000.0);
        pacer.bind_local_stream(&StreamInfo { ssrc: AUDIO, mime_type: "audio/opus".into(), ..Default::default() });
        pacer.bind_local_stream(&StreamInfo { ssrc: VIDEO, mime_type: "video/H264".into(), ..Default::default() });
        // A 48 KB keyframe: about 0.4 s at 1 Mbps.
        for seq in 0..40 {
            pacer.handle_write(rtp(t0, VIDEO, seq, 1200)).unwrap();
        }
        pacer.handle_write(rtp(t0, AUDIO, 0, 300)).unwrap();

        let out = drain(&mut pacer, t0);
        assert_eq!(out.len(), 41);
        let (position, &(_, at)) = out.iter().enumerate().find(|(_, (ssrc, _))| *ssrc == AUDIO).unwrap();
        assert!(at < Duration::from_millis(20), "audio waited {at:?} (packet {position})");
        // The total rate is still respected (after the 0.1 s burst a full bucket allows).
        let last = out.last().unwrap().1;
        assert!(last > Duration::from_millis(250), "everything left by {last:?}");
    }

    #[test]
    fn unknown_streams_queue_as_video_in_order() {
        let t0 = Instant::now();
        let mut pacer = AudioFirstPacer::new(1_000_000.0);
        for seq in 0..20 {
            pacer.handle_write(rtp(t0, VIDEO, seq, 1200)).unwrap();
        }
        pacer.handle_write(rtp(t0, AUDIO, 0, 300)).unwrap();
        let out = drain(&mut pacer, t0);
        assert_eq!(out.last().unwrap().0, AUDIO);
    }
}
