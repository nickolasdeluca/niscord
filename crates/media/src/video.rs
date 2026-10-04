//! H.264 encoding and decoding (OpenH264, software).
//!
//! H.264 Constrained Baseline is what every WebRTC stack can carry, which
//! keeps the transport milestone simple. Hardware encoders can slot in behind
//! the same interface later.

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};

use crate::{Error, Result, RgbaImage};

/// How often to send a full keyframe even when nobody asks for one, so a
/// viewer that lost packets recovers on its own.
const KEYFRAME_INTERVAL_SECS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// Bitrate that looks good for screen content at this size and frame rate.
pub fn suggested_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    // ~0.07 bits per pixel per frame, kept within what home uplinks manage.
    let bps = width as f64 * height as f64 * fps as f64 * 0.07;
    (bps as u32).clamp(1_000_000, 8_000_000)
}

/// One encoded access unit, Annex B formatted (NAL units with start codes).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
}

pub struct VideoEncoder {
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    /// Scratch space for cropping odd-sized frames.
    even: Vec<u8>,
    force_keyframe: bool,
}

impl VideoEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        let config = EncoderConfig::new()
            // Despite the name, the camera mode suits screen sharing better
            // here: OpenH264's screen-content rate control largely ignores
            // the bitrate target (5+ Mbps when asked for 0.5 on busy content)
            // and gave lower quality on scrolling text (~48 vs ~56 dB PSNR).
            // See examples/rc_probe.rs.
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.fps as f32))
            // When even the coarsest quantizer can't fit the target, drop a
            // frame rather than overflow the network link (which would lose
            // packets and freeze the picture until the next keyframe).
            .skip_frames(true)
            .intra_frame_period(IntraFramePeriod::from_num_frames(settings.fps * KEYFRAME_INTERVAL_SECS));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config).map_err(codec_error)?;
        Ok(Self { encoder, yuv: None, even: Vec::new(), force_keyframe: false })
    }

    /// Make the next frame a keyframe (a new viewer joined, or one lost data).
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Change the target bitrate without restarting the stream (follows the
    /// network's bandwidth estimate).
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        use openh264_sys2::{ENCODER_OPTION_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};
        let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: bps.min(i32::MAX as u32) as i32 };
        // SAFETY: the encoder is initialised (created in `new`), and
        // ENCODER_OPTION_BITRATE takes a pointer to an SBitrateInfo that only
        // needs to live for the duration of the call.
        let rc = unsafe { self.encoder.raw_api().set_option(ENCODER_OPTION_BITRATE, (&raw mut info).cast()) };
        if rc != 0 {
            return Err(Error::Codec(format!("setting bitrate to {bps} failed ({rc})")));
        }
        Ok(())
    }

    /// Encode one frame. Returns `None` when rate control skipped it.
    /// Frame sizes may change between calls (e.g. a resized window).
    pub fn encode(&mut self, image: &RgbaImage, timestamp_ms: u64) -> Result<Option<EncodedFrame>> {
        // 4:2:0 chroma needs even dimensions; drop a trailing row/column.
        let width = image.width & !1;
        let height = image.height & !1;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let pixels = if (width, height) == (image.width, image.height) {
            &image.pixels[..]
        } else {
            crop(&image.pixels, image.width, width, height, &mut self.even);
            &self.even[..]
        };

        let rgba = RgbaSliceU8::new(pixels, (width as usize, height as usize));
        let yuv = match &mut self.yuv {
            Some(yuv) if yuv.dimensions() == (width as usize, height as usize) => {
                yuv.read_rgba8(rgba);
                yuv
            }
            slot => slot.insert(YUVBuffer::from_rgba8_source(rgba)),
        };

        if std::mem::take(&mut self.force_keyframe) {
            self.encoder.force_intra_frame();
        }
        let stream =
            self.encoder.encode_at(yuv, openh264::Timestamp::from_millis(timestamp_ms)).map_err(codec_error)?;
        let keyframe = match stream.frame_type() {
            FrameType::IDR | FrameType::I => true,
            FrameType::P | FrameType::IPMixed => false,
            FrameType::Skip | FrameType::Invalid => return Ok(None),
        };
        let data = stream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame { data, keyframe, width, height }))
    }
}

fn crop(src: &[u8], src_width: u32, width: u32, height: u32, out: &mut Vec<u8>) {
    out.clear();
    let src_row = src_width as usize * 4;
    let row = width as usize * 4;
    for y in 0..height as usize {
        out.extend_from_slice(&src[y * src_row..y * src_row + row]);
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        // Real-time: output every frame as soon as it is decoded.
        let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), config).map_err(codec_error)?;
        Ok(Self { decoder })
    }

    /// Decode one access unit. `None` means the decoder needs more data
    /// (e.g. it is waiting for a keyframe after joining or after loss).
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<RgbaImage>> {
        let Some(yuv) = self.decoder.decode(data).map_err(codec_error)? else { return Ok(None) };
        let (width, height) = yuv.dimensions();
        let mut pixels = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut pixels);
        Ok(Some(RgbaImage { width: width as u32, height: height as u32, pixels }))
    }
}

