//! Background thumbnail grabbing for the source picker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use niscord_media::{RgbaImage, SourceId};

pub const WIDTH: u32 = 320;
pub const HEIGHT: u32 = 180;
/// Each grab spins up a capture session (~300 ms), so run a few at once.
const WORKERS: usize = 4;
const GRAB_TIMEOUT: Duration = Duration::from_millis(1500);
const REFRESH_EVERY: Duration = Duration::from_secs(3);

/// Grabs thumbnails until dropped.
pub struct Job {
    stop: Arc<AtomicBool>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Grab a thumbnail of every `(key, source)` and hand each to `deliver`.
/// With `live`, repeat every few seconds until the job is dropped.
pub fn spawn(
    targets: Vec<(i32, SourceId)>,
    live: bool,
    deliver: impl Fn(i32, RgbaImage) + Send + Sync + 'static,
) -> Job {
    let stop = Arc::new(AtomicBool::new(false));
    let job = Job { stop: stop.clone() };
    let spawned = std::thread::Builder::new().name("thumbnails".into()).spawn(move || {
        loop {
            let next = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..WORKERS.min(targets.len()) {
                    scope.spawn(|| {
                        while !stop.load(Ordering::Relaxed) {
                            let Some(&(key, id)) = targets.get(next.fetch_add(1, Ordering::Relaxed)) else { return };
                            match niscord_media::capture_thumbnail(id, WIDTH, HEIGHT, GRAB_TIMEOUT) {
                                Ok(image) if !stop.load(Ordering::Relaxed) => deliver(key, image),
                                Ok(_) => return,
                                Err(err) => tracing::debug!(?id, "thumbnail failed: {err}"),
                            }
                        }
                    });
                }
            });
            if !live {
                return;
            }
            let mut waited = Duration::ZERO;
            while waited < REFRESH_EVERY {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
                waited += Duration::from_millis(100);
            }
        }
    });
    if let Err(err) = spawned {
        tracing::warn!("could not start thumbnail thread: {err}");
    }
    job
}
