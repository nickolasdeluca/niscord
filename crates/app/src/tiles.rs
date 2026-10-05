//! The stage's video tiles: your own stream (key [`SELF_KEY`]) and each
//! stream you watch (keyed by the sharer's peer id).

use std::collections::HashMap;
use std::sync::Mutex;

use niscord_media::RgbaImage;
use slint::{Image, Model, Rgba8Pixel, SharedPixelBuffer};

use crate::{App, StreamTile, with_app};

pub const SELF_KEY: &str = "self";

/// Collects decoded pictures from decoder threads, keeping only the newest
/// per tile, and schedules one UI update for whatever is pending. A busy UI
/// thread therefore skips frames instead of queueing them.
#[derive(Default)]
pub struct FrameMailbox {
    pending: Mutex<HashMap<String, RgbaImage>>,
}

impl FrameMailbox {
    pub fn offer(&self, key: String, image: RgbaImage) {
        let schedule = {
            let mut pending = self.pending.lock().unwrap();
            let was_empty = pending.is_empty();
            pending.insert(key, image);
            was_empty
        };
        if schedule {
            let _ = slint::invoke_from_event_loop(|| with_app(|app| app.flush_frames()));
        }
    }

    fn take(&self) -> HashMap<String, RgbaImage> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }
}

pub fn to_image(image: &RgbaImage) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&image.pixels, image.width, image.height))
}

impl App {
    fn tile_row(&self, key: &str) -> Option<usize> {
        self.tiles.iter().position(|t| t.key == key)
    }

    pub fn tile(&self, key: &str) -> Option<StreamTile> {
        self.tile_row(key).and_then(|row| self.tiles.row_data(row))
    }

    pub fn has_tile(&self, key: &str) -> bool {
        self.tile_row(key).is_some()
    }

    /// Add a tile, or replace the one with the same key. Your own stream
    /// always comes first.
    pub fn upsert_tile(&self, mut tile: StreamTile) {
        match self.tile_row(&tile.key) {
            Some(row) => {
                // A replaced tile (e.g. a new source) stays where it was shown.
                tile.popped = self.tiles.row_data(row).is_some_and(|old| old.popped);
                self.sync_popout(&tile);
                self.tiles.set_row_data(row, tile);
            }
            None if tile.is_self => self.tiles.insert(0, tile),
            None => self.tiles.push(tile),
        }
        self.ensure_ticking();
    }

    pub fn update_tile(&self, key: &str, f: impl FnOnce(&mut StreamTile)) {
        if let Some(row) = self.tile_row(key) {
            let mut tile = self.tiles.row_data(row).unwrap();
            f(&mut tile);
            self.sync_popout(&tile);
            self.tiles.set_row_data(row, tile);
        }
    }

    pub fn remove_tile(&self, key: &str) {
        self.close_popout(key);
        if let Some(row) = self.tile_row(key) {
            self.tiles.remove(row);
        }
        let ui = self.ui();
        if ui.get_focused_stream() == key {
            ui.set_focused_stream("".into());
        }
        self.stream_stats.borrow_mut().remove(key);
    }

    /// Drop every tile but your own (after a disconnect, ids are stale).
    pub fn remove_remote_tiles(&self) {
        let keys: Vec<_> = self.tiles.iter().filter(|t| !t.is_self).map(|t| t.key.to_string()).collect();
        for key in keys {
            self.remove_tile(&key);
        }
    }

    pub fn flush_frames(&self) {
        for (key, image) in self.frames.take() {
            self.update_tile(&key, |tile| {
                tile.frame = to_image(&image);
                tile.has_frame = true;
                if !tile.failed {
                    tile.status = "".into();
                }
            });
        }
    }

    /// Refresh stats once a second while any tile is shown.
    fn ensure_ticking(&self) {
        if !self.stats_timer.running() {
            self.stats_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), || {
                with_app(|app| app.tick_stats());
            });
        }
    }

    fn tick_stats(&self) {
        if self.tiles.row_count() == 0 {
            self.stats_timer.stop();
            return;
        }
        self.update_share_stats();
        self.update_watch_stats();
    }
}

/// "1.2 Mbps" / "850 kbps".
pub fn format_rate(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.1} Mbps", kbps / 1000.0) } else { format!("{kbps:.0} kbps") }
}

pub fn new_tile(key: impl Into<slint::SharedString>, name: &str, title: &str, is_self: bool) -> StreamTile {
    StreamTile {
        key: key.into(),
        name: name.into(),
        title: title.into(),
        frame: Image::default(),
        has_frame: false,
        stats: "".into(),
        status: "Connecting…".into(),
        is_self,
        failed: false,
        has_audio: false,
        volume: 1.0,
        muted: false,
        popped: false,
    }
}
