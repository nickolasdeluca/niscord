//! Development helper: joins a server as a fake friend who is "sharing"
//! (no actual media), so the presence/watch UI can be tested with one app.
//!
//! cargo run -p niscord-server --example fake_peer -- ws://127.0.0.1:8080 "Fake Friend" [password]

use futures_util::{SinkExt, StreamExt};
use niscord_protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg, ShareKind};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:8080".into());
    let name = args.next().unwrap_or_else(|| "Fake Friend".into());
    let password = args.next().unwrap_or_default();

    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let send = |msg: ClientMsg| Message::text(msg.to_json());
    ws.send(send(ClientMsg::Hello { name, password, protocol: PROTOCOL_VERSION })).await?;
    ws.send(send(ClientMsg::ShareStart { kind: ShareKind::Window, title: "Some Game".into(), audio: true })).await?;

    while let Some(frame) = ws.next().await {
        let Message::Text(text) = frame? else { continue };
        match serde_json::from_str::<ServerMsg>(&text)? {
            ServerMsg::Peers { peers } => {
                println!("online: {}", peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "));
            }
            other => println!("{other:?}"),
        }
    }
    Ok(())
}
