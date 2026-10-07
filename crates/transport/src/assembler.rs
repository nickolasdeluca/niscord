//! Reassembles H.264 access units from RTP packets with no added delay.
//!
//! webrtc-rs's `SampleBuilder` holds every frame until the next frame's first
//! packet arrives (to learn its duration). That adds a frame of latency and,
//! worse, leaves the last picture of a static screen undisplayed. This emits
//! a frame as soon as its marker packet arrives with no sequence gaps, waits a
//! little for retransmissions (NACK) when packets are missing, and otherwise
//! skips to the next frame, flagging the loss so a keyframe can be requested.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rtc::rtp::Packet;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;

/// How long to wait for a missing packet to be retransmitted.
const MAX_GAP_WAIT: Duration = Duration::from_millis(250);
/// Hard cap on buffered packets (a 4K keyframe is well under this).
const MAX_BUFFERED: usize = 4096;

#[derive(Debug)]
pub struct Frame {
    pub data: Bytes,
    /// Packets were lost before this frame; the decoder needs a keyframe.
    pub after_loss: bool,
}

#[derive(Default)]
pub struct FrameAssembler {
    /// Extended sequence number -> (arrival, packet).
    packets: BTreeMap<u64, (Instant, Packet)>,
    /// Highest extended sequence number seen, to unwrap 16-bit numbers.
    highest: Option<u64>,
    /// First packet of the next frame, once known.
    next: Option<u64>,
    loss_pending: bool,
}

impl FrameAssembler {
    pub fn push(&mut self, now: Instant, packet: Packet) {
        let seq = self.extend(packet.header.sequence_number);
        // Late duplicates of packets already emitted or skipped.
        if self.next.is_some_and(|next| seq < next) {
            return;
        }
        self.packets.insert(seq, (now, packet));
        if self.packets.len() > MAX_BUFFERED
            && let Some((&first, _)) = self.packets.first_key_value()
        {
            self.packets.remove(&first);
            self.loss_pending = true;
        }
    }

