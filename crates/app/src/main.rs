// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod links;
mod net;
mod settings;
mod share;
mod thumbnails;
mod tiles;
mod watch;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use niscord_media::pipeline::Snapshot;

use niscord_protocol::{ClientMsg, PeerId, PeerInfo, ServerMsg, ShareKind};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tracing_subscriber::EnvFilter;

use crate::settings::Settings;

slint::include_modules!();

/// Main-thread application state. Network events reach it through
/// `slint::invoke_from_event_loop`, so it is only ever touched on the UI thread.
struct App {
    ui: slint::Weak<AppWindow>,
    rt: tokio::runtime::Runtime,
    session: RefCell<Option<net::Session>>,
    /// Bumped for every new session so late events from an old one are ignored.
    generation: Cell<u64>,
    my_id: Cell<Option<PeerId>>,
    peers: Rc<VecModel<PeerItem>>,
    notice_timer: slint::Timer,
    picker: RefCell<Option<share::Picker>>,
    /// Bumped whenever the picker opens or closes, to drop stale thumbnails.
    picker_generation: Cell<u64>,
    share: RefCell<Option<share::ActiveShare>>,
    /// Bumped whenever sharing starts or stops, to drop stale frames.
    share_generation: Cell<u64>,
    stats_timer: slint::Timer,
    /// WebRTC connections for the current server session.
    links: RefCell<Option<Arc<links::Links>>>,
    /// Stage tiles: your stream and the streams you watch.
    tiles: Rc<VecModel<StreamTile>>,
    frames: Arc<tiles::FrameMailbox>,
    /// Last counters per watched stream, for per-second rates.
    stream_stats: RefCell<HashMap<String, Snapshot>>,
    viewers_connected: Cell<usize>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn with_app(f: impl FnOnce(&Rc<App>)) {
    let app = APP.with(|a| a.borrow().clone());
    if let Some(app) = app {
        f(&app);
    }
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window outlives the app state")
    }

    fn connect(&self) {
        let ui = self.ui();
        let params = net::Params {
            url: net::normalize_url(&ui.get_server_url()),
            name: ui.get_display_name().trim().to_owned(),
            password: ui.get_password().to_string(),
        };
        if params.url == "ws://" {
            ui.set_connect_error("Enter the server address.".into());
            return;
        }
        if params.name.is_empty() {
            ui.set_connect_error("Choose a display name.".into());
            return;
        }
        let mut settings = Settings::load();
        settings.server_url = ui.get_server_url().trim().to_owned();
        settings.name = params.name.clone();
        settings.password = params.password.clone();
        settings.save();

        ui.set_connect_error("".into());
        ui.set_connecting(true);
        ui.set_my_name(params.name.as_str().into());

        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let session = net::Session::start(self.rt.handle(), params, move |event| {
            let _ = slint::invoke_from_event_loop(move || with_app(|app| app.on_net_event(generation, event)));
        });
        self.start_links(session.sender());
        *self.session.borrow_mut() = Some(session);
    }

    fn disconnect(&self, error: Option<String>) {
        self.close_picker();
        self.stop_share(None);
        self.remove_remote_tiles();
        self.links.borrow_mut().take();
        self.generation.set(self.generation.get() + 1);
        self.session.borrow_mut().take();
        self.my_id.set(None);
        self.peers.set_vec(Vec::new());
        let ui = self.ui();
        ui.set_connected(false);
        ui.set_connecting(false);
        ui.set_online(false);
        ui.set_connect_error(error.unwrap_or_default().into());
    }

    fn send(&self, msg: ClientMsg) {
        if let Some(session) = self.session.borrow().as_ref() {
            session.send(msg);
        }
    }

