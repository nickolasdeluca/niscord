//! Per-user stream preferences (volume, mute, pop-out window), remembered so
//! a stream opens the way you left it the next time you watch that user.

use std::time::Duration;

use slint::{ComponentHandle, PhysicalPosition, PhysicalSize};

use crate::settings::{Settings, StreamPrefs, WindowPlace, stream_key};
use crate::tiles::SELF_KEY;
use crate::{App, StreamWindow, with_app};

/// A volume drag changes the preferences many times a second; write the file
/// once it settles.
const SAVE_DELAY: Duration = Duration::from_millis(750);

/// Of a restored window, at least this much of its top edge (the title bar)
/// must land on a screen, so it can still be grabbed after monitors change.
const GRAB_HEIGHT: i32 = 40;

impl App {
    /// Preferences for the stream behind a tile key; `None` for your own.
    pub(crate) fn stream_prefs(&self, key: &str) -> Option<StreamPrefs> {
        let name = self.stream_owner(key)?;
        Some(self.stream_prefs.borrow().get(&stream_key(&name)).cloned().unwrap_or_default())
    }

    /// Change the preferences for a tile key's stream and save them soon.
    pub(crate) fn edit_stream_prefs(&self, key: &str, f: impl FnOnce(&mut StreamPrefs)) {
        let Some(name) = self.stream_owner(key) else { return };
        f(self.stream_prefs.borrow_mut().entry(stream_key(&name)).or_default());
        self.prefs_timer.start(slint::TimerMode::SingleShot, SAVE_DELAY, || with_app(|app| app.save_stream_prefs()));
    }

    /// Write pending preference changes now (also run at exit).
    pub(crate) fn save_stream_prefs(&self) {
        self.prefs_timer.stop();
        let mut settings = Settings::load();
        if settings.streams != *self.stream_prefs.borrow() {
            settings.streams = self.stream_prefs.borrow().clone();
            settings.save();
        }
    }

    /// The sharer's name for a remote stream's tile. Taken from the tile, not
    /// the peer list, because a sharer who left is already gone from that.
    fn stream_owner(&self, key: &str) -> Option<String> {
        if key == SELF_KEY {
            return None;
        }
        self.tile(key).map(|tile| tile.name.to_string())
    }

    /// Remember where a pop-out window is, unless it is full screen,
    /// maximized or minimized (those aren't places to reopen at), or already
    /// hidden (closed with its X, which saved the place before hiding).
    pub(crate) fn remember_window_place(&self, key: &str, window: &StreamWindow) {
        let w = window.window();
        if !w.is_visible() || w.is_fullscreen() || w.is_maximized() || w.is_minimized() {
            return;
        }
        let (position, size) = (w.position(), w.size());
        if size.width == 0 || size.height == 0 {
            return;
        }
        let place = WindowPlace { x: position.x, y: position.y, width: size.width, height: size.height };
        self.edit_stream_prefs(key, |prefs| prefs.window = Some(place));
    }
}

/// Move a pop-out window to where it was last, if that is still on a screen.
pub(crate) fn restore_window_place(window: &StreamWindow, place: Option<WindowPlace>) {
    let Some(place) = place.filter(is_on_screen) else { return };
    window.window().set_size(PhysicalSize::new(place.width, place.height));
    window.window().set_position(PhysicalPosition::new(place.x, place.y));
}

#[cfg(windows)]
fn is_on_screen(place: &WindowPlace) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};

    let title_bar = RECT {
        left: place.x,
        top: place.y,
        right: place.x.saturating_add(place.width as i32),
        bottom: place.y.saturating_add(GRAB_HEIGHT),
    };
    // SAFETY: MonitorFromRect only reads the rectangle passed by reference.
    !unsafe { MonitorFromRect(&title_bar, MONITOR_DEFAULTTONULL) }.is_invalid()
}

#[cfg(not(windows))]
fn is_on_screen(place: &WindowPlace) -> bool {
    let _ = (place, GRAB_HEIGHT);
    true
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn far_off_screen_places_are_not_restored() {
        let gone = WindowPlace { x: -100_000, y: -100_000, width: 960, height: 540 };
        assert!(!is_on_screen(&gone));
    }
}
