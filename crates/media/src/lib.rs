//! Screen capture (and, in later milestones, encoding and decoding) for Niscord.
//!
//! Capture uses Windows Graphics Capture, which can record whole monitors or
//! single windows, including GPU-rendered ones such as games. On other
//! platforms every entry point reports [`Error::Unsupported`].

pub mod audio;
pub mod color;
#[cfg(windows)]
mod mf_encoder;
mod pace;
pub mod pipeline;
mod scale;
pub mod video;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub mod windows_audio;

use std::time::Duration;

pub use scale::{downscale_rgba, fit};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("screen capture is not supported on this system")]
    Unsupported,
    #[error("the source is no longer available")]
    SourceGone,
    #[error("timed out waiting for a frame")]
    Timeout,
    #[error("capture failed: {0}")]
    Capture(String),
    #[error("video codec error: {0}")]
    Codec(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Identifies a capturable source. Raw handles are stored as integers so the
/// id is `Send` and cheap to copy; they are only valid while the source exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceId {
    Monitor(isize),
    Window(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Screen,
    Window,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub id: SourceId,
    pub kind: SourceKind,
    /// Window title, or "Screen 1" etc. for monitors.
    pub title: String,
    /// Executable name for windows (e.g. "chrome.exe"); resolution for monitors.
    pub detail: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// Minimized windows can't be captured until restored.
    pub minimized: bool,
    /// Process owning the window, whose audio goes with it. `None` for screens.
    pub process_id: Option<u32>,
}

/// A tightly packed RGBA8 image.
#[derive(Debug, Clone)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureOptions {
    /// Frames faster than this are dropped.
    pub max_fps: u32,
    /// Frames are downscaled (keeping aspect ratio) to fit in this box.
    pub max_width: u32,
    pub max_height: u32,
    pub show_cursor: bool,
}

/// Lists monitors first, then windows ordered top-most first. Niscord's own
/// windows are excluded.
pub fn list_sources() -> Vec<Source> {
    #[cfg(windows)]
    return windows::list_sources();
    #[cfg(not(windows))]
    Vec::new()
}

/// The window in front of the user, if it can be shared (not Niscord's own,
/// not the desktop, not minimized).
pub fn foreground_window() -> Option<Source> {
    #[cfg(windows)]
    return windows::foreground_window();
    #[cfg(not(windows))]
    None
}

/// Whether captures can run without Windows drawing a yellow border around
/// the source (Windows 11 and later). When false, every capture flashes a
/// border, so callers should avoid frequent one-shot captures.
pub fn can_hide_capture_border() -> bool {
    #[cfg(windows)]
    return windows::can_hide_border();
    #[cfg(not(windows))]
    false
}

/// Grab a single frame of `id`, scaled to fit `max_width` x `max_height`.
pub fn capture_thumbnail(id: SourceId, max_width: u32, max_height: u32, timeout: Duration) -> Result<RgbaImage> {
    #[cfg(windows)]
    return windows::capture_thumbnail(id, max_width, max_height, timeout);
    #[cfg(not(windows))]
    {
        let _ = (id, max_width, max_height, timeout);
        Err(Error::Unsupported)
    }
}

/// Callbacks for a running capture. Both run on the capture thread.
pub trait FrameSink: Send + 'static {
    fn frame(&mut self, image: RgbaImage);
    /// The source went away (window closed, monitor unplugged).
    fn closed(&mut self);
}

/// A running capture; stops when dropped.
pub struct Capture {
    #[cfg(windows)]
    inner: windows::RunningCapture,
}

impl Capture {
    pub fn start(id: SourceId, options: CaptureOptions, sink: impl FrameSink) -> Result<Self> {
        #[cfg(windows)]
        return Ok(Self { inner: windows::RunningCapture::start(id, options, Box::new(sink))? });
        #[cfg(not(windows))]
        {
            let _ = (id, options, sink);
            Err(Error::Unsupported)
        }
    }

    /// Stop capturing and wait for the capture thread to exit.
    pub fn stop(self) {
        #[cfg(windows)]
        self.inner.stop();
    }
}
