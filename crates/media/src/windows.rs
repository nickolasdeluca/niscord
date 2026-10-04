//! Windows Graphics Capture backend.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::HWND;
use ::windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use ::windows::Win32::UI::WindowsAndMessaging::IsIconic;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, GraphicsCaptureItemType,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::pace::Pacer;
use crate::scale::downscale_rgba;
use crate::{CaptureOptions, Error, FrameSink, Result, RgbaImage, Source, SourceId, SourceKind};

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

pub fn can_hide_border() -> bool {
    GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false)
}

pub fn list_sources() -> Vec<Source> {
    let mut sources = Vec::new();

    let primary = Monitor::primary().ok().map(|m| m.as_raw_hmonitor());
    for (i, monitor) in Monitor::enumerate().unwrap_or_default().into_iter().enumerate() {
        let width = monitor.width().unwrap_or(0);
        let height = monitor.height().unwrap_or(0);
        sources.push(Source {
            id: SourceId::Monitor(monitor.as_raw_hmonitor() as isize),
            kind: SourceKind::Screen,
            title: format!("Screen {}", i + 1),
            detail: format!("{width} × {height}"),
            width,
            height,
            primary: Some(monitor.as_raw_hmonitor()) == primary,
            minimized: false,
        });
    }

    let own_pid = std::process::id();
    for window in Window::enumerate().unwrap_or_default() {
        if window.process_id().ok() == Some(own_pid) || is_cloaked(&window) {
            continue;
        }
        let Ok(title) = window.title() else { continue };
        let title = title.trim().to_owned();
        if title.is_empty() {
            continue;
        }
        let process = window.process_name().unwrap_or_default();
        // The desktop itself shows up as a window.
        if title == "Program Manager" && process.eq_ignore_ascii_case("explorer.exe") {
            continue;
        }
        let width = window.width().unwrap_or(0).max(0) as u32;
        let height = window.height().unwrap_or(0).max(0) as u32;
        let minimized = unsafe { IsIconic(HWND(window.as_raw_hwnd())).as_bool() };
        if !minimized && (width < 16 || height < 16) {
            continue;
        }
        sources.push(Source {
            id: SourceId::Window(window.as_raw_hwnd() as isize),
            kind: SourceKind::Window,
            title,
            detail: process,
            width,
            height,
            primary: false,
            minimized,
        });
    }
    sources
}

