//! H.264 encoding and decoding.
//!
//! Encoding uses the GPU's hardware encoder when there is one (through Media
//! Foundation, see `mf_encoder`), and OpenH264 in software otherwise or if
//! the hardware fails. Both produce Constrained Baseline, which OpenH264
//! decodes on the viewer's side.

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};

use crate::{Error, Result, RgbaImage};

/// How often to send a full keyframe even when nobody asks for one, so a
/// viewer that lost packets recovers on its own.
pub(crate) const KEYFRAME_INTERVAL_SECS: u32 = 5;

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

/// Whether an Annex B access unit contains a NAL unit of this type
/// (5 = IDR slice, 7 = SPS).
pub(crate) fn has_nal(annex_b: &[u8], nal_type: u8) -> bool {
    annex_b.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == nal_type)
}

/// Which encoder to use. Hardware falls back to software if it's missing or
/// fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPreference {
    Hardware,
    Software,
}

impl EncoderPreference {
    /// `NISCORD_ENCODER=software` forces OpenH264 (for comparison/debugging).
    pub fn from_env() -> Self {
        match std::env::var("NISCORD_ENCODER") {
            Ok(v) if v.eq_ignore_ascii_case("software") => Self::Software,
            _ => Self::Hardware,
        }
    }
}

/// A hardware encoder takes ~0.5 s to open and only works at one frame
/// size. It is opened on a helper thread once the size has been steady this
/// long, and software covers the frames in between (start-up, resizing).
#[cfg(windows)]
const STEADY_SIZE: std::time::Duration = std::time::Duration::from_millis(300);
/// Give up on hardware after this many sizes failed to open.
#[cfg(windows)]
const MAX_OPEN_FAILURES: u32 = 3;

#[cfg(windows)]
enum Hardware {
    Idle,
    Opening { size: (u32, u32), result: std::sync::mpsc::Receiver<Result<crate::mf_encoder::MfEncoder>> },
    Active(crate::mf_encoder::MfEncoder),
    Unavailable,
}

#[cfg(windows)]
impl Hardware {
    fn start_opening(size: (u32, u32), settings: EncoderSettings) -> Self {
        let (tx, result) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("open-encoder".into()).spawn(move || {
            let _ = tx.send(crate::mf_encoder::MfEncoder::new(size.0, size.1, settings));
        });
        match spawned {
            Ok(_) => Self::Opening { size, result },
            Err(_) => Self::Unavailable,
        }
    }
}

/// Closing a hardware session can take a while too; don't make the encoder
/// thread wait for it.
#[cfg(windows)]
fn close_in_background(encoder: crate::mf_encoder::MfEncoder) {
    let _ = std::thread::Builder::new().name("close-encoder".into()).spawn(move || drop(encoder));
}

pub struct VideoEncoder {
    settings: EncoderSettings,
    #[cfg(windows)]
    hardware: Hardware,
    /// Frame size and since when it hasn't changed.
    #[cfg(windows)]
    size: Option<((u32, u32), std::time::Instant)>,
    #[cfg(windows)]
    open_failures: u32,
    /// The last size hardware wouldn't open (e.g. below its minimum).
    #[cfg(windows)]
    failed_size: Option<(u32, u32)>,
    /// Which encoder made the last frame: switching needs a keyframe.
    last_was_hardware: bool,
    software: Option<SoftwareEncoder>,
    /// Scratch space for cropping odd-sized frames.
    even: Vec<u8>,
    force_keyframe: bool,
}