    /// Frames that are complete now, in order. Call after every push, and
    /// periodically so stalled gaps time out.
    pub fn pop(&mut self, now: Instant) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(start) = self.frame_start() {
            match self.complete_frame(start) {
                Ok(end) => {
                    let data = self.depacketize(start, end);
                    self.next = Some(end + 1);
                    let after_loss = std::mem::take(&mut self.loss_pending) || data.is_none();
                    if let Some(data) = data {
                        frames.push(Frame { data, after_loss });
                    } else {
                        self.loss_pending = true;
                    }
                }
                Err(gap) => {
                    if self.waited_out(gap, now) && self.skip_past(gap) {
                        continue;
                    }
                    break;
                }
            }
        }
        frames
    }

    /// Packets have been missing for long enough that nothing more will come
    /// out without a keyframe.
    pub fn stalled(&self, now: Instant) -> bool {
        match self.next {
            Some(next) => self.complete_frame(next).is_err_and(|gap| self.waited_out(gap, now)),
            // No frame start yet (joined mid-frame, or the first one was lost).
            None => self.packets.values().any(|(t, _)| now.duration_since(*t) >= MAX_GAP_WAIT),
        }
    }

    /// Whether the packet at `gap` has been missing for too long: counted
    /// from when something after it arrived or, when it's the end of the
    /// frame that's missing, from the last arrival. Not from the frame's
    /// first packet: a big keyframe takes a while to arrive at the paced
    /// rate, and asking for another one because of that just made the next
    /// one late too (a keyframe a second on a CS2 stream).
    fn waited_out(&self, gap: u64, now: Instant) -> bool {
        let first_after = self.packets.range(gap..).next().map(|(_, (t, _))| *t);
        let since = first_after.or_else(|| self.packets.values().map(|(t, _)| *t).max());
        since.is_some_and(|t| now.duration_since(t) >= MAX_GAP_WAIT)
    }

    fn extend(&mut self, seq: u16) -> u64 {
        let ext = match self.highest {
            None => (1 << 32) + seq as u64,
            Some(highest) => {
                let delta = seq.wrapping_sub(highest as u16) as i16 as i64;
                (highest as i64 + delta) as u64
            }
        };
        if self.highest.is_none_or(|h| ext > h) {
            self.highest = Some(ext);
        }
        ext
    }

    /// Sequence number where the next frame begins. Before the first frame,
    /// that is the first buffered packet that starts a frame.
    fn frame_start(&mut self) -> Option<u64> {
        if let Some(next) = self.next {
            return self.packets.contains_key(&next).then_some(next).or_else(|| {
                // Missing; `complete_frame` reports the gap.
                self.packets.range(next..).next().map(|_| next)
            });
        }
        let probe = H264Packet::default();
        let first = self.packets.iter().find(|(_, (_, p))| probe.is_partition_head(&p.payload)).map(|(k, _)| *k)?;
        self.packets.retain(|k, _| *k >= first);
        self.next = Some(first);
        Some(first)
    }

    /// The last sequence number of the frame starting at `start`, or the
    /// first missing sequence number.
    fn complete_frame(&self, start: u64) -> Result<u64, u64> {
        let Some((_, first)) = self.packets.get(&start) else { return Err(start) };
        let timestamp = first.header.timestamp;
        let mut seq = start;
        loop {
            match self.packets.get(&seq) {
                None => return Err(seq),
                // No marker seen but the next frame began: the end was lost.
                Some((_, p)) if p.header.timestamp != timestamp => return Err(seq),
                Some((_, p)) if p.header.marker => return Ok(seq),
                Some(_) => seq += 1,
            }
        }
    }

    /// Drop everything up to the first frame start after `gap`. Returns false
    /// when there is no such frame yet.
    fn skip_past(&mut self, gap: u64) -> bool {
        let probe = H264Packet::default();
        let current_ts = self.next.and_then(|n| self.packets.get(&n)).map(|(_, p)| p.header.timestamp);
        let resume = self
            .packets
            .range(gap..)
            .find(|(_, (_, p))| Some(p.header.timestamp) != current_ts && probe.is_partition_head(&p.payload))
            .map(|(k, _)| *k);
        let Some(resume) = resume else { return false };
        self.packets.retain(|k, _| *k >= resume);
        self.next = Some(resume);
        self.loss_pending = true;
        true
    }

    fn depacketize(&mut self, start: u64, end: u64) -> Option<Bytes> {
        let mut depacketizer = H264Packet::default();
        let mut out = BytesMut::new();
        let mut ok = true;
        for seq in start..=end {
            let (_, packet) = self.packets.remove(&seq)?;
            match depacketizer.depacketize(&packet.payload) {
                Ok(nal) => out.extend_from_slice(&nal),
                Err(_) => ok = false,
            }
        }
        (ok && !out.is_empty()).then(|| out.freeze())
    }
}

#[cfg(test)]
mod tests {
    use rtc::rtp::codec::h264::H264Payloader;
    use rtc::rtp::header::Header;
    use rtc::rtp::packetizer::Payloader;

    use super::*;

