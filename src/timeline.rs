//! Horizontal timeline: a fixed playhead in the middle, the day slides underneath.
//! Drag (with momentum) to scrub, tap to jump, pinch to zoom.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use gtk::prelude::*;
use gtk::{cairo, glib};

type Cb = RefCell<Option<Box<dyn Fn(f64)>>>;

#[derive(Default)]
pub struct Inner {
    span: Cell<f64>,
    center: Cell<f64>,
    now: Cell<f64>,
    runs: RefCell<Vec<(f64, f64)>>,
    /// (start, end, alert)
    events: RefCell<Vec<(f64, f64, bool)>>,
    drag0: Cell<f64>,
    press_x: Cell<f64>,
    dragging: Cell<bool>,
    moved: Cell<bool>,
    zooming: Cell<bool>,
    zoom0: Cell<f64>,
    animating: Cell<bool>,
    samples: RefCell<Vec<(Instant, f64)>>,
    on_scrub: Cb,
    on_commit: Cb,
    on_span: Cb,
}

#[derive(Clone)]
pub struct Timeline {
    pub area: gtk::DrawingArea,
    inner: Rc<Inner>,
}

pub const MIN_SPAN: f64 = 10.0 * 60.0;
pub const MAX_SPAN: f64 = 24.0 * 3600.0;
const OLDEST: f64 = 10.0 * 86400.0;

fn utc_offset() -> f64 {
    glib::DateTime::now_local().map(|d| d.utc_offset().as_seconds() as f64).unwrap_or(0.0)
}

