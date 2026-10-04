//! Threads that tie capture, encoding and decoding together.
//!
//! [`VideoSender`] runs capture -> encode and hands encoded frames to a sink
//! (the network, or a local [`VideoReceiver`] for a loopback preview).
//! [`VideoReceiver`] runs decode and hands pictures to a callback.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::video::{EncodedFrame, EncoderSettings, VideoDecoder, VideoEncoder};
use crate::{Capture, CaptureOptions, FrameSink, Result, RgbaImage, SourceId};

/// If the decoder falls this far behind, skip ahead to the next keyframe.
const MAX_DECODE_BACKLOG: usize = 30;

/// Cumulative counters, updated by pipeline threads and read by the UI.
#[derive(Debug, Default)]
pub struct Counters {
    frames: AtomicU64,
    bytes: AtomicU64,
    /// Total time spent encoding or decoding.
    busy_us: AtomicU64,
    /// Frames dropped because the pipeline was busy.
    dropped: AtomicU64,
    /// Frames the encoder chose not to emit (rate control).
    skipped: AtomicU64,
    /// Frames delivered by capture (after frame-rate limiting), whether or
    /// not they were encoded. Shows whether the source or we are the limit.
    captured: AtomicU64,
    /// Sum of capture-to-output delay (only meaningful on one machine).
    latency_us: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
}

impl Counters {
    fn record(&self, bytes: usize, busy: Duration, latency: Option<Duration>, width: u32, height: u32) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.busy_us.fetch_add(busy.as_micros() as u64, Ordering::Relaxed);
        if let Some(latency) = latency {
            self.latency_us.fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
        }
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            at: Instant::now(),
            frames: self.frames.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            busy_us: self.busy_us.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            captured: self.captured.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            latency_us: self.latency_us.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed),
            height: self.height.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    at: Instant,
    frames: u64,
    bytes: u64,
    busy_us: u64,
    dropped: u64,
    captured: u64,
    skipped: u64,
    latency_us: u64,
    pub width: u32,
    pub height: u32,
}

/// Rates over the interval between two snapshots.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rates {
    pub fps: f64,
    pub kbps: f64,
    /// Average time per frame spent encoding/decoding.
    pub busy_ms: f64,
    pub latency_ms: f64,
    pub dropped: u64,
    /// Frames per second arriving from capture.
    pub source_fps: f64,
    pub skipped: u64,
}

impl Snapshot {
    pub fn rates_since(&self, earlier: &Snapshot) -> Rates {
        let secs = self.at.duration_since(earlier.at).as_secs_f64().max(1e-3);
        let frames = self.frames.saturating_sub(earlier.frames);
        let per_frame = |us: u64| if frames == 0 { 0.0 } else { us as f64 / frames as f64 / 1000.0 };
        Rates {
            fps: frames as f64 / secs,
            kbps: self.bytes.saturating_sub(earlier.bytes) as f64 * 8.0 / secs / 1000.0,
            busy_ms: per_frame(self.busy_us.saturating_sub(earlier.busy_us)),
            latency_ms: per_frame(self.latency_us.saturating_sub(earlier.latency_us)),
            dropped: self.dropped.saturating_sub(earlier.dropped),
            source_fps: self.captured.saturating_sub(earlier.captured) as f64 / secs,
            skipped: self.skipped.saturating_sub(earlier.skipped),
        }
    }
}

// ---------------------------------------------------------------------------
// Sending side

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSettings {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub show_cursor: bool,
}

/// Receives encoded frames on the encoder thread.
pub trait EncodedSink: Send + 'static {
    fn encoded(&mut self, frame: &EncodedFrame, captured_at: Instant);
    /// The captured source went away.
    fn source_closed(&mut self);
}

enum Control {
    Keyframe,
    Bitrate(u32),
    SourceClosed,
}

/// Cheap, thread-safe handle for steering a running [`VideoSender`].
#[derive(Clone)]
pub struct SenderControl {
    tx: mpsc::Sender<Control>,
}

impl SenderControl {
    pub fn request_keyframe(&self) {
        let _ = self.tx.send(Control::Keyframe);
    }

    pub fn set_bitrate(&self, bps: u32) {
        let _ = self.tx.send(Control::Bitrate(bps));
    }
}

/// Captures a source and encodes it to H.264 until dropped.
pub struct VideoSender {
    capture: Option<Capture>,
    control: mpsc::Sender<Control>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
}

struct CaptureToEncoder {
    frames: mpsc::SyncSender<(Instant, RgbaImage)>,
    control: mpsc::Sender<Control>,
    counters: Arc<Counters>,
}

