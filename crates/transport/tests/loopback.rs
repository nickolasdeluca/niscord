//! A sharer and a viewer in one process, connected over 127.0.0.1 with
//! in-memory signaling, streaming real H.264 from Niscord's encoder.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use niscord_media::RgbaImage;
use niscord_media::audio::{DEFAULT_BITRATE, FRAME_DURATION, FRAME_LEN, OpusDecoder, OpusEncoder};
use niscord_media::video::{EncoderPreference, EncoderSettings, VideoDecoder, VideoEncoder};
use niscord_transport::{
    BitrateLimits, IncomingEvents, IncomingPeer, OutgoingEvents, OutgoingPeer, PeerEvents, PeerState, SignalData,
    TransportConfig,
};
use tokio::sync::{mpsc, watch};

struct SharerSide {
    signals: mpsc::UnboundedSender<SignalData>,
    state: watch::Sender<PeerState>,
    keyframe_requests: AtomicU32,
}

impl PeerEvents for SharerSide {
    fn signal(&self, data: SignalData) {
        let _ = self.signals.send(data);
    }
    fn state(&self, state: PeerState) {
        let _ = self.state.send(state);
    }
}

impl OutgoingEvents for SharerSide {
    fn keyframe_requested(&self) {
        self.keyframe_requests.fetch_add(1, Ordering::Relaxed);
    }
}

struct ViewerSide {
    signals: mpsc::UnboundedSender<SignalData>,
    state: watch::Sender<PeerState>,
    frames: mpsc::UnboundedSender<(Bytes, bool, std::time::Instant)>,
    audio: mpsc::UnboundedSender<(Bytes, usize)>,
}

impl PeerEvents for ViewerSide {
    fn signal(&self, data: SignalData) {
        let _ = self.signals.send(data);
    }
    fn state(&self, state: PeerState) {
        let _ = self.state.send(state);
    }
}

impl IncomingEvents for ViewerSide {
    fn frame(&self, data: Bytes, keyframe: bool) {
        let _ = self.frames.send((data, keyframe, std::time::Instant::now()));
    }
    fn audio(&self, packet: Bytes, lost: usize) {
        let _ = self.audio.send((packet, lost));
    }
}

/// A frame with a moving pattern, so consecutive frames differ.
fn picture(width: u32, height: u32, t: u32) -> RgbaImage {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[(x + t * 7) as u8, (y + t * 3) as u8, ((x ^ y) + t) as u8, 255]);
        }
    }
    RgbaImage { width, height, pixels }
}

