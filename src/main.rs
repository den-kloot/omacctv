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
mod frigate;
mod stream;
mod talk;
mod timeline;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
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
                stream::vod_args(&frigate::vod_url(&cfg, &cam.id, (r.1 - 60.0).max(r.0), r.1), 640, cam.heavy_record, false)
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
    seek_at: Cell<Instant>,
    /// Paused because the chosen time has no footage on any shown camera (▶ = next footage).
    gap_hold: Cell<bool>,
    /// Sound in the single view (the grid is always silent).
    sound: Cell<bool>,
    /// Talking to the shown camera (two-way audio), if active.
    talking: RefCell<Option<talk::Talk>>,
    talk_btn: gtk::Button,
    talk_status: gtk::Label,
    talk_tx: async_channel::Sender<talk::Event>,
    /// The last seek came from the user (not from skipping a gap during playback).
    user_seek: Cell<bool>,
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
    date_label: gtk::Label,
    live_btn: gtk::Button,
    play_btn: gtk::Button,
    spans: Vec<(f64, gtk::ToggleButton)>,
    scrub_due: Cell<Option<Instant>>,
    shown_second: Cell<i64>,
}

impl App {
    fn time(&self) -> f64 {
        match self.mode.get() {
            Mode::Live => now(),
            Mode::Scrub => self.base.get(),
            Mode::Play => match (self.paused.get(), self.since.get()) {
                (false, Some(s)) => self.base.get() + s.elapsed().as_secs_f64(),
                _ => self.base.get(),
            },
        }
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
        self.paused.set(false);
        self.gap_hold.set(false);
        self.user_seek.set(user);
        self.gen.set(self.gen.get() + 1);
        self.sync();
    }

    fn scrub(&self, t: f64) {
        self.mode.set(Mode::Scrub);
        self.base.set(t);
        self.scrub_due.set(Some(Instant::now() + Duration::from_millis(180)));
        self.sync();
    }

    fn toggle_pause(&self) {
        match self.mode.get() {
            Mode::Live => {
                self.mode.set(Mode::Play);
                self.base.set(now());
                self.paused.set(true);
            }
            Mode::Scrub => {
                self.paused.set(true);
                self.mode.set(Mode::Play);
            }
            Mode::Play if self.paused.get() && self.gap_hold.get() => {
                // ▶ in a gap: play from the next footage.
                let next = self.data.borrow().next_start(&self.active_cams(), self.base.get());
                return match next {
                    Some(n) => self.start_at(n, false),
                    None => self.go_live(),
                };
            }
            Mode::Play if self.paused.get() => return self.seek(self.base.get()),
            Mode::Play => {
                self.base.set(self.time());
                self.paused.set(true);
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
                let data = self.data.borrow();
                let cams = self.active_cams();
                let mut t = self.time();
                if data.loaded && !cams.iter().any(|&c| data.run_at(c, t).is_some()) && self.user_seek.get() {
                    // The user picked a time without footage: hold there and show the last pictures before it.
                    drop(data);
                    self.paused.set(true);
                    self.gap_hold.set(true);
                    self.user_seek.set(false);
                    return self.sync();
                }
                if data.loaded && !cams.iter().any(|&c| data.run_at(c, t).is_some()) {
                    // Playback ran into a gap: jump to the next footage.
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
                            let key = format!("vod:{c}:{}:{:.0}:{}:{audio}", tile.full, run.0, self.gen.get());
                            let url = frigate::vod_url(&self.cfg, &cam.id, t, run.1 + 1.0);
                            let width = if tile.full { 1440 } else if cam.span == 2 { 1080 } else { 640 };
                            tile.ensure(key, || stream::vod_args(&url, width, cam.heavy_record, audio), false);
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
                    self.user_seek.set(false);
                }
            }
        }
        for tile in &active {
            tile.live.set_visible(mode == Mode::Live && !tile.full);
        }
        self.update_controls();
    }

    /// Scrub previews: one recording snapshot per shown camera, newest request wins.
    fn request_snapshots(&self) {
        let t = self.base.get();
        for tile in self.active() {
            self.still(&tile, t);
        }
    }

    /// A still picture for time `t`: the recording at `t`, or else the last picture recorded
    /// before `t` (with a note saying when), or "No footage" when there is none.
    fn still(&self, tile: &Tile, t: f64) {
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
                tile.note.set_text(&format!("Last picture · {when}"));
                tile.note.set_visible(true);
            }
            None => tile.note.set_visible(false),
        }
        if (tile.still_at.get() - at).abs() < 0.01 {
            return;
        }
        tile.still_at.set(at);
        let req = tile.snap_req.get() + 1;
        tile.snap_req.set(req);
        let (cfg, id, slot) = (self.cfg.clone(), self.cfg.cameras[c].id.clone(), tile.slot.clone());
        let height = if tile.full { 1080 } else { 360 };
        std::thread::spawn(move || {
            if let Ok(j) = frigate::snapshot(&cfg, &id, at, height) {
                slot.lock().unwrap().jpeg = Some((req, j));
            }
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
            Mode::Scrub => false,
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

    fn open(self: &Rc<Self>, c: usize) {
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
        self.talk_stop();
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
.seg { background: rgba(255,255,255,0.08); border-radius: 9px; padding: 2px; }
.seg button { border-radius: 7px; padding: 2px 14px; font-weight: 600; font-size: 13px; background: none; min-height: 26px; }
.seg button:checked { background: rgba(255,255,255,0.22); }
.dayrow { padding: 8px 14px; }
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
    single_view.add_top_bar(&adw::HeaderBar::new());
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

    let nav = adw::NavigationView::new();
    nav.add(&grid_page);
    nav.set_vexpand(true);

    // Controls: play/pause · clock+date · LIVE, then the timeline, then the zoom segments.
    let play_btn = gtk::Button::from_icon_name("media-playback-pause-symbolic");
    play_btn.add_css_class("circular");
    play_btn.add_css_class("flat");
    play_btn.set_valign(gtk::Align::Center);
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
    top.set_start_widget(Some(&play_btn));
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

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Cameras")
        .default_width(405)
        .default_height(860)
        .content(&content)
        .build();

    let app_state = Rc::new(App {
        cfg: cfg.clone(),
        data: RefCell::new(frigate::Data::default()),
        mode: Cell::new(Mode::Live),
        base: Cell::new(now()),
        since: Cell::new(None),
        paused: Cell::new(false),
        seek_at: Cell::new(Instant::now()),
        gap_hold: Cell::new(false),
        sound: Cell::new(true),
        talking: RefCell::new(None),
        talk_btn: talk_btn.clone(),
        talk_status: talk_status.clone(),
        talk_tx,
        user_seek: Cell::new(false),
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
        date_label,
        live_btn: live_btn.clone(),
        play_btn: play_btn.clone(),
        spans,
        scrub_due: Cell::new(None),
        shown_second: Cell::new(0),
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
        if let Some(due) = a.scrub_due.get() {
            if Instant::now() >= due && a.mode.get() == Mode::Scrub {
                a.scrub_due.set(None);
                a.request_snapshots();
            }
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