    /// Packetize a fake access unit (SPS + big IDR slice) into RTP packets.
    fn packets(seq: &mut u16, timestamp: u32, size: usize) -> Vec<Packet> {
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x33, 0, 0, 0, 1, 0x65];
        au.extend((0..size).map(|i| (i % 251) as u8 | 1));
        let payloads = H264Payloader::default().payload(1200, &Bytes::from(au)).unwrap();
        let n = payloads.len();
        payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| {
                let header = Header {
                    version: 2,
                    marker: i + 1 == n,
                    payload_type: 102,
                    sequence_number: {
                        let s = *seq;
                        *seq = seq.wrapping_add(1);
                        s
                    },
                    timestamp,
                    ssrc: 1,
                    ..Default::default()
                };
                Packet { header, payload }
            })
            .collect()
    }

    #[test]
    fn emits_frame_on_marker_without_waiting_for_next() {
        let mut asm = FrameAssembler::default();
        let now = Instant::now();
        let mut seq = 10;
        for p in packets(&mut seq, 1000, 5000) {
            asm.push(now, p);
        }
        let frames = asm.pop(now);
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].after_loss);
        assert!(crate::is_keyframe(&frames[0].data));
    }

    #[test]
    fn reordered_packets_are_fine() {
        let mut asm = FrameAssembler::default();
        let now = Instant::now();
        let mut seq = 0;
        let mut ps = packets(&mut seq, 1000, 6000);
        ps.swap(1, 3);
        let mut frames = Vec::new();
        for p in ps {
            asm.push(now, p);
            frames.extend(asm.pop(now));
        }
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].after_loss);
    }

    #[test]
    fn sequence_wraparound() {
        let mut asm = FrameAssembler::default();
        let now = Instant::now();
        let mut seq = u16::MAX - 2;
        let mut frames = Vec::new();
        for ts in [1000, 4000, 7000] {
            for p in packets(&mut seq, ts, 3000) {
                asm.push(now, p);
            }
            frames.extend(asm.pop(now));
        }
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| !f.after_loss));
    }

    #[test]
    fn waits_for_retransmission_then_skips_and_flags_loss() {
        let mut asm = FrameAssembler::default();
        let t0 = Instant::now();
        let mut seq = 0;
        let first = packets(&mut seq, 1000, 4000);
        let second = packets(&mut seq, 4000, 4000);
        let third = packets(&mut seq, 7000, 4000);
        // Frame 1 arrives whole; frame 2 loses a middle packet.
        for p in first {
            asm.push(t0, p);
        }
        assert_eq!(asm.pop(t0).len(), 1);
        let lost = second[1].clone();
        for (i, p) in second.into_iter().enumerate() {
            if i != 1 {
                asm.push(t0, p);
            }
        }
        for p in third {
            asm.push(t0, p);
        }
        // Within the retransmission window nothing is emitted.
        assert!(asm.pop(t0 + Duration::from_millis(50)).is_empty());

        // A retransmission in time completes frame 2 normally.
        asm.push(t0 + Duration::from_millis(60), lost);
        let frames = asm.pop(t0 + Duration::from_millis(60));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| !f.after_loss));
    }

    #[test]
    fn gives_up_after_timeout() {
        let mut asm = FrameAssembler::default();
        let t0 = Instant::now();
        let mut seq = 0;
        let first = packets(&mut seq, 1000, 4000);
        let second = packets(&mut seq, 4000, 4000);
        let third = packets(&mut seq, 7000, 4000);
        for p in first {
            asm.push(t0, p);
        }
        assert_eq!(asm.pop(t0).len(), 1);
        for (i, p) in second.into_iter().enumerate() {
            if i != 1 {
                asm.push(t0, p);
            }
        }
        for p in third {
            asm.push(t0, p);
        }
        assert!(!asm.stalled(t0 + Duration::from_millis(100)));
        let later = t0 + MAX_GAP_WAIT + Duration::from_millis(1);
        assert!(asm.stalled(later));
        let frames = asm.pop(later);
        assert_eq!(frames.len(), 1, "frame 2 is skipped, frame 3 is emitted");
        assert!(frames[0].after_loss);
    }

    #[test]
    fn a_slowly_arriving_frame_is_not_a_stall() {
        let mut asm = FrameAssembler::default();
        let t0 = Instant::now();
        let mut seq = 0;
        // A keyframe trickling in over a second, one packet every 25 ms.
        let keyframe = packets(&mut seq, 1000, 48_000);
        let n = keyframe.len();
        let mut frames = Vec::new();
        for (i, p) in keyframe.into_iter().enumerate() {
            let now = t0 + Duration::from_millis(25 * i as u64);
            asm.push(now, p);
            frames.extend(asm.pop(now));
            assert!(!asm.stalled(now + Duration::from_millis(24)), "stalled after packet {i} of {n}");
        }
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].after_loss);
    }

    #[test]
    fn a_lost_end_of_frame_stalls_once_nothing_more_arrives() {
        let mut asm = FrameAssembler::default();
        let t0 = Instant::now();
        let mut seq = 0;
        let mut frame = packets(&mut seq, 1000, 4000);
        frame.pop();
        for p in frame {
            asm.push(t0, p);
        }
        assert!(asm.pop(t0).is_empty());
        assert!(!asm.stalled(t0 + Duration::from_millis(100)));
        assert!(asm.stalled(t0 + MAX_GAP_WAIT));
    }
}
