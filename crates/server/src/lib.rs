//! Niscord signaling server.
//!
//! Tracks who is online and who is sharing, and relays WebRTC signaling
//! between clients. Media never flows through this process: it goes
//! peer-to-peer, or through a TURN server when a direct path is impossible.

mod state;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use niscord_protocol::{ClientMsg, IceServer, PeerId};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};

use crate::state::{Outgoing, State};

/// Largest WebSocket message accepted from a client.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// How often the server pings each client.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// A client that sends nothing (not even a pong) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Shared password required to join. Empty means anyone can join.
    pub password: String,
    pub stun_urls: Vec<String>,
    pub turn_urls: Vec<String>,
    /// coturn `static-auth-secret`, used to mint short-lived credentials.
    pub turn_secret: String,
    pub turn_ttl: Duration,
}

impl Config {
    /// ICE servers handed to a client, with TURN credentials in coturn's
    /// "use-auth-secret" (TURN REST API) format so the long-term secret is
    /// never shipped to clients.
    pub fn ice_servers_for(&self, id: PeerId) -> Vec<IceServer> {
        let mut servers = Vec::new();
        if !self.stun_urls.is_empty() {
            servers.push(IceServer { urls: self.stun_urls.clone(), username: None, credential: None });
        }
        if !self.turn_urls.is_empty() && !self.turn_secret.is_empty() {
            let expires = unix_millis() / 1000 + self.turn_ttl.as_secs();
            let username = format!("{expires}:{id}");
            let mut mac = Hmac::<sha1::Sha1>::new_from_slice(self.turn_secret.as_bytes())
                .expect("HMAC accepts keys of any length");
            mac.update(username.as_bytes());
            let credential = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
            servers.push(IceServer {
                urls: self.turn_urls.clone(),
                username: Some(username),
                credential: Some(credential),
            });
        }
        servers
    }
}

pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct Shared {
    config: Config,
    state: Mutex<State>,
}

/// Accept connections on `listener` forever.
pub async fn run(listener: TcpListener, config: Config) -> anyhow::Result<()> {
    let shared = Arc::new(Shared { config, state: Mutex::new(State::default()) });
    loop {
        let (stream, addr) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, addr, shared).await {
                tracing::debug!(%addr, "connection ended with error: {err:#}");
            }
        });
    }
}

async fn handle_connection(stream: TcpStream, addr: SocketAddr, shared: Arc<Shared>) -> anyhow::Result<()> {
    let ws_config =
        WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES)).max_frame_size(Some(MAX_MESSAGE_BYTES));
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config)).await?;
    let (mut sink, mut source) = ws.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let id = shared.state.lock().unwrap().add_client(tx);

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(Outgoing::Msg(msg)) => {
                        if sink.send(Message::text(msg.to_json())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outgoing::Close(reason)) => {
                        let frame = CloseFrame { code: CloseCode::Policy, reason: reason.into() };
                        let _ = sink.send(Message::Close(Some(frame))).await;
                        break;
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    loop {
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, source.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(err))) => {
                tracing::debug!(%addr, "read error: {err}");
                break;
            }
            Ok(None) => break,
            Err(_) => {
                tracing::debug!(%addr, "idle timeout");
                break;
            }
        };
        let text = match frame {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(msg) = serde_json::from_str::<ClientMsg>(&text) else {
            tracing::debug!(%addr, "ignoring malformed message");
            continue;
        };
        shared.state.lock().unwrap().handle(id, msg, addr, &shared.config);
    }

    shared.state.lock().unwrap().remove_client(id);
    writer.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_credentials_match_coturn_scheme() {
        let config = Config {
            turn_urls: vec!["turn:example.com:3478".into()],
            turn_secret: "secret".into(),
            turn_ttl: Duration::from_secs(60),
            ..Default::default()
        };
        let servers = config.ice_servers_for(PeerId::nil());
        assert_eq!(servers.len(), 1);
        let turn = &servers[0];
        let username = turn.username.as_deref().unwrap();
        assert!(username.ends_with(&format!(":{}", PeerId::nil())));

        let mut mac = Hmac::<sha1::Sha1>::new_from_slice(b"secret").unwrap();
        mac.update(username.as_bytes());
        let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(turn.credential.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn no_turn_without_secret() {
        let config = Config {
            stun_urls: vec!["stun:example.com".into()],
            turn_urls: vec!["turn:example.com".into()],
            ..Default::default()
        };
        let servers = config.ice_servers_for(PeerId::nil());
        assert_eq!(servers.len(), 1);
        assert!(servers[0].username.is_none());
    }
}
