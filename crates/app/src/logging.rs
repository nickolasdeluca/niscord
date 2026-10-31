//! Logs go to `%APPDATA%\Niscord\niscord.log` (the previous run's are kept in
//! `niscord.old.log`), since release builds have no console. Crashes are
//! logged there too, and a dialog tells the user where to find them.
//!
//! When the server asks for them (see `ServerMsg::Welcome::log_filter`), lines
//! matching its filter are also queued for upload, so problems on a friend's
//! machine can be diagnosed without asking them for files.

use std::collections::VecDeque;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

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

/// Lines waiting to be sent to the server. Bounded: when the server is out of
/// reach (or the filter is chatty) the oldest go first.
const MAX_PENDING_LINES: usize = 10_000;
/// Longest line uploaded; the rest is cut, to keep messages under the
/// server's 64 KB limit.
const MAX_LINE_BYTES: usize = 4 * 1024;
/// Raw text per upload. JSON escaping can grow it, so well under 64 KB.
const MAX_BATCH_BYTES: usize = 24 * 1024;

#[derive(Default)]
struct Pending {
    lines: VecDeque<String>,
    dropped: usize,
}

impl Pending {
    fn push(&mut self, mut line: String) {
        if line.len() > MAX_LINE_BYTES {
            let mut end = MAX_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str(" [cut]");
        }
        if self.lines.len() >= MAX_PENDING_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    /// The next batch to send, oldest first.
    fn take(&mut self) -> Vec<String> {
        let mut batch = Vec::new();
        let mut bytes = 0;
        if self.dropped > 0 {
            batch.push(format!("--- {} log lines were dropped here ---", self.dropped));
            self.dropped = 0;
        }
        while let Some(line) = self.lines.front() {
            if !batch.is_empty() && bytes + line.len() > MAX_BATCH_BYTES {
                break;
            }
            bytes += line.len();
            batch.extend(self.lines.pop_front());
        }
        batch
    }
}

static PENDING: Mutex<Pending> = Mutex::new(Pending { lines: VecDeque::new(), dropped: 0 });
static REMOTE_FILTER: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

/// Collects one formatted event; queued when the formatter is done with it.
struct RemoteWriter(Vec<u8>);

impl std::io::Write for RemoteWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RemoteWriter {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.0);
        let line = text.trim_end();
        if !line.is_empty() {
            PENDING.lock().unwrap().push(line.to_owned());
        }
    }
}

/// Start (or stop, with `None`) queueing log lines for the server.
pub fn set_remote_filter(filter: Option<&str>) {
    let Some(handle) = REMOTE_FILTER.get() else { return };
    let parsed = filter.map(EnvFilter::try_new).transpose().unwrap_or_else(|err| {
        tracing::warn!("ignoring the server's log filter: {err}");
        None
    });
    let enabled = parsed.is_some();
    if let Err(err) = handle.reload(parsed.unwrap_or_else(|| EnvFilter::new("off"))) {
        tracing::warn!("can't change the server log filter: {err}");
        return;
    }
    if enabled {
        tracing::info!(version = env!("CARGO_PKG_VERSION"), filter, "sending logs to the server");
    } else {
        *PENDING.lock().unwrap() = Pending::default();
    }
}

/// The next batch of lines for the server, if any are waiting.
pub fn take_remote_lines() -> Vec<String> {
    PENDING.lock().unwrap().take()
}

/// The layer that queues lines for the server, off until its filter is set.
fn remote_layer() -> (impl Layer<Registry>, reload::Handle<EnvFilter, Registry>) {
    let (filter, handle) = reload::Layer::new(EnvFilter::new("off"));
    let layer =
        tracing_subscriber::fmt::layer().with_ansi(false).with_writer(|| RemoteWriter(Vec::new())).with_filter(filter);
    (layer, handle)
}

/// Set up logging and the crash handler. `RUST_LOG` overrides the level.
pub fn init() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (remote, handle) = remote_layer();
    let _ = REMOTE_FILTER.set(handle);
    let registry = Registry::default().with(remote);
    let log_path = match open_log_file() {
        Some((path, file)) => {
            let local = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::io::stderr.and(Mutex::new(file)))
                .with_filter(filter());
            registry.with(local).init();
            Some(path)
        }
        None => {
            registry.with(tracing_subscriber::fmt::layer().with_filter(filter())).init();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_are_bounded_and_in_order() {
        let mut pending = Pending::default();
        for i in 0..MAX_PENDING_LINES + 5 {
            pending.push(format!("{i:0>1000}"));
        }
        let first = pending.take();
        assert_eq!(first[0], "--- 5 log lines were dropped here ---");
        assert!(first[1].ends_with('5'), "the oldest kept line comes first");
        assert!(first.iter().map(String::len).sum::<usize>() <= MAX_BATCH_BYTES + 100);

        let mut sent = first.len() - 1;
        loop {
            let batch = pending.take();
            if batch.is_empty() {
                break;
            }
            sent += batch.len();
        }
        assert_eq!(sent, MAX_PENDING_LINES);
    }

    #[test]
    fn lines_are_queued_only_while_the_server_asks() {
        let (layer, handle) = remote_layer();
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("before");
            handle.reload(EnvFilter::new("niscord=debug")).unwrap();
            tracing::debug!(bps = 700_000, "encoder bitrate");
            tracing::trace!("too detailed");
            tracing::debug!(target: "webrtc", "other crate");
            handle.reload(EnvFilter::new("off")).unwrap();
            tracing::info!("after");
        });
        let lines = take_remote_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("DEBUG niscord::logging::tests: encoder bitrate bps=700000"), "{}", lines[0]);
    }

    #[test]
    fn long_lines_are_cut_on_a_char_boundary() {
        let mut pending = Pending::default();
        pending.push("é".repeat(MAX_LINE_BYTES));
        let line = &pending.take()[0];
        assert!(line.len() <= MAX_LINE_BYTES + " [cut]".len());
        assert!(line.ends_with(" [cut]"));
    }
}
