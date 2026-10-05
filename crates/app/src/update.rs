//! Self-update from GitHub releases.
//!
//! Every few hours the app asks GitHub for the latest release. When it is
//! newer, `niscord.exe` is downloaded next to the running exe and checked
//! against the release's `SHA256SUMS.txt`; the user then restarts when it
//! suits them (an update never interrupts a stream). Restarting renames the
//! running exe aside, which Windows allows, moves the new one into its place
//! and starts it. The old file is removed on the next launch.
//!
//! Releases are public and so carry no server address; the one a friend's
//! private build came with is already saved in their settings by then.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Where releases come from (`owner/repo`); overridable at build time.
const REPO: &str = match option_env!("NISCORD_UPDATE_REPO") {
    Some(repo) => repo,
    None => "nickolasdeluca/niscord",
};
const EXE_ASSET: &str = "niscord.exe";
const SUMS_ASSET: &str = "SHA256SUMS.txt";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Wait a little after start-up so checking doesn't compete with connecting.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const MAX_EXE_BYTES: u64 = 200 * 1024 * 1024;
const USER_AGENT: &str = concat!("Niscord/", env!("CARGO_PKG_VERSION"));

/// A downloaded, verified update waiting for a restart.
#[derive(Debug, Clone)]
pub struct Staged {
    pub version: String,
    path: PathBuf,
}

/// Major, minor, patch. Pre-release suffixes ("-beta") are ignored.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let core = text.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next().unwrap_or(Some(0))?, parts.next().unwrap_or(Some(0))?);
    parts.next().is_none().then_some(version)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(c), Some(r)) if c > r)
}

/// The SHA-256 listed for `file` in a `sha256sum` output.
fn checksum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (name == file && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(120))).build().into()
}

struct Release {
    version: String,
    exe_url: String,
    sums_url: String,
}

/// The latest published release, if there is one at all.
fn latest_release(agent: &ureq::Agent) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut response =
        match agent.get(&url).header("User-Agent", USER_AGENT).header("Accept", "application/vnd.github+json").call() {
            Ok(response) => response,
            // No release published yet.
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(err) => return Err(err).context("asking GitHub for the latest release"),
        };
    let json: serde_json::Value =
        serde_json::from_str(&response.body_mut().read_to_string().context("reading the release")?)?;
    let version = json["tag_name"].as_str().context("release has no tag")?.trim_start_matches('v').to_owned();
    let asset = |name: &str| {
        json["assets"].as_array()?.iter().find(|a| a["name"] == name)?["browser_download_url"]
            .as_str()
            .map(str::to_owned)
    };
    let (Some(exe_url), Some(sums_url)) = (asset(EXE_ASSET), asset(SUMS_ASSET)) else {
        tracing::debug!(version, "release has no {EXE_ASSET} and {SUMS_ASSET}");
        return Ok(None);
    };
    Ok(Some(Release { version, exe_url, sums_url }))
}

/// Where the update is downloaded to: next to the exe, so moving it into
/// place is a rename on the same drive.
fn staging_path(exe: &Path) -> PathBuf {
    exe.with_extension("exe.new")
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

fn download(agent: &ureq::Agent, release: &Release, exe: &Path) -> Result<Staged> {
    let sums = agent
        .get(&release.sums_url)
        .header("User-Agent", USER_AGENT)
        .call()
        .context("downloading checksums")?
        .body_mut()
        .read_to_string()?;
    let expected = checksum_for(&sums, EXE_ASSET).context("no checksum for niscord.exe in the release")?;

    let path = staging_path(exe);
    let response = agent.get(&release.exe_url).header("User-Agent", USER_AGENT).call().context("downloading")?;
    let mut reader = response.into_body().into_with_config().limit(MAX_EXE_BYTES).reader();
    let mut file = File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer).context("downloading")?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        file.write_all(&buffer[..n])?;
    }
    file.sync_all()?;
    drop(file);

    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        bail!("the download doesn't match the release checksum");
    }
    Ok(Staged { version: release.version.clone(), path })
}

