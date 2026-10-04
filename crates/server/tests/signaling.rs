//! End-to-end tests of the signaling server over real WebSockets.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use niscord_protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, PeerId, Role, ServerMsg, ShareKind, SignalData};
use niscord_server::Config;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start(config: Config) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(niscord_server::run(listener, config));
    url
}

struct TestClient {
    ws: Ws,
}

impl TestClient {
    async fn connect(url: &str) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        Self { ws }
    }

    async fn join(url: &str, name: &str, password: &str) -> (Self, PeerId) {
        let mut c = Self::connect(url).await;
        c.send(ClientMsg::Hello { name: name.into(), password: password.into(), protocol: PROTOCOL_VERSION }).await;
        match c.recv().await {
            ServerMsg::Welcome { id, .. } => (c, id),
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    async fn send(&mut self, msg: ClientMsg) {
        self.ws.send(Message::text(msg.to_json())).await.unwrap();
    }

    async fn recv(&mut self) -> ServerMsg {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("timed out waiting for a message")
                .expect("connection closed")
                .unwrap();
            if let Message::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    /// Skip `peers` updates until some other message arrives.
    async fn recv_non_peers(&mut self) -> ServerMsg {
        loop {
            match self.recv().await {
                ServerMsg::Peers { .. } => continue,
                other => return other,
            }
        }
    }

    /// Wait for a `peers` update matching `pred`. Any other message is a bug.
    async fn wait_peers(&mut self, pred: impl Fn(&[niscord_protocol::PeerInfo]) -> bool) {
        loop {
            match self.recv().await {
                ServerMsg::Peers { peers } if pred(&peers) => return,
                ServerMsg::Peers { .. } => continue,
                other => panic!("unexpected message while waiting for peers: {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let url = start(Config { password: "letmein".into(), ..Default::default() }).await;
    let mut c = TestClient::connect(&url).await;
    c.send(ClientMsg::Hello { name: "Ana".into(), password: "nope".into(), protocol: PROTOCOL_VERSION }).await;
    match c.recv().await {
        ServerMsg::Error { code, .. } => assert_eq!(code, ErrorCode::BadPassword),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn protocol_mismatch_is_rejected() {
    let url = start(Config::default()).await;
    let mut c = TestClient::connect(&url).await;
    c.send(ClientMsg::Hello { name: "Ana".into(), password: String::new(), protocol: 0 }).await;
    match c.recv().await {
        ServerMsg::Error { code, .. } => assert_eq!(code, ErrorCode::ProtocolMismatch),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn presence_watch_and_signal_relay() {
    let url = start(Config { password: "pw".into(), ..Default::default() }).await;
    let (mut ana, ana_id) = TestClient::join(&url, "Ana", "pw").await;
    let (mut bia, bia_id) = TestClient::join(&url, "Bia", "pw").await;

    ana.wait_peers(|p| p.len() == 2).await;
    bia.wait_peers(|p| p.len() == 2).await;

    // Watching someone who isn't sharing fails immediately.
    bia.send(ClientMsg::Watch { target: ana_id }).await;
    assert_eq!(bia.recv_non_peers().await, ServerMsg::ShareEnded { from: ana_id });

    // Signals are not relayed without a watch relationship.
    ana.send(ClientMsg::Signal { to: bia_id, role: Role::Sharer, data: SignalData::Offer { sdp: "x".into() } }).await;

    ana.send(ClientMsg::ShareStart { kind: ShareKind::Screen, title: "Display 1".into(), audio: true }).await;
    bia.wait_peers(|p| p.iter().any(|p| p.id == ana_id && p.share.is_some())).await;

    bia.send(ClientMsg::Watch { target: ana_id }).await;
    assert_eq!(ana.recv_non_peers().await, ServerMsg::WatchRequest { from: bia_id });
    ana.wait_peers(|p| p.iter().any(|p| p.id == ana_id && p.viewers == vec![bia_id])).await;

    let offer = SignalData::Offer { sdp: "v=0 offer".into() };
    ana.send(ClientMsg::Signal { to: bia_id, role: Role::Sharer, data: offer.clone() }).await;
    // The first relayed signal must be this one, not the rejected one above.
    assert_eq!(bia.recv_non_peers().await, ServerMsg::Signal { from: ana_id, role: Role::Sharer, data: offer });

    let answer = SignalData::Answer { sdp: "v=0 answer".into() };
    bia.send(ClientMsg::Signal { to: ana_id, role: Role::Viewer, data: answer.clone() }).await;
    assert_eq!(ana.recv_non_peers().await, ServerMsg::Signal { from: bia_id, role: Role::Viewer, data: answer });

    bia.send(ClientMsg::Unwatch { target: ana_id }).await;
    assert_eq!(ana.recv_non_peers().await, ServerMsg::ViewerLeft { from: bia_id });

    bia.send(ClientMsg::Watch { target: ana_id }).await;
    assert_eq!(ana.recv_non_peers().await, ServerMsg::WatchRequest { from: bia_id });

    ana.send(ClientMsg::ShareStop).await;
    assert_eq!(bia.recv_non_peers().await, ServerMsg::ShareEnded { from: ana_id });
}

#[tokio::test]
async fn disconnecting_sharer_ends_share_for_viewers() {
    let url = start(Config::default()).await;
    let (mut ana, ana_id) = TestClient::join(&url, "Ana", "").await;
    let (mut bia, bia_id) = TestClient::join(&url, "Bia", "").await;

    ana.send(ClientMsg::ShareStart { kind: ShareKind::Window, title: "Game".into(), audio: false }).await;
    bia.wait_peers(|p| p.iter().any(|p| p.id == ana_id && p.share.is_some())).await;
    bia.send(ClientMsg::Watch { target: ana_id }).await;
    assert_eq!(ana.recv_non_peers().await, ServerMsg::WatchRequest { from: bia_id });

    drop(ana);
    assert_eq!(bia.recv_non_peers().await, ServerMsg::ShareEnded { from: ana_id });
    bia.wait_peers(|p| p.len() == 1 && p[0].id == bia_id).await;
}
