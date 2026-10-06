//! Hardware H.264 encoding through Media Foundation.
//!
//! GPU vendors ship their encoders (NVIDIA NVENC, AMD AMF, Intel Quick Sync)
//! as Media Foundation transforms, so one code path reaches all of them
//! without vendor SDKs. These transforms are asynchronous: they announce,
//! through events, when they want input and when output is ready. Niscord
//! drives them synchronously from the encoder thread, polling those events.
//!
//! Output is Constrained Baseline without B-frames, like OpenH264's, so every
//! viewer can decode it with OpenH264 regardless of who encoded it.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use crate::color::nv12_len;
use crate::video::{EncodedFrame, EncoderSettings, KEYFRAME_INTERVAL_SECS, has_nal};
use crate::{Error, Result};

/// How long to wait for the encoder to want input or produce output before
/// giving up on this frame. Hardware takes a few milliseconds.
const INPUT_TIMEOUT: Duration = Duration::from_millis(500);
const OUTPUT_WAIT: Duration = Duration::from_millis(40);
const POLL: Duration = Duration::from_micros(500);
const MIN_QP: u32 = 12;

fn mf_error(context: &str, err: windows::core::Error) -> Error {
    Error::Codec(format!("{context}: {err}"))
}

fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, |v| v.ulVal = value)
}

fn variant_bool(value: bool) -> VARIANT {
    variant(VT_BOOL, |v| v.boolVal = if value { VARIANT_TRUE } else { Default::default() })
}

fn variant(vt: VARENUM, set: impl FnOnce(&mut windows::Win32::System::Variant::VARIANT_0_0_0)) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: writing plain values into a zeroed VARIANT's tag and union.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = vt;
        set(&mut inner.Anonymous);
    }
    v
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Friendly names of the hardware H.264 encoders on this machine, best first.
#[cfg_attr(not(test), allow(dead_code))]
pub fn hardware_encoders() -> Vec<String> {
    init_thread();
    enumerate().map(|list| list.iter().map(friendly_name).collect()).unwrap_or_default()
}

fn init_thread() {
    // SAFETY: plain COM/MF initialisation; both are reference counted and a
    // thread that already joined the MTA just gets S_FALSE.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    }
}

fn enumerate() -> windows::core::Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates `list` with CoTaskMemAlloc and hands us
    // `count` owned references, which we take out before freeing it.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )?;
        let activates = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
        CoTaskMemFree(Some(list as *const _));
        Ok(activates)
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    // SAFETY: GetAllocatedString hands out a CoTaskMemAlloc'd string we free.
    unsafe {
        let mut name = windows::core::PWSTR::null();
        let mut len = 0;
        if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_err() {
            return "hardware encoder".into();
        }
        let text = name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(name.0 as *const _));
        text
    }
}

pub struct MfEncoder {
    /// Owns the transform: `ShutdownObject` on it is what frees the GPU
    /// encoder session (GeForce cards only have a few).
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: ICodecAPI,
    name: String,
    width: u32,
    height: u32,
    input_id: u32,
    output_id: u32,
    /// The encoder asked for this many inputs we haven't given yet.
    wants_input: u32,
    /// Encoded frames ready to hand out, oldest first.
    ready: VecDeque<Vec<u8>>,
    /// SPS/PPS, for encoders that don't repeat them before keyframes.
    sequence_header: Vec<u8>,
    frame_duration: i64,
}