impl FrameSink for CaptureToEncoder {
    fn frame(&mut self, image: RgbaImage) {
        self.counters.captured.fetch_add(1, Ordering::Relaxed);
        // Never queue behind a busy encoder: a fresh frame beats a stale one.
        if self.frames.try_send((Instant::now(), image)).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn closed(&mut self) {
        let _ = self.control.send(Control::SourceClosed);
    }
}

impl VideoSender {
    pub fn start(source: SourceId, settings: StreamSettings, sink: impl EncodedSink) -> Result<Self> {
        let encoder = VideoEncoder::new(EncoderSettings { fps: settings.fps, bitrate_bps: settings.bitrate_bps })?;
        let counters = Arc::new(Counters::default());
        let (frame_tx, frame_rx) = mpsc::sync_channel(1);
        let (control_tx, control_rx) = mpsc::channel();

        let thread = {
            let counters = counters.clone();
            std::thread::Builder::new()
                .name("video-encoder".into())
                .spawn(move || encode_loop(encoder, frame_rx, control_rx, sink, &counters))
                .map_err(|e| crate::Error::Capture(e.to_string()))?
        };

        let options = CaptureOptions {
            max_fps: settings.fps,
            max_width: settings.max_width,
            max_height: settings.max_height,
            show_cursor: settings.show_cursor,
        };
        let capture_sink =
            CaptureToEncoder { frames: frame_tx, control: control_tx.clone(), counters: counters.clone() };
        // If capture fails, dropping its sink disconnects the encoder thread.
        let capture = Capture::start(source, options, capture_sink)?;
        Ok(Self { capture: Some(capture), control: control_tx, thread: Some(thread), counters })
    }

