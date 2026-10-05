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
| 7. Packaging: self-contained `.exe`, Ubuntu install script, release builds | ✅ done |

## Using it

- **Go live:** *Share screen*, pick a screen or window, quality and whether to share
  its sound. Friends see you as LIVE and click *Watch*.
- **Go-live shortcut:** set one in *Settings* (e.g. Ctrl+Alt+S). It works from any
  app: it shares the window in front, and pressing it again on that window stops.
- **Pop out:** any stream (yours too) can move to its own window with *Pop out*.
  Double-click it or press F11 for full screen, Esc to leave full screen; *Pop in* or
  closing the window puts it back.
- **Hide your preview:** *Hide* on your own tile (or the checkbox in *Settings*) keeps
  you streaming without showing, or decoding, your own picture. It's remembered;
  *Show my preview* in the sidebar brings it back.
- Each stream you watch has its own mute and volume.

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
deploy/       Ubuntu server install script and systemd unit
.github/      CI (fmt, clippy, tests) and tagged release builds
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

## Hosting the server

### On an Ubuntu/Debian VPS

`deploy/install.sh` sets up everything on a fresh VPS: the Niscord server as a systemd
service, [Caddy](https://caddyserver.com) for `wss://` with a Let's Encrypt certificate,
and [coturn](https://github.com/coturn/coturn) for STUN/TURN. It builds the server from
source, so it only needs a copy of this repository.

1. Point a DNS `A` record for your domain (e.g. `niscord.example.com`) at the VPS.
2. Allow these in your VPS provider's firewall: TCP 80 and 443, UDP 3478,
   UDP 49160-49200. (The script opens them in `ufw` itself if it is active.)
3. Copy the source over and run the script:

   ```sh
   # On your PC, from the repository:
   git archive --format=tar.gz -o niscord.tar.gz HEAD
   scp niscord.tar.gz you@your-vps:

   # On the VPS:
   mkdir niscord && tar -xzf niscord.tar.gz -C niscord && cd niscord
   sudo deploy/install.sh --domain niscord.example.com
   ```

It prints the server address (`wss://niscord.example.com`) and a generated password
for your friends; pass `--password` to choose your own. To update later, copy the new
source and run the script again: it keeps the password and TURN secret. With a
release binary instead of building, pass `--binary ./niscord-server-linux-x86_64`.

Settings live in `/etc/niscord/niscord.env` (`sudo systemctl restart niscord-server`
after editing); logs with `journalctl -u niscord-server -f`.

### Configuration

The server is a single binary configured through environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `NISCORD_BIND` | `0.0.0.0:8080` | Address to listen on |
| `NISCORD_PASSWORD` | *(empty)* | Shared password to join. **Set this**, or anyone who finds the server can join |
| `NISCORD_STUN_URLS` | `stun:stun.l.google.com:19302` | Comma-separated STUN URLs |
| `NISCORD_TURN_URLS` | *(empty)* | Comma-separated TURN URLs, e.g. `turn:turn.example.com:3478?transport=udp` |
| `NISCORD_TURN_SECRET` | *(empty)* | coturn `static-auth-secret`; clients get short-lived credentials derived from it |
| `NISCORD_TURN_TTL_SECONDS` | `43200` | Lifetime of those credentials |
| `RUST_LOG` | `info` | Log verbosity |

```sh
cargo build --release -p niscord-server
NISCORD_PASSWORD=change-me ./target/release/niscord-server
```

The server speaks plain `ws://`; put it behind a TLS reverse proxy for `wss://` (the
install script uses Caddy). TURN matters: many home connections (mobile, CGNAT) can't
accept direct peer connections, and then video only flows through the relay. Niscord
uses TURN over UDP only. See `deploy/install.sh` for a working coturn configuration.

## Distributing the app

### Releases

Pushing a tag builds everything on GitHub Actions and publishes a release with
`niscord.exe`, the Windows and Linux (x86_64, static) server binaries and checksums.
Bump `version` in the workspace `Cargo.toml` first; the tag must match it:

```sh
git tag v0.2.0
git push origin v0.2.0
```

**Self-update:** the app checks GitHub for a newer release at start-up and every six
hours, downloads `niscord.exe` in the background, verifies it against the release's
`SHA256SUMS.txt`, and offers *Restart* in the sidebar (an update never interrupts a
stream). Development builds don't update themselves, and `NISCORD_NO_UPDATE=1`
turns it off.

Releases are public, so they don't carry a server address. Give friends their first
copy from a private build with the address filled in (below); the app saves it in
their settings, and later updates from public releases keep using it. (To bake an
address into release builds anyway, set the repository variable
`NISCORD_DEFAULT_SERVER`.)

### Building it yourself

```sh
NISCORD_DEFAULT_SERVER=wss://niscord.example.com cargo build --release -p niscord
```

### For your friends

`niscord.exe` is a single file with no installer and no runtime to install (the C
runtime is linked in). It needs Windows 10 version 2004 or later (Windows 11 is best:
it hides the capture border). The first time a friend watches or shares, Windows
Firewall asks whether Niscord may use the network: they should allow it (private
networks is enough), or direct connections between friends will fail and everything
will have to go through TURN.

The app keeps its settings in `%APPDATA%\Niscord\settings.json` (the password is
stored in plain text) and its log in `%APPDATA%\Niscord\niscord.log` (the previous
run's in `niscord.old.log`). If something goes wrong, that log is what to send.

## License notes

Slint is used under its royalty-free license, which requires attribution. The app's
About dialog shows it.
