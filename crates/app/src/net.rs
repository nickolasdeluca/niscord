//! Connection to the signaling server.
//!
//! Runs on the tokio runtime. Reconnects with backoff when an established
//! connection drops; a failure before the first successful join (bad URL,
//! wrong password, ...) is reported as fatal so the user can fix the form.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use niscord_protocol::{ClientMsg, IceServer, PROTOCOL_VERSION, PeerId, PeerInfo, ServerMsg};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// The server pings every 15 s, so this much silence means the link is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Params {
    pub url: String,
    pub name: String,
    pub password: String,
}

#[derive(Debug)]
pub enum Event {
    /// Joined (or re-joined) the server. The id changes on every join.
    Joined {
        id: PeerId,
        ice_servers: Vec<IceServer>,
    },
    Peers(Vec<PeerInfo>),
    /// The connection dropped; retrying in the background.
    Reconnecting(String),
    /// Giving up; the session is over.
    Fatal(String),
    /// Any other server message (watch requests, signaling, ...).
    Server(ServerMsg),
}

/// A live connection task. Dropping it disconnects.
pub struct Session {
    tx: mpsc::UnboundedSender<ClientMsg>,
    task: JoinHandle<()>,
}

impl Session {
    pub fn start(
        rt: &tokio::runtime::Handle,
        params: Params,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = rt.spawn(run(params, rx, on_event));
        Self { tx, task }
    }

    /// Queue a message. Messages sent while reconnecting are dropped.
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(msg);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Accept `host:port`, `http(s)://` and `ws(s)://` forms.
pub fn normalize_url(input: &str) -> String {
    let input = input.trim().trim_end_matches('/');
    if let Some(rest) = input.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = input.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if input.contains("://") {
        input.to_owned()
    } else {
        format!("ws://{input}")
    }
}

enum End {
    /// The user closed the session.
    Closed,
    /// The server refused us; retrying won't help.
    Rejected(String),
    /// Network trouble; worth retrying.
    Lost(String),
}

async fn run(
    params: Params,
    mut rx: mpsc::UnboundedReceiver<ClientMsg>,
    on_event: impl Fn(Event) + Send + Sync + 'static,
) {
    let mut ever_joined = false;
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut joined = false;
        match connect_once(&params, &mut rx, &on_event, &mut joined).await {
            End::Closed => return,
            End::Rejected(msg) => return on_event(Event::Fatal(msg)),
            End::Lost(msg) if !ever_joined && !joined => {
                return on_event(Event::Fatal(format!("Couldn't connect to the server: {msg}")));
            }
            End::Lost(msg) => {
                tracing::info!("connection lost: {msg}");
                ever_joined = true;
                if joined {
                    backoff = Duration::from_secs(1);
                }
                on_event(Event::Reconnecting(msg));
            }
        }

        // Wait before retrying, dropping anything the UI sends meanwhile.
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                msg = rx.recv() => if msg.is_none() { return },
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_once(
    params: &Params,
    rx: &mut mpsc::UnboundedReceiver<ClientMsg>,
    on_event: &(impl Fn(Event) + Send + Sync),
    joined: &mut bool,
) -> End {
    let connect = tokio_tungstenite::connect_async(params.url.as_str());
    let mut ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(err)) => return End::Lost(err.to_string()),
        Err(_) => return End::Lost("timed out".into()),
    };

    let hello =
        ClientMsg::Hello { name: params.name.clone(), password: params.password.clone(), protocol: PROTOCOL_VERSION };
    if let Err(err) = ws.send(Message::text(hello.to_json())).await {
        return End::Lost(err.to_string());
    }

    loop {
        tokio::select! {
            frame = tokio::time::timeout(READ_TIMEOUT, ws.next()) => {
                let text = match frame {
                    Ok(Some(Ok(Message::Text(text)))) => text,
                    Ok(Some(Ok(Message::Close(frame)))) => {
                        let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                        return End::Lost(if reason.is_empty() { "server closed the connection".into() } else { reason });
                    }
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(err))) => return End::Lost(err.to_string()),
                    Ok(None) => return End::Lost("server closed the connection".into()),
                    Err(_) => return End::Lost("server stopped responding".into()),
                };
                let msg = match serde_json::from_str::<ServerMsg>(&text) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!("ignoring unknown server message: {err}");
                        continue;
                    }
                };
                match msg {
                    ServerMsg::Welcome { id, ice_servers, .. } => {
                        *joined = true;
                        on_event(Event::Joined { id, ice_servers });
                    }
                    ServerMsg::Error { message, .. } => return End::Rejected(message),
                    ServerMsg::Peers { peers } => on_event(Event::Peers(peers)),
                    other => on_event(Event::Server(other)),
                }
            }
            msg = rx.recv() => {
                let Some(msg) = msg else {
                    let _ = ws.close(None).await;
                    return End::Closed;
                };
                if let Err(err) = ws.send(Message::text(msg.to_json())).await {
                    return End::Lost(err.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn urls_are_normalized() {
        assert_eq!(normalize_url(" example.com:8080 "), "ws://example.com:8080");
        assert_eq!(normalize_url("https://example.com/"), "wss://example.com");
        assert_eq!(normalize_url("http://1.2.3.4:8080"), "ws://1.2.3.4:8080");
        assert_eq!(normalize_url("wss://example.com/niscord"), "wss://example.com/niscord");
    }
}
