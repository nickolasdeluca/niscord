//! Watching other users' streams, and serving ours to viewers.

use std::sync::Arc;

use niscord_media::pipeline::Snapshot;
use niscord_protocol::{ClientMsg, PeerId, ServerMsg};
use niscord_transport::PeerState;
use slint::Model;

use crate::links::{LinkEvent, Links};
use crate::tiles::{FrameMailbox, SELF_KEY, format_rate, new_tile};
use crate::{App, with_app};

impl App {
    /// Create the connection manager for a new server session.
    pub fn start_links(&self, signaling: tokio::sync::mpsc::UnboundedSender<ClientMsg>) {
        let frames: Arc<FrameMailbox> = self.frames.clone();
        let generation = self.generation.get();
        let links = Links::new(self.rt.handle().clone(), signaling, move |event| match event {
            LinkEvent::StreamFrame { sharer, image } => frames.offer(sharer.to_string(), image),
            other => {
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| {
                        if app.generation.get() == generation {
                            app.on_link_event(other);
                        }
                    })
                });
            }
        });
        *self.links.borrow_mut() = Some(links);
    }

    pub fn links(&self) -> Option<Arc<Links>> {
        self.links.borrow().clone()
    }

    pub fn watch(&self, target: PeerId) {
        let key = target.to_string();
        if self.has_tile(&key) {
            return;
        }
        let Some(links) = self.links() else { return };
        if let Err(err) = links.watch(target) {
            self.show_notice(format!("Couldn't open the stream: {err}"));
            return;
        }
        let (name, title) = self.peer_share(target);
        self.upsert_tile(new_tile(key, &name, &title, false));
        self.send(ClientMsg::Watch { target });
    }

    pub fn unwatch(&self, target: PeerId) {
        if let Some(links) = self.links() {
            links.unwatch(target);
        }
        self.remove_tile(&target.to_string());
        self.send(ClientMsg::Unwatch { target });
    }

    /// Reconnect to a stream whose connection failed.
    pub fn retry_watch(&self, target: PeerId) {
        let Some(links) = self.links() else { return };
        if let Err(err) = links.watch(target) {
            self.show_notice(format!("Couldn't open the stream: {err}"));
            return;
        }
        self.update_tile(&target.to_string(), |tile| {
            tile.failed = false;
            tile.status = "Connecting…".into();
        });
        // The sharer sets up a fresh connection for every watch request.
        self.send(ClientMsg::Watch { target });
    }

    /// Tile close button: stop sharing, or stop watching that stream.
    pub fn close_stream(&self, key: &str) {
        if key == SELF_KEY {
            self.stop_share(None);
        } else if let Ok(target) = key.parse() {
            self.unwatch(target);
        }
    }

    pub fn retry_stream(&self, key: &str) {
        if let Ok(target) = key.parse() {
            self.retry_watch(target);
        }
    }

    fn on_link_event(&self, event: LinkEvent) {
        match event {
            LinkEvent::StreamState { sharer, state } => {
                let name = self.peer_name(sharer);
                self.update_tile(&sharer.to_string(), |tile| match state {
                    PeerState::Connecting if !tile.has_frame => tile.status = "Connecting…".into(),
                    PeerState::Connecting => {}
                    PeerState::Connected => {
                        if !tile.has_frame {
                            tile.status = "Waiting for video…".into();
                        }
                    }
                    PeerState::Failed => {
                        tile.failed = true;
                        tile.status = format!(
                            "Couldn't connect to {name}'s stream.\nOne of you may be behind a strict firewall or NAT; \
                             a TURN server on the Niscord server usually fixes this."
                        )
                        .into();
                    }
                    PeerState::Closed => {}
                });
            }
            LinkEvent::ViewersConnected(count) => self.viewers_connected.set(count),
            LinkEvent::StreamFrame { .. } => unreachable!("frames go through the mailbox"),
        }
    }

    /// Server messages that concern streams.
    pub fn on_stream_msg(&self, msg: ServerMsg) {
        let Some(links) = self.links() else { return };
        match msg {
            ServerMsg::WatchRequest { from } => {
                if self.share.borrow().is_some() {
                    tracing::info!(viewer = %from, "new viewer");
                    links.add_viewer(from);
                }
            }
            ServerMsg::ViewerLeft { from } => links.remove_viewer(from),
            ServerMsg::Signal { from, role, data } => links.on_signal(from, role, data),
            ServerMsg::ShareEnded { from } => {
                links.unwatch(from);
                if self.has_tile(&from.to_string()) {
                    self.remove_tile(&from.to_string());
                    self.show_notice(format!("{} stopped sharing", self.peer_name(from)));
                }
            }
            _ => {}
        }
    }

    pub fn update_watch_stats(&self) {
        let Some(links) = self.links() else { return };
        let keys: Vec<String> = self.tiles.iter().filter(|t| !t.is_self).map(|t| t.key.to_string()).collect();
        for key in keys {
            let Some(counters) = key.parse().ok().and_then(|id| links.stream_counters(id)) else { continue };
            let now = counters.snapshot();
            let previous: Option<Snapshot> = self.stream_stats.borrow_mut().insert(key.clone(), now);
            let Some(previous) = previous else { continue };
            if now.width == 0 {
                continue;
            }
            let r = now.rates_since(&previous);
            let mut text = format!(
                "{}×{} · {:.0} fps · {} · decode {:.1} ms",
                now.width,
                now.height,
                r.fps,
                format_rate(r.kbps),
                r.busy_ms
            );
            if r.dropped > 0 {
                text += &format!(" · {} dropped", r.dropped);
            }
            self.update_tile(&key, |tile| tile.stats = text.into());
        }
    }
}