    fn show_notice(&self, text: impl Into<SharedString>) {
        self.ui().set_notice(text.into());
        let ui = self.ui.clone();
        self.notice_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(4), move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_notice("".into());
            }
        });
    }

    fn on_net_event(&self, generation: u64, event: net::Event) {
        if generation != self.generation.get() {
            return;
        }
        let ui = self.ui();
        match event {
            net::Event::Joined { id, ice_servers } => {
                tracing::info!(%id, ice = ice_servers.len(), "joined");
                if let Some(links) = self.links() {
                    links.set_ice_servers(ice_servers);
                }
                self.my_id.set(Some(id));
                ui.set_connecting(false);
                ui.set_connected(true);
                ui.set_online(true);
                ui.set_status_text("Connected".into());
                // After a reconnect the server has forgotten our share.
                self.announce_share();
            }
            net::Event::Peers(peers) => self.set_peers(peers),
            net::Event::Reconnecting(reason) => {
                tracing::info!("reconnecting: {reason}");
                // The old list is stale and its ids are about to change, so
                // every peer connection is dead too.
                if let Some(links) = self.links() {
                    links.close_all();
                }
                self.remove_remote_tiles();
                self.viewers_connected.set(0);
                self.my_id.set(None);
                self.peers.set_vec(Vec::new());
                ui.set_online(false);
                ui.set_status_text("Connection lost, reconnecting…".into());
            }
            net::Event::Fatal(message) => self.disconnect(Some(message)),
            net::Event::Server(msg) => self.on_server_msg(msg),
        }
    }

    fn on_server_msg(&self, msg: ServerMsg) {
        self.on_stream_msg(msg);
    }

    fn peer_name(&self, id: PeerId) -> String {
        self.peer_share(id).0
    }

    /// Name and share title of a peer, from the latest presence list.
    fn peer_share(&self, id: PeerId) -> (String, String) {
        use slint::Model;
        let id = SharedString::from(id.to_string());
        self.peers
            .iter()
            .find(|p| p.id == id)
            .map_or_else(|| ("Someone".into(), String::new()), |p| (p.name.to_string(), p.share_title.to_string()))
    }

    fn set_peers(&self, peers: Vec<PeerInfo>) {
        let me = self.my_id.get();
        let mut items: Vec<PeerItem> = peers
            .into_iter()
            .map(|p| {
                let is_me = Some(p.id) == me;
                let initial = p.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
                PeerItem {
                    id: p.id.to_string().into(),
                    initial: initial.into(),
                    is_me,
                    live: p.share.is_some(),
                    share_title: p.share.as_ref().map(|s| s.title.as_str()).unwrap_or_default().into(),
                    share_is_window: p.share.as_ref().is_some_and(|s| s.kind == ShareKind::Window),
                    viewer_count: p.viewers.len() as i32,
                    watching: me.is_some_and(|me| p.viewers.contains(&me)),
                    name: p.name.into(),
                }
            })
            .collect();
        // Keep the local user at the top; the server already sorts the rest.
        items.sort_by_key(|p| !p.is_me);
        self.peers.set_vec(items);
    }
}

fn parse_id(id: &str) -> Option<PeerId> {
    id.parse().ok()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let ui = AppWindow::new()?;

    let settings = Settings::load();
    ui.set_server_url(settings.server_url.into());
    ui.set_display_name(settings.name.into());
    ui.set_password(settings.password.into());

    let peers = Rc::new(VecModel::default());
    ui.set_peers(ModelRc::from(peers.clone()));
    let tiles = Rc::new(VecModel::default());
    ui.set_tiles(ModelRc::from(tiles.clone()));

    let app = Rc::new(App {
        ui: ui.as_weak(),
        rt,
        session: RefCell::new(None),
        generation: Cell::new(0),
        my_id: Cell::new(None),
        peers,
        notice_timer: slint::Timer::default(),
        picker: RefCell::new(None),
        picker_generation: Cell::new(0),
        share: RefCell::new(None),
        share_generation: Cell::new(0),
        stats_timer: slint::Timer::default(),
        links: RefCell::new(None),
        tiles,
        frames: Arc::default(),
        stream_stats: RefCell::new(HashMap::new()),
        viewers_connected: Cell::new(0),
    });
    APP.with(|a| *a.borrow_mut() = Some(app));

    ui.on_connect(|| with_app(|app| app.connect()));
    ui.on_disconnect(|| with_app(|app| app.disconnect(None)));
    ui.on_watch(|id| {
        with_app(|app| {
            if let Some(target) = parse_id(&id) {
                app.watch(target);
            }
        })
    });
    ui.on_unwatch(|id| {
        with_app(|app| {
            if let Some(target) = parse_id(&id) {
                app.unwatch(target);
            }
        })
    });
    ui.on_close_stream(|key| with_app(|app| app.close_stream(&key)));
    ui.on_retry_stream(|key| with_app(|app| app.retry_stream(&key)));
    ui.on_share(|| with_app(|app| app.open_picker()));
    ui.on_picker_cancel(|| with_app(|app| app.close_picker()));
    ui.on_picker_choose(|key| with_app(|app| app.choose_source(key)));
    ui.on_stop_share(|| with_app(|app| app.stop_share(None)));

    ui.run()?;

    // Tear down the network before the runtime goes away.
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().take() {
            app.picker.borrow_mut().take();
            app.share.borrow_mut().take();
            app.links.borrow_mut().take();
            app.session.borrow_mut().take();
        }
    });
    Ok(())
}
