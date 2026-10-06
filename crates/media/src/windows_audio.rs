//! WASAPI audio capture (process loopback) and playback.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

use crate::audio::{CHANNELS, FRAME_LEN, OpusDecoder, OpusEncoder, PlayoutBuffer, SAMPLE_RATE};
use crate::{Error, Result};

/// Which audio to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// Only this process (and its children): sharing one app's window.
    Process(u32),
    /// Everything except this process: sharing a whole screen without
    /// re-broadcasting the streams Niscord itself is playing.
    AllExcept(u32),
}

fn format() -> WaveFormat {
    WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None)
}

fn audio_error(context: &str, err: impl std::fmt::Display) -> Error {
    Error::Capture(format!("{context}: {err}"))
}

fn bytes_to_f32(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b))
}

/// Captures audio until dropped, handing out 20 ms interleaved frames.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start(source: AudioSource, on_frame: impl FnMut(&[f32]) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || capture_loop(source, &stop, ready_tx, on_frame))
                .map_err(|e| audio_error("starting capture thread", e))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread) }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(Error::Capture("audio capture did not start".into()))
            }
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn capture_loop(
    source: AudioSource,
    stop: &AtomicBool,
    ready: mpsc::SyncSender<Result<()>>,
    mut on_frame: impl FnMut(&[f32]),
) {
    let _ = initialize_mta();
    let setup = || -> Result<_> {
        let (pid, include) = match source {
            AudioSource::Process(pid) => (pid, true),
            AudioSource::AllExcept(pid) => (pid, false),
        };
        let mut client = AudioClient::new_application_loopback_client(pid, include)
            .map_err(|e| audio_error("process loopback (needs Windows 10 2004 or later)", e))?;
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
        client.initialize_client(&format(), &Direction::Capture, &mode).map_err(|e| audio_error("initialising", e))?;
        let event = client.set_get_eventhandle().map_err(|e| audio_error("event handle", e))?;
        let capture = client.get_audiocaptureclient().map_err(|e| audio_error("capture client", e))?;
        client.start_stream().map_err(|e| audio_error("starting", e))?;
        Ok((client, event, capture))
    };
    let (client, event, capture) = match setup() {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let frame_bytes = FRAME_LEN * 4;
    let mut queue: VecDeque<u8> = VecDeque::new();
    let mut frame = Vec::with_capacity(FRAME_LEN);
    while !stop.load(Ordering::Relaxed) {
        // No event while the source is silent; just check `stop` again.
        if event.wait_for_event(100).is_err() {
            continue;
        }
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => {
                    if let Err(err) = capture.read_from_device_to_deque(&mut queue) {
                        tracing::warn!("audio capture read failed: {err}");
                        break;
                    }
                }
                Ok(_) => break,
                Err(err) => {
                    tracing::warn!("audio capture failed: {err}");
                    return;
                }
            }
        }
        while queue.len() >= frame_bytes {
            let bytes: Vec<u8> = queue.drain(..frame_bytes).collect();
            frame.clear();
            frame.extend(bytes_to_f32(&bytes));
            on_frame(&frame);
        }
    }
    let _ = client.stop_stream();
}

/// Plays audio on the default output device until dropped.
pub struct AudioPlayer {
    buffer: Arc<Mutex<PlayoutBuffer>>,
    volume: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Cushion against network jitter, and the most delay we tolerate before
/// dropping audio (below that, playback catches up gradually).
const PLAYOUT_TARGET: Duration = Duration::from_millis(60);
const PLAYOUT_MAX: Duration = Duration::from_millis(400);

impl AudioPlayer {
    pub fn start() -> Result<Self> {
        let buffer = Arc::new(Mutex::new(PlayoutBuffer::new(PLAYOUT_TARGET, PLAYOUT_MAX)));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = {
            let (buffer, volume, stop) = (buffer.clone(), volume.clone(), stop.clone());
            std::thread::Builder::new()
                .name("audio-playback".into())
                .spawn(move || playback_loop(&buffer, &volume, &stop, ready_tx))
                .map_err(|e| audio_error("starting playback thread", e))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { buffer, volume, stop, thread: Some(thread) }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(Error::Capture("audio playback did not start".into()))
            }
        }
    }

