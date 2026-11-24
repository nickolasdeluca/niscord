//! Remembered connect-form values and preferences, stored in
//! `%APPDATA%\Niscord\settings.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Server URL baked in at build time (`NISCORD_DEFAULT_SERVER=wss://... cargo build`),
/// so friends don't have to type it.
const DEFAULT_SERVER: Option<&str> = option_env!("NISCORD_DEFAULT_SERVER");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    #[serde(rename = "720p")]
    P720,
    #[default]
    #[serde(rename = "1080p")]
    P1080,
    /// Native size, up to what the encoder supports (4K).
    #[serde(rename = "source")]
    Source,
}

impl Resolution {
    pub const ALL: [Self; 3] = [Self::P720, Self::P1080, Self::Source];

    /// Bounding box frames are scaled down to fit.
    pub fn max_size(self) -> (u32, u32) {
        match self {
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::Source => (3840, 2160),
        }
    }
}

pub const FRAME_RATES: [u32; 3] = [15, 30, 60];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server_url: String,
    pub name: String,
    /// Stored in plain text: it is a shared group password, not an account
    /// secret, and retyping it on every launch would be annoying.
    pub password: String,
    pub resolution: Resolution,
    pub fps: u32,
    pub share_audio: bool,
    /// Keep streaming but don't show (or decode) your own preview.
    pub hide_preview: bool,
    /// Go-live shortcut, e.g. "Ctrl+Alt+S"; empty for none.
    pub shortcut: String,
    /// How you like to watch each user's stream, by [`stream_key`].
    pub streams: BTreeMap<String, StreamPrefs>,
}

/// What Niscord remembers about watching one user's stream, so it opens the
/// same way next time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamPrefs {
    pub volume: f32,
    pub muted: bool,
    /// Open it in its own window rather than on the stage.
    pub popped: bool,
    /// Where that window was last.
    pub window: Option<WindowPlace>,
}

impl Default for StreamPrefs {
    fn default() -> Self {
        Self { volume: 1.0, muted: false, popped: false, window: None }
    }
}

/// A pop-out window's position (outer, including the frame) and inner size,
/// in physical pixels, the way Slint reports and takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPlace {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Peer ids change on every connection, so streams are remembered by the
/// sharer's display name, ignoring case and surrounding spaces.
pub fn stream_key(name: &str) -> String {
    name.trim().to_lowercase()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            name: String::new(),
            password: String::new(),
            resolution: Resolution::default(),
            fps: 30,
            share_audio: true,
            hide_preview: false,
            shortcut: String::new(),
            streams: BTreeMap::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Niscord").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let mut settings = path().and_then(|p| std::fs::read(p).ok()).map_or_else(Self::default, |b| Self::parse(&b));
        if !FRAME_RATES.contains(&settings.fps) {
            settings.fps = 30;
        }
        if settings.server_url.is_empty() {
            settings.server_url = DEFAULT_SERVER.unwrap_or_default().to_owned();
        }
        settings
    }

    fn parse(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&path, serde_json::to_vec_pretty(self).unwrap()));
        if let Err(err) = result {
            tracing::warn!("could not save settings to {}: {err}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_before_stream_prefs_still_load() {
        let old = br#"{"server_url":"wss://niscord.example.com","name":"Ana","fps":60,"shortcut":"Ctrl+Alt+S"}"#;
        let settings = Settings::parse(old);
        assert_eq!(settings.name, "Ana");
        assert_eq!(settings.fps, 60);
        assert!(settings.streams.is_empty());
    }

    #[test]
    fn stream_prefs_round_trip() {
        let mut settings = Settings::default();
        let prefs = StreamPrefs {
            volume: 0.4,
            muted: true,
            popped: true,
            window: Some(WindowPlace { x: -1700, y: 120, width: 1280, height: 720 }),
        };
        settings.streams.insert(stream_key("Bob"), prefs.clone());
        let loaded = Settings::parse(&serde_json::to_vec(&settings).unwrap());
        assert_eq!(loaded.streams.get("bob"), Some(&prefs));
    }

    #[test]
    fn missing_stream_fields_take_defaults() {
        let settings = Settings::parse(br#"{"streams":{"bob":{"muted":true}}}"#);
        let prefs = &settings.streams["bob"];
        assert!(prefs.muted);
        assert_eq!(prefs.volume, 1.0);
        assert!(!prefs.popped);
        assert_eq!(prefs.window, None);
    }

    #[test]
    fn stream_key_ignores_case_and_spaces() {
        assert_eq!(stream_key("  Bob "), stream_key("bob"));
        assert_ne!(stream_key("Bob"), stream_key("Bobby"));
    }
}