    pub fn control(&self) -> SenderControl {
        SenderControl { tx: self.control.clone() }
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }
}

impl Drop for VideoSender {
    fn drop(&mut self) {
        // Stopping capture drops its frame sender, which ends the encoder loop.
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Windows only delivers frames when the source changes. While it is idle,
/// re-encode the last picture this often, so a viewer who joins (or lost
/// packets) gets a picture without waiting for something to move.
const REPEAT_INTERVAL: Duration = Duration::from_millis(500);

fn encode_loop(
    mut encoder: VideoEncoder,
    frames: mpsc::Receiver<(Instant, RgbaImage)>,
    control: mpsc::Receiver<Control>,
    mut sink: impl EncodedSink,
    counters: &Counters,
) {
    let start = Instant::now();
    let mut last: Option<(Instant, RgbaImage)> = None;
    let mut keyframe_pending = false;
    loop {
        for msg in control.try_iter() {
            match msg {
                Control::Keyframe => {
                    encoder.request_keyframe();
                    keyframe_pending = true;
                }
                Control::Bitrate(bps) => {
                    if let Err(err) = encoder.set_bitrate(bps) {
                        tracing::warn!("{err}");
                    }
                }
                Control::SourceClosed => sink.source_closed(),
            }
        }
        let wait = if keyframe_pending { Duration::from_millis(20) } else { Duration::from_millis(50) };
        let (captured_at, image) = match frames.recv_timeout(wait) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => match last.take() {
                // Nothing new: repeat the last picture if someone is waiting
                // for a keyframe, or it has been a while.
                Some((at, image)) if keyframe_pending || at.elapsed() >= REPEAT_INTERVAL => (Instant::now(), image),
                other => {
                    last = other;
                    continue;
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let began = Instant::now();
        let timestamp_ms = captured_at.saturating_duration_since(start).as_millis() as u64;
        match encoder.encode(&image, timestamp_ms) {
            Ok(Some(frame)) => {
                keyframe_pending &= !frame.keyframe;
                counters.record(frame.data.len(), began.elapsed(), None, frame.width, frame.height);
                sink.encoded(&frame, captured_at);
            }
            Ok(None) => {
                counters.skipped.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => tracing::warn!("encode failed: {err}"),
        }
        last = Some((captured_at, image));
    }
}

// ---------------------------------------------------------------------------
// Receiving side

struct Packet {
    data: Vec<u8>,
    keyframe: bool,
    captured_at: Option<Instant>,
}

/// Decodes H.264 on its own thread until dropped.
pub struct VideoReceiver {
    tx: Option<mpsc::Sender<Packet>>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
}

impl VideoReceiver {
    /// `on_frame` runs on the decoder thread for every decoded picture.
    pub fn start(on_frame: impl FnMut(RgbaImage) + Send + 'static) -> Result<Self> {
        let decoder = VideoDecoder::new()?;
        let counters = Arc::new(Counters::default());
        let (tx, rx) = mpsc::channel();
        let thread = {
            let counters = counters.clone();
            std::thread::Builder::new()
                .name("video-decoder".into())
                .spawn(move || decode_loop(decoder, rx, on_frame, &counters))
                .map_err(|e| crate::Error::Capture(e.to_string()))?
        };
        Ok(Self { tx: Some(tx), thread: Some(thread), counters })
    }

    /// Queue an encoded frame. `captured_at` enables latency stats when the
    /// sender runs in the same process.
    pub fn push(&self, data: Vec<u8>, keyframe: bool, captured_at: Option<Instant>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Packet { data, keyframe, captured_at });
        }
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn decode_loop(
    mut decoder: VideoDecoder,
    rx: mpsc::Receiver<Packet>,
    mut on_frame: impl FnMut(RgbaImage),
    counters: &Counters,
) {
    let mut queue = Vec::new();
    while let Ok(first) = rx.recv() {
        queue.push(first);
        queue.extend(rx.try_iter());
        // Hopelessly behind: everything before the newest keyframe is useless.
        if queue.len() > MAX_DECODE_BACKLOG
            && let Some(key) = queue.iter().rposition(|p| p.keyframe)
        {
            counters.dropped.fetch_add(key as u64, Ordering::Relaxed);
            queue.drain(..key);
        }
        for packet in queue.drain(..) {
            let began = Instant::now();
            match decoder.decode(&packet.data) {
                Ok(Some(image)) => {
                    let latency = packet.captured_at.map(|t| t.elapsed());
                    counters.record(packet.data.len(), began.elapsed(), latency, image.width, image.height);
                    on_frame(image);
                }
                Ok(None) => {}
                Err(err) => tracing::debug!("decode failed: {err}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{EncoderSettings, VideoEncoder};

    struct Collect(mpsc::Sender<(bool, Instant)>);

    impl EncodedSink for Collect {
        fn encoded(&mut self, frame: &EncodedFrame, _captured_at: Instant) {
            let _ = self.0.send((frame.keyframe, Instant::now()));
        }
        fn source_closed(&mut self) {}
    }

    #[test]
    fn idle_source_still_answers_keyframe_requests() {
        let (frame_tx, frame_rx) = mpsc::channel();
        let (control_tx, control_rx) = mpsc::channel();
        let (out_tx, out) = mpsc::channel();
        let encoder = VideoEncoder::new(EncoderSettings { fps: 30, bitrate_bps: 1_000_000 }).unwrap();
        let thread = std::thread::spawn(move || {
            encode_loop(encoder, frame_rx, control_rx, Collect(out_tx), &Counters::default());
        });

        // One picture, then the source goes quiet (a static window).
        let pixels = (0..64 * 48).flat_map(|i| [i as u8, 0, 0, 255]).collect();
        frame_tx.send((Instant::now(), RgbaImage { width: 64, height: 48, pixels })).unwrap();
        let (first_is_key, _) = out.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first_is_key);

        // A viewer joins and asks for a keyframe: it comes from the last picture.
        let asked = Instant::now();
        control_tx.send(Control::Keyframe).unwrap();
        let (keyframe, at) = out.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(keyframe, "repeat answers the request");
        assert!(at - asked < Duration::from_millis(200), "answered after {:?}", at - asked);

        // And the picture keeps being refreshed while idle.
        let (_, refreshed) = out.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(refreshed - at >= REPEAT_INTERVAL - Duration::from_millis(20));

        drop(frame_tx);
        thread.join().unwrap();
    }

    #[test]
    fn receiver_decodes_on_its_own_thread_and_counts() {
        let (tx, rx) = mpsc::channel();
        let receiver = VideoReceiver::start(move |image| {
            let _ = tx.send((image.width, image.height));
        })
        .unwrap();

        let mut encoder = VideoEncoder::new(EncoderSettings { fps: 30, bitrate_bps: 1_000_000 }).unwrap();
        for t in 0..5u8 {
            let pixels = (0..64 * 48).flat_map(|i| [(i as u8).wrapping_add(t * 9), t, 0, 255]).collect();
            let image = RgbaImage { width: 64, height: 48, pixels };
            if let Some(frame) = encoder.encode(&image, t as u64 * 33).unwrap() {
                receiver.push(frame.data, frame.keyframe, Some(Instant::now()));
            }
        }
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), (64, 48));
        drop(receiver); // joins the thread, so every queued frame is decoded

        let decoded = 1 + rx.try_iter().count();
        assert!(decoded >= 3, "only {decoded} frames decoded");
    }

    #[test]
    fn rates_are_per_second() {
        let counters = Counters::default();
        let a = counters.snapshot();
        counters.record(1000, Duration::from_millis(4), None, 10, 10);
        counters.record(1000, Duration::from_millis(6), None, 10, 10);
        let mut b = counters.snapshot();
        b.at = a.at + Duration::from_secs(2);
        let r = b.rates_since(&a);
        assert_eq!(r.fps, 1.0);
        assert_eq!(r.kbps, 8.0);
        assert_eq!(r.busy_ms, 5.0);
    }
}