impl VideoEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        Self::with_preference(settings, EncoderPreference::from_env())
    }

    pub fn with_preference(settings: EncoderSettings, preference: EncoderPreference) -> Result<Self> {
        let software = match preference {
            EncoderPreference::Software => Some(SoftwareEncoder::new(settings)?),
            EncoderPreference::Hardware => None,
        };
        Ok(Self {
            settings,
            #[cfg(windows)]
            hardware: if software.is_some() { Hardware::Unavailable } else { Hardware::Idle },
            #[cfg(windows)]
            size: None,
            #[cfg(windows)]
            open_failures: 0,
            #[cfg(windows)]
            failed_size: None,
            last_was_hardware: false,
            software,
            even: Vec::new(),
            force_keyframe: false,
        })
    }

    /// The target bitrate, in bits per second.
    pub fn bitrate(&self) -> u32 {
        self.settings.bitrate_bps
    }

    /// Name of the encoder in use, once the first frame went through.
    pub fn backend(&self) -> Option<&str> {
        #[cfg(windows)]
        if let Hardware::Active(hw) = &self.hardware {
            return Some(hw.name());
        }
        self.software.as_ref().map(|_| "OpenH264")
    }

    pub fn is_hardware(&self) -> bool {
        #[cfg(windows)]
        if let Hardware::Active(_) = self.hardware {
            return true;
        }
        false
    }

    /// Use software from now on (e.g. the hardware's output didn't decode).
    pub fn disable_hardware(&mut self) {
        #[cfg(windows)]
        if let Hardware::Active(hw) = std::mem::replace(&mut self.hardware, Hardware::Unavailable) {
            tracing::warn!(encoder = hw.name(), "hardware encoder disabled, using OpenH264");
            close_in_background(hw);
        }
    }

    /// Make the next frame a keyframe (a new viewer joined, or one lost data).
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Change the target bitrate without restarting the stream (follows the
    /// network's bandwidth estimate).
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.settings.bitrate_bps = bps;
        #[cfg(windows)]
        if let Hardware::Active(hw) = &mut self.hardware {
            return hw.set_bitrate(bps);
        }
        match &mut self.software {
            Some(software) => software.set_bitrate(bps),
            None => Ok(()),
        }
    }

    /// Encode one frame. Returns `None` when rate control skipped it (or the
    /// hardware will hand it out on a later call).
    /// Frame sizes may change between calls (e.g. a resized window).
    pub fn encode(&mut self, image: &RgbaImage, timestamp_ms: u64) -> Result<Option<EncodedFrame>> {
        // 4:2:0 chroma needs even dimensions; drop a trailing row/column.
        let width = image.width & !1;
        let height = image.height & !1;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let mut even = std::mem::take(&mut self.even);
        let pixels = if (width, height) == (image.width, image.height) {
            &image.pixels[..]
        } else {
            crop(&image.pixels, image.width, width, height, &mut even);
            &even[..]
        };
        let result = self.encode_even(pixels, width, height, timestamp_ms);
        self.even = even;
        result
    }

    fn encode_even(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> Result<Option<EncodedFrame>> {
        #[cfg(windows)]
        if let Some(result) = self.encode_hardware(pixels, width, height, timestamp_ms) {
            self.last_was_hardware = true;
            return result;
        }

        // Coming from hardware, the viewer's decoder has none of software's
        // reference frames.
        let keyframe = std::mem::take(&mut self.force_keyframe) | std::mem::take(&mut self.last_was_hardware);
        let software = match &mut self.software {
            Some(software) => software,
            slot => slot.insert(SoftwareEncoder::new(self.settings)?),
        };
        software.encode(pixels, width, height, timestamp_ms, keyframe)
    }

    /// `None` when hardware isn't ready for this frame, so software takes it.
    #[cfg(windows)]
    fn encode_hardware(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> Option<Result<Option<EncodedFrame>>> {
        use std::time::Instant;

        let size = (width, height);
        let steady = match self.size {
            Some((last, since)) if last == size => since.elapsed() >= STEADY_SIZE,
            // The very first frame: no reason to wait.
            None => {
                self.size = Some((size, Instant::now()));
                true
            }
            Some(_) => {
                self.size = Some((size, Instant::now()));
                false
            }
        };

        // A session is for one size: retire it when the frames change size.
        match std::mem::replace(&mut self.hardware, Hardware::Idle) {
            Hardware::Active(hw) if hw.size() != size => close_in_background(hw),
            Hardware::Opening { size: opening, result } if opening != size => {
                let _ = std::thread::Builder::new().spawn(move || drop(result.recv()));
            }
            Hardware::Opening { size: opening, result } => {
                self.hardware = match result.try_recv() {
                    Ok(Ok(mut hw)) => {
                        // The bitrate may have moved while it was opening.
                        let _ = hw.set_bitrate(self.settings.bitrate_bps);
                        tracing::info!(encoder = hw.name(), width, height, "switched to hardware encoding");
                        Hardware::Active(hw)
                    }
                    Ok(Err(err)) => {
                        self.open_failures += 1;
                        tracing::info!(width, height, "no hardware encoder for this size, using OpenH264: {err}");
                        self.failed_size = Some(size);
                        if self.open_failures >= MAX_OPEN_FAILURES { Hardware::Unavailable } else { Hardware::Idle }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => Hardware::Opening { size: opening, result },
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Hardware::Unavailable,
                };
            }
            other => self.hardware = other,
        }
        if let Hardware::Idle = self.hardware
            && steady
            && self.failed_size != Some(size)
        {
            self.hardware = Hardware::start_opening(size, self.settings);
        }

        let Hardware::Active(hw) = &mut self.hardware else { return None };
        // Switching from software: start the hardware stream with a keyframe
        // (its first frame is one anyway; this covers a later switch back).
        let keyframe = std::mem::take(&mut self.force_keyframe) | !self.last_was_hardware;
        let fill = |nv12: &mut [u8]| crate::color::convert_into(pixels, width as usize, height as usize, nv12);
        match hw.encode(fill, timestamp_ms, keyframe) {
            Ok(frame) => Some(Ok(frame)),
            Err(err) => {
                tracing::warn!(encoder = hw.name(), "hardware encoder failed, switching to OpenH264: {err}");
                if let Hardware::Active(hw) = std::mem::replace(&mut self.hardware, Hardware::Unavailable) {
                    close_in_background(hw);
                }
                None
            }
        }
    }
}

/// OpenH264, the software fallback.
struct SoftwareEncoder {
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
}

impl SoftwareEncoder {
    fn new(settings: EncoderSettings) -> Result<Self> {
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
        Ok(Self { encoder, yuv: None })
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
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

    fn encode(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
        keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        let rgba = RgbaSliceU8::new(pixels, (width as usize, height as usize));
        let yuv = match &mut self.yuv {
            Some(yuv) if yuv.dimensions() == (width as usize, height as usize) => {
                yuv.read_rgba8(rgba);
                yuv
            }
            slot => slot.insert(YUVBuffer::from_rgba8_source(rgba)),
        };

        if keyframe {
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
        let mut encoder = VideoEncoder::with_preference(
            EncoderSettings { fps: 30, bitrate_bps: 2_000_000 },
            EncoderPreference::Software,
        )
        .unwrap();
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
        let mut encoder = VideoEncoder::with_preference(
            EncoderSettings { fps: 30, bitrate_bps: 2_000_000 },
            EncoderPreference::Software,
        )
        .unwrap();
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
        let mut encoder = VideoEncoder::with_preference(
            EncoderSettings { fps: 30, bitrate_bps: 2_000_000 },
            EncoderPreference::Software,
        )
        .unwrap();
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
        let mut encoder = VideoEncoder::with_preference(
            EncoderSettings { fps: 30, bitrate_bps: 4_000_000 },
            EncoderPreference::Software,
        )
        .unwrap();
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
