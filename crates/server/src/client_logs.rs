//! Logs uploaded by clients, for diagnosing problems on friends' machines
//! without asking them for files. Off unless the operator sets a filter.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use niscord_protocol::PeerId;

/// Stop writing a connection's log past this size, so one chatty client (or a
/// filter set too wide) can't fill the disk.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ClientLogs {
    /// `RUST_LOG`-style filter clients apply to what they send.
    pub filter: String,
    /// Where each connection's log is written.
    pub dir: PathBuf,
}

/// One connection's log file, opened on the first lines it sends.
pub struct ClientLog {
    file: Option<File>,
    written: u64,
    full: bool,
}

impl ClientLog {
    pub fn new() -> Self {
        Self { file: None, written: 0, full: false }
    }

    /// Append `lines` from the client `name` (`id`) to its file in `dir`.
    pub fn append(&mut self, dir: &Path, name: &str, id: PeerId, lines: &[String]) -> std::io::Result<()> {
        if self.full {
            return Ok(());
        }
        let file = match &mut self.file {
            Some(file) => file,
            None => {
                std::fs::create_dir_all(dir)?;
                let path = dir.join(file_name(crate::unix_millis() / 1000, name, id));
                tracing::info!(%id, path = %path.display(), "writing client log");
                self.file.insert(OpenOptions::new().create(true).append(true).open(path)?)
            }
        };
        let mut text = String::new();
        for line in lines {
            text.push_str(line.trim_end_matches(['\r', '\n']));
            text.push('\n');
        }
        if self.written + text.len() as u64 > MAX_FILE_BYTES {
            self.full = true;
            text = format!("--- log reached {} MB, nothing more is written ---\n", MAX_FILE_BYTES / 1024 / 1024);
        }
        self.written += text.len() as u64;
        file.write_all(text.as_bytes())
    }
}

/// `2026-10-31_173109_Nick_8d26eb6b.log`: sorts by time, says who it is,
/// and stays a safe file name whatever the display name holds.
fn file_name(unix_secs: u64, name: &str, id: PeerId) -> String {
    let (year, month, day) = civil_from_days((unix_secs / 86_400) as i64);
    let secs = unix_secs % 86_400;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let name: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(32).collect();
    let id = id.simple().to_string();
    format!("{year:04}-{month:02}-{day:02}_{h:02}{m:02}{s:02}_{name}_{}.log", &id[..8])
}

/// Days since 1970-01-01 to a (year, month, day) date, in UTC. Howard
/// Hinnant's algorithm, to avoid a date crate for one file name.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_dated_and_safe() {
        let id = PeerId::parse_str("8d26eb6b-b667-4cb5-a76f-5b0641020d1c").unwrap();
        assert_eq!(file_name(1_793_467_869, "Nick de Luca", id), "2026-10-31_173109_Nick_de_Luca_8d26eb6b.log");
        assert_eq!(file_name(0, "a/../é", id), "1970-01-01_000000_a______8d26eb6b.log");
    }

    #[test]
    fn dates_across_leap_years() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_757), (2026, 10, 31));
    }

    #[test]
    fn stops_at_the_size_cap() {
        let dir = std::env::temp_dir().join(format!("niscord-client-log-{}", uuid::Uuid::new_v4()));
        let mut log = ClientLog::new();
        log.written = MAX_FILE_BYTES - 10;
        log.append(&dir, "a", PeerId::nil(), &["short".into()]).unwrap();
        log.append(&dir, "a", PeerId::nil(), &["this one does not fit".into()]).unwrap();
        log.append(&dir, "a", PeerId::nil(), &["nor this".into()]).unwrap();
        let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let text = std::fs::read_to_string(file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.starts_with("short\n--- log reached 64 MB"), "{text}");
        assert!(!text.contains("nor this"));
    }
}
