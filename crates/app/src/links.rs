//! WebRTC connections to other users, run on the tokio runtime.
//!
//! * As a sharer: one [`OutgoingPeer`] per viewer. Encoded frames fan out to
//!   all of them; their keyframe requests and bandwidth estimates steer the
//!   single encoder (the bitrate follows the slowest viewer).
//! * As a viewer: one [`IncomingPeer`] per stream being watched, each with its
//!   own decoder thread.
//!
//! Each connection is an actor task with a mailbox, so signaling messages for
//! one peer are applied strictly in the order the server relayed them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use niscord_media::RgbaImage;
use niscord_media::pipeline::{Counters, SenderControl, VideoReceiver};
use niscord_protocol::{ClientMsg, IceServer, PeerId, Role, SignalData};
use niscord_transport::{
    BitrateLimits, IncomingEvents, IncomingPeer, OutgoingEvents, OutgoingPeer, PeerEvents, PeerState, TransportConfig,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

/// Never ask the encoder for keyframes more often than this; several
/// viewers asking at once should cost one keyframe.
const KEYFRAME_COOLDOWN: Duration = Duration::from_millis(250);
/// How often the encoder bitrate is re-aimed at the bandwidth estimates.
const BITRATE_INTERVAL: Duration = Duration::from_secs(1);
/// Only retune the encoder when the estimate moved by more than this.
const BITRATE_HYSTERESIS: f64 = 0.10;
const MIN_BITRATE: u32 = 300_000;
/// Starting point for a new viewer's estimate; it ramps up from here.
const INITIAL_BITRATE: u32 = 2_500_000;

/// Things the UI needs to hear about. Sent from runtime threads.
pub enum LinkEvent {
    /// Connection state of a stream we watch.
    StreamState { sharer: PeerId, state: PeerState },
    /// A decoded picture from a stream we watch.
    StreamFrame { sharer: PeerId, image: RgbaImage },
    /// How many viewers are connected to our stream right now.
    ViewersConnected(usize),
}

enum Cmd {
    Signal(SignalData),
    Close,
}

struct Encoder {
    control: SenderControl,
    max_bitrate: u32,
    applied: Option<u32>,
    last_keyframe: Option<Instant>,
}

pub struct Links {
    rt: Handle,
    signaling: mpsc::UnboundedSender<ClientMsg>,
    config: Mutex<TransportConfig>,
    ui: Box<dyn Fn(LinkEvent) + Send + Sync>,
    /// Viewers of our stream: mailbox per viewer, plus the peers ready for frames.
    outgoing: Mutex<HashMap<PeerId, mpsc::UnboundedSender<Cmd>>>,
    ready: Mutex<HashMap<PeerId, Arc<OutgoingPeer>>>,
    /// Streams we watch.
    incoming: Mutex<HashMap<PeerId, Incoming>>,
    encoder: Mutex<Option<Encoder>>,
    frames: mpsc::UnboundedSender<(Bytes, Duration)>,
}

struct Incoming {
    mailbox: mpsc::UnboundedSender<Cmd>,
    counters: Arc<Counters>,
}

impl Links {
    pub fn new(
        rt: Handle,
        signaling: mpsc::UnboundedSender<ClientMsg>,
        ui: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let (frames, frames_rx) = mpsc::unbounded_channel();
        let links = Arc::new(Self {
            rt: rt.clone(),
            signaling,
            config: Mutex::new(TransportConfig {
                // Debugging aid: e.g. 127.0.0.1:0 keeps WebRTC on this machine.
                udp_addr: std::env::var("NISCORD_UDP_ADDR").unwrap_or_else(|_| "0.0.0.0:0".into()),
                ..TransportConfig::new(Vec::new())
            }),
            ui: Box::new(ui),
            outgoing: Mutex::default(),
            ready: Mutex::default(),
            incoming: Mutex::default(),
            encoder: Mutex::default(),
            frames,
        });
        rt.spawn(fan_out(Arc::downgrade(&links), frames_rx));
        rt.spawn(steer_bitrate(Arc::downgrade(&links)));
        links
    }

    /// STUN/TURN servers handed out by the signaling server.
    pub fn set_ice_servers(&self, ice_servers: Vec<IceServer>) {
        self.config.lock().unwrap().ice_servers = ice_servers;
    }

    #[cfg(test)]
    pub fn set_udp_addr(&self, addr: &str) {
        self.config.lock().unwrap().udp_addr = addr.into();
    }

    fn signal(&self, to: PeerId, role: Role, data: SignalData) {
        let _ = self.signaling.send(ClientMsg::Signal { to, role, data });
    }

    // -----------------------------------------------------------------------
    // Sharing

    /// Attach the encoder of the current share (or detach with `None`).
    /// Connections to viewers survive a source switch; the new encoder
    /// starts with a keyframe.
    pub fn set_encoder(&self, control: Option<SenderControl>, max_bitrate: u32) {
        *self.encoder.lock().unwrap() =
            control.map(|control| Encoder { control, max_bitrate, applied: None, last_keyframe: None });
    }

    /// Queue an encoded frame for every connected viewer.
    pub fn send_frame(&self, data: Bytes, duration: Duration) {
        let _ = self.frames.send((data, duration));
    }

    fn request_keyframe(&self) {
        if let Some(encoder) = self.encoder.lock().unwrap().as_mut()
            && encoder.last_keyframe.is_none_or(|t| t.elapsed() >= KEYFRAME_COOLDOWN)
        {
            encoder.last_keyframe = Some(Instant::now());
            encoder.control.request_keyframe();
        }
    }

    /// A viewer asked to watch: (re)connect to them.
    pub fn add_viewer(self: &Arc<Self>, viewer: PeerId) {
        self.remove_viewer(viewer);
        let (tx, rx) = mpsc::unbounded_channel();
        self.outgoing.lock().unwrap().insert(viewer, tx);
        let max = self.encoder.lock().unwrap().as_ref().map_or(INITIAL_BITRATE, |e| e.max_bitrate);
        let limits = BitrateLimits { initial: INITIAL_BITRATE.min(max), min: MIN_BITRATE.min(max), max };
        self.rt.spawn(run_outgoing(Arc::downgrade(self), viewer, limits, rx));
    }

    pub fn remove_viewer(&self, viewer: PeerId) {
        if let Some(mailbox) = self.outgoing.lock().unwrap().remove(&viewer) {
            let _ = mailbox.send(Cmd::Close);
        }
    }

    pub fn remove_all_viewers(&self) {
        for (_, mailbox) in self.outgoing.lock().unwrap().drain() {
            let _ = mailbox.send(Cmd::Close);
        }
    }

    fn report_viewers(&self) {
        let connected = self.ready.lock().unwrap().values().filter(|p| p.is_connected()).count();
        (self.ui)(LinkEvent::ViewersConnected(connected));
    }

    // -----------------------------------------------------------------------
    // Watching

    /// Start receiving `sharer`'s stream; the connection comes up when its
    /// offer arrives. `on_frame` runs on a decoder thread.
    pub fn watch(self: &Arc<Self>, sharer: PeerId) -> anyhow::Result<()> {
        self.unwatch(sharer);
        let links = Arc::downgrade(self);
        let receiver = Arc::new(VideoReceiver::start(move |image| {
            if let Some(links) = links.upgrade() {
                (links.ui)(LinkEvent::StreamFrame { sharer, image });
            }
        })?);
        let (tx, rx) = mpsc::unbounded_channel();
        self.incoming.lock().unwrap().insert(sharer, Incoming { mailbox: tx, counters: receiver.counters() });
        self.rt.spawn(run_incoming(Arc::downgrade(self), sharer, receiver, rx));
        Ok(())
    }

    pub fn unwatch(&self, sharer: PeerId) {
        if let Some(incoming) = self.incoming.lock().unwrap().remove(&sharer) {
            let _ = incoming.mailbox.send(Cmd::Close);
        }
    }

    pub fn stream_counters(&self, sharer: PeerId) -> Option<Arc<Counters>> {
        self.incoming.lock().unwrap().get(&sharer).map(|i| i.counters.clone())
    }

    // -----------------------------------------------------------------------

    /// Route a relayed signaling message to the right connection.
    pub fn on_signal(&self, from: PeerId, role: Role, data: SignalData) {
        let mailbox = match role {
            // From a sharer: belongs to a stream we watch.
            Role::Sharer => self.incoming.lock().unwrap().get(&from).map(|i| i.mailbox.clone()),
            // From a viewer: belongs to our stream.
            Role::Viewer => self.outgoing.lock().unwrap().get(&from).cloned(),
        };
        match mailbox {
            Some(mailbox) => {
                let _ = mailbox.send(Cmd::Signal(data));
            }
            None => tracing::debug!(%from, ?role, "signal for no connection"),
        }
    }

    pub fn close_all(&self) {
        self.remove_all_viewers();
        for (_, incoming) in self.incoming.lock().unwrap().drain() {
            let _ = incoming.mailbox.send(Cmd::Close);
        }
    }
}

impl Drop for Links {
    fn drop(&mut self) {
        self.close_all();
    }
}

// ---------------------------------------------------------------------------
// Actors

struct ToViewer {
    links: Weak<Links>,
    viewer: PeerId,
}

impl PeerEvents for ToViewer {
    fn signal(&self, data: SignalData) {
        if let Some(links) = self.links.upgrade() {
            links.signal(self.viewer, Role::Sharer, data);
        }
    }

    fn state(&self, state: PeerState) {
        tracing::info!(viewer = %self.viewer, ?state, "viewer connection");
        if let Some(links) = self.links.upgrade() {
            links.report_viewers();
        }
    }
}

impl OutgoingEvents for ToViewer {
    fn keyframe_requested(&self) {
        if let Some(links) = self.links.upgrade() {
            links.request_keyframe();
        }
    }
}

async fn run_outgoing(links: Weak<Links>, viewer: PeerId, limits: BitrateLimits, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let Some(config) = links.upgrade().map(|l| l.config.lock().unwrap().clone()) else { return };
    let events = Arc::new(ToViewer { links: links.clone(), viewer });
    let peer = match OutgoingPeer::start(&config, limits, events).await {
        Ok(peer) => Arc::new(peer),
        Err(err) => {
            tracing::warn!(%viewer, "could not start connection to viewer: {err:#}");
            return;
        }
    };
    if let Some(links) = links.upgrade() {
        links.ready.lock().unwrap().insert(viewer, peer.clone());
    }
    while let Some(Cmd::Signal(data)) = rx.recv().await {
        if let Err(err) = peer.handle_signal(data).await {
            tracing::warn!(%viewer, "bad signal from viewer: {err:#}");
        }
    }
    if let Some(links) = links.upgrade() {
        let mut ready = links.ready.lock().unwrap();
        if ready.get(&viewer).is_some_and(|p| Arc::ptr_eq(p, &peer)) {
            ready.remove(&viewer);
        }
        drop(ready);
        links.report_viewers();
    }
    peer.close().await;
}

struct FromSharer {
    links: Weak<Links>,
    sharer: PeerId,
    receiver: Arc<VideoReceiver>,
}

impl PeerEvents for FromSharer {
    fn signal(&self, data: SignalData) {
        if let Some(links) = self.links.upgrade() {
            links.signal(self.sharer, Role::Viewer, data);
        }
    }

    fn state(&self, state: PeerState) {
        tracing::info!(sharer = %self.sharer, ?state, "stream connection");
        if let Some(links) = self.links.upgrade() {
            (links.ui)(LinkEvent::StreamState { sharer: self.sharer, state });
        }
    }
}

impl IncomingEvents for FromSharer {
    fn frame(&self, data: Bytes, keyframe: bool) {
        self.receiver.push(data.to_vec(), keyframe, None);
    }
}

async fn run_incoming(
    links: Weak<Links>,
    sharer: PeerId,
    receiver: Arc<VideoReceiver>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let Some(config) = links.upgrade().map(|l| l.config.lock().unwrap().clone()) else { return };
    let events = Arc::new(FromSharer { links: links.clone(), sharer, receiver: receiver.clone() });
    let peer = match IncomingPeer::start(&config, events.clone()).await {
        Ok(peer) => peer,
        Err(err) => {
            tracing::warn!(%sharer, "could not start connection to sharer: {err:#}");
            events.state(PeerState::Failed);
            return;
        }
    };
    while let Some(Cmd::Signal(data)) = rx.recv().await {
        if let Err(err) = peer.handle_signal(data).await {
            tracing::warn!(%sharer, "bad signal from sharer: {err:#}");
        }
    }
    peer.close().await;
    // Joining the decoder thread can block briefly; keep it off the runtime.
    drop(events);
    let _ = tokio::task::spawn_blocking(move || drop(receiver)).await;
}

/// Sends every encoded frame to every connected viewer.
async fn fan_out(links: Weak<Links>, mut frames: mpsc::UnboundedReceiver<(Bytes, Duration)>) {
    while let Some((data, duration)) = frames.recv().await {
        let Some(peers) = links.upgrade().map(|l| l.ready.lock().unwrap().values().cloned().collect::<Vec<_>>()) else {
            return;
        };
        for peer in peers {
            if let Err(err) = peer.send_frame(data.clone(), duration).await {
                tracing::debug!("sending frame failed: {err:#}");
            }
        }
    }
}

/// Keeps the encoder's bitrate at what the slowest connected viewer can take.
async fn steer_bitrate(links: Weak<Links>) {
    let mut tick = tokio::time::interval(BITRATE_INTERVAL);
    loop {
        tick.tick().await;
        let Some(links) = links.upgrade() else { return };
        let estimate =
            links.ready.lock().unwrap().values().filter(|p| p.is_connected()).map(|p| p.target_bitrate()).min();
        let mut encoder = links.encoder.lock().unwrap();
        let Some(encoder) = encoder.as_mut() else { continue };
        if let Some(target) = next_bitrate(estimate, encoder.max_bitrate, encoder.applied) {
            tracing::debug!(bps = target, "encoder bitrate");
            encoder.control.set_bitrate(target);
            encoder.applied = Some(target);
        }
    }
}

/// The bitrate to give the encoder, if it should change: the slowest
/// viewer's estimate (or the quality preset's maximum when nobody watches),
/// clamped to sane bounds, ignoring small wobbles.
fn next_bitrate(slowest_estimate: Option<u32>, max: u32, applied: Option<u32>) -> Option<u32> {
    let target = slowest_estimate.unwrap_or(max).clamp(MIN_BITRATE.min(max), max);
    let changed =
        applied.is_none_or(|applied| (target as f64 - applied as f64).abs() / applied as f64 > BITRATE_HYSTERESIS);
    changed.then_some(target)
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
