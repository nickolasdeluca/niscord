//! Logs go to `%APPDATA%\Niscord\niscord.log` (the previous run's are kept in
//! `niscord.old.log`), since release builds have no console. Crashes are
//! logged there too, and a dialog tells the user where to find them.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub fn log_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Niscord"))
}

fn open_log_file() -> Option<(PathBuf, File)> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("niscord.log");
    let _ = std::fs::rename(&path, dir.join("niscord.old.log"));
    let file = File::create(&path).ok()?;
    Some((path, file))
}

/// Set up logging and the crash handler. `RUST_LOG` overrides the level.
pub fn init() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_path = match open_log_file() {
        Some((path, file)) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(std::io::stderr.and(Mutex::new(file)))
                .init();
            Some(path)
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter()).init();
            None
        }
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Niscord starting");

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| format!(" at {l}")).unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".into());
        let thread = std::thread::current().name().unwrap_or("unnamed").to_owned();
        tracing::error!("crash in thread '{thread}'{location}: {message}");
        default_hook(info);
        let where_logged = match &log_path {
            Some(path) => format!("\n\nDetails were saved to:\n{}", path.display()),
            None => String::new(),
        };
        show_error(&format!("Niscord ran into a problem and has to close.\n\n{message}{where_logged}"));
        // A panic on any thread leaves the app half-working; better to exit.
        std::process::exit(1);
    }));
}

#[cfg(windows)]
fn show_error(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    // SAFETY: a plain modal message box with owned, NUL-terminated strings.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Niscord"), MB_OK | MB_ICONERROR);
    }
}

#[cfg(not(windows))]
fn show_error(_text: &str) {}
