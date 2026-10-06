//! Audio: Opus encoding/decoding and a small playout buffer.
//!
//! Everything runs at 48 kHz stereo, interleaved `f32`, in 20 ms frames,
//! which is what WebRTC's Opus profile expects. Capture and playback through
//! WASAPI live in `windows_audio`.

use std::collections::VecDeque;
use std::time::Duration;

use crate::{Error, Result};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one 20 ms frame.
pub const FRAME_SAMPLES: usize = 960;
/// Interleaved values in one frame.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
/// Stereo music bitrate: transparent for game/video audio.
pub const DEFAULT_BITRATE: i32 = 128_000;

fn opus_error(context: &str, code: i32) -> Error {
    Error::Codec(format!("{context}: opus error {code}"))
}

pub struct OpusEncoder {
    st: *mut unsafe_libopus::OpusEncoder,
    out: Vec<u8>,
}

// SAFETY: the encoder state is only reached through `&mut self`.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(bitrate: i32) -> Result<Self> {
        let mut err = 0;
        // SAFETY: plain libopus constructor; checked for null/error below.
        let st = unsafe {
            unsafe_libopus::opus_encoder_create(
                SAMPLE_RATE as i32,
                CHANNELS as i32,
                unsafe_libopus::OPUS_APPLICATION_AUDIO,
                &mut err,
            )
        };
        if st.is_null() || err != unsafe_libopus::OPUS_OK {
            return Err(opus_error("creating encoder", err));
        }
        let encoder = Self { st, out: vec![0; 4000] };
        // SAFETY: `st` is a valid encoder for all of these requests.
        unsafe {
            unsafe_libopus::opus_encoder_ctl!(st, unsafe_libopus::OPUS_SET_BITRATE_REQUEST, bitrate);
            // In-band FEC lets the receiver rebuild a single lost packet from
            // the next one; it is tuned by the expected loss rate.
            unsafe_libopus::opus_encoder_ctl!(st, unsafe_libopus::OPUS_SET_INBAND_FEC_REQUEST, 1);
            unsafe_libopus::opus_encoder_ctl!(st, unsafe_libopus::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5);
        }
        Ok(encoder)
    }

    /// Encode one 20 ms frame of interleaved stereo.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        if pcm.len() != FRAME_LEN {
            return Err(Error::Codec(format!("expected {FRAME_LEN} samples, got {}", pcm.len())));
        }
        // SAFETY: `pcm` holds exactly one frame and `out` is large enough
        // for any Opus packet (max 1275 bytes per frame).
        let n = unsafe {
            unsafe_libopus::opus_encode_float(
                self.st,
                pcm.as_ptr(),
                FRAME_SAMPLES as i32,
                self.out.as_mut_ptr(),
                self.out.len() as i32,
            )
        };
        if n < 0 {
            return Err(opus_error("encoding", n));
        }
        Ok(self.out[..n as usize].to_vec())
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and not freed elsewhere.
        unsafe { unsafe_libopus::opus_encoder_destroy(self.st) }
    }
}

pub struct OpusDecoder {
    st: *mut unsafe_libopus::OpusDecoder,
}

// SAFETY: the decoder state is only reached through `&mut self`.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        // SAFETY: plain libopus constructor; checked below.
        let st = unsafe { unsafe_libopus::opus_decoder_create(SAMPLE_RATE as i32, CHANNELS as i32, &mut err) };
        if st.is_null() || err != unsafe_libopus::OPUS_OK {
            return Err(opus_error("creating decoder", err));
        }
        Ok(Self { st })
    }

    fn run(&mut self, packet: Option<&[u8]>, fec: bool) -> Result<Vec<f32>> {
        let mut pcm = vec![0f32; FRAME_LEN];
        let (ptr, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        // SAFETY: `pcm` has room for one 20 ms frame; a null packet asks
        // libopus for packet loss concealment.
        let n = unsafe {
            unsafe_libopus::opus_decode_float(self.st, ptr, len, pcm.as_mut_ptr(), FRAME_SAMPLES as i32, fec as i32)
        };
        if n < 0 {
            return Err(opus_error("decoding", n));
        }
        pcm.truncate(n as usize * CHANNELS);
        Ok(pcm)
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        self.run(Some(packet), false)
    }

    /// Rebuild the frame *before* `next_packet` from its forward error
    /// correction data (falls back to concealment if it has none).
    pub fn recover(&mut self, next_packet: &[u8]) -> Result<Vec<f32>> {
        self.run(Some(next_packet), true)
    }

    /// Synthesize a plausible frame for a lost packet.
    pub fn conceal(&mut self) -> Result<Vec<f32>> {
        self.run(None, false)
    }

    /// Decode `packet`, first filling in `lost` missing frames before it.
    pub fn decode_after_loss(&mut self, packet: &[u8], lost: usize) -> Result<Vec<f32>> {
        let mut out = Vec::with_capacity((lost + 1) * FRAME_LEN);
        // Concealment for older gaps, FEC for the frame right before.
        for _ in 1..lost.min(10) {
            out.extend(self.conceal()?);
        }
        if lost > 0 {
            out.extend(self.recover(packet)?);
        }
        out.extend(self.decode(packet)?);
        Ok(out)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create and not freed elsewhere.
        unsafe { unsafe_libopus::opus_decoder_destroy(self.st) }
    }
}