fn codec_error(err: openh264::Error) -> Error {
    Error::Codec(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with a moving gradient, so consecutive frames differ.
    fn frame(width: u32, height: u32, t: u32) -> RgbaImage {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[(x + t * 8) as u8, y as u8, ((x + y) / 2) as u8, 255]);
            }
        }
        RgbaImage { width, height, pixels }
    }

    #[test]
    fn round_trip_preserves_size_and_roughly_the_picture() {
        let mut encoder = VideoEncoder::new(EncoderSettings { fps: 30, bitrate_bps: 2_000_000 }).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let mut decoded = 0;
        let mut keyframes = 0;
        for t in 0..10 {
            let input = frame(320, 180, t);
            let Some(encoded) = encoder.encode(&input, t as u64 * 33).unwrap() else { continue };
            keyframes += encoded.keyframe as u32;
            if let Some(output) = decoder.decode(&encoded.data).unwrap() {
                assert_eq!((output.width, output.height), (320, 180));
                // Lossy, but the average error should be small.
                let diff: u64 = input
                    .pixels
                    .iter()
                    .zip(&output.pixels)
                    .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
                    .sum();
                let mean = diff as f64 / input.pixels.len() as f64;
                assert!(mean < 12.0, "mean abs error {mean}");
                decoded += 1;
            }
        }
        assert!(decoded >= 5, "only {decoded} frames decoded");
        assert_eq!(keyframes, 1, "only the first frame should be a keyframe");
    }

    #[test]
    fn odd_sizes_are_cropped_and_size_changes_work() {
        let mut encoder = VideoEncoder::new(EncoderSettings { fps: 30, bitrate_bps: 2_000_000 }).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();

        let encoded = encoder.encode(&frame(321, 181, 0), 0).unwrap().unwrap();
        assert_eq!((encoded.width, encoded.height), (320, 180));
        let out = decoder.decode(&encoded.data).unwrap().unwrap();
        assert_eq!((out.width, out.height), (320, 180));

        // A resize restarts the stream with a keyframe the decoder follows.
        let encoded = encoder.encode(&frame(200, 100, 1), 33).unwrap().unwrap();
        assert!(encoded.keyframe);
        let out = decoder.decode(&encoded.data).unwrap().unwrap();
        assert_eq!((out.width, out.height), (200, 100));
    }

    #[test]
    fn requested_keyframe_is_honoured() {
        let mut encoder = VideoEncoder::new(EncoderSettings { fps: 30, bitrate_bps: 2_000_000 }).unwrap();
        assert!(encoder.encode(&frame(320, 180, 0), 0).unwrap().unwrap().keyframe);
        let second = encoder.encode(&frame(320, 180, 1), 33).unwrap();
        assert!(second.is_none_or(|f| !f.keyframe));
        encoder.request_keyframe();
        assert!(encoder.encode(&frame(320, 180, 2), 66).unwrap().unwrap().keyframe);
    }

    #[test]
    fn bitrate_can_change_mid_stream() {
        /// Busy moving content, so the encoder uses every bit it is allowed.
        fn noisy(t: u32) -> RgbaImage {
            let mut pixels = Vec::with_capacity(640 * 360 * 4);
            for y in 0..360u32 {
                for x in 0..640u32 {
                    let v = ((x + t * 13) ^ (y * 3 + t * 7)) as u8;
                    pixels.extend_from_slice(&[v, (x / 5 + t * 4) as u8, (y / 3) as u8 ^ v, 255]);
                }
            }
            RgbaImage { width: 640, height: 360, pixels }
        }
        fn bytes_for(encoder: &mut VideoEncoder, from: u32) -> usize {
            (from..from + 30)
                .filter_map(|t| encoder.encode(&noisy(t), t as u64 * 33).unwrap())
                .map(|f| f.data.len())
                .sum()
        }
        let mut encoder = VideoEncoder::new(EncoderSettings { fps: 30, bitrate_bps: 4_000_000 }).unwrap();
        let high = bytes_for(&mut encoder, 0);
        encoder.set_bitrate(500_000).unwrap();
        let _settle = bytes_for(&mut encoder, 30);
        let low = bytes_for(&mut encoder, 60);
        assert!(low * 2 < high, "lowering the bitrate had no effect: {high} -> {low} bytes");
    }

    #[test]
    fn suggested_bitrates_are_sane() {
        assert_eq!(suggested_bitrate(320, 180, 15), 1_000_000);
        let b1080p30 = suggested_bitrate(1920, 1080, 30);
        assert!((3_000_000..6_000_000).contains(&b1080p30));
        assert_eq!(suggested_bitrate(3840, 2160, 60), 8_000_000);
    }
}