pub fn fmt(t: f64, pattern: &str) -> String {
    glib::DateTime::from_unix_local(t.floor() as i64)
        .ok()
        .and_then(|d| d.format(pattern).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    cr.close_path();
}

impl Timeline {
    pub fn new() -> Timeline {
        let area = gtk::DrawingArea::new();
        area.set_content_height(62);
        area.set_hexpand(true);
        let inner = Rc::new(Inner::default());
        inner.span.set(3600.0);
        inner.now.set(crate::now());
        inner.center.set(crate::now());
        let tl = Timeline { area, inner };
        tl.setup();
        tl
    }

    pub fn connect_scrub(&self, f: impl Fn(f64) + 'static) {
        *self.inner.on_scrub.borrow_mut() = Some(Box::new(f));
    }
    pub fn connect_commit(&self, f: impl Fn(f64) + 'static) {
        *self.inner.on_commit.borrow_mut() = Some(Box::new(f));
    }
    pub fn connect_span(&self, f: impl Fn(f64) + 'static) {
        *self.inner.on_span.borrow_mut() = Some(Box::new(f));
    }

    /// The user is touching or the timeline is still gliding: don't move it from outside.
    pub fn busy(&self) -> bool {
        self.inner.dragging.get() || self.inner.animating.get() || self.inner.zooming.get()
    }

    pub fn set_position(&self, center: f64, now: f64) {
        let i = &self.inner;
        if (i.center.get() - center).abs() > 1e-3 || (i.now.get() - now).abs() > 0.5 {
            i.center.set(center);
            i.now.set(now);
            self.area.queue_draw();
        }
    }

    pub fn set_span(&self, span: f64) {
        self.inner.span.set(span.clamp(MIN_SPAN, MAX_SPAN));
        self.area.queue_draw();
    }

    pub fn set_data(&self, runs: Vec<(f64, f64)>, events: Vec<(f64, f64, bool)>) {
        *self.inner.runs.borrow_mut() = runs;
        *self.inner.events.borrow_mut() = events;
        self.area.queue_draw();
    }

    fn clamp(&self, t: f64) -> f64 {
        let now = crate::now();
        t.clamp(now - OLDEST, now)
    }

    fn per_px(&self) -> f64 {
        self.inner.span.get() / (self.area.width().max(1) as f64)
    }

    fn emit(cb: &Cb, t: f64) {
        if let Some(f) = cb.borrow().as_ref() {
            f(t);
        }
    }

    fn setup(&self) {
        let drag = gtk::GestureDrag::new();
        let me = self.clone();
        drag.connect_drag_begin(move |_, x, _| {
            let i = &me.inner;
            i.dragging.set(true);
            i.moved.set(false);
            i.animating.set(false);
            i.drag0.set(i.center.get());
            i.press_x.set(x);
            i.samples.borrow_mut().clear();
        });
        let me = self.clone();
        drag.connect_drag_update(move |_, dx, _| {
            let i = &me.inner;
            if i.zooming.get() {
                return;
            }
            if !i.moved.get() && dx.abs() > 6.0 {
                i.moved.set(true);
            }
            if !i.moved.get() {
                return;
            }
            let c = me.clamp(i.drag0.get() - dx * me.per_px());
            i.center.set(c);
            {
                let mut s = i.samples.borrow_mut();
                s.push((Instant::now(), i.press_x.get() + dx));
                if s.len() > 8 {
                    s.remove(0);
                }
            }
            Self::emit(&i.on_scrub, c);
            me.area.queue_draw();
        });
        let me = self.clone();
        drag.connect_drag_end(move |_, _, _| {
            let i = &me.inner;
            i.dragging.set(false);
            if i.zooming.get() {
                return;
            }
            if !i.moved.get() {
                me.tap(i.press_x.get());
                return;
            }
            // Fling velocity from the last ~80 ms of samples, in px/s.
            let v = {
                let s = i.samples.borrow();
                let last = s.last().copied();
                let first = s.iter().find(|(t, _)| last.map_or(false, |l| l.0.duration_since(*t).as_millis() <= 80)).copied();
                match (first, last) {
                    (Some(a), Some(b)) if b.0 > a.0 && b.0.elapsed().as_millis() < 60 => {
                        (b.1 - a.1) / b.0.duration_since(a.0).as_secs_f64()
                    }
                    _ => 0.0,
                }
            };
            if v.abs() > 250.0 {
                me.glide(v);
            } else {
                Self::emit(&i.on_commit, i.center.get());
            }
        });
        self.area.add_controller(drag);

        let zoom = gtk::GestureZoom::new();
        let me = self.clone();
        zoom.connect_begin(move |_, _| {
            me.inner.zooming.set(true);
            me.inner.zoom0.set(me.inner.span.get());
        });
        let me = self.clone();
        zoom.connect_scale_changed(move |_, s| {
            if s > 0.0 {
                me.set_span(me.inner.zoom0.get() / s);
                Self::emit(&me.inner.on_span, me.inner.span.get());
            }
        });
        let me = self.clone();
        zoom.connect_end(move |_, _| {
            me.inner.zooming.set(false);
        });
        self.area.add_controller(zoom);

        let me = self.clone();
        self.area.set_draw_func(move |area, cr, w, h| me.draw(area, cr, w as f64, h as f64));
    }

    fn tap(&self, x: f64) {
        let i = &self.inner;
        let w = self.area.width() as f64;
        let ppx = self.per_px();
        let t = i.center.get() + (x - w / 2.0) * ppx;
        // Nearest event marker within reach of a finger wins: jump to its start.
        let hit = i
            .events
            .borrow()
            .iter()
            .filter(|e| t >= e.0 - 14.0 * ppx && t <= e.1 + 14.0 * ppx)
            .min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs()))
            .map(|e| e.0 - 1.0);
        let target = self.clamp(hit.unwrap_or(t));
        i.center.set(target);
        self.area.queue_draw();
        Self::emit(&i.on_commit, target);
    }

    fn glide(&self, v0: f64) {
        let i = &self.inner;
        i.animating.set(true);
        let v = Cell::new(v0);
        let last = Cell::new(Instant::now());
        let me = self.clone();
        self.area.add_tick_callback(move |_, _| {
            let i = &me.inner;
            if !i.animating.get() {
                return glib::ControlFlow::Break;
            }
            let dt = last.get().elapsed().as_secs_f64().min(0.05);
            last.set(Instant::now());
            let c = i.center.get() - v.get() * dt * me.per_px();
            let clamped = me.clamp(c);
            i.center.set(clamped);
            v.set(v.get() * 0.94f64.powf(dt * 60.0));
            Self::emit(&i.on_scrub, clamped);
            me.area.queue_draw();
            if v.get().abs() < 40.0 || clamped != c {
                i.animating.set(false);
                Self::emit(&i.on_commit, clamped);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    fn draw(&self, area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64) {
        let i = &self.inner;
        let fg = area.color();
        let (fr, fgc, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
        let span = i.span.get();
        let t0 = i.center.get() - span / 2.0;
        let x = |t: f64| (t - t0) / span * w;
        let now = i.now.get().max(crate::now() - 1.0);
        let (ty, th) = (8.0, 24.0);

        // Track background up to "now".
        cr.set_source_rgba(fr, fgc, fb, 0.08);
        let xn = x(now).min(w);
        if xn > 0.0 {
            rounded(cr, 0.0, ty, xn, th, 6.0);
            let _ = cr.fill();
        }
        // Recorded footage.
        cr.set_source_rgba(fr, fgc, fb, 0.30);
        for &(s, e) in i.runs.borrow().iter() {
            if e < t0 || s > t0 + span {
                continue;
            }
            let (a, b) = (x(s).max(-10.0), x(e).min(w + 10.0));
            rounded(cr, a, ty, (b - a).max(1.5), th, 3.0);
            let _ = cr.fill();
        }
        // Events: detections (orange) under alerts (red).
        for alert in [false, true] {
            if alert {
                cr.set_source_rgba(1.0, 0.27, 0.23, 1.0);
            } else {
                cr.set_source_rgba(1.0, 0.62, 0.04, 1.0);
            }
            for &(s, e, a) in i.events.borrow().iter() {
                if a != alert || e < t0 || s > t0 + span {
                    continue;
                }
                let (xa, xb) = (x(s), x(e));
                let bw = (xb - xa).max(5.0);
                rounded(cr, xa, ty + 5.0, bw, th - 10.0, 3.0);
                let _ = cr.fill();
            }
        }

        // Ticks and labels in local time.
        let px_per_s = w / span;
        let majors = [300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0, 21600.0];
        let major = *majors.iter().find(|m| *m * px_per_s >= 64.0).unwrap_or(&21600.0);
        let minor = if major >= 3600.0 { major / 4.0 } else { major / 5.0 };
        let off = utc_offset();
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(11.0);
        let first = ((t0 + off) / minor).floor() * minor - off;
        let mut t = first;
        while t <= t0 + span + minor {
            let xt = x(t).round() + 0.5;
            let is_major = (((t + off) / major).round() * major - (t + off)).abs() < 1.0;
            let midnight = (((t + off) / 86400.0).round() * 86400.0 - (t + off)).abs() < 1.0;
            cr.set_source_rgba(fr, fgc, fb, if is_major { 0.45 } else { 0.18 });
            cr.set_line_width(1.0);
            cr.move_to(xt, ty + th + 3.0);
            cr.line_to(xt, ty + th + if is_major { 9.0 } else { 6.0 });
            let _ = cr.stroke();
            if is_major {
                let label = if midnight { fmt(t + 1.0, "%a %e") } else { fmt(t + 1.0, "%H:%M") };
                if midnight {
                    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
                    cr.set_source_rgba(fr, fgc, fb, 0.85);
                } else {
                    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
                    cr.set_source_rgba(fr, fgc, fb, 0.55);
                }
                if let Ok(ext) = cr.text_extents(&label) {
                    cr.move_to(xt - ext.width() / 2.0 - ext.x_bearing(), h - 6.0);
                    let _ = cr.show_text(&label);
                }
            }
            t += minor;
        }

        // Playhead.
        let xc = (w / 2.0).round();
        cr.set_source_rgba(1.0, 0.84, 0.04, 1.0);
        cr.set_line_width(3.0);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.move_to(xc, 3.0);
        cr.line_to(xc, ty + th + 5.0);
        let _ = cr.stroke();
    }
}
