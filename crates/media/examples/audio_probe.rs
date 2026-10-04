//! Checks audio capture and playback on this machine: plays a very quiet
//! 440 Hz tone (-46 dBFS) and captures it two ways.
//!
//! * `Process(own pid)` must hear the tone (sharing an app's audio).
//! * `AllExcept(own pid)` must not (sharing a screen without echoing the
//!   streams Niscord itself plays).
//!
//! cargo run --release -p niscord-media --example audio_probe
//!
//! With `--listen <pid>`, instead reports how much 440 Hz tone (at the probe's
//! level) another process is playing, e.g. a Niscord viewer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use niscord_media::audio::{FRAME_LEN, FRAME_SAMPLES, SAMPLE_RATE};
use niscord_media::windows_audio::{AudioCapture, AudioPlayer, AudioSource};

const AMPLITUDE: f32 = 0.005;

/// Strength of 440 Hz in the left channel, relative to the tone we play
/// (1.0 = all of it). Goertzel filter.
fn tone_level(samples: &[f32]) -> f32 {
    let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
    if left.is_empty() {
        return 0.0;
    }
    let k = 2.0 * (std::f32::consts::TAU * 440.0 / SAMPLE_RATE as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for x in &left {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - k * s1 * s2;
    // A pure tone of amplitude A over N samples has power (A*N/2)^2.
    power.sqrt() / (AMPLITUDE * left.len() as f32 / 2.0)
}

fn capture_for(source: AudioSource, seconds: u64) -> Vec<f32> {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let capture = AudioCapture::start(source, move |frame| sink.lock().unwrap().extend_from_slice(frame));
    match capture {
        Ok(capture) => {
            std::thread::sleep(Duration::from_secs(seconds));
            drop(capture);
        }
        Err(err) => println!("  capture failed: {err}"),
    }
    Arc::try_unwrap(captured).unwrap().into_inner().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, pid] = args.as_slice()
        && flag == "--listen"
    {
        let pid: u32 = pid.parse().expect("process id");
        let heard = capture_for(AudioSource::Process(pid), 3);
        let peak = heard.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!(
            "Process({pid}): {} samples ({:.2} s), peak {peak:.4}, 440 Hz level {:.2}",
            heard.len(),
            heard.len() as f32 / 2.0 / SAMPLE_RATE as f32,
            tone_level(&heard)
        );
        return;
    }

    let player = AudioPlayer::start().expect("audio output");
    let feeder = std::thread::spawn(move || {
        // ~4.5 s of tone, fed in 20 ms frames at real-time pace.
        for frame in 0..225 {
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .flat_map(|i| {
                    let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * AMPLITUDE;
                    [v, v]
                })
                .collect();
            assert_eq!(pcm.len(), FRAME_LEN);
            player.push(&pcm);
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    std::thread::sleep(Duration::from_millis(300));

    let pid = std::process::id();
    let own = capture_for(AudioSource::Process(pid), 2);
    println!(
        "Process(own pid):   {:>6} samples ({:.2} s), 440 Hz level {:.2}",
        own.len(),
        own.len() as f32 / 2.0 / SAMPLE_RATE as f32,
        tone_level(&own)
    );
    let others = capture_for(AudioSource::AllExcept(pid), 2);
    println!(
        "AllExcept(own pid): {:>6} samples ({:.2} s), 440 Hz level {:.2}",
        others.len(),
        others.len() as f32 / 2.0 / SAMPLE_RATE as f32,
        tone_level(&others)
    );
    feeder.join().unwrap();
}
