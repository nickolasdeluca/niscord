//! Choosing a source to share and running the local capture.

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use niscord_media::{Capture, CaptureOptions, FrameSink, RgbaImage, Source, SourceKind};
use niscord_protocol::{ClientMsg, ShareKind};
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::{App, SourceItem, thumbnails, with_app};

/// Local preview only for now; the encoder will get its own settings.
const PREVIEW: CaptureOptions = CaptureOptions { max_fps: 30, max_width: 1280, max_height: 720, show_cursor: true };

pub struct Picker {
    sources: Vec<Source>,
    windows: Rc<VecModel<SourceItem>>,
    screens: Rc<VecModel<SourceItem>>,
    _thumbnails: thumbnails::Job,
}

pub struct ActiveShare {
    source: Source,
    _capture: Capture,
}

fn to_pixel_buffer(image: &RgbaImage) -> SharedPixelBuffer<Rgba8Pixel> {
    SharedPixelBuffer::clone_from_slice(&image.pixels, image.width, image.height)
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
        self.close_picker();
        if let Some(source) = source {
            self.start_share(source);
        }
    }

    fn start_share(&self, source: Source) {
        // Release the previous capture before starting a new one.
        let was_sharing = self.share.borrow_mut().take().is_some();
        let generation = self.share_generation.get() + 1;
        self.share_generation.set(generation);

        let sink = PreviewSink { generation, latest: Arc::default() };
        let capture = match Capture::start(source.id, PREVIEW, sink) {
            Ok(capture) => capture,
            Err(err) => {
                tracing::warn!(title = source.title, "capture failed: {err}");
                self.show_notice(format!("Couldn't capture \"{}\": {err}", source.title));
                self.ui().set_sharing(false);
                if was_sharing {
                    self.send(ClientMsg::ShareStop);
                }
                return;
            }
        };
        tracing::info!(title = source.title, "sharing");

        let ui = self.ui();
        ui.set_share_title(source.title.as_str().into());
        ui.set_preview(Image::default());
        ui.set_sharing(true);
        *self.share.borrow_mut() = Some(ActiveShare { source, _capture: capture });
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
}

/// Forwards captured frames to the UI, keeping only the newest one so a busy
/// UI thread skips frames instead of queueing them.
struct PreviewSink {
    generation: u64,
    latest: Arc<Mutex<Option<RgbaImage>>>,
}

impl FrameSink for PreviewSink {
    fn frame(&mut self, image: RgbaImage) {
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

    fn closed(&mut self) {
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
