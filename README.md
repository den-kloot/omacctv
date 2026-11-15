# OmaCCTV

A native camera app for Linux phones (and desktops): every camera live in one grid, a horizontal
timeline to slide through the day's recordings, and two-way audio. It is made for
[Frigate](https://frigate.video) NVRs with their built-in go2rtc, but it also plays any RTSP camera on
its own, without Frigate. Written in Rust with GTK4/libadwaita; ffmpeg does the decoding.

It was built for [Omarchy Mobile](https://github.com/derekross/omarchy-mobile) on a Pixel 9a, because Frigate's
web UI is slow and not smooth on a phone. The aim was something like the Reolink app: open it, see
everything, swipe back to the moment the parcel arrived.

<p>
  <img src="docs/grid.jpg" width="300" alt="Grid of all cameras, played back at 11:31 with the timeline at the bottom">
  &nbsp;
  <img src="docs/single.jpg" width="300" alt="The entrance camera big, with talk button and timeline">
</p>

*Real cameras (three Reolinks and a MacBook webcam) on Frigate 0.18, played back from the recordings.
The house number is pixelated and the basement is blurred for this README.*

## Features

- **Grid:** all cameras live (go2rtc sub streams), silent, with a LIVE dot. Wide cameras such as the
  Reolink Duo get a full row. Tap a camera to open it big, with sound.
- **Timeline:** a fixed yellow playhead with the day sliding underneath. Grey = footage, orange =
  detection, red = alert (Frigate review items). Drag (with momentum) to scrub: the picture follows
  the finger (Frigate snapshots, the next one as soon as the last has arrived). Play/pause is a
  lasting choice: while playing, a drag or tap plays on from the new spot (Frigate HLS VOD); while
  paused it stays paused and shows that moment. Tap a marker to jump to that event. Pinch or tap 1 h / 6 h / 24 h to zoom, tap the date to pick another day, LIVE to go
  back. Gaps without footage are skipped.
- **Speed:** 0.5x, 1x, 2x, 4x, 8x or 16x next to play/pause. From 8x (4x in the grid) only key
  frames are decoded; sound up to 2x. The clock follows the picture when the network can't keep up
  (a 4 MP camera's recordings are about 40 MB a minute, so 16x needs about 10 MB/s per camera).
- **Share:** in the single view, a picture of the moment (full size from the recording, or what is
  on screen live) or a video clip: start it, then play or slide to its end and tap Share (Frigate
  cuts the MP4, up to 10 min). Saved to Pictures/OmaCCTV or Videos/OmaCCTV, then Copy (a picture
  as PNG, a clip as a file), LocalSend or Show in Files.
- **No footage at the chosen time:** each camera shows its last recorded picture before then
  ("Last picture · 10:20").
- **Single view:** pinch to zoom, one finger to pan, double tap for 2.5x and back. Speaker button
  to mute.
- **Two-way audio:** a mic button for cameras with a speaker. Either straight from the phone to the
  camera over its ONVIF RTSP backchannel (G.711 mu-law), or through a small relay next to Frigate so
  that camera passwords never sit on the phone (see [Two-way audio](#two-way-audio)).
- **Command line for setup:** `omacctv scan` finds cameras and NVRs on the network, `add` and
  `remove` manage cameras and probe them (resolution, codec, does it have a speaker?), `check`
  tests the whole config. Everything prints plain text or JSON, so an AI agent can set it up too
  (see [AGENTS.md](AGENTS.md)).

## Install

Requirements: GTK 4.12+, libadwaita 1.6+, and at runtime `ffmpeg` and `ffprobe` with RTSP, HLS and
PulseAudio/PipeWire support (`pactl` for muting while you talk).

```sh
# Arch / Omarchy:  sudo pacman -S --needed rust gtk4 libadwaita ffmpeg libpulse
# Debian / Ubuntu: sudo apt install cargo libgtk-4-dev libadwaita-1-dev ffmpeg pulseaudio-utils
cargo build --release
./install.sh          # ~/.local/bin/omacctv + launcher entry + icon
```

Building on a phone works but is slow and needs a lot of RAM. Building on another aarch64 machine
and copying the binary over is easier (`./install.sh path/to/omacctv`).

## Set up

All configuration lives in one JSON file, `~/.config/omacctv/config.json`. The fastest way:

```sh
omacctv scan                      # what is on the network?
omacctv discover http://192.168.1.10:5000 --write   # write a config from Frigate's cameras
omacctv check                     # test it
omacctv                           # open the app
```

`omacctv scan` sends an ONVIF WS-Discovery probe and checks the local /24 for RTSP (554), go2rtc
(8554, 1984) and Frigate (5000). For each find it prints a ready `omacctv add` or `discover` line:

```
192.168.178.51  frigate · Frigate 0.18.2-cfa08c5  [ports 5000,8554]
                frigate cameras: entree, terasse, keller, keller-cam-macbook
                → omacctv discover http://192.168.178.51:5000 --write
192.168.178.222 onvif camera · RLC-540A · IPC-BO  [ports 554]
                RTSP needs a login
                → omacctv add rtsp://USER:PASSWORD@192.168.178.222:554/h264Preview_01_sub --full rtsp://USER:PASSWORD@192.168.178.222:554/h264Preview_01_main
```

### Adding and removing cameras

```sh
omacctv add entree --name Entrance          # a Frigate camera: live + recordings
omacctv add rtsp://admin:pw@192.168.1.20/h264Preview_01_sub \
    --full rtsp://admin:pw@192.168.1.20/h264Preview_01_main --name Garage   # any camera, live only
omacctv remove garage                       # by id or by URL
omacctv list
```

`add` probes the stream with ffprobe (size → tile shape and width; very big H.265 → the phone uses the
sub stream and decodes recordings with the loop filter skipped) and asks the camera itself whether
it has an ONVIF audio backchannel. If it has one, talking is switched on and goes straight to the
camera:

```
added camera entree (Entrance)
  video: 2560x1920 h264
  two-way audio: through the talk relay 192.168.178.51:8557 (it needs TALK_URL_entree)
```

Frigate and go2rtc don't pass the backchannel on, so for Frigate cameras give the camera's own URL
to test it (`--talk-url rtsp://USER:PASS@CAMERA/...`), or say `--talk` to use the relay. When the
file holds passwords it is saved with mode 600.

### The config file

Every key is optional. `{"frigate": "http://192.168.1.10:5000"}` alone is a complete config: the
cameras are then read from Frigate at start. List them to choose names, order, layout and talking.
[`examples/config.json`](examples/config.json) is the author's real setup.

| key | meaning |
|---|---|
| `frigate` | Frigate base URL, plain `http://` (LAN or VPN). `""` = no Frigate, live-only cameras. |
| `rtsp` | go2rtc RTSP base. Default: Frigate's host, port 8554. |
| `talk_relay` | `host:port` of `omacctv-talk-relay`. Empty = only direct talking. |
| `cameras[]` | the cameras in grid order. Without this list: all of Frigate's. |

Per camera:

| key | default | meaning |
|---|---|---|
| `id` | (required) | Frigate camera name (recordings, events, snapshots) |
| `name` | `id` | label on the tile |
| `live_grid` | `id` | go2rtc stream for the grid, e.g. `door_sub` |
| `live_full` | `live_grid` | go2rtc stream for the single view, e.g. `door_main` |
| `url` | none | a stream of its own (`rtsp://…`, `http://…`, anything ffmpeg plays) instead of go2rtc. Live only, no timeline. |
| `url_full` | `url` | bigger stream for the single view |
| `span` | 1 | 1 = half width, 2 = full row |
| `aspect` | 1.778 | width / height, for the tile shape before the first picture |
| `heavy_record` | false | recordings are big H.265: decode them with the loop filter skipped |
| `talk` | false (true with `talk_url`) | show the mic button |
| `talk_url` | none | the camera's own RTSP URL: talk straight to it over the ONVIF backchannel |

Keys starting with `_` are ignored (comments). Unknown keys are reported by `omacctv check`.
The config file is looked up at `--config PATH`, `$OMACCTV_CONFIG`,
`$XDG_CONFIG_HOME/omacctv/config.json` and `~/.config/omacctv/config.json`.

## Two-way audio

The phone's microphone is recorded with ffmpeg (PulseAudio `default` source), cut to 8 kHz G.711
mu-law, and sent one of two ways:

1. **Direct** (`talk_url` set): OmaCCTV opens the camera's RTSP stream with
   `Require: www.onvif.org/ver20/backchannel`, sets up the camera's `sendonly` PCMU track and sends
   RTP over the same TCP connection. It is the same exchange the relay does (used daily with a Reolink
   RLC-540A and Duo 2V), and should work with ONVIF Profile T cameras that have a speaker. The
   password is in the phone's config.
2. **Relay** (`talk_relay` set, no `talk_url`): the phone sends `TALK <camera>\n` plus the raw audio
   over TCP to [`relay/omacctv-talk-relay`](relay/), a single Python 3 file (standard library
   only) running next to Frigate. The relay holds the camera URLs in
   `/etc/omacctv-talk-relay.env` ([example](relay/talk-relay.env.example)) and talks to the
   backchannel itself. It can also play to a speaker on its own machine (`TALK_LOCAL`), for a USB
   webcam. Install: copy the script to `/usr/local/bin`, create a `omacctv-talk` system user in the
   `audio` group, and install and enable [the unit](relay/omacctv-talk-relay.service). Keep port 8557
   LAN-only: anyone who can reach it can talk through your cameras.

While you talk, the camera sound on the phone is muted so the speaker doesn't feed back.

## How it works

- `src/stream.rs`: one `ffmpeg` child per visible tile writes scaled RGB frames as PPM to a pipe.
  Live streams use `-fflags nobuffer -flags low_delay`. Recordings are read at `-readrate <speed>` with
  `-fps_mode passthrough` (otherwise ffmpeg duplicates frames) and `-progress`, so the clock knows
  where the picture is. Sound only in the single view, as a
  second ffmpeg output to PulseAudio (application name "OmaCCTV").
- `src/frigate.rs`: recordings (`/api/<cam>/recordings`), review items (`/api/review`), scrub
  snapshots (`/api/<cam>/recordings/<t>/snapshot.jpg`), HLS VOD (`/vod/<cam>/start/S/end/E/master.m3u8`;
  `index.m3u8` gives 404), clips (`/api/<cam>/start/S/end/E/clip.mp4`). The first picture of a tile is Frigate's `latest.jpg` until the stream's
  first key frame arrives.
- `src/rtsp.rs`: a small RTSP client (digest/basic auth, interleaved RTP) for DESCRIBE probes and the
  backchannel. `src/scan.rs`: WS-Discovery and the port sweep. `src/md5.rs`: MD5 for digest auth, so
  there's no crypto dependency.
- CPU on a Pixel 9a: four live tiles ≈ 0.8 cores, four recordings ≈ 2.5 cores. A 5120x1552 H.265 main
  stream only decodes at about real time on the phone, so use the sub stream live.

Command line, for scripts and agents:

```
omacctv [--config PATH] [COMMAND]
  list | add SOURCE [OPTIONS] | remove ID|URL | scan [NET] [--json]
  config [--raw] | discover [URL] [--write] | check | probe [SECONDS] [vod] | help
```

`omacctv probe 6` decodes every camera without a window and prints first-frame time and fps;
`probe 6 vod` does the same with the last minute of recordings. `OMACCTV_DEBUG=1` shows ffmpeg's output.

## Security

Frigate's API on port 5000 and go2rtc's RTSP on 8554 have no authentication. Only use them on your LAN
or over a VPN, never forwarded to the internet. OmaCCTV only speaks plain HTTP to Frigate for the same
reason. `omacctv config` and `list` hide passwords in URLs (`--raw` shows them).

## License

MIT
