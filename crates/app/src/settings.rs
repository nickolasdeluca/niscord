//! Remembered connect-form values, stored in `%APPDATA%\Niscord\settings.json`.

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
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Niscord").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let mut settings: Self = path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if !FRAME_RATES.contains(&settings.fps) {
            settings.fps = 30;
        }
        if settings.server_url.is_empty() {
            settings.server_url = DEFAULT_SERVER.unwrap_or_default().to_owned();
        }
        settings
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
