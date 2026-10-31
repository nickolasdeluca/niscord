//! Test aid: loses some outgoing video packets on the wire, like a congested
//! link would, so recovery (NACK, keyframe requests) can be tested on
//! loopback, where nothing is ever lost.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::Instant;

use rtc::interceptor::{Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

use crate::SimulatedLoss;
use crate::common::H264_PAYLOAD_TYPE;

/// Wire-most: below the congestion controller (1_000), so the packet counts
/// as sent and its loss shows up in the feedback like real loss.
pub const SLOT: usize = 500;

pub struct LossyLink {
    loss: SimulatedLoss,
    count: u32,
    lost: HashSet<u16>,
    write_queue: VecDeque<TaggedPacket>,
    read_queue: VecDeque<TaggedPacket>,
}

impl LossyLink {
    pub fn new(loss: SimulatedLoss) -> Self {
        Self { loss, count: 0, lost: HashSet::new(), write_queue: VecDeque::new(), read_queue: VecDeque::new() }
    }

    /// Whether this video packet is lost on the way out.
    fn lose(&mut self, seq: u16) -> bool {
        if self.lost.contains(&seq) {
            self.loss.retransmissions.fetch_add(1, Ordering::Relaxed);
            return self.loss.permanent;
        }
        self.count += 1;
        if self.count.is_multiple_of(self.loss.every.max(1)) {
            self.lost.insert(seq);
            self.loss.lost.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        false
    }
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for LossyLink {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtp(packet) = &msg.message.packet
            && packet.header.payload_type == H264_PAYLOAD_TYPE
            && self.lose(packet.header.sequence_number)
        {
            return Ok(());
        }
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for LossyLink {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
