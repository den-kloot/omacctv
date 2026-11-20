//! omacctv: all Frigate cameras live in one grid (multiview), tap one for the big
//! picture, and a horizontal timeline at the bottom to slide back through the recordings.
//!
//! Live pictures come from go2rtc (inside Frigate) over RTSP, recordings from Frigate's
//! HLS VOD; both are decoded by the system ffmpeg (see stream.rs).

mod cli;
mod config;
mod md5;
mod rtsp;
mod scan;
mod share;
mod frigate;
mod fullview;
mod stream;
mod talk;
mod timeline;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::{gdk, glib};

use config::Config;
use stream::{Slot, Stream};
use timeline::{fmt, Timeline};

const APP_ID: &str = "lu.kloot.OmaCCTV";

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn main() -> glib::ExitCode {
    // `--config PATH` is read by Config::load; strip it from the rest.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--config" || a == "-c") {
        args.drain(i..(i + 2).min(args.len()));
    }
    match args.first().map(String::as_str) {
        Some("probe") => std::process::exit(probe(&args[1..])),
        Some("config") => std::process::exit(cli::config(&args[1..])),
        Some("list" | "ls") => std::process::exit(cli::list()),
        Some("add") => std::process::exit(cli::add(&args[1..])),
        Some("remove" | "rm") => std::process::exit(cli::remove(&args[1..])),
        Some("scan") => std::process::exit(scan::run(&args[1..])),
        Some("discover") => std::process::exit(cli::discover(&args[1..])),
        Some("check") => std::process::exit(cli::check()),
        Some("help" | "--help" | "-h") => {
            print!("{}", cli::HELP);
            return glib::ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("omacctv {}", env!("CARGO_PKG_VERSION"));
            return glib::ExitCode::SUCCESS;
        }
        Some(other) => {
            eprint!("omacctv: unknown command \"{other}\"\n\n{}", cli::HELP);
            std::process::exit(2);
        }
        None => {}
    }
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| match app.active_window() {
        Some(w) => w.present(),
        None => build(app),
    });
    app.run_with_args::<&str>(&[])
}

/// `omacctv probe [SECONDS] [vod]`: frames per second of every camera, no window.
fn probe(args: &[String]) -> i32 {
    let secs: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(6.0);
    let vod = args.iter().any(|a| a == "vod");
    let cfg = Config::load();
    let data = if vod {
        match frigate::load(&cfg, 1.0) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("frigate: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    if let Some(d) = &data {
        for (c, cam) in cfg.cameras.iter().enumerate() {
            let footage: f64 = d.runs[c].iter().map(|r| r.1 - r.0).sum();
            let n = d.events.iter().filter(|e| e.cam == c).count();
            println!("{:<20} {:>5} runs, {:>6.0} s footage, {n} events (24 h)", cam.id, d.runs[c].len(), footage);
        }
    }
    let mut streams = Vec::new();
    let t0 = Instant::now();
    for (c, cam) in cfg.cameras.iter().enumerate() {
        let slot: Slot = Default::default();
        let args = match &data {
            None => stream::live_args(&cam.live_url(&cfg.rtsp, false), 640, false),
            Some(d) => {
                // Last minute of footage that is at least 30 s long.
                let Some(r) = d.runs[c].iter().rev().find(|r| r.1 - r.0 > 30.0) else {
                    println!("{:<20} no recording to play", cam.id);
                    continue;
                };
                stream::vod_args(&frigate::vod_url(&cfg, &cam.id, (r.1 - 60.0).max(r.0), r.1), 640, cam.heavy_record, false, 1.0, false)
            }
        };
        streams.push((cam.id.clone(), slot.clone(), Stream::spawn(args, slot, data.is_none())));
    }
    let mut first: Vec<Option<f64>> = vec![None; streams.len()];
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_millis(50));
        for (k, (_, slot, _)) in streams.iter().enumerate() {
            if first[k].is_none() && slot.lock().unwrap().count > 0 {
                first[k] = Some(t0.elapsed().as_secs_f64());
            }
        }
    }
    for (k, (id, slot, _)) in streams.iter().enumerate() {
        let s = slot.lock().unwrap();
        let size = s.frame.as_ref().map(|f| format!("{}x{}", f.width, f.height)).unwrap_or("-".into());
        let fps = first[k].map(|f| s.count as f64 / (secs - f).max(0.1)).unwrap_or(0.0);
        println!(
            "{id:<20} first frame {:>5}  {fps:5.1} fps  {size}",
            first[k].map(|f| format!("{f:.2}s")).unwrap_or("never".into())
        );
    }
    0
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Live,
    Play,
    Scrub,
}

struct Tile {
    cam: Cell<usize>,
    full: bool,
    root: gtk::Overlay,
    picture: gtk::Picture,
    name: gtk::Label,
    live: gtk::Box,
    empty: gtk::Label,
    /// "Last picture · 16:52" when showing the last picture before the playhead.
    note: gtk::Label,
    /// Time of the still currently requested (NaN = none).
    still_at: Cell<f64>,
    /// Single view only: pinch zoom.
    sw: Option<gtk::ScrolledWindow>,
    zoom: Cell<f64>,
    /// Scroll position to apply once the zoomed size is laid out: (x, y, width, height).
    pending: Cell<Option<(f64, f64, f64, f64)>>,
    slot: Slot,
    shown: Cell<u64>,
    stream: RefCell<Option<Stream>>,
    key: RefCell<String>,
    /// Frame count when the current stream started (first new frame = it plays).
    started: Cell<u64>,
    snap_req: Cell<u64>,
    /// Footage time where the tile's recording stream started (its progress counts from here).
    vod_t0: Cell<f64>,
    /// A still picture is being fetched (one at a time per tile; the next waits for it).
    fetching: Arc<AtomicBool>,
}

impl Tile {
    fn new(full: bool) -> Rc<Tile> {
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(if full { gtk::ContentFit::Contain } else { gtk::ContentFit::Cover });
        let root = gtk::Overlay::new();
        let sw = full.then(|| {
            let sw = gtk::ScrolledWindow::new();
            sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
            sw.set_child(Some(&picture));
            root.set_child(Some(&sw));
            sw
        });
        if !full {
            root.set_child(Some(&picture));
        }
        root.set_overflow(gtk::Overflow::Hidden);
        root.add_css_class(if full { "tile-full" } else { "tile" });

        let name = gtk::Label::new(None);
        name.add_css_class("pill");
        name.set_halign(gtk::Align::Start);
        name.set_valign(gtk::Align::End);
        name.set_margin_start(8);
        name.set_margin_bottom(8);
        name.set_visible(!full);
        root.add_overlay(&name);

        let live = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        live.add_css_class("pill");
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("live-dot");
        dot.set_valign(gtk::Align::Center);
        live.append(&dot);
        live.append(&gtk::Label::new(Some("LIVE")));
        live.set_halign(gtk::Align::Start);
        live.set_valign(gtk::Align::Start);
        live.set_margin_start(8);
        live.set_margin_top(8);
        root.add_overlay(&live);

        let empty = gtk::Label::new(Some("No footage"));
        empty.add_css_class("empty");
        empty.set_halign(gtk::Align::Center);
        empty.set_valign(gtk::Align::Center);
        empty.set_visible(false);
        root.add_overlay(&empty);

        let note = gtk::Label::new(None);
        note.add_css_class("pill");
        note.add_css_class("note");
        note.set_halign(gtk::Align::End);
        note.set_valign(gtk::Align::Start);
        note.set_margin_end(8);
        note.set_margin_top(8);
        note.set_visible(false);
        root.add_overlay(&note);

        Rc::new(Tile {
            cam: Cell::new(0),
            full,
            root,
            picture,
            name,
            live,
            empty,
            note,
            still_at: Cell::new(f64::NAN),
            sw,
            zoom: Cell::new(1.0),
            pending: Cell::new(None),
            slot: Default::default(),
            shown: Cell::new(0),
            stream: RefCell::new(None),
            key: RefCell::new(String::new()),
            started: Cell::new(0),
            snap_req: Cell::new(0),
            fetching: Default::default(),
            vod_t0: Cell::new(0.0),
        })
    }