// SAFETY: the encoder is created and used on one thread at a time, which is
// in the multithreaded apartment (`init_thread`), where COM objects may be
// called from any thread.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Open the best hardware encoder for this frame size.
    pub fn new(width: u32, height: u32, settings: EncoderSettings) -> Result<Self> {
        init_thread();
        let activates = enumerate().map_err(|e| mf_error("listing hardware encoders", e))?;
        if activates.is_empty() {
            return Err(Error::Unsupported);
        }
        let mut last_error = Error::Unsupported;
        for activate in activates {
            let name = friendly_name(&activate);
            match Self::open(&activate, name.clone(), width, height, settings) {
                Ok(encoder) => return Ok(encoder),
                Err(err) => {
                    tracing::debug!(encoder = name, "can't use hardware encoder: {err}");
                    // SAFETY: releases whatever the failed attempt activated.
                    let _ = unsafe { activate.ShutdownObject() };
                    last_error = err;
                }
            }
        }
        Err(last_error)
    }

    fn open(activate: &IMFActivate, name: String, width: u32, height: u32, settings: EncoderSettings) -> Result<Self> {
        let fps = settings.fps.max(1);
        // SAFETY: straightforward Media Foundation calls on objects we own;
        // every pointer passed lives for the duration of the call.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(|e| mf_error("activating", e))?;
            let attributes = transform.GetAttributes().map_err(|e| mf_error("attributes", e))?;
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| mf_error("unlocking async", e))?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            let codec: ICodecAPI = transform.cast().map_err(|e| mf_error("codec API", e))?;
            let events: IMFMediaEventGenerator = transform.cast().map_err(|e| mf_error("not asynchronous", e))?;

            // Real-time streaming: constant bitrate, no B-frames (they add
            // delay), keyframes mostly on request.
            let set = |api: &GUID, value: VARIANT| codec.SetValue(api, &value);
            let _ = set(&CODECAPI_AVLowLatencyMode, variant_bool(true));
            let _ = set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
            let _ = set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(settings.bitrate_bps));
            let _ = set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
            let _ = set(&CODECAPI_AVEncMPVGOPSize, variant_u32(fps * KEYFRAME_INTERVAL_SECS));
            // At very low quantizers (a small frame with bits to spare) NVENC
            // emits coefficient codes Baseline doesn't allow (level_prefix >
            // 15), which OpenH264 rejects. Visually lossless long before this.
            if let Err(err) = set(&CODECAPI_AVEncVideoMinQP, variant_u32(MIN_QP)) {
                tracing::debug!(encoder = name, "can't set a minimum QP: {err}");
            }

            // Most encoders don't number their streams (E_NOTIMPL): then it's 0.
            let (mut inputs, mut outputs) = ([0u32], [0u32]);
            if transform.GetStreamIDs(&mut inputs, &mut outputs).is_err() {
                (inputs, outputs) = ([0], [0]);
            }
            let (input_id, output_id) = (inputs[0], outputs[0]);

            let output = MFCreateMediaType().map_err(|e| mf_error("media type", e))?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| mf_error("output type", e))?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| mf_error("output type", e))?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_bps).map_err(|e| mf_error("output type", e))?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)).map_err(|e| mf_error("output type", e))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| mf_error("output type", e))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| mf_error("output type", e))?;
            output
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| mf_error("output type", e))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                .map_err(|e| mf_error("output type", e))?;
            transform.SetOutputType(output_id, &output, 0).map_err(|e| mf_error("setting output type", e))?;

            let mut input = None;
            for index in 0.. {
                let Ok(candidate) = transform.GetInputAvailableType(input_id, index) else { break };
                if candidate.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    input = Some(candidate);
                    break;
                }
            }
            let input = input.ok_or_else(|| Error::Codec("encoder doesn't take NV12".into()))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)).map_err(|e| mf_error("input type", e))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| mf_error("input type", e))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| mf_error("input type", e))?;
            transform.SetInputType(input_id, &input, 0).map_err(|e| mf_error("setting input type", e))?;

            let info = transform.GetOutputStreamInfo(output_id).map_err(|e| mf_error("output stream", e))?;
            if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                // Every hardware encoder we know of allocates its own output.
                return Err(Error::Codec("encoder wants caller-allocated output".into()));
            }

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).map_err(|e| mf_error("flush", e))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| mf_error("begin", e))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| mf_error("start", e))?;

            let mut encoder = Self {
                activate: activate.clone(),
                transform,
                events,
                codec,
                name,
                width,
                height,
                input_id,
                output_id,
                wants_input: 0,
                ready: VecDeque::new(),
                sequence_header: Vec::new(),
                frame_duration: 10_000_000 / fps as i64,
            };
            encoder.read_sequence_header();
            tracing::info!(encoder = encoder.name, width, height, fps, "hardware encoder ready");
            Ok(encoder)
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        // SAFETY: setting a plain value on the codec API.
        unsafe { self.codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bps)) }
            .map_err(|e| mf_error("setting bitrate", e))
    }

    /// Encode one frame; `fill` writes it as NV12 straight into the
    /// encoder's input buffer. Usually returns the frame right away; if the
    /// hardware is slower, it comes out of a later call instead.
    pub fn encode(
        &mut self,
        fill: impl FnOnce(&mut [u8]),
        timestamp_ms: u64,
        keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        let deadline = Instant::now() + INPUT_TIMEOUT;
        while self.wants_input == 0 {
            if !self.poll_event()? {
                if Instant::now() > deadline {
                    return Err(Error::Timeout);
                }
                std::thread::sleep(POLL);
            }
        }
        // SAFETY: building and submitting a sample from memory we own.
        unsafe {
            let len = nv12_len(self.width as usize, self.height as usize);
            let buffer = MFCreateMemoryBuffer(len as u32).map_err(|e| mf_error("buffer", e))?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(|e| mf_error("lock", e))?;
            fill(std::slice::from_raw_parts_mut(data, len));
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(len as u32).map_err(|e| mf_error("buffer length", e))?;
            let sample = MFCreateSample().map_err(|e| mf_error("sample", e))?;
            sample.AddBuffer(&buffer).map_err(|e| mf_error("sample", e))?;
            let _ = sample.SetSampleTime(timestamp_ms as i64 * 10_000);
            let _ = sample.SetSampleDuration(self.frame_duration);
            if keyframe {
                let _ = self.codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
            self.transform.ProcessInput(self.input_id, &sample, 0).map_err(|e| mf_error("encoding", e))?;
        }
        self.wants_input -= 1;

        let deadline = Instant::now() + OUTPUT_WAIT;
        while self.ready.is_empty() && Instant::now() < deadline {
            if !self.poll_event()? {
                std::thread::sleep(POLL);
            }
        }
        Ok(self.ready.pop_front().map(|data| {
            let keyframe = has_nal(&data, 5);
            let data = if keyframe && !has_nal(&data, 7) && !self.sequence_header.is_empty() {
                [&self.sequence_header[..], &data[..]].concat()
            } else {
                data
            };
            EncodedFrame { data, keyframe, width: self.width, height: self.height }
        }))
    }

    /// Handle one pending event, if any. Returns whether there was one.
    fn poll_event(&mut self) -> Result<bool> {
        // SAFETY: non-blocking event query on our own transform.
        let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(err) => return Err(mf_error("encoder events", err)),
        };
        // SAFETY: reading the type of an event we were handed.
        let kind = unsafe { event.GetType() }.map_err(|e| mf_error("event type", e))?;
        let kind = MF_EVENT_TYPE(kind as i32);
        if kind == METransformNeedInput {
            self.wants_input += 1;
        } else if kind == METransformHaveOutput {
            self.collect_output()?;
        }
        Ok(true)
    }

    fn collect_output(&mut self) -> Result<()> {
        let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        // SAFETY: the transform fills `output` with a sample it allocated;
        // we take ownership of both fields so they are released.
        let result = unsafe { self.transform.ProcessOutput(0, &mut outputs, &mut status) };
        let [output] = &mut outputs;
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut output.pEvents) });
        match result {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder settled on different output details; accept them.
                // SAFETY: re-applying a type the transform itself proposed.
                unsafe {
                    let proposed =
                        self.transform.GetOutputAvailableType(self.output_id, 0).map_err(|e| mf_error("type", e))?;
                    self.transform.SetOutputType(self.output_id, &proposed, 0).map_err(|e| mf_error("type", e))?;
                }
                self.read_sequence_header();
                return Ok(());
            }
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(err) => return Err(mf_error("reading output", err)),
        }
        let Some(sample) = sample else { return Ok(()) };
        // SAFETY: copying out of a locked buffer of the reported length.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(|e| mf_error("output buffer", e))?;
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len)).map_err(|e| mf_error("lock output", e))?;
            let bytes = std::slice::from_raw_parts(data, len as usize).to_vec();
            let _ = buffer.Unlock();
            if !bytes.is_empty() {
                self.ready.push_back(bytes);
            }
        }
        Ok(())
    }

    fn read_sequence_header(&mut self) {
        // SAFETY: reading a blob attribute into a buffer of its reported size.
        unsafe {
            let Ok(kind) = self.transform.GetOutputCurrentType(self.output_id) else { return };
            let Ok(size) = kind.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return };
            let mut header = vec![0; size as usize];
            if kind.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None).is_ok() {
                self.sequence_header = header;
            }
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: telling our own transform the stream is over, then shutting
        // it down through its activation object, as Media Foundation requires.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RgbaImage;
    use crate::color::convert_into;
    use crate::video::{EncoderPreference, VideoDecoder, VideoEncoder};

    /// GeForce cards allow only a few encoder sessions at once; parallel
    /// tests would hit that limit (MF_E_UNSUPPORTED_D3D_TYPE).
    static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gpu() -> std::sync::MutexGuard<'static, ()> {
        GPU.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn frame(width: u32, height: u32, t: u32) -> RgbaImage {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[(x + t * 8) as u8, y as u8, ((x + y) / 2) as u8, 255]);
            }
        }
        RgbaImage { width, height, pixels }
    }

    /// Runs where a GPU encoder exists (skips elsewhere, e.g. CI or a VM).
    #[test]
    fn hardware_output_decodes_with_openh264() {
        let _gpu = gpu();
        let names = hardware_encoders();
        println!("hardware encoders: {names:?}");
        if names.is_empty() {
            return;
        }
        // Not a multiple of 16, like most window sizes.
        let (width, height) = (626, 392);
        let settings = EncoderSettings { fps: 30, bitrate_bps: 2_000_000 };
        let mut encoder = MfEncoder::new(width, height, settings).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let (mut encoded, mut decoded, mut keyframes) = (0, 0, Vec::new());
        let mut worst = Duration::ZERO;
        for t in 0..60u32 {
            let input = frame(width, height, t);
            let started = Instant::now();
            let fill = |nv12: &mut [u8]| convert_into(&input.pixels, width as usize, height as usize, nv12);
            // Ask for a keyframe mid-stream, like a viewer joining.
            let Some(out) = encoder.encode(fill, t as u64 * 33, t == 30).unwrap() else { continue };
            worst = worst.max(started.elapsed());
            encoded += 1;
            if out.keyframe {
                keyframes.push(t);
            }
            if let Some(picture) = decoder.decode(&out.data).unwrap() {
                assert_eq!((picture.width, picture.height), (width, height));
                let diff: u64 =
                    picture.pixels.iter().zip(&input.pixels).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>();
                let mean = diff as f64 / input.pixels.len() as f64;
                assert!(mean < 12.0, "frame {t}: mean error {mean:.1}");
                decoded += 1;
            }
            if t == 40 {
                encoder.set_bitrate(500_000).unwrap();
            }
        }
        println!(
            "{}: encoded {encoded}, decoded {decoded}, keyframes at {keyframes:?}, slowest {worst:?}",
            encoder.name()
        );
        assert!(encoded >= 58, "frames came out: {encoded}");
        assert_eq!(decoded, encoded, "every frame decodes");
        assert_eq!(keyframes.first(), Some(&0), "starts with a keyframe");
        assert!(keyframes.iter().any(|t| (30..=32).contains(t)), "forced keyframe: {keyframes:?}");
    }

    /// Small frames with bits to spare drive the quantizer to the bottom,
    /// where NVENC used to emit coefficients OpenH264 rejects (56 of 240
    /// sizes in a sweep failed before the minimum QP, these among them).
    #[test]
    fn small_frames_with_spare_bits_stay_decodable() {
        let _gpu = gpu();
        if hardware_encoders().is_empty() {
            return;
        }
        let settings = EncoderSettings { fps: 30, bitrate_bps: 8_000_000 };
        let mut failed = Vec::new();
        for (width, height) in [(164, 64), (146, 148), (308, 92), (200, 100), (182, 148), (626, 392), (1920, 1080)] {
            {
                let mut encoder = MfEncoder::new(width, height, settings).unwrap();
                let mut decoder = VideoDecoder::new().unwrap();
                for t in 0..3u32 {
                    // Flat grey, then noise: both extremes of coefficient sizes.
                    let pixels: Vec<u8> = (0..width * height)
                        .flat_map(|i| {
                            let v = if t == 0 { 128 } else { ((i ^ (i >> 3)).wrapping_mul(2654435761) >> 24) as u8 };
                            [v, v.wrapping_add(t as u8 * 40), 255 - v, 255]
                        })
                        .collect();
                    let fill = |nv12: &mut [u8]| convert_into(&pixels, width as usize, height as usize, nv12);
                    let Some(out) = encoder.encode(fill, t as u64 * 33, false).unwrap() else { continue };
                    if decoder.decode(&out.data).is_err() {
                        failed.push((width, height, t));
                        break;
                    }
                }
            }
        }
        assert!(failed.is_empty(), "undecodable: {failed:?}");
    }

    /// Through `VideoEncoder`: software covers start-up and resizes while a
    /// hardware session opens in the background, and the stream stays
    /// decodable across every switch.
    #[test]
    fn switches_between_software_and_hardware_seamlessly() {
        let _gpu = gpu();
        if hardware_encoders().is_empty() {
            return;
        }
        let settings = EncoderSettings { fps: 60, bitrate_bps: 2_000_000 };
        let mut encoder = VideoEncoder::with_preference(settings, EncoderPreference::Hardware).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let mut t = 0u32;
        let mut log = Vec::new();
        for (width, height) in [(640u32, 360u32), (501, 333)] {
            let started = Instant::now();
            let mut was_hardware = None;
            // Until the hardware session for this size has taken over.
            while started.elapsed() < Duration::from_secs(5) {
                let took = Instant::now();
                let Some(out) = encoder.encode(&frame(width, height, t), t as u64 * 16).unwrap() else {
                    t += 1;
                    continue;
                };
                assert!(took.elapsed() < Duration::from_millis(100), "encoding stalled for {:?}", took.elapsed());
                let hardware = encoder.is_hardware();
                if was_hardware != Some(hardware) {
                    assert!(out.keyframe, "switch to {} at frame {t} must start with a keyframe", hardware);
                    log.push((width, hardware));
                }
                was_hardware = Some(hardware);
                let picture = decoder.decode(&out.data).unwrap().expect("picture");
                assert_eq!((picture.width, picture.height), (width & !1, height & !1));
                t += 1;
                if hardware {
                    break;
                }
                std::thread::sleep(Duration::from_millis(16));
            }
            assert_eq!(was_hardware, Some(true), "hardware took over at {width}x{height}");
        }
        println!("backend switches: {log:?}");
    }
}