/// Jitter buffer between network arrival and the sound card.
///
/// Waits until `target` audio is queued before (re)starting playback, so
/// small arrival jitter doesn't cause clicks. Away from the target it plays a
/// little faster or slower until it is back: the sender's sound card and ours
/// run at slightly different speeds, so the queue would otherwise slowly run
/// dry (a gap) or pile up until audio is dropped (a jump), and a burst after
/// a network hiccup leaves extra queued. Only beyond `max` is audio dropped.
pub struct PlayoutBuffer {
    samples: VecDeque<f32>,
    target: usize,
    max: usize,
    playing: bool,
    /// Read position in frames from the front of `samples`; fractional while
    /// playing faster or slower than real time.
    position: f64,
}

/// Fastest catch-up, as a change of playback speed: a third of a semitone
/// at most, and only while far from the target.
const MAX_SPEED_CHANGE: f64 = 0.02;
/// Speed change per second away from the target: 100 ms extra plays 1%
/// faster, so an excess halves in about 7 s.
const SPEED_CHANGE_PER_SECOND: f64 = 0.1;
/// This close to the target, play exactly as received.
const DEADBAND: Duration = Duration::from_millis(10);

impl PlayoutBuffer {
    pub fn new(target: Duration, max: Duration) -> Self {
        let to_len = |d: Duration| (d.as_secs_f64() * SAMPLE_RATE as f64) as usize * CHANNELS;
        Self { samples: VecDeque::new(), target: to_len(target), max: to_len(max), playing: false, position: 0.0 }
    }

    pub fn push(&mut self, pcm: &[f32]) {
        self.samples.extend(pcm);
        if self.samples.len() > self.max {
            let excess = self.samples.len() - self.target;
            self.samples.drain(..excess);
            self.position = 0.0;
        }
    }

    /// Fill `out` (interleaved) with audio, or silence while buffering.
    pub fn pull(&mut self, out: &mut [f32], volume: f32) {
        if !self.playing && self.samples.len() >= self.target {
            self.playing = true;
        }
        let mut written = 0;
        if self.playing {
            let speed = self.speed();
            let frames = self.samples.len() / CHANNELS;
            for dst in out.as_chunks_mut::<CHANNELS>().0 {
                // Between frames `i` and `i + 1`; exactly on `i` at normal speed.
                let i = self.position as usize;
                if i + 1 >= frames {
                    break;
                }
                let t = (self.position - i as f64) as f32;
                for (c, dst) in dst.iter_mut().enumerate() {
                    let (a, b) = (self.samples[i * CHANNELS + c], self.samples[(i + 1) * CHANNELS + c]);
                    *dst = (a + (b - a) * t) * volume;
                }
                self.position += speed;
                written += CHANNELS;
            }
            let consumed = (self.position as usize).min(frames);
            self.samples.drain(..consumed * CHANNELS);
            self.position -= consumed as f64;
        }
        out[written..].fill(0.0);
        if written < out.len() {
            // Underrun: rebuild the cushion before playing again.
            self.playing = false;
        }
    }

    /// Playback speed that steers the queue back towards the target.
    fn speed(&self) -> f64 {
        let off = (self.samples.len() as f64 - self.target as f64) / CHANNELS as f64 / SAMPLE_RATE as f64;
        if off.abs() < DEADBAND.as_secs_f64() {
            return 1.0;
        }
        1.0 + (off * SPEED_CHANGE_PER_SECOND).clamp(-MAX_SPEED_CHANGE, MAX_SPEED_CHANGE)
    }