    fn ensure(&self, key: String, args: impl FnOnce() -> Vec<String>, restart: bool) {
        if *self.key.borrow() == key {
            return;
        }
        self.stream.replace(None);
        self.still_at.set(f64::NAN);
        self.note.set_visible(false);
        self.started.set(self.slot.lock().unwrap().count);
        *self.stream.borrow_mut() = Some(Stream::spawn(args(), self.slot.clone(), restart));
        *self.key.borrow_mut() = key;
    }

    fn stop(&self, key: &str) {
        if *self.key.borrow() != key {
            self.stream.replace(None);
            *self.key.borrow_mut() = key.into();
        }
    }

    fn has_new_frame(&self) -> bool {
        self.slot.lock().unwrap().count > self.started.get()
    }

    fn clear(&self) {
        self.still_at.set(f64::NAN);
        self.note.set_visible(false);
        self.picture.set_paintable(None::<&gdk::Paintable>);
        let mut s = self.slot.lock().unwrap();
        s.frame = None;
        s.jpeg = None;
    }

    /// Zoom to `z` (1 = whole picture) keeping the point (fx, fy) of the view under the fingers.
    fn set_zoom(&self, z: f64, fx: f64, fy: f64) {
        let Some(sw) = &self.sw else { return };
        let z = z.clamp(1.0, 8.0);
        let (w, h) = (sw.width() as f64, sw.height() as f64);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        if z <= 1.01 {
            self.zoom.set(1.0);
            self.pending.set(None);
            self.picture.set_size_request(-1, -1);
            sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
            return;
        }
        // Size of the picture at zoom 1 (contained), then scaled.
        let a = self.picture.paintable().map(|p| p.intrinsic_aspect_ratio()).unwrap_or(0.0);
        let (cw, ch) = if a <= 0.0 {
            (w, h)
        } else if w / h > a {
            (h * a, h)
        } else {
            (w, w / a)
        };
        let (nw, nh) = ((cw * z).max(w), (ch * z).max(h));
        let (ha, va) = (sw.hadjustment(), sw.vadjustment());
        let (ow, oh) = if self.zoom.get() > 1.01 { (ha.upper().max(w), va.upper().max(h)) } else { (w, h) };
        let (rx, ry) = ((ha.value() + fx) / ow, (va.value() + fy) / oh);
        self.zoom.set(z);
        sw.set_policy(gtk::PolicyType::External, gtk::PolicyType::External);
        self.picture.set_size_request(nw.round() as i32, nh.round() as i32);
        self.pending.set(Some((rx * nw - fx, ry * nh - fy, nw, nh)));
    }

    /// Applies the scroll position of the last zoom step once the new size is laid out.
    fn apply_zoom_scroll(&self) {
        let (Some(sw), Some((x, y, nw, nh))) = (&self.sw, self.pending.get()) else { return };
        let (ha, va) = (sw.hadjustment(), sw.vadjustment());
        if (ha.upper() - nw).abs() < 2.0 && (va.upper() - nh).abs() < 2.0 {
            ha.set_value(x);
            va.set_value(y);
            self.pending.set(None);
        }
    }

    /// Puts the newest decoded frame (or scrub JPEG) on screen.
    fn present(&self) {
        let (frame, jpeg) = {
            let mut s = self.slot.lock().unwrap();
            let frame = if s.count != self.shown.get() {
                self.shown.set(s.count);
                s.frame.take()
            } else {
                None
            };
            let jpeg = match &s.jpeg {
                Some((id, _)) if *id == self.snap_req.get() => s.jpeg.take().map(|j| j.1),
                _ => None,
            };
            (frame, jpeg)
        };
        if let Some(f) = frame {
            let stride = (f.width * 3) as usize;
            let bytes = glib::Bytes::from_owned(f.rgb);
            let tex = gdk::MemoryTexture::new(f.width, f.height, gdk::MemoryFormat::R8g8b8, &bytes, stride);
            self.picture.set_paintable(Some(&tex));
        }
        if let Some(j) = jpeg {
            if let Ok(tex) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(j)) {
                self.picture.set_paintable(Some(&tex));
            }
        }
    }
}

struct App {
    cfg: Config,
    data: RefCell<frigate::Data>,
    mode: Cell<Mode>,
    /// Playback / scrub position at `since` (or frozen while `since` is None or paused).
    base: Cell<f64>,
    since: Cell<Option<Instant>>,
    paused: Cell<bool>,
    /// The user wants playback (▶ pressed / LIVE): scrubbing or tapping the timeline keeps
    /// playing from the new spot; after a pause it holds still at the new spot instead.
    want_play: Cell<bool>,
    /// Playback speed (one of stream::RATES).
    rate: Cell<f64>,
    seek_at: Cell<Instant>,
    /// Paused showing still pictures of `base` (seek while paused; ▶ = play from there or
    /// from the next footage).
    gap_hold: Cell<bool>,
    /// Sound in the single view (the grid is always silent).
    sound: Cell<bool>,
    /// Talking to the shown camera (two-way audio), if active.
    talking: RefCell<Option<talk::Talk>>,
    talk_btn: gtk::Button,
    talk_status: gtk::Label,
    talk_tx: async_channel::Sender<talk::Event>,
    gen: Cell<u64>,
    focus: Cell<Option<usize>>,
    tiles: Vec<Rc<Tile>>,
    single: Rc<Tile>,
    single_page: adw::NavigationPage,
    nav: adw::NavigationView,
    grid: gtk::Grid,
    last_width: Cell<i32>,
    timeline: Timeline,
    time_label: gtk::Label,
    full: Rc<fullview::FullView>,
    date_label: gtk::Label,
    live_btn: gtk::Button,
    play_btn: gtk::Button,
    speed_btn: gtk::MenuButton,
    spans: Vec<(f64, gtk::ToggleButton)>,
    shown_second: Cell<i64>,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    /// Start of the clip being chosen (single view); the playhead is its other end.
    clip_start: Cell<Option<f64>>,
    clip_bar: gtk::Box,
    clip_label: gtk::Label,
    clip_go: gtk::Button,
    /// A clip is being cut and downloaded.
    clip_busy: Cell<bool>,
    share_tx: async_channel::Sender<ShareDone>,
}

/// A picture or clip saved for sharing (or why not).
struct ShareDone {
    result: Result<std::path::PathBuf, String>,
    video: bool,
}

impl App {
    fn time(&self) -> f64 {
        match self.mode.get() {
            Mode::Live => now(),
            Mode::Scrub => self.base.get(),
            Mode::Play => match (self.paused.get(), self.since.get()) {
                (false, Some(s)) => {
                    let wall = self.base.get() + s.elapsed().as_secs_f64() * self.rate.get();
                    match self.shown_progress() {
                        // Not further ahead of the picture than normal: reports come every
                        // 0.2 s, and fast playback shows only key frames (~2 s apart).
                        Some(p) => wall.min(p + 2.5 + 0.6 * self.rate.get()),
                        None => wall,
                    }
                }
                _ => self.base.get(),
            },
        }
    }

