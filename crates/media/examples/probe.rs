//! Lists capture sources and times a thumbnail grab of each.
//! cargo run -p niscord-media --example probe

use std::time::{Duration, Instant};

fn main() {
    println!("border can be hidden: {}", niscord_media::can_hide_capture_border());
    let t = Instant::now();
    let sources = niscord_media::list_sources();
    println!("{} sources listed in {:?}", sources.len(), t.elapsed());
    for source in sources {
        let t = Instant::now();
        let result = niscord_media::capture_thumbnail(source.id, 320, 180, Duration::from_millis(1500));
        let outcome = match result {
            Ok(img) => format!("{}x{}", img.width, img.height),
            Err(err) => err.to_string(),
        };
        println!(
            "{:?} {:?} [{}] {}x{} min={} -> {outcome} in {:?}",
            source.kind,
            source.title,
            source.detail,
            source.width,
            source.height,
            source.minimized,
            t.elapsed()
        );
    }
}
