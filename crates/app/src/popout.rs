//! Streams popped out into their own windows (like Discord's pop-out).
//!
//! A popped tile stays in the tile model (marked `popped`, which filters it
//! off the stage) so every update keeps flowing through `update_tile`; the
//! window just mirrors it.

use slint::{CloseRequestResponse, ComponentHandle};

use crate::{App, StreamTile, StreamWindow, with_app};

/// Run `f` on the app after the current UI callback: window callbacks can
/// close their own window, which must not be dropped mid-callback.
fn later(f: impl FnOnce(&App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_app(|app| f(app)));
}

impl App {
    pub fn pop_out(&self, key: &str) {
        if let Some(window) = self.popouts.borrow().get(key) {
            let _ = window.show();
            return;
        }
        let Some(tile) = self.tile(key) else { return };
        let window = match StreamWindow::new() {
            Ok(window) => window,
            Err(err) => {
                self.show_notice(format!("Couldn't open a window: {err}"));
                return;
            }
        };

        let k = key.to_owned();
        window.on_close_stream({
            let k = k.clone();
            move || {
                let k = k.clone();
                later(move |app| app.close_stream(&k));
            }
        });
        window.on_retry({
            let k = k.clone();
            move || {
                let k = k.clone();
                later(move |app| app.retry_stream(&k));
            }
        });
        window.on_change_source(|| later(|app| app.open_picker()));
        window.on_pop_in({
            let k = k.clone();
            move || {
                let k = k.clone();
                later(move |app| app.pop_in(&k));
            }
        });
        window.on_hide_preview(|| later(|app| app.set_preview_hidden(true)));
        window.on_set_volume({
            let k = k.clone();
            move |volume| with_app(|app| app.set_stream_volume(&k, volume))
        });
        window.on_toggle_mute({
            let k = k.clone();
            move || with_app(|app| app.toggle_stream_mute(&k))
        });
        window.on_toggle_fullscreen({
            let weak = window.as_weak();
            move || {
                if let Some(window) = weak.upgrade() {
                    let fullscreen = !window.window().is_fullscreen();
                    window.window().set_fullscreen(fullscreen);
                    window.set_fullscreen(fullscreen);
                }
            }
        });
        window.window().on_close_requested(move || {
            let k = k.clone();
            later(move |app| app.pop_in(&k));
            CloseRequestResponse::HideWindow
        });

        window.set_tile(StreamTile { popped: true, ..tile });
        if let Err(err) = window.show() {
            self.show_notice(format!("Couldn't open a window: {err}"));
            return;
        }
        let ui = self.ui();
        if ui.get_focused_stream() == key {
            ui.set_focused_stream("".into());
        }
        self.popouts.borrow_mut().insert(key.to_owned(), window);
        self.update_tile(key, |tile| tile.popped = true);
        self.ui().set_popped_count(self.popouts.borrow().len() as i32);
    }

    /// Put a popped-out stream back on the stage.
    pub fn pop_in(&self, key: &str) {
        self.close_popout(key);
        self.update_tile(key, |tile| tile.popped = false);
    }

    /// Mirror a tile change into its window, if it has one.
    pub(crate) fn sync_popout(&self, tile: &StreamTile) {
        if let Some(window) = self.popouts.borrow().get(tile.key.as_str()) {
            window.set_tile(tile.clone());
        }
    }

    pub(crate) fn close_popout(&self, key: &str) {
        let window = self.popouts.borrow_mut().remove(key);
        if let Some(window) = window {
            let _ = window.hide();
            self.ui().set_popped_count(self.popouts.borrow().len() as i32);
        }
    }

    /// When the main window closes, the pop-outs go with it.
    pub fn close_all_popouts(&self) {
        let windows: Vec<_> = self.popouts.borrow_mut().drain().map(|(_, window)| window).collect();
        for window in windows {
            let _ = window.hide();
        }
    }
}