    /// Footage time the furthest running recording stream has reached.
    fn shown_progress(&self) -> Option<f64> {
        self.active()
            .iter()
            .filter(|t| t.key.borrow().starts_with("vod:"))
            .filter_map(|t| t.slot.lock().unwrap().progress.map(|p| t.vod_t0.get() + p))
            .reduce(f64::max)
    }

    fn active(&self) -> Vec<Rc<Tile>> {
        match self.focus.get() {
            Some(_) => vec![self.single.clone()],
            None => self.tiles.clone(),
        }
    }

    fn active_cams(&self) -> Vec<usize> {
        match self.focus.get() {
            Some(c) => vec![c],
            None => (0..self.cfg.cameras.len()).collect(),
        }
    }

    fn go_live(&self) {
        self.mode.set(Mode::Live);
        self.paused.set(false);
        self.want_play.set(true);
        self.gap_hold.set(false);
        self.gen.set(self.gen.get() + 1);
        self.sync();
    }

    fn seek(&self, t: f64) {
        self.start_at(t, true);
    }

    fn start_at(&self, t: f64, user: bool) {
        if t >= now() - 4.0 {
            return self.go_live();
        }
        self.mode.set(Mode::Play);
        self.base.set(t);
        self.since.set(None);
        self.seek_at.set(Instant::now());
        // A seek while paused stays paused and shows the pictures at the new spot.
        let hold = user && !self.want_play.get();
        self.paused.set(hold);
        self.gap_hold.set(hold);
        self.gen.set(self.gen.get() + 1);
        self.sync();
    }

    fn set_rate(&self, rate: f64) {
        self.speed_btn.set_label(&stream::rate_label(rate));
        if rate == self.rate.get() {
            return;
        }
        let playing = self.mode.get() == Mode::Play && !self.paused.get();
        let t = self.time();
        self.rate.set(rate);
        if playing {
            // Same moment, now at the new speed.
            self.start_at(t, false);
        }
    }

    fn scrub(&self, t: f64) {
        self.mode.set(Mode::Scrub);
        self.base.set(t);
        self.sync();
    }

    fn toggle_pause(&self) {
        match self.mode.get() {
            Mode::Live => {
                self.mode.set(Mode::Play);
                self.base.set(now());
                self.paused.set(true);
                self.want_play.set(false);
            }
            Mode::Scrub => {
                // Paused at the scrub position (its preview stays on screen).
                self.paused.set(true);
                self.gap_hold.set(true);
                self.want_play.set(false);
                self.mode.set(Mode::Play);
            }
            Mode::Play if self.paused.get() => {
                self.want_play.set(true);
                let (cams, t) = (self.active_cams(), self.base.get());
                let data = self.data.borrow();
                if !data.loaded || cams.iter().any(|&c| data.run_at(c, t).is_some()) {
                    drop(data);
                    return self.start_at(t, false);
                }
                // ▶ in a gap: play from the next footage.
                let next = data.next_start(&cams, t);
                drop(data);
                return match next {
                    Some(n) => self.start_at(n, false),
                    None => self.go_live(),
                };
            }
            Mode::Play => {
                self.base.set(self.time());
                self.paused.set(true);
                self.want_play.set(false);
            }
        }
        self.gen.set(self.gen.get() + 1);
        self.sync();
    }

    /// Starts, keeps or stops each tile's stream so it matches the mode and the playhead.
    fn sync(&self) {
        let active = self.active();
        for t in self.tiles.iter().chain(std::iter::once(&self.single)) {
            if !active.iter().any(|a| Rc::ptr_eq(a, t)) {
                t.stop("off");
            }
        }
        let mode = self.mode.get();
        match mode {
            Mode::Live => {
                for tile in &active {
                    let cam = &self.cfg.cameras[tile.cam.get()];
                    let url = cam.live_url(&self.cfg.rtsp, tile.full);
                    let width = if tile.full { 1440 } else if cam.span == 2 { 1080 } else { 640 };
                    let fresh = !tile.key.borrow().starts_with("live:");
                    let audio = tile.full && self.sound.get();
                    tile.ensure(format!("live:{url}:{audio}"), || stream::live_args(&url, width, audio), true);
                    if fresh {
                        tile.empty.set_visible(false);
                        tile.note.set_visible(false);
                    }
                    if fresh && cam.recorded() && self.cfg.has_frigate {
                        // Show Frigate's latest picture until the stream's first key frame.
                        let req = tile.snap_req.get() + 1;
                        tile.snap_req.set(req);
                        let (slot, started) = (tile.slot.clone(), tile.started.get());
                        let url = format!(
                            "{}/api/{}/latest.jpg?height={}",
                            self.cfg.frigate,
                            cam.id,
                            if tile.full { 720 } else { 360 }
                        );
                        std::thread::spawn(move || {
                            if let Ok(j) = frigate::get(&url, Duration::from_secs(4)) {
                                let mut s = slot.lock().unwrap();
                                if s.count == started {
                                    s.jpeg = Some((req, j));
                                }
                            }
                        });
                    }
                }
            }
            Mode::Scrub => {
                for tile in &active {
                    tile.stop("scrub");
                }
            }
            Mode::Play if self.paused.get() => {
                let t = self.base.get();
                for tile in &active {
                    tile.stop("paused");
                    if self.gap_hold.get() {
                        self.still(tile, t);
                    }
                }
            }
            Mode::Play => {
                // The picture fell behind the clock (too fast for the network or decoder):
                // the clock goes on from the picture.
                if let Some(s) = self.since.get() {
                    let t = self.time();
                    if self.base.get() + s.elapsed().as_secs_f64() * self.rate.get() > t + 0.01 {
                        self.base.set(t);
                        self.since.set(Some(Instant::now()));
                    }
                }
                let data = self.data.borrow();
                let cams = self.active_cams();
                let mut t = self.time();
                if data.loaded && !cams.iter().any(|&c| data.run_at(c, t).is_some()) {
                    // Playback (or a seek while playing) ran into a gap: jump to the next footage.
                    match data.next_start(&cams, t) {
                        Some(n) => {
                            self.base.set(n);
                            self.since.set(None);
                            self.seek_at.set(Instant::now());
                            self.gen.set(self.gen.get() + 1);
                            t = n;
                        }
                        None => {
                            drop(data);
                            return self.go_live();
                        }
                    }
                }
                for tile in &active {
                    let c = tile.cam.get();
                    let cam = &self.cfg.cameras[c];
                    match data.run_at(c, t) {
                        Some(run) => {
                            let audio = tile.full && self.sound.get();
                            let rate = self.rate.get();
                            let key = format!("vod:{c}:{}:{:.0}:{}:{audio}:{rate}", tile.full, run.0, self.gen.get());
                            let url = frigate::vod_url(&self.cfg, &cam.id, t, run.1 + 1.0);
                            let width = if tile.full { 1440 } else if cam.span == 2 { 1080 } else { 640 };
                            tile.ensure(
                                key,
                                || {
                                    tile.vod_t0.set(t);
                                    stream::vod_args(&url, width, cam.heavy_record, audio, rate, tile.full)
                                },
                                false,
                            );
                            tile.empty.set_visible(false);
                            tile.note.set_visible(false);
                        }
                        None => {
                            tile.stop("still");
                            self.still(tile, t);
                        }
                    }
                }
                // The clock starts when the first picture is there (VOD start takes a moment).
                if self.since.get().is_none()
                    && (active.iter().any(|t| t.has_new_frame()) || self.seek_at.get().elapsed() > Duration::from_secs(3))
                {
                    self.since.set(Some(Instant::now()));
                }
            }
        }
        for tile in &active {
            tile.live.set_visible(mode == Mode::Live && !tile.full);
        }
        self.update_controls();
    }

