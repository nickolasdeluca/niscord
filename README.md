# Niscord

Private, self-hosted screen sharing for a group of friends, inspired by Discord's
"Go Live". Everyone runs the desktop app and connects to your server. You see who is
online and who is live, and you can watch each other's screens or individual windows.

Everything is native Rust: the GUI uses [Slint](https://slint.dev). Video goes
peer-to-peer over WebRTC, so the server only coordinates who talks to whom and never
carries video or audio. When a direct connection is impossible (for example behind carrier-grade
NAT), media is relayed through a TURN server you also host.

## Status

| Milestone | State |
|---|---|
| 1. Workspace, signaling server, app with connect screen and online list | ✅ done |
| 2. Screen/window picker with live thumbnails, local preview of your share | ✅ done |
| 3. H.264 encode/decode pipeline; your preview shows what viewers will get, with live stats | ✅ done |
| 4. WebRTC between friends: Watch shows the live stream, bitrate follows the network | ✅ done |
| 5. Audio: a window shares its app's sound, a screen shares everything but Niscord; Opus | ✅ done |
| 6. Quality: GPU encoding (NVIDIA/AMD/Intel) with software fallback, bitrate adaptation, keyframe recovery | ✅ done |
| 7. Packaging: portable `.exe`, server release binaries | ⏳ next |

## Layout

```
crates/
  protocol/   JSON messages shared by app and server
  media/      capture (Windows Graphics Capture), H.264 (GPU via Media Foundation, or
              OpenH264), encode/decode threads,
              audio (WASAPI process loopback, Opus via unsafe-libopus)
  transport/  WebRTC (webrtc-rs): one connection per sharer/viewer pair, H.264 + Opus tracks
  server/     signaling server (presence, watch requests, WebRTC signaling relay)
  app/        the Slint desktop app (`niscord.exe`)
```

## Development

Requires a stable Rust toolchain and [NASM](https://www.nasm.us) on `PATH`
(`winget install NASM.NASM`). Without NASM, OpenH264 silently builds without its SIMD
code and encodes 4-6x slower; the build prints a warning when it is missing. After
installing it, run `cargo clean -p openh264-sys2` once. Friends running the built app
don't need it.

```sh
cargo run -p niscord-server                  # server on ws://0.0.0.0:8080
cargo run -p niscord                          # the app; connect to 127.0.0.1:8080
cargo test --workspace
```

To test with one machine, run a fake friend who appears to be sharing:

```sh
cargo run -p niscord-server --example fake_peer -- ws://127.0.0.1:8080 "Bia" [password]
```

To list capture sources and time a thumbnail grab of each, or to time the whole
capture → encode → decode pipeline on your primary screen:

```sh
cargo run -p niscord-media --example probe
cargo run --release -p niscord-media --example bench_codec -- 1920 1080 30
```

To check audio capture on this machine (plays a very quiet tone and captures it):

```sh
cargo run --release -p niscord-media --example audio_probe
```

To compare the GPU and software encoders on synthetic 1080p60 and 1440p60 content:

```sh
cargo run --release -p niscord-media --example bench_encoders
```

Screen capture needs Windows 10 version 1903 or later. On Windows 10, Windows draws a
yellow border around whatever is being captured; on Windows 11 Niscord hides it.

### Encoding

Niscord encodes with the GPU when it has an H.264 encoder (NVIDIA NVENC, AMD AMF or
Intel Quick Sync, all reached through Media Foundation), and with OpenH264 on the CPU
otherwise. On an RTX 3060 a 1080p60 frame takes about 4.5 ms on the GPU against 13 ms
on a Ryzen 5 5600X, which makes 1080p60 and 1440p60 practical. The share's stats show
which one is in use (`encode … ms (GPU)`).

A GPU encoder takes about half a second to start and works at one frame size, so it
starts in the background: the first frames of a share, and the frames while a shared
window is being resized, are encoded in software, and the stream switches over (with a
keyframe) once the GPU is ready. If the GPU encoder fails, or your own preview can't
decode what it produced, Niscord switches to software for the rest of the share. Set
`NISCORD_ENCODER=software` to always use the CPU.

Viewers always decode with OpenH264: about 4 ms per 1080p frame and 9 ms per 1440p
frame on a Ryzen 5 5600X.

### Audio

"Share audio" in the picker sends sound along with the picture:

* **A window** shares only its app's sound (and its child processes', which covers
  browsers that play audio from a helper process).
* **A screen** shares everything you hear except Niscord itself, so the streams you
  are watching don't echo back to their sharers. Other voice apps are *not* excluded:
  if you are in a Discord call while sharing a screen, your friends hear the call too.
  Share a window instead to avoid that.

Per-app capture needs Windows 10 version 2004 or later; on older versions Niscord
shares the picture without sound. Viewers get a mute button and volume slider on each
stream.

### Testing WebRTC on one machine

The first time a new build of the app (or a test binary) opens a UDP socket, Windows
Firewall asks whether to allow it, even for sockets bound to `127.0.0.1`. Traffic on
the same machine flows either way, so these prompts can be cancelled safely. To keep a
debug build on loopback only, set `NISCORD_UDP_ADDR=127.0.0.1:0` (and run the server with
`NISCORD_BIND=127.0.0.1:8080`).

## Running the server

The server is a single binary configured through environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `NISCORD_BIND` | `0.0.0.0:8080` | Address to listen on |
| `NISCORD_PASSWORD` | *(empty)* | Shared password to join. **Set this**, or anyone who finds the server can join |
| `NISCORD_STUN_URLS` | `stun:stun.l.google.com:19302` | Comma-separated STUN URLs |
| `NISCORD_TURN_URLS` | *(empty)* | Comma-separated TURN URLs, e.g. `turn:turn.example.com:3478,turns:turn.example.com:5349` |
| `NISCORD_TURN_SECRET` | *(empty)* | coturn `static-auth-secret`; clients get short-lived credentials derived from it |
| `NISCORD_TURN_TTL_SECONDS` | `43200` | Lifetime of those credentials |
| `RUST_LOG` | `info` | Log verbosity |

```sh
cargo build --release -p niscord-server
NISCORD_PASSWORD=change-me ./target/release/niscord-server
```

**TLS:** the server speaks plain `ws://`. To use `wss://`, put it behind a reverse
proxy. With Caddy, for example:

```
niscord.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

### TURN (strongly recommended)

Many home connections (especially mobile and CGNAT ISPs) can't accept direct peer
connections. Install [coturn](https://github.com/coturn/coturn) and give it a config
like:

```
listening-port=3478
realm=niscord.example.com
use-auth-secret
static-auth-secret=<long random string, same as NISCORD_TURN_SECRET>
# Relay ports: open these (UDP) in the firewall.
min-port=49160
max-port=49200
no-cli
```

Then start the server with `NISCORD_TURN_URLS=turn:niscord.example.com:3478` and
`NISCORD_TURN_SECRET=<same secret>`.

## Distributing the app

Bake your server address into the build so friends only need a name and the password:

```sh
NISCORD_DEFAULT_SERVER=wss://niscord.example.com cargo build --release -p niscord
```

Share `target/release/niscord.exe`. It needs no installer or runtime. The first time a
friend watches or shares, Windows Firewall asks whether Niscord may use the network:
they should allow it (private networks is enough), or direct connections between
friends will fail and everything will have to go through TURN. The app remembers
the server, name and password in `%APPDATA%\Niscord\settings.json` (the password is
stored in plain text).

## License notes

Slint is used under its royalty-free license, which requires attribution. The app's
About dialog shows it.
