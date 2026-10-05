//! End-to-end: real signaling server, two client sessions, their `Links`,
//! and WebRTC over 127.0.0.1. Covers the routing the UI layer relies on:
//! watch -> watch request -> offer/answer/candidates -> video + audio -> unwatch.

use std::time::Duration;

use bytes::Bytes;
use niscord_media::RgbaImage;
use niscord_media::audio::{DEFAULT_BITRATE, FRAME_LEN, OpusEncoder};
use niscord_media::video::{EncoderPreference, EncoderSettings, VideoEncoder};
use niscord_protocol::{ServerMsg, ShareKind};
use tokio::sync::mpsc;

use super::*;
use crate::net;

async fn start_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(niscord_server::run(listener, niscord_server::Config::default()));
    url
}

struct Client {
    id: PeerId,
    session: net::Session,
    links: Arc<Links>,
    events: mpsc::UnboundedReceiver<LinkEvent>,
}

/// Join the server and route stream-related messages to `Links`, the same
/// way the app does.
async fn join(url: &str, name: &str) -> Client {
    let (net_tx, mut net_rx) = mpsc::unbounded_channel();
    let params = net::Params { url: url.into(), name: name.into(), password: String::new() };
    let session = net::Session::start(&Handle::current(), params, move |event| {
        let _ = net_tx.send(event);
    });
    let (link_tx, events) = mpsc::unbounded_channel();
    let links = Links::new(Handle::current(), session.sender(), move |event| {
        let _ = link_tx.send(event);
    });
    links.set_udp_addr("127.0.0.1:0");

    let id = loop {
        match tokio::time::timeout(Duration::from_secs(5), net_rx.recv()).await.unwrap().unwrap() {
            net::Event::Joined { id, ice_servers } => {
                links.set_ice_servers(ice_servers);
                break id;
            }
            net::Event::Fatal(err) => panic!("{err}"),
            _ => {}
        }
    };

    let router = Arc::downgrade(&links);
    tokio::spawn(async move {
        while let Some(event) = net_rx.recv().await {
            let Some(links) = router.upgrade() else { return };
            if let net::Event::Server(msg) = event {
                match msg {
                    ServerMsg::WatchRequest { from } => links.add_viewer(from),
                    ServerMsg::ViewerLeft { from } => links.remove_viewer(from),
                    ServerMsg::Signal { from, role, data } => links.on_signal(from, role, data),
                    ServerMsg::ShareEnded { from } => links.unwatch(from),
                    _ => {}
                }
            }
        }
    });
    Client { id, session, links, events }
}

fn picture(t: u32) -> RgbaImage {
    let (width, height) = (320, 180);
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[(x + t * 7) as u8, (y + t * 3) as u8, ((x ^ y) + t) as u8, 255]);
        }
    }
    RgbaImage { width, height, pixels }
}

async fn next_event(events: &mut mpsc::UnboundedReceiver<LinkEvent>, mut want: impl FnMut(&LinkEvent) -> bool) {
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.recv().await {
            if want(&event) {
                return;
            }
        }
    })
    .await;
    assert!(found.is_ok(), "timed out waiting for a link event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_stream_through_server() {
    let url = start_server().await;
    let mut ana = join(&url, "Ana").await;
    let mut bia = join(&url, "Bia").await;

    // Ana goes live; Bia watches.
    ana.session.send(ClientMsg::ShareStart { kind: ShareKind::Screen, title: "Screen 1".into(), audio: false });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Muted: the test plays (silent) audio on the default output device.
    bia.links.watch(ana.id, 0.0).unwrap();
    bia.session.send(ClientMsg::Watch { target: ana.id });

    next_event(&mut ana.events, |e| matches!(e, LinkEvent::ViewersConnected(1))).await;

    // Ana streams ~2 s of video and audio through her Links.
    let mut opus = OpusEncoder::new(DEFAULT_BITRATE).unwrap();
    // Software: deterministic (a GPU encoder may hand a frame out one call late).
    let mut encoder =
        VideoEncoder::with_preference(EncoderSettings { fps: 30, bitrate_bps: 1_000_000 }, EncoderPreference::Software)
            .unwrap();
    let sent = 60;
    for t in 0..sent {
        if let Some(frame) = encoder.encode(&picture(t), t as u64 * 33).unwrap() {
            ana.links.send_frame(Bytes::from(frame.data), Duration::from_millis(33));
        }
        ana.links.send_audio(Bytes::from(opus.encode(&[0.0; FRAME_LEN]).unwrap()));
        tokio::time::sleep(Duration::from_millis(33)).await;
    }

    // Bia's decoder thread hands out pictures.
    let mut frames = 0;
    let mut connected = false;
    let mut audio = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, bia.events.recv()).await {
        match event {
            LinkEvent::StreamFrame { sharer, image } => {
                assert_eq!(sharer, ana.id);
                assert_eq!((image.width, image.height), (320, 180));
                frames += 1;
                if frames == sent {
                    break;
                }
            }
            LinkEvent::StreamState { state: PeerState::Connected, .. } => connected = true,
            LinkEvent::StreamAudio { sharer, ok } => {
                assert_eq!(sharer, ana.id);
                audio = Some(ok);
            }
            _ => {}
        }
    }
    println!("Bia decoded {frames}/{sent} frames, audio playback {audio:?}");
    // `ok` depends on the machine having an output device; arriving is what counts.
    assert!(audio.is_some(), "Bia never got audio");
    assert!(connected, "Bia never saw the connection come up");
    assert_eq!(frames, sent);
    let counters = bia.links.stream_counters(ana.id).expect("stream counters");
    assert_eq!(counters.snapshot().width, 320);

    // Bia stops watching; Ana's connection to her goes away.
    bia.links.unwatch(ana.id);
    bia.session.send(ClientMsg::Unwatch { target: ana.id });
    next_event(&mut ana.events, |e| matches!(e, LinkEvent::ViewersConnected(0))).await;
    assert!(bia.links.stream_counters(ana.id).is_none());
}

#[test]
fn bitrate_follows_slowest_viewer_within_bounds() {
    let max = 4_000_000;
    // First decision always applies; nobody watching means full quality.
    assert_eq!(next_bitrate(None, max, None), Some(max));
    // Follows the estimate down with headroom (85%, minus audio), never
    // below the floor or above the preset.
    assert_eq!(next_bitrate(Some(2_000_000), max, Some(max)), Some(1_550_000));
    assert_eq!(next_bitrate(Some(300_000), max, Some(1_000_000)), Some(MIN_VIDEO_BITRATE));
    assert_eq!(next_bitrate(Some(9_000_000), max, Some(1_000_000)), Some(max));
    // Video stays under the estimate even near the bottom.
    for estimate in [300_000, 500_000, 1_000_000, 3_000_000] {
        let target = next_bitrate(Some(estimate), max, None).unwrap();
        assert!(target + AUDIO_RESERVE <= estimate || target == MIN_VIDEO_BITRATE, "{estimate}: {target}");
    }
    // Small wobbles are ignored.
    assert_eq!(next_bitrate(Some(2_050_000), max, Some(1_550_000)), None);
    // A preset below the floor is respected.
    assert_eq!(next_bitrate(Some(50_000), 100_000, None), Some(100_000));
}