    /// Scrub previews: the picture at the playhead for every shown camera.
    fn request_snapshots(&self) {
        let t = self.base.get();
        for tile in self.active() {
            self.still(&tile, t);
        }
    }

    /// A still picture for time `t`: the recording at `t`, or else the last picture recorded
    /// before `t` (with a note saying when), or "No footage" when there is none.
    fn still(&self, tile: &Tile, t: f64) {
        if tile.fetching.load(Ordering::SeqCst) {
            return;
        }
        let data = self.data.borrow();
        let c = tile.cam.get();
        let (at, before) = if data.run_at(c, t).is_some() {
            (t, None)
        } else if let Some(r) = data.last_before(c, t) {
            (r.1 - 0.5, Some(r.1))
        } else {
            if tile.picture.paintable().is_some() || !tile.still_at.get().is_nan() {
                tile.clear();
            }
            tile.empty.set_text(if self.cfg.cameras[c].recorded() { "No footage" } else { "Live only" });
            tile.empty.set_visible(data.loaded);
            return;
        };
        tile.empty.set_visible(false);
        match before {
            Some(end) => {
                let day = |x: f64| fmt(x, "%Y%m%d");
                let when = if day(end) == day(t) { fmt(end, "%H:%M") } else { fmt(end, "%a %e, %H:%M") };
                let text = format!("Last picture · {when}");
                if tile.note.text() != text {
                    tile.note.set_text(&text);
                }
                tile.note.set_visible(true);
            }
            None => tile.note.set_visible(false),
        }
        if (tile.still_at.get() - at).abs() < 0.01 || tile.fetching.load(Ordering::SeqCst) {
            // Same picture, or the previous one is still on its way: the caller asks again.
            return;
        }
        tile.still_at.set(at);
        let req = tile.snap_req.get() + 1;
        tile.snap_req.set(req);
        let (cfg, id, slot) = (self.cfg.clone(), self.cfg.cameras[c].id.clone(), tile.slot.clone());
        let height = if tile.full { 1080 } else { 360 };
        let fetching = tile.fetching.clone();
        fetching.store(true, Ordering::SeqCst);
        std::thread::spawn(move || {
            if let Ok(j) = frigate::snapshot(&cfg, &id, at, height) {
                slot.lock().unwrap().jpeg = Some((req, j));
            }
            fetching.store(false, Ordering::SeqCst);
        });
    }

