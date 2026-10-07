//! WebRTC transport for Niscord: one peer connection per (sharer, viewer)
//! pair, carrying an H.264 video track and optionally an Opus audio track
//! from sharer to viewer.
//!
//! * [`OutgoingPeer`] runs on the sharer for each viewer. It makes the offer,
//!   sends encoded frames, reports keyframe requests from the viewer, and
//!   estimates the available bandwidth (Google Congestion Control over TWCC).
//! * [`IncomingPeer`] runs on the viewer. It answers, reassembles frames from
//!   RTP, and asks for a keyframe when packets were lost.
//!
//! Signaling (offers, answers, ICE candidates) goes through the caller as
//! [`SignalData`]; Niscord relays it over the signaling server.

mod assembler;
mod common;
mod forwarder;
mod incoming;
mod outgoing;
mod pacer;

use bytes::Bytes;
pub use niscord_protocol::{IceServer, SignalData};

pub use crate::incoming::IncomingPeer;
pub use crate::outgoing::{BitrateLimits, OutgoingPeer};

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub ice_servers: Vec<IceServer>,
    /// Local address to bind UDP sockets to. `0.0.0.0:0` uses every
    /// interface; `127.0.0.1:0` keeps traffic on this machine (tests).
    pub udp_addr: String,
}

impl TransportConfig {
    pub fn new(ice_servers: Vec<IceServer>) -> Self {
        Self { ice_servers, udp_addr: "0.0.0.0:0".into() }
    }

    /// Loopback only, no STUN/TURN: for tests on one machine.
    pub fn loopback() -> Self {
        Self { ice_servers: Vec::new(), udp_addr: "127.0.0.1:0".into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    Connected,
    /// ICE or DTLS failed; the connection will not recover by itself.
    Failed,
    Closed,
}

/// Callbacks shared by both directions. They run on the async runtime and
/// must not block.
pub trait PeerEvents: Send + Sync + 'static {
    /// Send this to the remote peer through signaling.
    fn signal(&self, data: SignalData);
    fn state(&self, state: PeerState);
}

pub trait OutgoingEvents: PeerEvents {
    /// The viewer lost data or just joined and needs a keyframe.
    fn keyframe_requested(&self);
}

pub trait IncomingEvents: PeerEvents {
    /// One complete H.264 access unit (Annex B).
    fn frame(&self, data: Bytes, keyframe: bool);

    /// One Opus packet (20 ms). `lost` packets went missing just before it;
    /// the decoder fills them in.
    fn audio(&self, packet: Bytes, lost: usize) {
        let _ = (packet, lost);
    }
}

/// Whether an Annex B access unit contains an IDR slice or SPS, i.e. can be
/// decoded without earlier frames.
pub fn is_keyframe(annex_b: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < annex_b.len() {
        let start = if annex_b[i..].starts_with(&[0, 0, 1]) {
            3
        } else if annex_b[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            i += 1;
            continue;
        };
        let Some(&header) = annex_b.get(i + start) else { break };
        if matches!(header & 0x1f, 5 | 7) {
            return true;
        }
        i += start;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_detection() {
        // SPS (7), PPS (8), IDR (5).
        assert!(is_keyframe(&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4]));
        // Non-IDR slice (1) only.
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 9, 9, 9]));
        assert!(!is_keyframe(&[]));
    }
}
