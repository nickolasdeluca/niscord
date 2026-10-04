//! Wire protocol between Niscord clients and the signaling server.
//!
//! Every WebSocket text frame carries one JSON object tagged by `"type"`.
//! The server only coordinates presence and relays WebRTC signaling; media
//! flows directly between peers (or through TURN).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Bumped whenever a change would break older clients or servers.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest display name the server accepts, in characters.
pub const MAX_NAME_CHARS: usize = 32;

/// Longest share title the server keeps, in characters.
pub const MAX_TITLE_CHARS: usize = 120;

pub type PeerId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShareKind {
    Screen,
    Window,
}

/// What a peer is currently sharing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub kind: ShareKind,
    pub title: String,
    pub audio: bool,
    /// Unix time in milliseconds when the share started.
    pub started_at: u64,
}

/// A connected user as seen by everyone else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: PeerId,
    pub name: String,
    pub share: Option<ShareInfo>,
    /// Peers currently watching this peer's share.
    pub viewers: Vec<PeerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Which side of a stream sent a signaling message. A pair of peers can
/// watch each other at the same time, so the receiver uses this to pick the
/// right connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Sent by the peer streaming its screen.
    Sharer,
    /// Sent by the peer watching the stream.
    Viewer,
}

/// WebRTC negotiation payloads, relayed opaquely by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SignalData {
    Offer { sdp: String },
    Answer { sdp: String },
    Candidate { candidate: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientMsg {
    /// First message on every connection.
    Hello {
        name: String,
        #[serde(default)]
        password: String,
        protocol: u32,
    },
    /// Start sharing, or update what is being shared.
    ShareStart {
        kind: ShareKind,
        title: String,
        audio: bool,
    },
    ShareStop,
    Watch {
        target: PeerId,
    },
    Unwatch {
        target: PeerId,
    },
    Signal {
        to: PeerId,
        role: Role,
        data: SignalData,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    BadPassword,
    BadName,
    ProtocolMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMsg {
    Welcome {
        id: PeerId,
        protocol: u32,
        ice_servers: Vec<IceServer>,
    },
    /// Fatal: the server closes the connection right after sending it.
    Error {
        code: ErrorCode,
        message: String,
    },
    /// Full list of connected peers (including the recipient), sent whenever
    /// anything about presence, shares or viewers changes.
    Peers {
        peers: Vec<PeerInfo>,
    },
    /// Sent to a sharer: `from` wants to watch. The sharer should start a
    /// WebRTC offer towards them.
    WatchRequest {
        from: PeerId,
    },
    /// Sent to a sharer: `from` stopped watching.
    ViewerLeft {
        from: PeerId,
    },
    /// Sent to a viewer: the share it was watching (or asked for) is gone.
    ShareEnded {
        from: PeerId,
    },
    Signal {
        from: PeerId,
        role: Role,
        data: SignalData,
    },
}

impl ClientMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("client messages always serialize")
    }
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("server messages always serialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_with_stable_tags() {
        let msg =
            ClientMsg::Signal { to: Uuid::nil(), role: Role::Viewer, data: SignalData::Answer { sdp: "v=0".into() } };
        let json = msg.to_json();
        assert!(json.contains(r#""type":"signal""#));
        assert!(json.contains(r#""role":"viewer""#));
        assert!(json.contains(r#""kind":"answer""#));
        assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), msg);

        let json = ClientMsg::ShareStop.to_json();
        assert_eq!(json, r#"{"type":"share-stop"}"#);
    }
}