async fn wait_for(state: &mut watch::Receiver<PeerState>, want: PeerState) {
    let reached = tokio::time::timeout(Duration::from_secs(15), state.wait_for(|s| *s == want)).await.is_ok();
    assert!(reached, "timed out waiting for {want:?}, last {:?}", *state.borrow());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_flows_from_sharer_to_viewer() {
    let config = TransportConfig::loopback();

    let (to_viewer, mut viewer_inbox) = mpsc::unbounded_channel();
    let (to_sharer, mut sharer_inbox) = mpsc::unbounded_channel();
    let (sharer_state_tx, mut sharer_state) = watch::channel(PeerState::Connecting);
    let (viewer_state_tx, mut viewer_state) = watch::channel(PeerState::Connecting);
    let (frames_tx, mut frames) = mpsc::unbounded_channel();

    let sharer_side =
        Arc::new(SharerSide { signals: to_viewer, state: sharer_state_tx, keyframe_requests: AtomicU32::new(0) });
    let (audio_tx, mut audio) = mpsc::unbounded_channel();
    let viewer_side =
        Arc::new(ViewerSide { signals: to_sharer, state: viewer_state_tx, frames: frames_tx, audio: audio_tx });

    let viewer = Arc::new(IncomingPeer::start(&config, viewer_side.clone()).await.unwrap());
    let limits = BitrateLimits { initial: 1_000_000, min: 200_000, max: 4_000_000 };
    let sharer = Arc::new(OutgoingPeer::start(&config, limits, true, sharer_side.clone()).await.unwrap());

    // Signaling relay, preserving order like the real server does.
    let pump_viewer = {
        let viewer = viewer.clone();
        tokio::spawn(async move {
            while let Some(signal) = viewer_inbox.recv().await {
                viewer.handle_signal(signal).await.unwrap();
            }
        })
    };
    let pump_sharer = {
        let sharer = sharer.clone();
        tokio::spawn(async move {
            while let Some(signal) = sharer_inbox.recv().await {
                sharer.handle_signal(signal).await.unwrap();
            }
        })
    };

    wait_for(&mut sharer_state, PeerState::Connected).await;
    wait_for(&mut viewer_state, PeerState::Connected).await;
    assert!(sharer.is_connected());

    // Stream ~2 s of 30 fps video, honouring keyframe requests, with audio
    // alongside (a 20 ms Opus packet per frame is enough to prove it flows).
    let mut opus = OpusEncoder::new(DEFAULT_BITRATE).unwrap();
    // Software: deterministic (a GPU encoder may hand a frame out one call late).
    let mut encoder =
        VideoEncoder::with_preference(EncoderSettings { fps: 30, bitrate_bps: 1_000_000 }, EncoderPreference::Software)
            .unwrap();
    let mut answered = 0;
    let sent = 60;
    let mut sent_at = Vec::new();
    for t in 0..sent {
        let requests = sharer_side.keyframe_requests.load(Ordering::Relaxed);
        if requests > answered {
            answered = requests;
            encoder.request_keyframe();
        }
        if let Some(frame) = encoder.encode(&picture(320, 180, t), t as u64 * 33).unwrap() {
            sent_at.push(std::time::Instant::now());
            sharer.send_frame(Bytes::from(frame.data), Duration::from_millis(33)).await.unwrap();
        }
        let packet = opus.encode(&vec![0.1; FRAME_LEN]).unwrap();
        sharer.send_audio(Bytes::from(packet), FRAME_DURATION).await.unwrap();
        tokio::time::sleep(Duration::from_millis(33)).await;
    }

    // Decode everything that arrived.
    let decoded = Arc::new(Mutex::new(0u32));
    let mut decoder = VideoDecoder::new().unwrap();
    let mut received = 0;
    let mut saw_keyframe = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut delays = Vec::new();
    while let Ok(Some((data, keyframe, arrived))) = tokio::time::timeout_at(deadline, frames.recv()).await {
        if let Some(sent) = sent_at.get(received as usize) {
            delays.push(arrived.duration_since(*sent));
        }
        received += 1;
        saw_keyframe |= keyframe;
        if let Ok(Some(image)) = decoder.decode(&data) {
            assert_eq!((image.width, image.height), (320, 180));
            *decoded.lock().unwrap() += 1;
        }
        if received >= sent {
            break;
        }
    }
    let decoded = *decoded.lock().unwrap();
    delays.sort();
    let median = delays.get(delays.len() / 2).copied().unwrap_or_default();
    let worst = delays.last().copied().unwrap_or_default();
    println!(
        "received {received}/{sent}, decoded {decoded}, delay median {median:?} worst {worst:?},          keyframe requests {}, estimate {} kbps",
        sharer_side.keyframe_requests.load(Ordering::Relaxed),
        sharer.target_bitrate() / 1000
    );
    assert!(saw_keyframe, "no keyframe arrived");
    // Loopback loses nothing, and frames must not be held back waiting for
    // the next one (the last frame used to go missing).
    assert_eq!(received, sent, "frames arrived");
    assert_eq!(decoded, sent, "frames decoded");
    assert!(median < Duration::from_millis(20), "median delay {median:?}");
    assert!(sharer_side.keyframe_requests.load(Ordering::Relaxed) >= 1, "viewer never asked for a keyframe");

    // Every audio packet arrives, decodable, with nothing reported lost.
    let mut opus = OpusDecoder::new().unwrap();
    let mut packets = 0;
    while let Ok(Some((packet, lost))) = tokio::time::timeout(Duration::from_millis(500), audio.recv()).await {
        assert_eq!(lost, 0);
        assert_eq!(opus.decode(&packet).unwrap().len(), FRAME_LEN);
        packets += 1;
    }
    println!("audio packets {packets}/{sent}");
    assert_eq!(packets, sent, "audio packets arrived");

    // Asking for a keyframe from the viewer side reaches the sharer.
    let before = sharer_side.keyframe_requests.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(600)).await; // past the rate limit
    viewer.request_keyframe().await;
    let mut waited = 0;
    while sharer_side.keyframe_requests.load(Ordering::Relaxed) == before && waited < 50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    assert!(sharer_side.keyframe_requests.load(Ordering::Relaxed) > before, "PLI did not reach the sharer");

    sharer.close().await;
    viewer.close().await;
    pump_viewer.abort();
    pump_sharer.abort();
}
