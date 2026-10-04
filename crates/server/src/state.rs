//! Presence and watch bookkeeping. Everything here is synchronous and runs
//! under a single mutex; sending only queues onto per-client channels.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use niscord_protocol::{
    ClientMsg, ErrorCode, MAX_NAME_CHARS, MAX_TITLE_CHARS, PROTOCOL_VERSION, PeerId, PeerInfo, Role, ServerMsg,
    ShareInfo,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::{Config, unix_millis};

pub enum Outgoing {
    Msg(ServerMsg),
    /// Close the connection with this reason after flushing earlier messages.
    Close(String),
}

struct Client {
    tx: UnboundedSender<Outgoing>,
    /// Set once the client has completed a valid `hello`.
    name: Option<String>,
    share: Option<ShareInfo>,
    /// Sharers this client is watching.
    watching: HashSet<PeerId>,
}

#[derive(Default)]
pub struct State {
    clients: HashMap<PeerId, Client>,
}

impl State {
    pub fn add_client(&mut self, tx: UnboundedSender<Outgoing>) -> PeerId {
        let id = Uuid::new_v4();
        self.clients.insert(id, Client { tx, name: None, share: None, watching: HashSet::new() });
        id
    }

    pub fn remove_client(&mut self, id: PeerId) {
        let Some(client) = self.clients.remove(&id) else { return };
        let Some(name) = client.name else { return };
        if client.share.is_some() {
            self.notify_share_ended(id);
        }
        for sharer in client.watching {
            self.send(sharer, ServerMsg::ViewerLeft { from: id });
        }
        tracing::info!(%id, name, "left");
        self.broadcast_peers();
    }

    pub fn handle(&mut self, id: PeerId, msg: ClientMsg, addr: SocketAddr, config: &Config) {
        let joined = self.clients.get(&id).is_some_and(|c| c.name.is_some());
        match msg {
            ClientMsg::Hello { name, password, protocol } if !joined => {
                self.hello(id, name, password, protocol, addr, config)
            }
            _ if !joined => {}
            ClientMsg::Hello { .. } => {}
            ClientMsg::ShareStart { kind, title, audio } => {
                let client = self.clients.get_mut(&id).unwrap();
                let started_at = client.share.as_ref().map_or_else(unix_millis, |s| s.started_at);
                let title: String = clean_text(&title).chars().take(MAX_TITLE_CHARS).collect();
                tracing::info!(%id, ?kind, title, audio, "share started");
                client.share = Some(ShareInfo { kind, title, audio, started_at });
                self.broadcast_peers();
            }
            ClientMsg::ShareStop => {
                let client = self.clients.get_mut(&id).unwrap();
                if client.share.take().is_some() {
                    tracing::info!(%id, "share stopped");
                    self.notify_share_ended(id);
                    self.broadcast_peers();
                }
            }
            ClientMsg::Watch { target } => {
                let sharing = target != id && self.clients.get(&target).is_some_and(|c| c.share.is_some());
                if !sharing {
                    self.send(id, ServerMsg::ShareEnded { from: target });
                    return;
                }
                self.clients.get_mut(&id).unwrap().watching.insert(target);
                // Sent even when already watching, so a viewer can ask for a
                // fresh connection after a failure.
                self.send(target, ServerMsg::WatchRequest { from: id });
                self.broadcast_peers();
            }
            ClientMsg::Unwatch { target } => {
                if self.clients.get_mut(&id).unwrap().watching.remove(&target) {
                    self.send(target, ServerMsg::ViewerLeft { from: id });
                    self.broadcast_peers();
                }
            }
            ClientMsg::Signal { to, role, data } => {
                // Only relay between peers that have a watch relationship.
                let allowed = match role {
                    Role::Sharer => self.clients.get(&to).is_some_and(|c| c.watching.contains(&id)),
                    Role::Viewer => self.clients[&id].watching.contains(&to),
                };
                if allowed {
                    self.send(to, ServerMsg::Signal { from: id, role, data });
                }
            }
        }
    }

    fn hello(&mut self, id: PeerId, name: String, password: String, protocol: u32, addr: SocketAddr, config: &Config) {
        let reject = |state: &Self, code: ErrorCode, message: &str| {
            tracing::info!(%addr, ?code, "rejected");
            state.send(id, ServerMsg::Error { code, message: message.into() });
            if let Some(c) = state.clients.get(&id) {
                let _ = c.tx.send(Outgoing::Close(message.into()));
            }
        };
        if protocol != PROTOCOL_VERSION {
            let message = format!(
                "This app speaks protocol v{protocol} but the server speaks v{PROTOCOL_VERSION}. Please update Niscord."
            );
            return reject(self, ErrorCode::ProtocolMismatch, &message);
        }
        if !config.password.is_empty() && !password_matches(&password, &config.password) {
            return reject(self, ErrorCode::BadPassword, "Wrong server password.");
        }
        let name: String = clean_text(&name).chars().take(MAX_NAME_CHARS).collect();
        if name.is_empty() {
            return reject(self, ErrorCode::BadName, "Please choose a display name.");
        }

        tracing::info!(%id, %addr, name, "joined");
        self.clients.get_mut(&id).unwrap().name = Some(name);
        self.send(id, ServerMsg::Welcome { id, protocol: PROTOCOL_VERSION, ice_servers: config.ice_servers_for(id) });
        self.broadcast_peers();
    }

    /// Tell everyone watching `sharer` that the share is over and forget them.
    fn notify_share_ended(&mut self, sharer: PeerId) {
        let mut viewers = Vec::new();
        for (&vid, client) in &mut self.clients {
            if client.watching.remove(&sharer) {
                viewers.push(vid);
            }
        }
        for vid in viewers {
            self.send(vid, ServerMsg::ShareEnded { from: sharer });
        }
    }

    fn send(&self, to: PeerId, msg: ServerMsg) {
        if let Some(client) = self.clients.get(&to) {
            let _ = client.tx.send(Outgoing::Msg(msg));
        }
    }

    fn broadcast_peers(&self) {
        let mut peers: Vec<PeerInfo> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let name = c.name.clone()?;
                let viewers =
                    self.clients.iter().filter(|(_, v)| v.watching.contains(&id)).map(|(&vid, _)| vid).collect();
                Some(PeerInfo { id, name, share: c.share.clone(), viewers })
            })
            .collect();
        peers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        for client in self.clients.values().filter(|c| c.name.is_some()) {
            let _ = client.tx.send(Outgoing::Msg(ServerMsg::Peers { peers: peers.clone() }));
        }
    }
}

/// Constant-time comparison (hashing first hides the length too).
fn password_matches(given: &str, expected: &str) -> bool {
    let a = Sha256::digest(given.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    a.as_slice().ct_eq(b.as_slice()).into()
}

/// Drop control characters and collapse runs of whitespace.
fn clean_text(s: &str) -> String {
    s.split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_strips_controls_and_whitespace() {
        assert_eq!(clean_text("  Nick\u{0007}  \n de\tLuca "), "Nick de Luca");
        assert_eq!(clean_text("\u{0000}\u{001b}"), "");
    }

    #[test]
    fn password_comparison() {
        assert!(password_matches("hunter2", "hunter2"));
        assert!(!password_matches("hunter", "hunter2"));
    }
}
