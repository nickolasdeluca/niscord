//! Compares bitrate adherence and picture quality (PSNR) of OpenH264
//! modes on a capture of the primary screen, scrolled like a document.
//! This is how the encoder settings in `video.rs` were chosen.
//!
//! cargo run --release -p niscord-media --example rc_probe
use std::time::Duration;

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};

fn main() {
    let screen = niscord_media::list_sources().into_iter().find(|s| s.primary).expect("primary screen");
    let shot = niscord_media::capture_thumbnail(screen.id, 1920, 1080, Duration::from_secs(2)).expect("capture");
    let (w, h) = (shot.width as usize & !1, shot.height as usize & !1);
    // Scroll the capture vertically by 4 px per frame.
    let frames: Vec<Vec<u8>> = (0..60)
        .map(|t| {
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                let sy = (y + t * 4) % h;
                out.extend_from_slice(&shot.pixels[sy * shot.width as usize * 4..][..w * 4]);
            }
            out
        })
        .collect();

    for usage in [UsageType::ScreenContentRealTime, UsageType::CameraVideoRealTime] {
        for skip in [false, true] {
            for target in [1_500_000u32, 4_000_000] {
                let config = EncoderConfig::new()
                    .usage_type(usage)
                    .rate_control_mode(RateControlMode::Bitrate)
                    .bitrate(BitRate::from_bps(target))
                    .max_frame_rate(FrameRate::from_hz(30.0))
                    .skip_frames(skip)
                    .adaptive_quantization(false)
                    .background_detection(false);
                let mut enc = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
                let mut dec = Decoder::new().unwrap();
                let (mut bytes, mut emitted, mut psnr_sum) = (0usize, 0, 0.0);
                let mut encode_time = std::time::Duration::ZERO;
                for (t, px) in frames.iter().enumerate() {
                    let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(px, (w, h)));
                    let began = std::time::Instant::now();
                    let s = enc.encode_at(&yuv, openh264::Timestamp::from_millis(t as u64 * 33)).unwrap();
                    encode_time += began.elapsed();
                    if matches!(s.frame_type(), FrameType::Skip | FrameType::Invalid) {
                        continue;
                    }
                    let data = s.to_vec();
                    bytes += data.len();
                    emitted += 1;
                    if let Ok(Some(out)) = dec.decode(&data) {
                        // PSNR on luma.
                        let (oy, sy) = (out.y(), out.strides().0);
                        let mut mse = 0.0;
                        for y in 0..h {
                            for x in 0..w {
                                let d = yuv.y()[y * w + x] as f64 - oy[y * sy + x] as f64;
                                mse += d * d;
                            }
                        }
                        mse /= (w * h) as f64;
                        psnr_sum += 10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10();
                    }
                }
                println!(
                    "{:<22} skip={:<5} target {:>4} kbps -> {:>5.0} kbps, {:>2}/60 frames, PSNR {:.1} dB, encode {:.1} ms",
                    format!("{usage:?}"),
                    skip,
                    target / 1000,
                    bytes as f64 * 8.0 / 2.0 / 1000.0,
                    emitted,
                    psnr_sum / emitted.max(1) as f64,
                    encode_time.as_secs_f64() * 1000.0 / 60.0
                );
            }
        }
    }
}