    pub fn buffered(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / CHANNELS as f64 / SAMPLE_RATE as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frame: usize, freq: f32) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .flat_map(|i| {
                let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                let v = (t * freq * std::f32::consts::TAU).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    /// Normalised correlation of the decoded signal with a sine, allowing
    /// for the codec's delay by searching over a range of lags.
    fn similarity(a: &[f32], b: &[f32]) -> f32 {
        (0..800)
            .map(|lag| {
                let (x, y) = (&a[lag..], &b[..b.len() - lag]);
                let dot: f32 = x.iter().zip(y).map(|(p, q)| p * q).sum();
                let nx: f32 = x.iter().map(|p| p * p).sum::<f32>().sqrt();
                let ny: f32 = y.iter().map(|q| q * q).sum::<f32>().sqrt();
                dot / (nx * ny).max(1e-9)
            })
            .fold(0.0, f32::max)
    }

    #[test]
    fn opus_round_trip_keeps_the_signal() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for f in 0..25 {
            let pcm = sine(f, 440.0);
            let packet = enc.encode(&pcm).unwrap();
            assert!(!packet.is_empty() && packet.len() < 1275);
            output.extend(dec.decode(&packet).unwrap());
            input.extend(pcm);
        }
        assert_eq!(output.len(), input.len());
        // Skip the first frames while the codec settles.
        let s = similarity(&output[FRAME_LEN * 5..], &input[FRAME_LEN * 5..]);
        assert!(s > 0.95, "similarity {s}");
    }

    #[test]
    fn loss_is_filled_in() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let packets: Vec<_> = (0..10).map(|f| enc.encode(&sine(f, 440.0)).unwrap()).collect();
        let mut out = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            match i {
                5 => {}                                                // lost
                6 => out.extend(dec.decode_after_loss(p, 1).unwrap()), // FEC
                _ => out.extend(dec.decode(p).unwrap()),
            }
        }
        // Timing is preserved: every 20 ms frame is accounted for.
        assert_eq!(out.len(), 10 * FRAME_LEN);
    }

    #[test]
    fn playout_buffer_waits_then_plays_and_caps_delay() {
        let mut buf = PlayoutBuffer::new(Duration::from_millis(60), Duration::from_millis(200));
        let frame = vec![1.0f32; FRAME_LEN];
        let mut out = vec![9.0f32; FRAME_LEN];

        buf.push(&frame); // 20 ms: below target, still silent
        buf.pull(&mut out, 1.0);
        assert!(out.iter().all(|s| *s == 0.0));

        buf.push(&frame);
        buf.push(&frame); // 60 ms buffered: the silent pull above consumed nothing
        buf.pull(&mut out, 0.5);
        assert!(out.iter().all(|s| *s == 0.5), "plays at volume once the target is reached");

        for _ in 0..20 {
            buf.push(&frame);
        }
        assert!(buf.buffered() <= Duration::from_millis(200), "delay is capped");
    }

    const TARGET: Duration = Duration::from_millis(60);

    /// Feed the buffer `seconds` of constant audio from a sender whose clock
    /// runs `drift` faster than ours, while the sound card pulls 10 ms at a
    /// time. Returns the buffer level after each pull, and how many pulls
    /// were (partly) silent.
    fn simulate(buf: &mut PlayoutBuffer, drift: f64, seconds: usize) -> (Vec<Duration>, usize) {
        let pull = FRAME_SAMPLES / 2;
        let (mut produced, mut pushed) = (0.0, 0);
        let (mut levels, mut silent) = (Vec::new(), 0);
        let mut out = vec![0f32; pull * CHANNELS];
        for _ in 0..seconds * 100 {
            produced += pull as f64 * (1.0 + drift);
            let frames = produced as usize - pushed;
            pushed += frames;
            buf.push(&vec![1.0; frames * CHANNELS]);
            buf.pull(&mut out, 1.0);
            silent += out.contains(&0.0) as usize;
            levels.push(buf.buffered());
        }
        (levels, silent)
    }

    /// After a network hiccup the queue had piled up, and stayed piled up
    /// until it hit the maximum and jumped.
    #[test]
    fn extra_audio_is_caught_up_gradually() {
        let mut buf = PlayoutBuffer::new(TARGET, Duration::from_millis(400));
        buf.push(&vec![1.0; 13 * FRAME_LEN]); // 260 ms: a burst after a stall
        let (levels, silent) = simulate(&mut buf, 0.0, 30);
        assert_eq!(silent, 0);
        for pair in levels.windows(2) {
            // Never more than 2% faster than real time (10 ms per pull), so no jump.
            assert!(pair[0].saturating_sub(pair[1]) < Duration::from_micros(250), "{pair:?}");
        }
        let last = *levels.last().unwrap();
        assert!(last < TARGET + Duration::from_millis(25), "still {last:?} queued");
    }

    /// The two sound cards' clocks differ slightly; the queue must not run
    /// dry (a gap) or pile up (then jump) because of it.
    #[test]
    fn clock_drift_neither_drains_nor_piles_up() {
        for drift in [-0.002, 0.002] {
            let mut buf = PlayoutBuffer::new(TARGET, Duration::from_millis(400));
            buf.push(&vec![1.0; 3 * FRAME_LEN]);
            let (levels, silent) = simulate(&mut buf, drift, 120);
            assert_eq!(silent, 0, "gaps with drift {drift}");
            let max = levels.iter().max().unwrap();
            assert!(*max < TARGET + Duration::from_millis(50), "piled up to {max:?} with drift {drift}");
        }
    }
}
