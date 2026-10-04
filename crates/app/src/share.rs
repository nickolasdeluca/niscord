//! Choosing a source to share and running the video pipeline.
//!
//! Until peers can connect, the encoded stream is decoded locally, so the
//! preview shows exactly what viewers will get, with live stats.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use niscord_media::pipeline::{EncodedSink, Snapshot, StreamSettings, VideoReceiver, VideoSender};
use niscord_media::video::{EncodedFrame, suggested_bitrate};
use niscord_media::{RgbaImage, Source, SourceKind};
use niscord_protocol::{ClientMsg, ShareKind};
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::settings::{FRAME_RATES, Resolution, Settings};
use crate::{App, SourceItem, thumbnails, with_app};

pub struct Picker {
    sources: Vec<Source>,
    windows: Rc<VecModel<SourceItem>>,
    screens: Rc<VecModel<SourceItem>>,
    _thumbnails: thumbnails::Job,
}

pub struct ActiveShare {
    source: Source,
    // Field order matters: the sender (and its encoder thread, which feeds
    // the receiver) must stop before the receiver.
    sender: VideoSender,
    receiver: Arc<VideoReceiver>,
    last_stats: Cell<(Snapshot, Snapshot)>,
}

fn to_pixel_buffer(image: &RgbaImage) -> SharedPixelBuffer<Rgba8Pixel> {
    SharedPixelBuffer::clone_from_slice(&image.pixels, image.width, image.height)
}

/// Stream settings for a quality preset.
fn stream_settings(source: &Source, resolution: Resolution, fps: u32) -> StreamSettings {
    let (max_width, max_height) = resolution.max_size();
    // Budget bits for the size frames will actually have, not the preset box.
    let (w, h) = niscord_media::fit(source.width, source.height, max_width, max_height);
    StreamSettings { max_width, max_height, fps, bitrate_bps: suggested_bitrate(w, h, fps), show_cursor: true }
}