    fn update_controls(&self) {
        let live = self.mode.get() == Mode::Live;
        if live {
            self.live_btn.add_css_class("on");
        } else {
            self.live_btn.remove_css_class("on");
        }
        let playing = match self.mode.get() {
            Mode::Live => true,
            // While scrubbing the button shows what happens on release.
            Mode::Scrub => self.want_play.get(),
            Mode::Play => !self.paused.get(),
        };
        self.play_btn
            .set_icon_name(if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
    }

    fn update_clock(&self) {
        let t = self.time();
        let sec = t.floor() as i64;
        if sec != self.shown_second.get() {
            self.shown_second.set(sec);
            self.time_label.set_text(&fmt(t, "%H:%M:%S"));
            let day = |x: f64| fmt(x, "%Y%m%d");
            let date = if day(t) == day(now()) {
                "Today".to_string()
            } else if day(t) == day(now() - 86400.0) {
                "Yesterday".to_string()
            } else {
                fmt(t, "%A, %B %e")
            };
            self.date_label.set_text(&date);
        }
        if !self.timeline.busy() {
            self.timeline.set_position(t, now());
        }
    }

    fn refresh_timeline(&self) {
        let data = self.data.borrow();
        let cams = self.active_cams();
        let events = data
            .events
            .iter()
            .filter(|e| cams.contains(&e.cam))
            .map(|e| (e.start, e.end, e.alert))
            .collect();
        self.timeline.set_data(data.union(&cams), events);
    }

    fn set_span(&self, span: f64) {
        self.timeline.set_span(span);
        for (s, b) in &self.spans {
            b.set_active((s - span).abs() < 1.0);
        }
    }

    /// Tile heights follow the grid width (two columns, full-width tiles span both).
    fn layout(&self) {
        let w = self.grid.width();
        if w <= 0 || w == self.last_width.get() {
            return;
        }
        self.last_width.set(w);
        let col = (w - self.grid.column_spacing() as i32) as f64 / 2.0;
        for t in &self.tiles {
            let cam = &self.cfg.cameras[t.cam.get()];
            let width = if cam.span == 2 { w as f64 } else { col };
            t.root.set_size_request(-1, (width / cam.aspect).round() as i32);
        }
    }

    fn talk_toggle(&self) {
        if self.talking.borrow().is_some() {
            self.talk_stop();
            return;
        }
        let Some(c) = self.focus.get() else { return };
        if self.mode.get() != Mode::Live {
            self.go_live();
        }
        self.talk_btn.add_css_class("on");
        self.talk_status.set_text("Connecting …");
        self.talk_status.set_visible(true);
        *self.talking.borrow_mut() =
            Some(talk::Talk::start(self.cfg.talk_relay.clone(), self.cfg.cameras[c].clone(), self.talk_tx.clone()));
    }

    fn talk_stop(&self) {
        // Dropping stops the microphone; the thread then reports Ended.
        self.talking.replace(None);
        self.talk_btn.remove_css_class("on");
        self.talk_status.set_visible(false);
    }

    fn talk_event(&self, e: talk::Event) {
        match e {
            talk::Event::Talking => {
                if self.talking.borrow().is_some() {
                    self.talk_status.set_text("Talking · tap to stop");
                }
            }
            talk::Event::Ended(err) => {
                let was = self.talking.borrow().is_some();
                self.talking.replace(None);
                self.talk_btn.remove_css_class("on");
                match err {
                    Some(msg) if was => {
                        self.talk_status.set_text(&msg);
                        self.talk_status.set_visible(true);
                        let l = self.talk_status.clone();
                        glib::timeout_add_local_once(Duration::from_secs(4), move || l.set_visible(false));
                    }
                    _ => self.talk_status.set_visible(false),
                }
            }
        }
    }

    fn toast(&self, msg: &str) {
        let t = adw::Toast::new(msg);
        t.set_timeout(3);
        self.toasts.add_toast(t);
    }

    /// The picture on screen in the single view, as PNG.
    fn shown_png(&self) -> Option<Vec<u8>> {
        let p = self.single.picture.paintable()?;
        let tex = p.downcast::<gdk::Texture>().ok()?;
        Some(tex.save_to_png_bytes().to_vec())
    }

    /// Saves the picture at the playhead: from the recording in full size when there is one,
    /// else what is on screen (live, or a camera without recordings).
    fn share_picture(&self) {
        let Some(c) = self.focus.get() else { return };
        let cam = self.cfg.cameras[c].clone();
        let t = self.time();
        let shown = self.shown_png();
        let recording = self.mode.get() != Mode::Live
            && cam.recorded()
            && self.cfg.has_frigate
            && self.data.borrow().run_at(c, t).is_some();
        if !recording {
            let Some(png) = shown else { return self.toast("No picture yet") };
            let path = share::out_path(false, &cam.id, t, "png");
            let result = std::fs::write(&path, png).map(|_| path).map_err(|e| e.to_string());
            let _ = self.share_tx.try_send(ShareDone { result, video: false });
            return;
        }
        let (cfg, tx) = (self.cfg.clone(), self.share_tx.clone());
        std::thread::spawn(move || {
            let result = match frigate::snapshot(&cfg, &cam.id, t, 0) {
                Ok(jpg) => {
                    let path = share::out_path(false, &cam.id, t, "jpg");
                    std::fs::write(&path, jpg).map(|_| path).map_err(|e| e.to_string())
                }
                Err(e) => match shown {
                    Some(png) => {
                        let path = share::out_path(false, &cam.id, t, "png");
                        std::fs::write(&path, png).map(|_| path).map_err(|e| e.to_string())
                    }
                    None => Err(e),
                },
            };
            let _ = tx.send_blocking(ShareDone { result, video: false });
        });
    }

    fn clip_begin(&self) {
        let t = self.time();
        self.clip_start.set(Some(t));
        self.clip_busy.set(false);
        self.timeline.set_clip(Some(t));
        self.talk_btn.set_visible(false);
        self.clip_bar.set_visible(true);
        self.clip_label.set_text("");
        self.update_clip();
    }

    fn clip_range(&self) -> Option<(f64, f64)> {
        let s = self.clip_start.get()?;
        let t = self.time().min(now());
        Some((s.min(t), s.max(t)))
    }

    /// The clip bar: length and times of the range chosen so far.
    fn update_clip(&self) {
        let Some((a, b)) = self.clip_range() else { return };
        if self.clip_busy.get() {
            return;
        }
        let len = b - a;
        let text = if len < 1.0 {
            "Play or slide to the clip's end".to_string()
        } else if len > share::MAX_CLIP {
            format!("{} · longer than {} min", share::duration_label(len), (share::MAX_CLIP / 60.0) as i64)
        } else {
            format!("{} · {} – {}", share::duration_label(len), fmt(a, "%H:%M:%S"), fmt(b, "%H:%M:%S"))
        };
        if self.clip_label.text() != text {
            self.clip_label.set_text(&text);
        }
        self.clip_go.set_sensitive(len >= 1.0 && len <= share::MAX_CLIP);
    }

    fn clip_cancel(&self) {
        self.clip_start.set(None);
        self.clip_busy.set(false);
        self.timeline.set_clip(None);
        self.clip_bar.set_visible(false);
        if let Some(c) = self.focus.get() {
            self.talk_btn.set_visible(self.cfg.cameras[c].can_talk(&self.cfg.talk_relay));
        }
    }

    /// Frigate cuts the chosen range into one MP4; it is saved to Videos/OmaCCTV.
    fn clip_share(&self) {
        let (Some(c), Some((a, b))) = (self.focus.get(), self.clip_range()) else { return };
        self.clip_busy.set(true);
        self.clip_go.set_sensitive(false);
        self.clip_label.set_text("Preparing clip …");
        let (cfg, id, tx) = (self.cfg.clone(), self.cfg.cameras[c].id.clone(), self.share_tx.clone());
        std::thread::spawn(move || {
            let path = share::out_path(true, &id, a, "mp4");
            let result = frigate::download(&frigate::clip_url(&cfg, &id, a, b), &path, Duration::from_secs(120)).map(|_| path);
            let _ = tx.send_blocking(ShareDone { result, video: true });
        });
    }

    fn share_done(self: &Rc<Self>, d: ShareDone) {
        if d.video {
            if d.result.is_ok() {
                self.clip_cancel();
            } else {
                self.clip_busy.set(false);
                self.update_clip();
            }
        }
        let path = match d.result {
            Ok(p) => p,
            Err(e) => return self.toast(&format!("Couldn't save: {e}")),
        };
        let shown = path.strip_prefix(glib::home_dir()).map(|p| p.display().to_string()).unwrap_or(path.display().to_string());
        let dialog = adw::AlertDialog::new(Some(if d.video { "Clip saved" } else { "Picture saved" }), Some(&shown));
        if !d.video {
            let pic = gtk::Picture::for_filename(&path);
            pic.set_can_shrink(true);
            pic.set_size_request(-1, 170);
            pic.add_css_class("share-preview");
            dialog.set_extra_child(Some(&pic));
        }
        dialog.add_response("copy", "Copy");
        if share::localsend().is_some() {
            dialog.add_response("send", "LocalSend");
        }
        dialog.add_response("files", "Show in Files");
        dialog.add_response("close", "Done");
        dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("copy"));
        dialog.set_close_response("close");
        let (w, video) = (Rc::downgrade(self), d.video);
        dialog.connect_response(None, move |_, r| {
            let Some(a) = w.upgrade() else { return };
            match r {
                "copy" => match share::copy(&path, video, &gtk::prelude::WidgetExt::display(&a.window)) {
                    Ok(()) => a.toast(if video { "Clip copied · paste it into a chat" } else { "Picture copied" }),
                    Err(e) => a.toast(&e),
                },
                "send" => {
                    if let Err(e) = share::send(&path) {
                        a.toast(&e);
                    }
                }
                "files" => {
                    let w2 = Rc::downgrade(&a);
                    gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&path))).open_containing_folder(
                        Some(&a.window),
                        None::<&gtk::gio::Cancellable>,
                        move |r| {
                            if let (Err(e), Some(a)) = (r, w2.upgrade()) {
                                a.toast(&e.to_string());
                            }
                        },
                    );
                }
                _ => {}
            }
        });
        dialog.present(Some(&self.window));
    }

    fn open(self: &Rc<Self>, c: usize) {
        self.clip_cancel();
        self.talk_stop();
        self.talk_btn.set_visible(self.cfg.cameras[c].can_talk(&self.cfg.talk_relay));
        self.single.set_zoom(1.0, 0.0, 0.0);
        self.single_page.set_can_pop(true);
        self.single.cam.set(c);
        self.single.clear();
        self.single_page.set_title(&self.cfg.cameras[c].name);
        self.focus.set(Some(c));
        self.nav.push(&self.single_page);
        self.refresh_timeline();
        if self.mode.get() == Mode::Play {
            // Same moment, now from the single camera's footage.
            let t = self.time();
            self.seek(t);
        } else {
            self.sync();
        }
    }

    fn close_single(&self) {
        if self.focus.get().is_none() {
            return;
        }
        self.clip_cancel();
        self.talk_stop();
        self.full.leave(&self.window);
        self.focus.set(None);
        self.refresh_timeline();
        if self.mode.get() == Mode::Play {
            let t = self.time();
            self.seek(t);
        } else {
            self.sync();
        }
    }
}

