//! Choosing a source to share and running the video (and audio) pipelines.
//!
//! Until peers can connect, the encoded stream is decoded locally, so the
//! preview shows exactly what viewers will get, with live stats.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;

use niscord_media::audio::DEFAULT_BITRATE;
use niscord_media::pipeline::{EncodedSink, Snapshot, StreamSettings, VideoReceiver, VideoSender};
use niscord_media::video::{EncodedFrame, suggested_bitrate};
use niscord_media::windows_audio::{AudioSender, AudioSource};
use niscord_media::{RgbaImage, Source, SourceKind};
use niscord_protocol::{ClientMsg, ShareKind};
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::links::Links;
use crate::settings::{FRAME_RATES, Resolution, Settings};
use crate::tiles::{SELF_KEY, format_rate, new_tile};
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
    audio: Option<AudioSender>,
    last_stats: Cell<(Snapshot, Snapshot)>,
    /// Audio bytes and when they were counted, for the audio bitrate.
    last_audio: Cell<(u64, Instant)>,
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
        ui.set_picker_audio(settings.share_audio);
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
        let audio = ui.get_picker_audio();
        let mut settings = Settings::load();
        settings.resolution = resolution;
        settings.fps = fps;
        settings.share_audio = audio;
        settings.save();

        self.close_picker();
        if let Some(source) = source {
            self.start_share(source, resolution, fps, audio);
        }
    }

    pub fn start_share(&self, source: Source, resolution: Resolution, fps: u32, share_audio: bool) {
        // Release the previous pipeline before starting a new one.
        let was_sharing = self.share.borrow_mut().take().is_some();
        let generation = self.share_generation.get() + 1;
        self.share_generation.set(generation);

        let links = self.links();
        let settings = stream_settings(&source, resolution, fps);
        let frames = self.frames.clone();
        // Errors are handled in `update_share_stats`: the preview gets every
        // packet, so they mean the GPU encoder's output is broken.
        let started =
            VideoReceiver::start(move |image| frames.offer(SELF_KEY.to_owned(), image), || {}).and_then(|receiver| {
                let receiver = Arc::new(receiver);
                let sink = StreamSink {
                    generation,
                    loopback: receiver.clone(),
                    preview: self.preview.clone(),
                    links: links.clone(),
                    frame_interval: Duration::from_secs(1) / fps.max(1),
                    last_capture: None,
                };
                tracing::info!(title = source.title, ?settings, "sharing");
                VideoSender::start(source.id, settings, sink).map(|sender| (sender, receiver))
            });
        let (sender, receiver) = match started {
            Ok(pipeline) => pipeline,
            Err(err) => {
                tracing::warn!(title = source.title, "could not start sharing: {err}");
                self.show_notice(format!("Couldn't share \"{}\": {err}", source.title));
                self.end_share_ui(was_sharing);
                return;
            }
        };
        // Viewers already connected keep their connections; the new encoder
        // starts with a keyframe.
        if let Some(links) = &links {
            links.set_encoder(Some(sender.control()), settings.bitrate_bps);
        }

        let audio = if share_audio { self.start_audio(&source, links.clone()) } else { None };

        if self.preview.load(Ordering::Relaxed) {
            self.show_self_tile(&source.title);
        }
        self.ui().set_sharing(true);
        let last_stats = Cell::new((sender.counters().snapshot(), receiver.counters().snapshot()));
        let last_audio = Cell::new((0, Instant::now()));
        *self.share.borrow_mut() = Some(ActiveShare { source, sender, receiver, audio, last_stats, last_audio });
        self.announce_share();
    }

    fn show_self_tile(&self, title: &str) {
        let mut tile = new_tile(SELF_KEY, "You", title, true);
        tile.status = "Starting…".into();
        self.upsert_tile(tile);
    }

    /// Show or hide your own preview tile (remembered). While hidden the
    /// stream goes on, but it isn't decoded locally.
    pub fn set_preview_hidden(&self, hidden: bool) {
        let mut settings = Settings::load();
        settings.hide_preview = hidden;
        settings.save();
        self.preview.store(!hidden, Ordering::Relaxed);
        let ui = self.ui();
        ui.set_preview_hidden(hidden);
        ui.set_show_preview(!hidden);
        if hidden {
            self.remove_tile(SELF_KEY);
            return;
        }
        let share = self.share.borrow();
        if let Some(share) = share.as_ref()
            && !self.has_tile(SELF_KEY)
        {
            self.show_self_tile(&share.source.title);
            // The preview decoder skipped everything so far; it needs a keyframe.
            share.sender.control().request_keyframe();
        }
    }

    /// The window being shared, if it's a window.
    pub fn shared_source(&self) -> Option<Source> {
        self.share.borrow().as_ref().map(|share| share.source.clone())
    }

    /// Capture the source's sound: a window's app, or for a screen,
    /// everything but Niscord (so the streams we watch don't echo back).
    fn start_audio(&self, source: &Source, links: Option<Arc<Links>>) -> Option<AudioSender> {
        let links = links?;
        let audio_source = match (source.kind, source.process_id) {
            (SourceKind::Window, Some(pid)) => AudioSource::Process(pid),
            _ => AudioSource::AllExcept(std::process::id()),
        };
        let started = AudioSender::start(audio_source, DEFAULT_BITRATE, move |packet| {
            links.send_audio(Bytes::from(packet));
        });
        match started {
            Ok(sender) => Some(sender),
            Err(err) => {
                tracing::warn!(?audio_source, "could not capture audio: {err}");
                self.show_notice(format!("Sharing without sound: {err}"));
                None
            }
        }
    }

    /// Tell the server what we're sharing (again, after a reconnect).
    pub fn announce_share(&self) {
        if let Some(share) = self.share.borrow().as_ref() {
            let kind = match share.source.kind {
                SourceKind::Screen => ShareKind::Screen,
                SourceKind::Window => ShareKind::Window,
            };
            let audio = share.audio.is_some();
            self.send(ClientMsg::ShareStart { kind, title: share.source.title.clone(), audio });
        }
    }

    pub fn stop_share(&self, notice: Option<&str>) {
        self.share_generation.set(self.share_generation.get() + 1);
        let was_sharing = self.share.borrow_mut().take().is_some();
        self.end_share_ui(was_sharing);
        if let Some(notice) = notice {
            self.show_notice(notice.to_owned());
        }
    }

    fn end_share_ui(&self, was_sharing: bool) {
        if let Some(links) = self.links() {
            links.set_encoder(None, 0);
            links.remove_all_viewers();
        }
        self.remove_tile(SELF_KEY);
        self.ui().set_sharing(false);
        self.viewers_connected.set(0);
        if was_sharing {
            self.send(ClientMsg::ShareStop);
        }
    }

    pub fn update_share_stats(&self) {
        let share = self.share.borrow();
        let Some(share) = share.as_ref() else { return };
        let (prev_enc, prev_dec) = share.last_stats.get();
        let (enc, dec) = (share.sender.counters().snapshot(), share.receiver.counters().snapshot());
        share.last_stats.set((enc, dec));
        let (e, d) = (enc.rates_since(&prev_enc), dec.rates_since(&prev_dec));
        // The preview decodes exactly what viewers get, with nothing lost on
        // the way: if it can't, the GPU encoder's output is the problem.
        if enc.hardware && dec.errors > prev_dec.errors {
            tracing::warn!("our own stream didn't decode; switching to software encoding");
            share.sender.control().use_software();
        }

        let mut text = if enc.width == 0 {
            "Waiting for the first frame…".to_owned()
        } else {
            format!(
                "{}×{} · {:.0} fps (source {:.0}) · {} · encode {:.1} ms ({}) · delay {:.0} ms",
                enc.width,
                enc.height,
                e.fps,
                e.source_fps,
                format_rate(e.kbps),
                e.busy_ms,
                if enc.hardware { "GPU" } else { "CPU" },
                d.latency_ms
            )
        };
        if e.dropped + d.dropped > 0 {
            text += &format!(" · {} dropped", e.dropped + d.dropped);
        }
        if e.skipped > 0 {
            text += &format!(" · {} skipped to fit the bandwidth", e.skipped);
        }
        if let Some(audio) = &share.audio {
            let (prev_bytes, prev_at) = share.last_audio.get();
            let bytes = audio.bytes();
            let now = Instant::now();
            share.last_audio.set((bytes, now));
            let secs = now.duration_since(prev_at).as_secs_f64().max(0.001);
            text += &format!(" · audio {}", format_rate((bytes - prev_bytes) as f64 * 8.0 / 1000.0 / secs));
        }
        let viewers = self.viewers_connected.get();
        if viewers > 0 {
            text += &format!(" · {viewers} watching");
        }
        tracing::debug!(stats = %text, "sharing");
        self.update_tile(SELF_KEY, |tile| tile.stats = text.into());
    }
}

/// Where encoded frames go: the local preview decoder (shows exactly what
/// viewers receive) and every connected viewer.
struct StreamSink {
    generation: u64,
    loopback: Arc<VideoReceiver>,
    /// Whether the preview is shown; when not, skip decoding it.
    preview: Arc<AtomicBool>,
    links: Option<Arc<Links>>,
    frame_interval: Duration,
    last_capture: Option<Instant>,
}

impl EncodedSink for StreamSink {
    fn encoded(&mut self, frame: &EncodedFrame, captured_at: Instant) {
        if self.preview.load(Ordering::Relaxed) {
            self.loopback.push(frame.data.clone(), frame.keyframe, Some(captured_at));
        }
        if let Some(links) = &self.links {
            // RTP timestamps advance by the real gap between captures, so
            // viewers play frames at the pace they were captured.
            let duration = self
                .last_capture
                .map(|last| captured_at.saturating_duration_since(last))
                .filter(|d| !d.is_zero() && *d < Duration::from_secs(2))
                .unwrap_or(self.frame_interval);
            self.last_capture = Some(captured_at);
            links.send_frame(Bytes::copy_from_slice(&frame.data), duration);
        }
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