/// Cloaked windows are invisible to the user: suspended UWP apps, windows on
/// other virtual desktops, and so on.
fn is_cloaked(window: &Window) -> bool {
    let mut cloaked = 0u32;
    let result = unsafe {
        DwmGetWindowAttribute(
            HWND(window.as_raw_hwnd()),
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
    };
    result.is_ok() && cloaked != 0
}

enum Item {
    Monitor(Monitor),
    Window(Window),
}

impl TryInto<GraphicsCaptureItemType> for Item {
    type Error = ::windows::core::Error;

    fn try_into(self) -> std::result::Result<GraphicsCaptureItemType, Self::Error> {
        match self {
            Item::Monitor(m) => m.try_into(),
            Item::Window(w) => w.try_into(),
        }
    }
}

fn resolve(id: SourceId) -> Result<Item> {
    match id {
        SourceId::Monitor(raw) => {
            let raw = raw as *mut std::ffi::c_void;
            // Handles of unplugged monitors are not reused, so membership in the
            // current list is a sufficient liveness check.
            Monitor::enumerate()
                .unwrap_or_default()
                .into_iter()
                .find(|m| m.as_raw_hmonitor() == raw)
                .map(Item::Monitor)
                .ok_or(Error::SourceGone)
        }
        SourceId::Window(raw) => {
            let window = Window::from_raw_hwnd(raw as *mut std::ffi::c_void);
            if window.is_valid() { Ok(Item::Window(window)) } else { Err(Error::SourceGone) }
        }
    }
}

/// Settings that work on this OS build: optional features are only requested
/// when supported, since asking for an unsupported one fails the capture.
fn settings<F>(item: Item, show_cursor: bool, min_interval: Option<Duration>, flags: F) -> Settings<F, Item> {
    let cursor = match GraphicsCaptureApi::is_cursor_settings_supported() {
        Ok(true) if show_cursor => CursorCaptureSettings::WithCursor,
        Ok(true) => CursorCaptureSettings::WithoutCursor,
        _ => CursorCaptureSettings::Default,
    };
    let border = if can_hide_border() { DrawBorderSettings::WithoutBorder } else { DrawBorderSettings::Default };
    // Include the shared app's menus and popups.
    let secondary = match GraphicsCaptureApi::is_secondary_windows_supported() {
        Ok(true) => SecondaryWindowSettings::Include,
        _ => SecondaryWindowSettings::Default,
    };
    let interval = match (min_interval, GraphicsCaptureApi::is_minimum_update_interval_supported()) {
        (Some(d), Ok(true)) => MinimumUpdateIntervalSettings::Custom(d),
        _ => MinimumUpdateIntervalSettings::Default,
    };
    Settings::new(item, cursor, border, secondary, interval, DirtyRegionSettings::Default, ColorFormat::Rgba8, flags)
}

fn frame_to_rgba(frame: &mut Frame, max_w: u32, max_h: u32) -> std::result::Result<RgbaImage, HandlerError> {
    let (width, height) = (frame.width(), frame.height());
    let mut buffer = frame.buffer()?;
    let row_bytes = buffer.row_pitch() as usize;
    Ok(downscale_rgba(buffer.as_raw_buffer(), width, height, row_bytes, max_w, max_h))
}

fn capture_error(err: impl std::fmt::Display) -> Error {
    Error::Capture(err.to_string())
}

// ---------------------------------------------------------------------------
// One-shot thumbnails

struct ThumbnailHandler {
    max: (u32, u32),
    tx: mpsc::SyncSender<RgbaImage>,
}

impl GraphicsCaptureApiHandler for ThumbnailHandler {
    type Flags = ((u32, u32), mpsc::SyncSender<RgbaImage>);
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self { max: ctx.flags.0, tx: ctx.flags.1 })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let image = frame_to_rgba(frame, self.max.0, self.max.1)?;
        let _ = self.tx.try_send(image);
        control.stop();
        Ok(())
    }
}

pub fn capture_thumbnail(id: SourceId, max_width: u32, max_height: u32, timeout: Duration) -> Result<RgbaImage> {
    let item = resolve(id)?;
    let (tx, rx) = mpsc::sync_channel(1);
    let settings = settings(item, false, None, ((max_width, max_height), tx));
    let control = ThumbnailHandler::start_free_threaded(settings).map_err(capture_error)?;
    let result = rx.recv_timeout(timeout).map_err(|_| Error::Timeout);
    if !control.is_finished() {
        let _ = control.stop();
    }
    result
}

// ---------------------------------------------------------------------------
// Continuous capture

struct StreamHandler {
    options: CaptureOptions,
    sink: Box<dyn FrameSink>,
    pacer: Pacer,
}

impl GraphicsCaptureApiHandler for StreamHandler {
    type Flags = (CaptureOptions, Box<dyn FrameSink>);
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        let (options, sink) = ctx.flags;
        Ok(Self { pacer: Pacer::new(options.max_fps), options, sink })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        if !self.pacer.accept(Instant::now()) {
            return Ok(());
        }
        let image = frame_to_rgba(frame, self.options.max_width, self.options.max_height)?;
        self.sink.frame(image);
        Ok(())
    }

    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        self.sink.closed();
        Ok(())
    }
}

pub struct RunningCapture {
    control: Option<CaptureControl<StreamHandler, HandlerError>>,
}

impl RunningCapture {
    pub fn start(id: SourceId, options: CaptureOptions, sink: Box<dyn FrameSink>) -> Result<Self> {
        let item = resolve(id)?;
        // Ask Windows for up to twice the target rate and pace precisely
        // ourselves: its throttle measures from the last delivered frame,
        // which can halve the rate of a source just above the target.
        let interval = Duration::from_secs(1) / (options.max_fps.max(1) * 2);
        let settings = settings(item, options.show_cursor, Some(interval), (options, sink));
        let control = StreamHandler::start_free_threaded(settings).map_err(capture_error)?;
        Ok(Self { control: Some(control) })
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if let Some(control) = self.control.take()
            && let Err(err) = control.stop()
        {
            tracing::debug!("capture ended with error: {err}");
        }
    }
}

impl Drop for RunningCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}
