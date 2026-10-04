//! Compares the hardware and software encoders on synthetic moving content
//! (scrolling stripes and noise-like texture, so every frame differs).
//!
//! cargo run --release -p niscord-media --example bench_encoders

use std::time::{Duration, Instant};

use niscord_media::RgbaImage;
use niscord_media::video::{EncoderPreference, EncoderSettings, VideoDecoder, VideoEncoder, suggested_bitrate};

fn frame(width: u32, height: u32, t: u32) -> RgbaImage {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let sx = x + t * 6;
            let v = ((sx / 40) % 2) as u8 * 120 + ((sx ^ y).wrapping_mul(2654435761) >> 28) as u8 * 4;
            pixels.extend_from_slice(&[v, (y + t * 3) as u8, (sx / 3) as u8, 255]);
        }
    }
    RgbaImage { width, height, pixels }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

fn main() {
    for (width, height, fps) in [(1920, 1080, 60), (2560, 1440, 60)] {
        let frames: Vec<RgbaImage> = (0..120).map(|t| frame(width, height, t)).collect();
        let bitrate_bps = suggested_bitrate(width, height, fps);
        println!("{width}x{height} @ {fps} fps, {} kbps target", bitrate_bps / 1000);
        let mut nv12 = Vec::new();
        let started = Instant::now();
        for input in &frames {
            niscord_media::color::rgba_to_nv12(&input.pixels, width as usize, height as usize, &mut nv12);
        }
        println!("  RGBA -> NV12 conversion: {:.2} ms", started.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64);
        for preference in [EncoderPreference::Hardware, EncoderPreference::Software] {
            let mut encoder = VideoEncoder::with_preference(EncoderSettings { fps, bitrate_bps }, preference).unwrap();
            let mut decoder = VideoDecoder::new().unwrap();
            // Hardware opens in the background; measure it once it's in use.
            let warm_up = Instant::now();
            while preference == EncoderPreference::Hardware
                && !encoder.is_hardware()
                && warm_up.elapsed() < Duration::from_secs(3)
            {
                if let Some(out) = encoder.encode(&frames[0], 0).unwrap() {
                    let _ = decoder.decode(&out.data);
                }
                std::thread::sleep(Duration::from_millis(16));
            }
            let (mut times, mut decode_times, mut bytes, mut n, mut quality) = (Vec::new(), Vec::new(), 0, 0, 0.0);
            for (t, input) in frames.iter().enumerate() {
                let started = Instant::now();
                let Some(out) = encoder.encode(input, t as u64 * 1000 / fps as u64).unwrap() else { continue };
                // The first frames include opening the encoder.
                if t >= 5 {
                    times.push(started.elapsed());
                }
                bytes += out.data.len();
                n += 1;
                let decode_started = Instant::now();
                let decoded = decoder.decode(&out.data);
                decode_times.push(decode_started.elapsed());
                if let Ok(Some(picture)) = decoded {
                    quality += psnr(&picture.pixels, &input.pixels);
                }
            }
            let n = n.max(1);
            times.sort();
            decode_times.sort();
            let quantile = |times: &[Duration], q: f64| {
                times.get(((times.len() as f64 - 1.0) * q) as usize).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
            };
            let ms = |q: f64| quantile(&times, q);
            println!(
                "  {:<26} {n:>3} frames, encode median {:>5.1} ms (p95 {:>5.1}), decode median {:>5.1} ms, {:>6.0} kbps, PSNR {:.1} dB",
                encoder.backend().unwrap_or("?"),
                ms(0.5),
                ms(0.95),
                quantile(&decode_times, 0.5),
                bytes as f64 * 8.0 * fps as f64 / n as f64 / 1000.0,
                quality / n as f64,
            );
        }
    }
}
