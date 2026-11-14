# OmaCCTV for AI agents

How to install, configure and change OmaCCTV for a user. Read the [README](README.md) for what the
app does. Everything here works from a shell, with no window.

## Setting it up for a user

1. **Check what's there.** Run `omacctv check`. Exit code 0 means everything works. Every line starts
   with `ok` or `FAIL`; fix the FAIL lines.
2. **Find the cameras.** Run `omacctv scan --json` (or `omacctv scan 192.168.2.0/24` for another
   subnet). Each entry has `kind` (`frigate`, `go2rtc`, `onvif camera`, `rtsp`), `hardware` (the
   ONVIF model, e.g. `RLC-540A`), `rtsp_needs_login` and `next`: the commands to run next.
3. **There is a Frigate** (`kind: frigate`): prefer it, because recordings and the timeline only come
   from Frigate.
   - `omacctv discover http://HOST:5000 --write` writes `~/.config/omacctv/config.json` with all of
     Frigate's cameras. It refuses to overwrite an existing file; then print without `--write` and
     merge by hand.
   - Or pick cameras one by one: `omacctv add CAMERA_NAME --name "Front door"`.
4. **No Frigate:** `omacctv add rtsp://USER:PASS@IP:554/PATH --full rtsp://…/MAIN_PATH --name "…"`.
   Ask the user for the camera password; never guess or brute-force it. `scan` suggests the path for
   known brands (Reolink `/h264Preview_01_sub|main`, Hikvision `/Streaming/Channels/102|101`, Dahua
   and Amcrest `/cam/realmonitor?channel=1&subtype=1|0`, Tapo `/stream2|1`, …). Such cameras are
   live only.
5. **Two-way audio.** `add` reports `two-way audio: yes|no|not tested`.
   - `yes`: `talk_url` was set, so the phone talks straight to the camera.
   - For a Frigate camera, Frigate can't tell. If the user wants talking and agrees to keep the
     camera password on this device: `omacctv add CAM --talk-url rtsp://USER:PASS@CAMERA_IP:554/MAIN_PATH`.
     If the password must stay off the device, set up `relay/omacctv-talk-relay` on the Frigate host
     (README → Two-way audio), set `"talk_relay": "HOST:8557"` and run `omacctv add CAM --talk`.
6. **Verify:** `omacctv check`, then `omacctv probe 6`: every camera should show a first frame and
   about the camera's fps. `omacctv probe 6 vod` tests the recordings.
7. Tell the user to restart the app if it was open. The config is read only at start.

## Rules

- Passwords: `omacctv config` and `list` hide them; don't print the file with `cat`. Don't put camera
  passwords anywhere else (commit messages, chat, logs). The config is saved with mode 600 when it
  holds one.
- Frigate (5000) and go2rtc (8554, 1984) have no authentication: never expose them to the internet.
  The talk relay (8557) neither.
- Only scan networks the user owns. `scan` sends one multicast probe plus one TCP connect per
  host and port, nothing more.
- Write the config with `add`, `remove` or `discover --write`, or edit the JSON by hand. Both are
  fine; keep keys that start with `_` (the user's comments).

## Editing the config by hand

The format is in the README (Config file). Things to know:

- Without a `cameras` array, all of Frigate's cameras are shown. Once the array exists, only those.
  `add` and `remove` turn the discovered list into an explicit one first, so nothing disappears.
- Camera order in the array = order in the grid. `span: 2` = a full row (use it for wide cameras,
  aspect > 2.2).
- `live_grid` and `live_full` are go2rtc stream names (see Frigate → go2rtc → streams, or
  `http://HOST:5000/api/go2rtc/streams`); usually `<cam>_sub` and `<cam>_main`.
- A 4K+ H.265 main stream is too heavy for a phone to decode live: use the sub stream for
  `live_full` too, and set `heavy_record: true`.

## Changing the code

- Rust 2021, GTK4 0.9 / libadwaita 0.7 bindings, `serde_json` with `preserve_order`. No async
  runtime: worker threads, `async_channel` back to the GTK main loop.
- `src/main.rs`: UI and the state machine (`Mode::Live | Play | Scrub`; `sync()` starts and stops
  streams to match). `src/config.rs`: the file format plus Frigate discovery (unit tests there).
  `src/cli.rs`: the commands. `src/rtsp.rs`, `src/scan.rs`, `src/talk.rs`, `src/stream.rs`,
  `src/frigate.rs`, `src/timeline.rs` (the cairo-drawn timeline).
- `cargo test` runs the parsing tests (config, SDP, RTSP URLs, WS-Discovery, MD5, PPM). They need no
  network.
- UI text is English.
- Big builds on a phone can run out of memory. Build on another aarch64 machine if you can
  (`contrib/build-m5.sh` is the author's setup).
- Test the GUI on the device: `omacctv probe` for streams. For the UI, run it and take a screenshot
  (`grim` on wlroots/Hyprland).