impl App {
    pub fn open_picker(&self) {
        let generation = self.picker_generation.get() + 1;
        self.picker_generation.set(generation);

        let sources = niscord_media::list_sources();
        let item = |key: usize, s: &Source| SourceItem {
            key: key as i32,
            title: s.title.as_str().into(),
            detail: s.detail.as_str().into(),
            is_screen: s.kind == SourceKind::Screen,
            primary: s.primary,
            minimized: s.minimized,
            has_thumbnail: false,
            thumbnail: Image::default(),
        };
        let (mut windows, mut screens) = (Vec::new(), Vec::new());
        for (key, source) in sources.iter().enumerate() {
            match source.kind {
                SourceKind::Window => windows.push(item(key, source)),
                SourceKind::Screen => screens.push(item(key, source)),
            }
        }
        let windows = Rc::new(VecModel::from(windows));
        let screens = Rc::new(VecModel::from(screens));

        // Pre-select what is being shared, so "Switch" is one click away.
        let current = self.share.borrow().as_ref().map(|s| s.source.id);
        let selected = current.and_then(|id| sources.iter().position(|s| s.id == id)).map_or(-1, |k| k as i32);

        let targets = sources.iter().enumerate().filter(|(_, s)| !s.minimized).map(|(k, s)| (k as i32, s.id)).collect();
        // Without border control every grab flashes a yellow border, so only
        // refresh thumbnails when Windows lets us hide it.
        let live = niscord_media::can_hide_capture_border();
        let job = thumbnails::spawn(targets, live, move |key, image| {
            let buffer = to_pixel_buffer(&image);
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app| app.on_thumbnail(generation, key, buffer));
            });
        });

        let ui = self.ui();
        ui.set_picker_windows(ModelRc::from(windows.clone()));
        ui.set_picker_screens(ModelRc::from(screens.clone()));
        ui.set_picker_selected(selected);
        let settings = Settings::load();
        ui.set_picker_resolution(Resolution::ALL.iter().position(|r| *r == settings.resolution).unwrap_or(1) as i32);
        ui.set_picker_fps(FRAME_RATES.iter().position(|f| *f == settings.fps).unwrap_or(1) as i32);
        ui.set_picker_open(true);
        *self.picker.borrow_mut() = Some(Picker { sources, windows, screens, _thumbnails: job });
    }

    pub fn close_picker(&self) {
        self.picker_generation.set(self.picker_generation.get() + 1);
        self.picker.borrow_mut().take();
        let ui = self.ui();
        ui.set_picker_open(false);
        ui.set_picker_windows(ModelRc::default());
        ui.set_picker_screens(ModelRc::default());
    }

    fn on_thumbnail(&self, generation: u64, key: i32, buffer: SharedPixelBuffer<Rgba8Pixel>) {
        if generation != self.picker_generation.get() {
            return;
        }
        let picker = self.picker.borrow();
        let Some(picker) = picker.as_ref() else { return };
        for model in [&picker.windows, &picker.screens] {
            if let Some(row) = model.iter().position(|item| item.key == key) {
                let mut item = model.row_data(row).unwrap();
                item.thumbnail = Image::from_rgba8(buffer);
                item.has_thumbnail = true;
                model.set_row_data(row, item);
                return;
            }
        }
    }

    pub fn choose_source(&self, key: i32) {
        let source = self.picker.borrow().as_ref().and_then(|p| p.sources.get(key as usize).cloned());
        let ui = self.ui();
        let resolution = Resolution::ALL.get(ui.get_picker_resolution() as usize).copied().unwrap_or_default();
        let fps = FRAME_RATES.get(ui.get_picker_fps() as usize).copied().unwrap_or(30);
        let mut settings = Settings::load();
        settings.resolution = resolution;
        settings.fps = fps;
        settings.save();

        self.close_picker();
        if let Some(source) = source {
            self.start_share(source, resolution, fps);
        }
    }

    fn start_share(&self, source: Source, resolution: Resolution, fps: u32) {
        // Release the previous pipeline before starting a new one.
        let was_sharing = self.share.borrow_mut().take().is_some();
        let generation = self.share_generation.get() + 1;
        self.share_generation.set(generation);

        let slot = FrameSlot { generation, latest: Arc::default() };
        let started = VideoReceiver::start(move |image| slot.offer(image)).and_then(|receiver| {
            let receiver = Arc::new(receiver);
            let sink = LoopbackSink { generation, receiver: receiver.clone() };
            let settings = stream_settings(&source, resolution, fps);
            tracing::info!(title = source.title, ?settings, "sharing");
            VideoSender::start(source.id, settings, sink).map(|sender| (sender, receiver))
        });
        let (sender, receiver) = match started {
            Ok(pipeline) => pipeline,
            Err(err) => {
                tracing::warn!(title = source.title, "could not start sharing: {err}");
                self.show_notice(format!("Couldn't share \"{}\": {err}", source.title));
                self.ui().set_sharing(false);
                if was_sharing {
                    self.send(ClientMsg::ShareStop);
                }
                return;
            }
        };

        let ui = self.ui();
        ui.set_share_title(source.title.as_str().into());
        ui.set_preview(Image::default());
        ui.set_stream_stats("Starting…".into());
        ui.set_sharing(true);
        let last_stats = Cell::new((sender.counters().snapshot(), receiver.counters().snapshot()));
        *self.share.borrow_mut() = Some(ActiveShare { source, sender, receiver, last_stats });
        self.stats_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
            with_app(|app| app.update_stream_stats());
        });
        self.announce_share();
    }

    /// Tell the server what we're sharing (again, after a reconnect).
    pub fn announce_share(&self) {
        if let Some(share) = self.share.borrow().as_ref() {
            let kind = match share.source.kind {
                SourceKind::Screen => ShareKind::Screen,
                SourceKind::Window => ShareKind::Window,
            };
            self.send(ClientMsg::ShareStart { kind, title: share.source.title.clone(), audio: false });
        }
    }

    pub fn stop_share(&self, notice: Option<&str>) {
        self.share_generation.set(self.share_generation.get() + 1);
        self.stats_timer.stop();
        let was_sharing = self.share.borrow_mut().take().is_some();
        let ui = self.ui();
        ui.set_sharing(false);
        ui.set_preview(Image::default());
        if was_sharing {
            self.send(ClientMsg::ShareStop);
        }
        if let Some(notice) = notice {
            self.show_notice(notice.to_owned());
        }
    }

    fn on_preview_frame(&self, generation: u64, image: RgbaImage) {
        if generation == self.share_generation.get() {
            self.ui().set_preview(Image::from_rgba8(to_pixel_buffer(&image)));
        }
    }

    fn update_stream_stats(&self) {
        let share = self.share.borrow();
        let Some(share) = share.as_ref() else { return };
        let (prev_enc, prev_dec) = share.last_stats.get();
        let (enc, dec) = (share.sender.counters().snapshot(), share.receiver.counters().snapshot());
        share.last_stats.set((enc, dec));
        let (e, d) = (enc.rates_since(&prev_enc), dec.rates_since(&prev_dec));

        let mut text = if enc.width == 0 {
            "Waiting for the first frame…".to_owned()
        } else {
            let rate =
                if e.kbps >= 1000.0 { format!("{:.1} Mbps", e.kbps / 1000.0) } else { format!("{:.0} kbps", e.kbps) };
            format!(
                "{}×{} · {:.0} fps (source {:.0}) · {rate} · encode {:.1} ms · decode {:.1} ms · delay {:.0} ms",
                enc.width, enc.height, e.fps, e.source_fps, e.busy_ms, d.busy_ms, d.latency_ms
            )
        };
        if e.dropped + d.dropped > 0 {
            text += &format!(" · {} dropped", e.dropped + d.dropped);
        }
        if e.skipped > 0 {
            text += &format!(" · {} skipped by encoder", e.skipped);
        }
        self.ui().set_stream_stats(text.into());
    }
}

/// Hands decoded frames to the UI, keeping only the newest one so a busy UI
/// thread skips frames instead of queueing them.
struct FrameSlot {
    generation: u64,
    latest: Arc<Mutex<Option<RgbaImage>>>,
}

impl FrameSlot {
    fn offer(&self, image: RgbaImage) {
        // If a frame was already waiting, a UI update is already scheduled.
        if self.latest.lock().unwrap().replace(image).is_some() {
            return;
        }
        let latest = self.latest.clone();
        let generation = self.generation;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(image) = latest.lock().unwrap().take() {
                with_app(|app| app.on_preview_frame(generation, image));
            }
        });
    }
}

/// Feeds encoded frames straight into a local decoder (stand-in for the
/// network until peers can connect).
struct LoopbackSink {
    generation: u64,
    receiver: Arc<VideoReceiver>,
}

impl EncodedSink for LoopbackSink {
    fn encoded(&mut self, frame: &EncodedFrame, captured_at: Instant) {
        self.receiver.push(frame.data.clone(), frame.keyframe, Some(captured_at));
    }

    fn source_closed(&mut self) {
        let generation = self.generation;
        let _ = slint::invoke_from_event_loop(move || {
            with_app(|app| {
                if generation == app.share_generation.get() {
                    app.stop_share(Some("The shared window was closed"));
                }
            });
        });
    }
}
