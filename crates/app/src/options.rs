//! The Settings dialog, and what its options do: the go-live shortcut and
//! hiding your own preview (see `share.rs`).

use crate::hotkey::Shortcut;
use crate::settings::Settings;
use crate::{App, with_app};

impl App {
    /// Register the saved shortcut at start-up.
    pub fn init_shortcut(&self) {
        let saved = Settings::load().shortcut;
        let Some(shortcut) = Shortcut::parse(&saved) else { return };
        self.shortcut.set(Some(shortcut));
        if let Err(err) = self.register_shortcut(Some(shortcut)) {
            tracing::warn!("{err}");
            self.ui().set_shortcut_error(err.into());
        }
    }

    /// Replace the registered shortcut (`None` unregisters it).
    fn register_shortcut(&self, shortcut: Option<Shortcut>) -> Result<(), String> {
        self.hotkey.borrow_mut().take();
        let Some(shortcut) = shortcut else { return Ok(()) };
        #[cfg(windows)]
        {
            let listener = crate::hotkey::HotkeyListener::start(shortcut, || {
                let _ = slint::invoke_from_event_loop(|| with_app(|app| app.go_live_with_foreground()));
            })?;
            *self.hotkey.borrow_mut() = Some(listener);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = shortcut;
            Err("Shortcuts are only supported on Windows.".into())
        }
    }

    pub fn open_settings(&self) {
        let ui = self.ui();
        ui.set_shortcut(self.shortcut.get().map(|s| s.to_string()).unwrap_or_default().into());
        ui.set_show_preview(!Settings::load().hide_preview);
        ui.set_shortcut_recording(false);
        ui.set_settings_open(true);
    }

    pub fn close_settings(&self) {
        if self.ui().get_shortcut_recording() {
            self.cancel_recording();
        }
        self.ui().set_settings_open(false);
    }

    pub fn record_shortcut(&self) {
        // While registered, Windows would swallow the current combination
        // before it reached the dialog.
        self.hotkey.borrow_mut().take();
        let ui = self.ui();
        ui.set_shortcut_error("".into());
        ui.set_shortcut_recording(true);
    }

    /// A key went down while recording: take whatever combination is held.
    pub fn shortcut_key(&self, typed: &str) {
        let ui = self.ui();
        if !ui.get_shortcut_recording() {
            return;
        }
        #[cfg(windows)]
        let held = crate::hotkey::held_shortcut(typed);
        #[cfg(not(windows))]
        let held: Option<Shortcut> = {
            let _ = typed;
            None
        };
        // Only modifiers so far: keep waiting for the key.
        let Some(shortcut) = held else { return };
        if let Err(hint) = shortcut.check() {
            ui.set_shortcut_error(hint.into());
            return;
        }
        ui.set_shortcut_recording(false);
        match self.register_shortcut(Some(shortcut)) {
            Ok(()) => {
                self.shortcut.set(Some(shortcut));
                let mut settings = Settings::load();
                settings.shortcut = shortcut.to_string();
                settings.save();
                ui.set_shortcut(shortcut.to_string().into());
                ui.set_shortcut_error("".into());
            }
            Err(err) => {
                ui.set_shortcut_error(err.into());
                // Keep the previous one working.
                let _ = self.register_shortcut(self.shortcut.get());
            }
        }
    }

    pub fn cancel_recording(&self) {
        self.ui().set_shortcut_recording(false);
        if let Err(err) = self.register_shortcut(self.shortcut.get()) {
            self.ui().set_shortcut_error(err.into());
        }
    }

    pub fn clear_shortcut(&self) {
        let _ = self.register_shortcut(None);
        self.shortcut.set(None);
        let mut settings = Settings::load();
        settings.shortcut.clear();
        settings.save();
        let ui = self.ui();
        ui.set_shortcut("".into());
        ui.set_shortcut_error("".into());
    }

    /// The shortcut: share the window in front, or stop if it's the one
    /// already being shared.
    pub fn go_live_with_foreground(&self) {
        if !self.ui().get_online() {
            self.show_notice("Connect to a server before going live with the shortcut.");
            return;
        }
        let Some(source) = niscord_media::foreground_window() else {
            self.show_notice(
                "The shortcut shares the window in front, but that one can't be shared \
                 (it's Niscord itself, the desktop, or a minimized window).",
            );
            return;
        };
        if self.shared_source().is_some_and(|current| current.id == source.id) {
            tracing::info!(title = source.title, "shortcut: stop sharing");
            self.stop_share(Some("Stopped sharing"));
            return;
        }
        tracing::info!(title = source.title, "shortcut: share the window in front");
        self.close_picker();
        let settings = Settings::load();
        self.start_share(source, settings.resolution, settings.fps, settings.share_audio);
    }
}
