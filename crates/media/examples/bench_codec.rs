//! Captures the primary screen for a few seconds and times each pipeline
//! stage: capture -> encode -> decode.
//!
//! cargo run --release -p niscord-media --example bench_codec -- [width height fps]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use niscord_media::video::{EncoderSettings, VideoDecoder, VideoEncoder, suggested_bitrate};
use niscord_media::{Capture, CaptureOptions, FrameSink, RgbaImage, SourceKind};

struct Sink(mpsc::SyncSender<(Instant, RgbaImage)>);

impl FrameSink for Sink {
    fn frame(&mut self, image: RgbaImage) {
        let _ = self.0.try_send((Instant::now(), image));
    }
    fn closed(&mut self) {}
}

fn main() {
    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (max_w, max_h, fps) = match args[..] {
        [w, h, f] => (w, h, f),
        _ => (1920, 1080, 30),
    };
    let source = niscord_media::list_sources()
        .into_iter()
        .filter(|s| s.kind == SourceKind::Screen)
        .max_by_key(|s| s.primary)
        .expect("no screen");
    println!("capturing {} at up to {max_w}x{max_h} @ {fps} fps", source.title);

    let (tx, rx) = mpsc::sync_channel(2);
    let options = CaptureOptions { max_fps: fps, max_width: max_w, max_height: max_h, show_cursor: true };
    let capture = Capture::start(source.id, options, Sink(tx)).unwrap();

    let mut encoder = None;
    let mut decoder = VideoDecoder::new().unwrap();
    let (mut n, mut bytes, mut enc_t, mut dec_t, mut lat_t) =
        (0u32, 0usize, Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        let Ok((captured, image)) = rx.recv_timeout(Duration::from_millis(200)) else { continue };
        let encoder = encoder.get_or_insert_with(|| {
            let bitrate_bps = suggested_bitrate(image.width, image.height, fps);
            println!("frame {}x{}, bitrate {} kbps", image.width, image.height, bitrate_bps / 1000);
            VideoEncoder::new(EncoderSettings { fps, bitrate_bps }).unwrap()
        });
        let t = Instant::now();
        let Some(frame) = encoder.encode(&image, start.elapsed().as_millis() as u64).unwrap() else { continue };
        enc_t += t.elapsed();
        let t = Instant::now();
        decoder.decode(&frame.data).unwrap();
        dec_t += t.elapsed();
        lat_t += captured.elapsed();
        bytes += frame.data.len();
        n += 1;
    }
    capture.stop();
    let secs = start.elapsed().as_secs_f64();
    let n = n.max(1);
    println!(
        "{n} frames ({:.1} fps): encode {:.1} ms, decode {:.1} ms, capture->decoded {:.1} ms, {:.0} kbps",
        n as f64 / secs,
        enc_t.as_secs_f64() * 1000.0 / n as f64,
        dec_t.as_secs_f64() * 1000.0 / n as f64,
        lat_t.as_secs_f64() * 1000.0 / n as f64,
        bytes as f64 * 8.0 / secs / 1000.0,
    );
}
