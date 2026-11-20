//! Full screen view of the single camera: the picture fills the whole screen and can be
//! turned to landscape (90° either way) without the compositor rotating the display.
//! Tap shows/hides the buttons (they hide by themselves after a few seconds).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

mod imp {
    use super::*;

    /// Lays its one child out turned by `angle` degrees (0, 90, 180 or 270), or (angle 0)
    /// scaled by `zoom` and moved by (tx, ty).
    pub struct Rotator {
        pub child: RefCell<Option<gtk::Widget>>,
        pub angle: Cell<u32>,
        pub zoom: Cell<f64>,
        pub tx: Cell<f64>,
        pub ty: Cell<f64>,
    }

    impl Default for Rotator {
        fn default() -> Self {
            Rotator { child: Default::default(), angle: Cell::new(0), zoom: Cell::new(1.0), tx: Cell::new(0.0), ty: Cell::new(0.0) }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Rotator {
        const NAME: &'static str = "OmaCctvRotator";
        type Type = super::Rotator;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Rotator {
        fn dispose(&self) {
            if let Some(c) = self.child.take() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for Rotator {
        fn measure(&self, _o: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, w: i32, h: i32, _baseline: i32) {
            let Some(c) = self.child.borrow().clone() else { return };
            let (wf, hf) = (w as f32, h as f32);
            let (cw, ch, t) = match self.angle.get() {
                90 => (h, w, gsk::Transform::new().translate(&graphene::Point::new(wf, 0.0)).rotate(90.0)),
                180 => (w, h, gsk::Transform::new().translate(&graphene::Point::new(wf, hf)).rotate(180.0)),
                270 => (h, w, gsk::Transform::new().translate(&graphene::Point::new(0.0, hf)).rotate(270.0)),
                _ => (
                    w,
                    h,
                    gsk::Transform::new()
                        .translate(&graphene::Point::new(self.tx.get() as f32, self.ty.get() as f32))
                        .scale(self.zoom.get() as f32, self.zoom.get() as f32),
                ),
            };
            c.allocate(cw, ch, -1, Some(t));
        }
    }
}

glib::wrapper! {
    pub struct Rotator(ObjectSubclass<imp::Rotator>) @extends gtk::Widget;
}

impl Rotator {
    fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let r: Rotator = glib::Object::new();
        child.set_parent(&r);
        r.imp().child.replace(Some(child.clone().upcast()));
        r
    }

    fn set_angle(&self, a: u32) {
        self.imp().angle.set(a % 360);
        self.queue_allocate();
    }

    fn angle(&self) -> u32 {
        self.imp().angle.get()
    }

    fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    fn offset(&self) -> (f64, f64) {
        (self.imp().tx.get(), self.imp().ty.get())
    }

    /// Zoom `z` (1 = whole picture) with the child's top left at (tx, ty), kept inside the view.
    fn set_view(&self, z: f64, tx: f64, ty: f64) {
        let z = z.clamp(1.0, 8.0);
        let (w, h) = (self.width() as f64, self.height() as f64);
        let i = self.imp();
        // The picture inside the child (contained, so with black bars on two sides): keep it
        // covering the view where it is big enough, centred where it isn't.
        let a = i
            .child
            .borrow()
            .as_ref()
            .and_then(|c| c.downcast_ref::<gtk::Picture>().and_then(|p| p.paintable()))
            .map(|p| p.intrinsic_aspect_ratio())
            .unwrap_or(0.0);
        let (cw, ch) = if a <= 0.0 || w <= 0.0 || h <= 0.0 {
            (w, h)
        } else if w / h > a {
            (h * a, h)
        } else {
            (w, w / a)
        };
        let fit = |t: f64, view: f64, content: f64| {
            let bar = (view - content) / 2.0 * z;
            if content * z >= view {
                t.clamp(view - bar - content * z, -bar)
            } else {
                (view - view * z) / 2.0
            }
        };
        i.zoom.set(z);
        i.tx.set(fit(tx, w, cw));
        i.ty.set(fit(ty, h, ch));
        if std::env::var_os("OMACCTV_DEBUG").is_some() {
            eprintln!("fullview: view {w}x{h} aspect {a:.3} content {cw:.0}x{ch:.0} zoom {z:.2} -> t {:.0},{:.0}", i.tx.get(), i.ty.get());
        }
        self.queue_allocate();
    }

    /// Zoom to `z` keeping the point `(fx, fy)` of the view where it is.
    fn zoom_at(&self, z: f64, fx: f64, fy: f64) {
        let (z0, (tx, ty)) = (self.zoom(), self.offset());
        let (px, py) = ((fx - tx) / z0, (fy - ty) / z0);
        self.set_view(z, fx - z.clamp(1.0, 8.0) * px, fy - z.clamp(1.0, 8.0) * py);
    }
}

pub struct FullView {
    /// Goes on top of the whole window content; hidden until full screen is on.
    pub root: gtk::Overlay,
    rot: Rotator,
    zbox: Rotator,
    pub picture: gtk::Picture,
    pub name: gtk::Label,
    pub clock: gtk::Label,
    pub exit_btn: gtk::Button,
    buttons: Vec<gtk::Widget>,
    hide_gen: Cell<u64>,
}

impl FullView {
    pub fn new() -> Rc<FullView> {
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);

        let zbox = Rotator::new(&picture);
        zbox.set_overflow(gtk::Overflow::Hidden);
        zbox.set_hexpand(true);
        zbox.set_vexpand(true);
        let inner = gtk::Overlay::new();
        inner.set_child(Some(&zbox));
        inner.add_css_class("tile-full");

        let exit_btn = gtk::Button::from_icon_name("view-restore-symbolic");
        exit_btn.set_tooltip_text(Some("Leave full screen"));
        let rotate_btn = gtk::Button::from_icon_name("object-rotate-right-symbolic");
        rotate_btn.set_tooltip_text(Some("Turn the picture"));
        for (b, ha) in [(&exit_btn, gtk::Align::Start), (&rotate_btn, gtk::Align::End)] {
            b.add_css_class("osd");
            b.add_css_class("circular");
            b.add_css_class("fsbtn");
            b.set_halign(ha);
            b.set_valign(gtk::Align::Start);
            b.set_margin_start(14);
            b.set_margin_end(14);
            b.set_margin_top(14);
            inner.add_overlay(b);
        }
        let info = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let name = gtk::Label::new(None);
        name.add_css_class("pill");
        let clock = gtk::Label::new(None);
        clock.add_css_class("pill");
        clock.add_css_class("fsclock");
        info.append(&name);
        info.append(&clock);
        info.set_halign(gtk::Align::Start);
        info.set_valign(gtk::Align::End);
        info.set_margin_start(14);
        info.set_margin_bottom(14);
        info.set_can_target(false);
        inner.add_overlay(&info);

        let rot = Rotator::new(&inner);
        rot.set_angle(90);
        rot.set_hexpand(true);
        rot.set_vexpand(true);

        let root = gtk::Overlay::new();
        root.set_child(Some(&rot));
        root.add_css_class("tile-full");
        root.set_visible(false);

        let fv = Rc::new(FullView {
            root,
            rot,
            zbox,
            picture,
            name,
            clock,
            exit_btn,
            buttons: vec![rotate_btn.clone().upcast(), info.upcast()],
            hide_gen: Cell::new(0),
        });

        // Turn: landscape one way → the other way → upright → …
        let w = Rc::downgrade(&fv);
        rotate_btn.connect_clicked(move |_| {
            let Some(f) = w.upgrade() else { return };
            let next = match f.rot.angle() {
                90 => 270,
                270 => 0,
                _ => 90,
            };
            f.rot.set_angle(next);
            f.zbox.set_view(1.0, 0.0, 0.0);
            f.show_buttons();
        });
        // Pinch = zoom (and move with the fingers), one finger = move while zoomed,
        // double tap = 2.5x / back, tap = buttons on/off.
        let zb = &fv.zbox;
        let pinch = gtk::GestureZoom::new();
        // Picture point under the fingers at the start (in unzoomed child coordinates), start zoom.
        let start = Rc::new(Cell::new((0.0f64, 0.0f64, 1.0f64)));
        let zooming = Rc::new(Cell::new(false));
        let (z, st, zg) = (zb.clone(), start.clone(), zooming.clone());
        pinch.connect_begin(move |g, _| {
            let (cx, cy) = g.bounding_box_center().unwrap_or((0.0, 0.0));
            let (z0, (tx, ty)) = (z.zoom(), z.offset());
            st.set(((cx - tx) / z0, (cy - ty) / z0, z0));
            zg.set(true);
        });
        let (z, st) = (zb.clone(), start.clone());
        pinch.connect_scale_changed(move |g, s| {
            let (cx, cy) = g.bounding_box_center().unwrap_or((0.0, 0.0));
            let (px, py, z0) = st.get();
            let nz = (z0 * s).clamp(1.0, 8.0);
            z.set_view(nz, cx - nz * px, cy - nz * py);
        });
        let zg = zooming.clone();
        pinch.connect_end(move |_, _| zg.set(false));
        let zg = zooming.clone();
        pinch.connect_cancel(move |_, _| zg.set(false));
        zb.add_controller(pinch);

        let drag = gtk::GestureDrag::new();
        let last = Rc::new(Cell::new((0.0f64, 0.0f64)));
        let l = last.clone();
        drag.connect_drag_begin(move |_, _, _| l.set((0.0, 0.0)));
        let (z, l, zg) = (zb.clone(), last.clone(), zooming.clone());
        drag.connect_drag_update(move |_, dx, dy| {
            let (lx, ly) = l.get();
            l.set((dx, dy));
            if zg.get() || z.zoom() <= 1.01 {
                return;
            }
            let (tx, ty) = z.offset();
            z.set_view(z.zoom(), tx + dx - lx, ty + dy - ly);
        });
        zb.add_controller(drag);

        let tap = gtk::GestureClick::new();
        let z = zb.clone();
        tap.connect_pressed(move |_, n, x, y| {
            if n == 2 {
                let target = if z.zoom() > 1.01 { 1.0 } else { 2.5 };
                z.zoom_at(target, x, y);
            }
        });
        let w = Rc::downgrade(&fv);
        tap.connect_released(move |_, n, _, _| {
            let Some(f) = w.upgrade() else { return };
            if n != 1 {
                return;
            }
            if f.exit_btn.is_visible() {
                f.set_buttons(false);
            } else {
                f.show_buttons();
            }
        });
        zb.add_controller(tap);
        // A picture with another shape (still → stream): keep the zoomed view inside it.
        let aspect = Cell::new(0.0f64);
        let z = zb.clone();
        fv.picture.connect_paintable_notify(move |p| {
            let a = p.paintable().map(|p| p.intrinsic_aspect_ratio()).unwrap_or(0.0);
            if (a - aspect.get()).abs() > 0.01 {
                aspect.set(a);
                let (tx, ty) = z.offset();
                z.set_view(z.zoom(), tx, ty);
            }
        });
        fv
    }

    fn set_buttons(&self, on: bool) {
        self.exit_btn.set_visible(on);
        for b in &self.buttons {
            b.set_visible(on);
        }
    }

    /// Shows the buttons and hides them again after 4 s without a tap.
    pub fn show_buttons(self: &Rc<Self>) {
        self.set_buttons(true);
        let g = self.hide_gen.get() + 1;
        self.hide_gen.set(g);
        let w = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if let Some(f) = w.upgrade() {
                if f.hide_gen.get() == g {
                    f.set_buttons(false);
                }
            }
        });
    }

    pub fn is_on(&self) -> bool {
        self.root.is_visible()
    }

    pub fn enter(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        self.root.set_visible(true);
        self.zbox.set_view(1.0, 0.0, 0.0);
        window.as_ref().fullscreen();
        self.show_buttons();
    }

    pub fn leave(&self, window: &impl IsA<gtk::Window>) {
        if !self.is_on() {
            return;
        }
        self.root.set_visible(false);
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        window.as_ref().unfullscreen();
    }
}