/// One check: `Some` when a newer version was downloaded and is ready.
fn check_once(exe: &Path) -> Result<Option<Staged>> {
    let agent = agent();
    let Some(release) = latest_release(&agent)? else { return Ok(None) };
    if !is_newer(&release.version, env!("CARGO_PKG_VERSION")) {
        tracing::debug!(latest = release.version, "Niscord is up to date");
        return Ok(None);
    }
    tracing::info!(version = release.version, "downloading update");
    download(&agent, &release, exe).map(Some)
}

/// Whether this copy should update itself: not in development builds, and
/// not when disabled with `NISCORD_NO_UPDATE=1`.
fn enabled() -> bool {
    !cfg!(debug_assertions) && std::env::var_os("NISCORD_NO_UPDATE").is_none()
}

/// Check now and every few hours on a background thread; `on_ready` runs
/// (on that thread) once an update is downloaded.
pub fn start(on_ready: impl Fn(Staged) + Send + 'static) {
    let Ok(exe) = std::env::current_exe() else { return };
    // Leftover from the previous update, if any.
    let _ = std::fs::remove_file(old_path(&exe));
    let _ = std::fs::remove_file(staging_path(&exe));
    if !enabled() {
        return;
    }
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        std::thread::sleep(FIRST_CHECK_DELAY);
        loop {
            match check_once(&exe) {
                Ok(Some(staged)) => {
                    tracing::info!(version = staged.version, "update ready");
                    on_ready(staged);
                    return;
                }
                Ok(None) => {}
                Err(err) => tracing::warn!("update check failed: {err:#}"),
            }
            std::thread::sleep(CHECK_INTERVAL);
        }
    });
}

/// Put the staged exe in place of `exe`, keeping the old one aside.
fn swap_in(staged: &Path, exe: &Path) -> Result<()> {
    let old = old_path(exe);
    let _ = std::fs::remove_file(&old);
    // A running exe can't be overwritten or deleted, but it can be renamed.
    std::fs::rename(exe, &old).context("moving the running version aside")?;
    if let Err(err) = std::fs::rename(staged, exe) {
        // Put things back so the app still starts next time.
        let _ = std::fs::rename(&old, exe);
        return Err(err).context("moving the update into place");
    }
    Ok(())
}

/// Install the staged update and start it. The caller should exit next.
pub fn apply(staged: &Staged) -> Result<()> {
    let exe = std::env::current_exe()?;
    swap_in(&staged.path, &exe)?;
    std::process::Command::new(&exe).spawn().context("starting the new version")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"), "10 > 9, not as text");
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"), "unparsable never wins");
        assert_eq!(parse_version("v1.2.3+build"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn checksums_are_found_by_file_name() {
        let hash = "a".repeat(64);
        let sums = format!("{}  niscord-server-windows-x86_64.exe\n{hash}  niscord.exe\n", "b".repeat(64));
        assert_eq!(checksum_for(&sums, "niscord.exe"), Some(hash.clone()));
        assert_eq!(checksum_for(&format!("{hash} *niscord.exe"), "niscord.exe"), Some(hash), "binary mode marker");
        assert_eq!(checksum_for(&sums, "missing.exe"), None);
        assert_eq!(checksum_for("short  niscord.exe", "niscord.exe"), None);
    }

    #[test]
    fn swap_replaces_the_exe_and_keeps_the_old_one() {
        let dir = std::env::temp_dir().join(format!("niscord-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("niscord.exe");
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(staging_path(&exe), "new").unwrap();

        swap_in(&staging_path(&exe), &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old_path(&exe)).unwrap(), "old");
        assert!(!staging_path(&exe).exists());

        // A failed swap leaves the current exe where it was.
        assert!(swap_in(&dir.join("does-not-exist"), &exe).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Talks to GitHub: `cargo test -p niscord -- --ignored latest_release`.
    #[test]
    #[ignore]
    fn latest_release_query_works() {
        match latest_release(&agent()).unwrap() {
            Some(release) => println!("latest {} at {}", release.version, release.exe_url),
            None => println!("no release with {EXE_ASSET} yet"),
        }
    }
}
