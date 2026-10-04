//! Interceptor that lets keyframe requests (PLI/FIR) reach the application.
//!
//! Inbound RTCP normally stops at the end of the interceptor chain (the NACK
//! responder, bandwidth estimator, etc. act on it there). Keyframe requests
//! can only be answered by the encoder, so this marks them for delivery.
//! Adapted from webrtc-rs's `rtcp-processing` example.

use std::collections::VecDeque;
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

/// Must sit last in the chain so every other interceptor has seen the RTCP.
pub const SLOT: usize = 14_000;

#[derive(Default)]
pub struct KeyframeRequestForwarder {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

fn is_keyframe_request(packet: &dyn rtc::rtcp::Packet) -> bool {
    let any = packet.as_any();
    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequestForwarder {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let requests: Vec<_> = packets.iter().filter(|p| is_keyframe_request(p.as_ref())).cloned().collect();
            if requests.is_empty() {
                // Already handled by the interceptors; not the app's business.
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for KeyframeRequestForwarder {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