    /// Queue decoded interleaved stereo for playback.
    pub fn push(&self, pcm: &[f32]) {
        self.buffer.lock().unwrap().push(pcm);
    }

    /// 0.0 = muted, 1.0 = as received.
    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn playback_loop(
    buffer: &Mutex<PlayoutBuffer>,
    volume: &AtomicU32,
    stop: &AtomicBool,
    ready: mpsc::SyncSender<Result<()>>,
) {
    let _ = initialize_mta();
    let setup = || -> Result<_> {
        let device = DeviceEnumerator::new()
            .and_then(|e| e.get_default_device(&Direction::Render))
            .map_err(|e| audio_error("no output device", e))?;
        let mut client = device.get_iaudioclient().map_err(|e| audio_error("output client", e))?;
        // 20 ms device buffer: low delay, still safe for a busy machine.
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 };
        client.initialize_client(&format(), &Direction::Render, &mode).map_err(|e| audio_error("initialising", e))?;
        let event = client.set_get_eventhandle().map_err(|e| audio_error("event handle", e))?;
        let render = client.get_audiorenderclient().map_err(|e| audio_error("render client", e))?;
        client.start_stream().map_err(|e| audio_error("starting", e))?;
        Ok((client, event, render))
    };
    let (client, event, render) = match setup() {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let mut pcm = Vec::new();
    let mut bytes = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if event.wait_for_event(100).is_err() {
            continue;
        }
        let frames = match client.get_available_space_in_frames() {
            Ok(frames) => frames as usize,
            Err(err) => {
                tracing::warn!("audio output failed: {err}");
                return;
            }
        };
        if frames == 0 {
            continue;
        }
        pcm.resize(frames * CHANNELS, 0.0);
        buffer.lock().unwrap().pull(&mut pcm, f32::from_bits(volume.load(Ordering::Relaxed)));
        bytes.clear();
        bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        if let Err(err) = render.write_to_device(frames, &bytes, None) {
            tracing::warn!("audio output write failed: {err}");
            return;
        }
    }
    let _ = client.stop_stream();
}

/// Captures audio and encodes it to Opus, one packet per 20 ms.
pub struct AudioSender {
    bytes: Arc<AtomicU64>,
    _capture: AudioCapture,
}

impl AudioSender {
    pub fn start(
        source: AudioSource,
        bitrate: i32,
        mut on_packet: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> Result<Self> {
        let mut encoder = OpusEncoder::new(bitrate)?;
        let bytes = Arc::new(AtomicU64::new(0));
        let counter = bytes.clone();
        let capture = AudioCapture::start(source, move |pcm| match encoder.encode(pcm) {
            Ok(packet) => {
                counter.fetch_add(packet.len() as u64, Ordering::Relaxed);
                on_packet(packet);
            }
            Err(err) => tracing::warn!("audio encode failed: {err}"),
        })?;
        Ok(Self { bytes, _capture: capture })
    }

    /// Encoded bytes so far, for bitrate stats.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Decodes one remote stream's Opus packets and plays them.
pub struct AudioReceiver {
    decoder: Mutex<OpusDecoder>,
    player: AudioPlayer,
}

impl AudioReceiver {
    pub fn start() -> Result<Self> {
        Ok(Self { decoder: Mutex::new(OpusDecoder::new()?), player: AudioPlayer::start()? })
    }

    /// One packet, with how many were lost right before it.
    pub fn push(&self, packet: &[u8], lost: usize) {
        match self.decoder.lock().unwrap().decode_after_loss(packet, lost) {
            Ok(pcm) => self.player.push(&pcm),
            Err(err) => tracing::debug!("audio decode failed: {err}"),
        }
    }

    pub fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
    }
}