const CSS: &str = r#"
window { background: #000; }
.grid-bg { background: #000; }
.tile { border-radius: 14px; background: #1c1c1e; }
.tile-full { background: #000; }
.pill { background: rgba(0,0,0,0.50); color: white; border-radius: 999px; padding: 2px 9px; font-weight: 600; font-size: 12px; }
.live-dot { background: #ff3b30; border-radius: 999px; min-width: 7px; min-height: 7px; }
.talk { min-width: 64px; min-height: 64px; background: rgba(255,255,255,0.18); color: white; -gtk-icon-size: 26px; }
.talk.on { background: #ff3b30; }
.note { font-size: 11px; background: rgba(0,0,0,0.62); }
.empty { color: rgba(255,255,255,0.55); font-weight: 600; }
.controls { background: #1c1c1e; border-top: 1px solid rgba(255,255,255,0.08); padding: 6px 12px 10px 12px; }
.clock { font-size: 22px; font-weight: 700; font-feature-settings: "tnum"; }
.date { font-size: 12px; color: rgba(255,255,255,0.6); }
.livebtn { border-radius: 999px; padding: 4px 14px; font-weight: 800; font-size: 13px; background: rgba(255,255,255,0.10); color: rgba(255,255,255,0.85); }
.livebtn.on { background: #ff3b30; color: white; }
.speedbtn > button { border-radius: 999px; padding: 2px 9px; min-height: 26px; font-weight: 700; font-size: 13px; font-feature-settings: "tnum"; background: rgba(255,255,255,0.10); }
.speedrow { padding: 8px 28px; font-weight: 600; font-feature-settings: "tnum"; }
.speedrow.on { color: #0a84ff; }
.seg { background: rgba(255,255,255,0.08); border-radius: 9px; padding: 2px; }
.seg button { border-radius: 7px; padding: 2px 14px; font-weight: 600; font-size: 13px; background: none; min-height: 26px; }
.seg button:checked { background: rgba(255,255,255,0.22); }
.dayrow { padding: 8px 14px; }
.clipbar { background: rgba(0,0,0,0.70); border-radius: 999px; padding: 4px; color: white; }
.clipbar label { font-weight: 600; font-size: 13px; font-feature-settings: "tnum"; }
.clipbar .go { border-radius: 999px; padding: 4px 16px; font-weight: 700; }
.menurow { padding: 8px 18px; }
.share-preview { border-radius: 10px; }
.fsbtn { min-width: 48px; min-height: 48px; -gtk-icon-size: 22px; }
.fsclock { font-feature-settings: "tnum"; }
"#;

fn build(app: &adw::Application) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let cfg = Config::load();

    // Grid of all cameras.
    let grid = gtk::Grid::new();
    grid.set_column_homogeneous(true);
    grid.set_row_spacing(8);
    grid.set_column_spacing(8);
    grid.set_margin_start(8);
    grid.set_margin_end(8);
    grid.set_margin_top(8);
    grid.set_margin_bottom(8);
    grid.set_valign(gtk::Align::Start);
    let mut tiles = Vec::new();
    let (mut row, mut col) = (0, 0);
    for (c, cam) in cfg.cameras.iter().enumerate() {
        let tile = Tile::new(false);
        tile.cam.set(c);
        tile.name.set_text(&cam.name);
        tile.root.set_hexpand(true);
        if cam.span == 2 && col != 0 {
            row += 1;
            col = 0;
        }
        grid.attach(&tile.root, col, row, cam.span, 1);
        col += cam.span;
        if col >= 2 {
            col = 0;
            row += 1;
        }
        tiles.push(tile);
    }
    if cfg.cameras.is_empty() {
        let msg = gtk::Label::new(Some(&format!(
            "No cameras.\n\nFrigate at {} did not answer and the config lists none.\nRun \"omacctv check\" in a terminal.",
            cfg.frigate
        )));
        msg.set_justify(gtk::Justification::Center);
        msg.set_wrap(true);
        msg.set_margin_top(80);
        msg.add_css_class("dim-label");
        grid.attach(&msg, 0, 0, 2, 1);
    }
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroller.set_child(Some(&grid));
    scroller.set_vexpand(true);
    scroller.add_css_class("grid-bg");

    let grid_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    grid_view.add_top_bar(&header);
    grid_view.set_content(Some(&scroller));
    let grid_page = adw::NavigationPage::new(&grid_view, "Cameras");

    // Single camera.
    let single = Tile::new(true);
    single.root.set_vexpand(true);
    let single_view = adw::ToolbarView::new();
    let single_header = adw::HeaderBar::new();
    single_view.add_top_bar(&single_header);
    single_view.set_content(Some(&single.root));
    let single_page = adw::NavigationPage::new(&single_view, "Camera");
    let sound_btn = gtk::Button::from_icon_name("audio-volume-high-symbolic");
    sound_btn.add_css_class("osd");
    sound_btn.add_css_class("circular");
    sound_btn.set_halign(gtk::Align::End);
    sound_btn.set_valign(gtk::Align::End);
    sound_btn.set_margin_end(12);
    sound_btn.set_margin_bottom(12);
    single.root.add_overlay(&sound_btn);

    // Talk button (cameras with a speaker): tap = talk, tap again = stop.
    let talk_btn = gtk::Button::from_icon_name("audio-input-microphone-symbolic");
    talk_btn.add_css_class("talk");
    talk_btn.add_css_class("circular");
    talk_btn.set_tooltip_text(Some("Talk"));
    talk_btn.set_halign(gtk::Align::Center);
    talk_btn.set_valign(gtk::Align::End);
    talk_btn.set_margin_bottom(14);
    single.root.add_overlay(&talk_btn);
    let talk_status = gtk::Label::new(None);
    talk_status.add_css_class("pill");
    talk_status.set_halign(gtk::Align::Center);
    talk_status.set_valign(gtk::Align::End);
    talk_status.set_margin_bottom(86);
    talk_status.set_visible(false);
    single.root.add_overlay(&talk_status);
    let (talk_tx, talk_rx) = async_channel::unbounded::<talk::Event>();

    // Share: a picture of this moment, or a clip (choose its range on the timeline).
    let share_btn = gtk::MenuButton::new();
    share_btn.set_icon_name("send-to-symbolic");
    share_btn.set_tooltip_text(Some("Share"));
    single_header.pack_end(&share_btn);
    // Full screen: the picture over the whole screen, turnable to landscape.
    let full_btn = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    full_btn.set_tooltip_text(Some("Full screen"));
    single_header.pack_end(&full_btn);
    let full = fullview::FullView::new();
    let clip_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    clip_bar.add_css_class("clipbar");
    let clip_x = gtk::Button::from_icon_name("window-close-symbolic");
    clip_x.add_css_class("circular");
    clip_x.add_css_class("flat");
    clip_x.set_tooltip_text(Some("Cancel clip"));
    let clip_label = gtk::Label::new(None);
    clip_label.set_hexpand(true);
    clip_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let clip_go = gtk::Button::with_label("Share");
    clip_go.add_css_class("suggested-action");
    clip_go.add_css_class("go");
    clip_bar.append(&clip_x);
    clip_bar.append(&clip_label);
    clip_bar.append(&clip_go);
    clip_bar.set_valign(gtk::Align::End);
    clip_bar.set_margin_start(12);
    clip_bar.set_margin_end(64);
    clip_bar.set_margin_bottom(14);
    clip_bar.set_visible(false);
    single.root.add_overlay(&clip_bar);
    let (share_tx, share_rx) = async_channel::unbounded::<ShareDone>();

    let nav = adw::NavigationView::new();
    nav.add(&grid_page);
    nav.set_vexpand(true);

    // Controls: play/pause · clock+date · LIVE, then the timeline, then the zoom segments.
    let play_btn = gtk::Button::from_icon_name("media-playback-pause-symbolic");
    play_btn.add_css_class("circular");
    play_btn.add_css_class("flat");
    play_btn.set_valign(gtk::Align::Center);
    // Playback speed, next to ▶.
    let speed_btn = gtk::MenuButton::new();
    speed_btn.set_label(&stream::rate_label(1.0));
    speed_btn.add_css_class("speedbtn");
    speed_btn.set_always_show_arrow(false);
    speed_btn.set_valign(gtk::Align::Center);
    speed_btn.set_tooltip_text(Some("Playback speed"));
    let left = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    left.append(&play_btn);
    left.append(&speed_btn);
    let time_label = gtk::Label::new(Some("--:--:--"));
    time_label.add_css_class("clock");
    let date_label = gtk::Label::new(Some("Today"));
    date_label.add_css_class("date");
    let clock_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    clock_box.append(&time_label);
    clock_box.append(&date_label);
    let day_btn = gtk::MenuButton::new();
    day_btn.set_child(Some(&clock_box));
    day_btn.add_css_class("flat");
    day_btn.set_hexpand(true);
    day_btn.set_halign(gtk::Align::Center);
    day_btn.set_always_show_arrow(false);
    let live_btn = gtk::Button::with_label("LIVE");
    live_btn.add_css_class("livebtn");
    live_btn.add_css_class("on");
    live_btn.set_valign(gtk::Align::Center);
    let top = gtk::CenterBox::new();
    top.set_start_widget(Some(&left));
    top.set_center_widget(Some(&day_btn));
    top.set_end_widget(Some(&live_btn));

    let timeline = Timeline::new();
    timeline.area.set_margin_top(2);

    let seg = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    seg.add_css_class("seg");
    seg.set_halign(gtk::Align::Center);
    let mut spans = Vec::new();
    let mut group: Option<gtk::ToggleButton> = None;
    for (label, s) in [("1 h", 3600.0), ("6 h", 6.0 * 3600.0), ("24 h", 24.0 * 3600.0)] {
        let b = gtk::ToggleButton::with_label(label);
        if let Some(g) = &group {
            b.set_group(Some(g));
        } else {
            group = Some(b.clone());
        }
        seg.append(&b);
        spans.push((s, b));
    }
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 4);
    controls.add_css_class("controls");
    controls.append(&top);
    controls.append(&timeline.area);
    controls.append(&seg);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&nav);
    content.append(&controls);

    let top = gtk::Overlay::new();
    top.set_child(Some(&content));
    top.add_overlay(&full.root);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&top));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Cameras")
        .default_width(405)
        .default_height(860)
        .content(&toasts)
        .build();

    let app_state = Rc::new(App {
        cfg: cfg.clone(),
        data: RefCell::new(frigate::Data::default()),
        mode: Cell::new(Mode::Live),
        base: Cell::new(now()),
        since: Cell::new(None),
        paused: Cell::new(false),
        want_play: Cell::new(true),
        rate: Cell::new(1.0),
        seek_at: Cell::new(Instant::now()),
        gap_hold: Cell::new(false),
        sound: Cell::new(true),
        talking: RefCell::new(None),
        talk_btn: talk_btn.clone(),
        talk_status: talk_status.clone(),
        talk_tx,
        gen: Cell::new(0),
        focus: Cell::new(None),
        tiles: tiles.clone(),
        single: single.clone(),
        single_page: single_page.clone(),
        nav: nav.clone(),
        grid: grid.clone(),
        last_width: Cell::new(0),
        timeline: timeline.clone(),
        time_label,
        full: full.clone(),
        date_label,
        live_btn: live_btn.clone(),
        play_btn: play_btn.clone(),
        speed_btn: speed_btn.clone(),
        spans,
        shown_second: Cell::new(0),
        window: window.clone(),
        toasts,
        clip_start: Cell::new(None),
        clip_bar: clip_bar.clone(),
        clip_label,
        clip_go: clip_go.clone(),
        clip_busy: Cell::new(false),
        share_tx,
    });
    let a = &app_state;
    a.set_span(3600.0);

    for tile in &tiles {
        let click = gtk::GestureClick::new();
        let (w, c) = (Rc::downgrade(a), tile.cam.get());
        click.connect_released(move |_, _, _, _| {
            if let Some(a) = w.upgrade() {
                a.open(c);
            }
        });
        tile.root.add_controller(click);
    }
    // Big picture: pinch to zoom (pan with one finger while zoomed), double tap = 2.5x / back.
    // While zoomed the page can't be swiped away, so panning doesn't close it.
    let sw = single.sw.clone().expect("zoom view");
    let zoom = gtk::GestureZoom::new();
    let z0 = Rc::new(Cell::new(1.0));
    let (t, z) = (single.clone(), z0.clone());
    zoom.connect_begin(move |_, _| z.set(t.zoom.get()));
    let (t, z, page) = (single.clone(), z0.clone(), single_page.clone());
    zoom.connect_scale_changed(move |g, s| {
        let (cx, cy) = g.bounding_box_center().unwrap_or((0.0, 0.0));
        t.set_zoom(z.get() * s, cx, cy);
        page.set_can_pop(t.zoom.get() <= 1.01);
    });
    zoom.set_propagation_phase(gtk::PropagationPhase::Capture);
    sw.add_controller(zoom);
    let click = gtk::GestureClick::new();
    let (t, page) = (single.clone(), single_page.clone());
    click.connect_pressed(move |_, n, x, y| {
        if n == 2 {
            let target = if t.zoom.get() > 1.01 { 1.0 } else { 2.5 };
            t.set_zoom(target, x, y);
            page.set_can_pop(t.zoom.get() <= 1.01);
        }
    });
    sw.add_controller(click);

    // Full screen shows the same pictures as the single view.
    let f = full.clone();
    single.picture.connect_paintable_notify(move |p| {
        if f.is_on() {
            f.picture.set_paintable(p.paintable().as_ref());
        }
    });
    let w = Rc::downgrade(a);
    full_btn.connect_clicked(move |_| {
        let Some(a) = w.upgrade() else { return };
        let Some(c) = a.focus.get() else { return };
        a.full.name.set_text(&a.cfg.cameras[c].name);
        a.full.picture.set_paintable(a.single.picture.paintable().as_ref());
        a.full.enter(&a.window);
    });
    let w = Rc::downgrade(a);
    full.exit_btn.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.full.leave(&a.window);
        }
    });
    let keys = gtk::EventControllerKey::new();
    let w = Rc::downgrade(a);
    keys.connect_key_pressed(move |_, k, _, _| {
        let Some(a) = w.upgrade() else { return glib::Propagation::Proceed };
        if a.full.is_on() && (k == gdk::Key::Escape || k == gdk::Key::BackSpace) {
            a.full.leave(&a.window);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    let w = Rc::downgrade(a);
    nav.connect_popped(move |_, _| {
        if let Some(a) = w.upgrade() {
            a.close_single();
        }
    });
    let w = Rc::downgrade(a);
    sound_btn.connect_clicked(move |b| {
        let Some(a) = w.upgrade() else { return };
        a.sound.set(!a.sound.get());
        b.set_icon_name(if a.sound.get() { "audio-volume-high-symbolic" } else { "audio-volume-muted-symbolic" });
        // Same moment again, now with / without sound.
        match a.mode.get() {
            Mode::Play if !a.paused.get() => {
                let t = a.time();
                a.start_at(t, false);
            }
            _ => a.sync(),
        }
    });
    let w = Rc::downgrade(a);
    talk_btn.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.talk_toggle();
        }
    });
    let w = Rc::downgrade(a);
    share_btn.set_create_popup_func(move |btn| {
        let Some(a) = w.upgrade() else { return };
        let Some(c) = a.focus.get() else { return };
        let pop = gtk::Popover::new();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut items: Vec<(&str, &str, u8)> = vec![("camera-photo-symbolic", "Picture", 0)];
        if a.cfg.cameras[c].recorded() && a.cfg.has_frigate {
            items.push(("camera-video-symbolic", "Video clip …", 1));
        }
        for (icon, label, what) in items {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.append(&gtk::Image::from_icon_name(icon));
            row.append(&gtk::Label::new(Some(label)));
            let b = gtk::Button::new();
            b.set_child(Some(&row));
            b.add_css_class("flat");
            b.add_css_class("menurow");
            let (w2, pop2) = (Rc::downgrade(&a), pop.clone());
            b.connect_clicked(move |_| {
                pop2.popdown();
                let Some(a) = w2.upgrade() else { return };
                if what == 0 {
                    a.share_picture();
                } else {
                    a.clip_begin();
                    a.toast("Play or slide the timeline to the clip's end");
                }
            });
            list.append(&b);
        }
        pop.set_child(Some(&list));
        btn.set_popover(Some(&pop));
    });
    let w = Rc::downgrade(a);
    clip_x.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.clip_cancel();
        }
    });
    let w = Rc::downgrade(a);
    clip_go.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.clip_share();
        }
    });
    let w = Rc::downgrade(a);
    glib::spawn_future_local(async move {
        while let Ok(d) = share_rx.recv().await {
            let Some(a) = w.upgrade() else { return };
            a.share_done(d);
        }
    });
    let w = Rc::downgrade(a);
    glib::spawn_future_local(async move {
        while let Ok(e) = talk_rx.recv().await {
            let Some(a) = w.upgrade() else { return };
            a.talk_event(e);
        }
    });
    let w = Rc::downgrade(a);
    live_btn.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.go_live();
        }
    });
    let w = Rc::downgrade(a);
    play_btn.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.toggle_pause();
        }
    });
    let w = Rc::downgrade(a);
    speed_btn.set_create_popup_func(move |btn| {
        let Some(a) = w.upgrade() else { return };
        let pop = gtk::Popover::new();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for r in stream::RATES {
            let b = gtk::Button::with_label(&stream::rate_label(r));
            b.add_css_class("flat");
            b.add_css_class("speedrow");
            if r == a.rate.get() {
                b.add_css_class("on");
            }
            let (w2, pop2) = (Rc::downgrade(&a), pop.clone());
            b.connect_clicked(move |_| {
                pop2.popdown();
                if let Some(a) = w2.upgrade() {
                    a.set_rate(r);
                }
            });
            list.append(&b);
        }
        pop.set_child(Some(&list));
        btn.set_popover(Some(&pop));
    });
    for (s, b) in &a.spans {
        let (w, s) = (Rc::downgrade(a), *s);
        b.connect_toggled(move |b| {
            if b.is_active() {
                if let Some(a) = w.upgrade() {
                    a.timeline.set_span(s);
                }
            }
        });
    }
    let w = Rc::downgrade(a);
    timeline.connect_span(move |span| {
        if let Some(a) = w.upgrade() {
            for (s, b) in &a.spans {
                if (s - span).abs() > 1.0 && b.is_active() {
                    b.set_active(false);
                }
            }
        }
    });
    let w = Rc::downgrade(a);
    timeline.connect_scrub(move |t| {
        if let Some(a) = w.upgrade() {
            a.scrub(t);
        }
    });
    let w = Rc::downgrade(a);
    timeline.connect_commit(move |t| {
        if let Some(a) = w.upgrade() {
            a.seek(t);
        }
    });

    // Day picker: every day with footage, newest first.
    let w = Rc::downgrade(a);
    day_btn.set_create_popup_func(move |btn| {
        let Some(a) = w.upgrade() else { return };
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let data = a.data.borrow();
        let runs = data.union(&a.active_cams());
        let mut days: Vec<(String, f64)> = Vec::new();
        for r in runs.iter().rev() {
            let key = fmt(r.1 - 1.0, "%Y%m%d");
            if days.last().map(|d| d.0 != key).unwrap_or(true) {
                days.push((key, r.1 - 1.0));
            }
        }
        if days.is_empty() {
            list.append(&gtk::Label::new(Some("No recordings")));
        }
        let pop = gtk::Popover::new();
        for (_, t) in days {
            let label = if fmt(t, "%Y%m%d") == fmt(now(), "%Y%m%d") {
                "Today".to_string()
            } else {
                fmt(t, "%A, %B %e")
            };
            let b = gtk::Button::with_label(&label);
            b.add_css_class("flat");
            b.add_css_class("dayrow");
            let (w2, pop2) = (Rc::downgrade(&a), pop.clone());
            // Jump to the last minute of footage that day.
            b.connect_clicked(move |_| {
                pop2.popdown();
                if let Some(a) = w2.upgrade() {
                    a.seek(t - 60.0);
                }
            });
            list.append(&b);
        }
        let sc = gtk::ScrolledWindow::new();
        sc.set_child(Some(&list));
        sc.set_propagate_natural_height(true);
        sc.set_max_content_height(420);
        sc.set_hscrollbar_policy(gtk::PolicyType::Never);
        pop.set_child(Some(&sc));
        btn.set_popover(Some(&pop));
    });

    // Frame tick: new pictures, clock, layout, scrub previews.
    let w = Rc::downgrade(a);
    window.add_tick_callback(move |_, _| {
        let Some(a) = w.upgrade() else { return glib::ControlFlow::Break };
        a.layout();
        a.single.apply_zoom_scroll();
        for t in a.active() {
            t.present();
        }
        a.update_clock();
        if a.full.is_on() {
            let t = a.time_label.text();
            let live = if a.mode.get() == Mode::Live { "LIVE · " } else { "" };
            let s = format!("{live}{t}");
            if a.full.clock.text() != s {
                a.full.clock.set_text(&s);
            }
        }
        a.update_clip();
        // Scrub previews follow the finger: each camera fetches the picture at the current
        // spot as soon as its previous one has arrived (still() skips unchanged times).
        if a.mode.get() == Mode::Scrub || (a.mode.get() == Mode::Play && a.paused.get() && a.gap_hold.get()) {
            a.request_snapshots();
        }
        glib::ControlFlow::Continue
    });
    let w = Rc::downgrade(a);
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let Some(a) = w.upgrade() else { return glib::ControlFlow::Break };
        if a.mode.get() == Mode::Play {
            a.sync();
        }
        glib::ControlFlow::Continue
    });

    // Recordings + events from Frigate, refreshed every 30 s.
    let (tx, rx) = async_channel::bounded::<Result<frigate::Data, String>>(1);
    let cfg2 = cfg.clone();
    std::thread::spawn(move || loop {
        if tx.send_blocking(frigate::load(&cfg2, 10.0)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(30));
    });
    let w = Rc::downgrade(a);
    glib::spawn_future_local(async move {
        while let Ok(r) = rx.recv().await {
            let Some(a) = w.upgrade() else { return };
            match r {
                Ok(d) => {
                    *a.data.borrow_mut() = d;
                    a.refresh_timeline();
                }
                Err(e) => eprintln!("frigate: {e}"),
            }
        }
    });

    let w = Rc::downgrade(a);
    window.connect_close_request(move |_| {
        if let Some(a) = w.upgrade() {
            a.talk_stop();
            for t in a.tiles.iter().chain(std::iter::once(&a.single)) {
                t.stop("off");
            }
        }
        glib::Propagation::Proceed
    });
    // Keep the state alive as long as the window.
    let keep = app_state.clone();
    window.connect_destroy(move |_| {
        let _ = &keep;
    });

    app_state.sync();
    window.present();
}
