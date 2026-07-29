//! [`WaylandPane`] — a reusable GTK4 widget that embeds a hosted Wayland application.
//!
//! The widget owns a headless nested [Smithay] compositor (via
//! [`nested_wayland_session::spawn_headless`]), presents its composited output in an internal
//! [`gtk::Picture`], and translates GTK pointer/scroll/keyboard/focus events into the nested
//! compositor's seat. Drop it into any GTK layout; point a Wayland client at its
//! [`WaylandPane::wayland_socket`] and it renders inside the pane.
//!
//! # Two decoupled lifecycles
//!
//! * **Map / visibility** (`map`/`unmap`, hide/show, detach/re-attach). Only the *view* is
//!   affected: the frame pump and the resize poll start on `map` and pause on `unmap` (no
//!   readback while hidden — it saves CPU). Pausing them does **not** touch the compositor.
//! * **Object lifetime** (construct → `dispose`). The compositor and its hosted client live
//!   exactly as long as the `WaylandPane` object. They are spawned once in `constructed` and
//!   torn down once, on `dispose` (or an explicit [`WaylandPane::close`]).
//!
//! ## Embedder contract
//!
//! To hide the pane while keeping its browser alive, **keep a reference to the `WaylandPane`**
//! and detach it from the visible layout (or call [`gtk::prelude::WidgetExt::set_visible`]
//! `false`) — do **not** drop it. Hiding/unmapping preserves the hosted app's page, scroll,
//! form state, and any CDP connection. Dropping the last reference (or calling
//! [`WaylandPane::close`]) is what destroys the browser.
//!
//! [Smithay]: https://github.com/Smithay/smithay

use std::path::PathBuf;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crossbeam_channel::Sender;
use nested_wayland_session::input::SpikeInput;

/// Initial nested output width (device px). Replaced by the first resize-poll sample.
const INIT_OUT_W: u32 = 1280;
/// Initial nested output height (device px). Replaced by the first resize-poll sample.
const INIT_OUT_H: u32 = 800;
/// Frame-pump interval (~60 Hz). Active only while mapped.
const FRAME_PUMP_MS: u64 = 16;
/// Allocation-poll interval driving nested-output resizes. Active only while mapped.
const RESIZE_POLL_MS: u64 = 150;
/// Smallest nested output edge, in device pixels.
const MIN_OUT_PX: i32 = 16;
/// Largest nested output edge, in device pixels.
const MAX_OUT_PX: i32 = 16384;

/// Invert a `ContentFit::Contain` letterbox: widget-local point → compositor output pixels.
///
/// Computes the aspect-fit image rectangle inside `widget_w × widget_h`, rejects points that
/// land in the letterbox/pillarbox bars (`None`), and scales inside-rect coordinates back to
/// `out_w × out_h` output pixels. Pure and side-effect-free.
fn widget_to_output(
    wx: f64,
    wy: f64,
    widget_w: f64,
    widget_h: f64,
    out_w: f64,
    out_h: f64,
) -> Option<(f64, f64)> {
    if widget_w <= 0.0 || widget_h <= 0.0 || out_w <= 0.0 || out_h <= 0.0 {
        return None;
    }

    // Contain uses the smaller axis scale so the whole image fits.
    let scale = (widget_w / out_w).min(widget_h / out_h);
    let disp_w = out_w * scale;
    let disp_h = out_h * scale;

    // The image rect is centred: equal bars on opposite sides.
    let off_x = (widget_w - disp_w) / 2.0;
    let off_y = (widget_h - disp_h) / 2.0;

    let inside_x = wx - off_x;
    let inside_y = wy - off_y;
    if inside_x < 0.0 || inside_y < 0.0 || inside_x > disp_w || inside_y > disp_h {
        // Point fell in the letterbox bars — outside the client surface.
        return None;
    }

    let ox = (inside_x / scale).clamp(0.0, out_w);
    let oy = (inside_y / scale).clamp(0.0, out_h);
    Some((ox, oy))
}

/// Map a GTK pointer button number to its Linux `BTN_*` evdev code (unknown → `None`).
fn gtk_button_to_evdev(button: u32) -> Option<u32> {
    match button {
        1 => Some(0x110), // BTN_LEFT
        2 => Some(0x112), // BTN_MIDDLE
        3 => Some(0x111), // BTN_RIGHT
        _ => None,
    }
}

/// Nested output size (DEVICE pixels) to request for a widget allocation, or `None` to skip.
///
/// `logical size × scale_factor`, so the hosted app re-flows for the real pane at native
/// density. Returns `None` for a not-yet-allocated / nonsensical widget; otherwise clamps
/// each edge into [`MIN_OUT_PX`]..=[`MAX_OUT_PX`]. Pure and side-effect-free.
fn requested_output(widget_w: i32, widget_h: i32, scale_factor: i32) -> Option<(i32, i32)> {
    if widget_w <= 0 || widget_h <= 0 || scale_factor <= 0 {
        return None;
    }
    // i64 intermediate: a huge allocation × scale could overflow i32 before the clamp.
    let width = (widget_w as i64 * scale_factor as i64).clamp(MIN_OUT_PX as i64, MAX_OUT_PX as i64);
    let height =
        (widget_h as i64 * scale_factor as i64).clamp(MIN_OUT_PX as i64, MAX_OUT_PX as i64);
    Some((width as i32, height as i32))
}

mod imp {
    use super::*;

    use std::cell::{Cell, RefCell};
    use std::sync::OnceLock;
    use std::time::Duration;

    use gtk::gdk;
    use nested_wayland_session::{spawn_headless, DmabufFrame, HeadlessHandle};

    /// Whether the widget should prefer the zero-copy dmabuf present path.
    ///
    /// Mirrors the compositor's own `KLAMOTTENKISTE_PRESENT` selection: `readback` forces the
    /// CPU `MemoryTexture` path; anything else (including unset) prefers dmabuf import. If the
    /// compositor internally fell back to readback (pool alloc failed) or has not produced a
    /// dmabuf frame yet, `pump_tick` still falls through to the readback path, so this is a
    /// preference, not a hard commitment.
    fn present_dmabuf_preferred() -> bool {
        !matches!(
            std::env::var("KLAMOTTENKISTE_PRESENT"),
            Ok(value) if value.eq_ignore_ascii_case("readback")
        )
    }

    /// Import a compositor dmabuf slot as a zero-copy [`gdk::Texture`], wiring slot release.
    ///
    /// Builds a `GdkDmabufTexture` from the borrowed per-plane fds in `frame` and attaches a
    /// release closure that returns the pool slot (`frame.buffer_id`) to the compositor over
    /// `release_tx` only once GTK has finished sampling the texture. GTK does **not** take
    /// ownership of the fds (GTK4 semantics), so the compositor must keep them valid until
    /// that release fires — which the closure guarantees. On build failure GTK never runs the
    /// closure, so the slot is released here to avoid pinning it `in_flight` forever.
    fn build_dmabuf_texture(
        frame: &DmabufFrame,
        display: &gdk::Display,
        release_tx: Sender<u64>,
    ) -> Result<gdk::Texture, glib::Error> {
        let mut builder = gdk::DmabufTextureBuilder::new()
            .set_display(display)
            .set_width(frame.width)
            .set_height(frame.height)
            .set_fourcc(frame.fourcc)
            .set_modifier(frame.modifier)
            .set_n_planes(frame.planes.len() as u32)
            // Straight alpha, matching the readback path's `R8g8b8a8` (non-premultiplied).
            // For the opaque compositor output this is moot, but it keeps both paths identical.
            .set_premultiplied(false);

        for (index, plane) in frame.planes.iter().enumerate() {
            let idx = index as u32;
            // SAFETY: `plane.fd` is owned by the compositor's pool slot and stays valid until
            // `frame.buffer_id` is sent back through `release_tx`. The release closure below
            // does exactly that when GTK is done with the texture, satisfying `set_fd`'s
            // "fd must outlive the texture" contract.
            builder = unsafe { builder.set_fd(idx, plane.fd) }
                .set_offset(idx, plane.offset)
                .set_stride(idx, plane.stride);
        }

        let buffer_id = frame.buffer_id;
        let release_on_drop = release_tx.clone();
        // SAFETY: every plane's fd/offset/stride is set above; the release closure returns the
        // pool slot to the compositor only after GTK finishes sampling, so the borrowed fds
        // outlive every read GTK performs. The closure is `FnOnce + Send + 'static` (it moves a
        // crossbeam `Sender<u64>` and a `u64`), as `build_with_release_func` requires.
        let result = unsafe {
            builder.build_with_release_func(move || {
                let _ = release_on_drop.send(buffer_id);
            })
        };

        if result.is_err() {
            // Build failed: GTK produced no texture and will NOT run the release closure, so
            // hand the slot back ourselves — otherwise it stays `in_flight` and shrinks the
            // pool by one every frame.
            let _ = release_tx.send(buffer_id);
        }
        result
    }

    /// Private state of [`super::WaylandPane`].
    pub struct WaylandPane {
        /// The single child: presents composited frames (`ContentFit::Contain`).
        pub(super) picture: gtk::Picture,
        /// The hosted compositor. Spawned in `constructed`, torn down in `dispose`/`close`.
        pub(super) handle: RefCell<Option<HeadlessHandle>>,
        /// Clone of the seat-input sender (or `None` if startup failed / closed).
        pub(super) input_tx: RefCell<Option<Sender<SpikeInput>>>,
        /// Clone of the dmabuf slot-release sender (dmabuf path; `None` if startup failed).
        pub(super) release_tx: RefCell<Option<Sender<u64>>>,
        /// Whether to prefer the zero-copy dmabuf present path (from `KLAMOTTENKISTE_PRESENT`).
        pub(super) present_dmabuf: Cell<bool>,
        /// Latched once a dmabuf import has failed, so the fallback warning prints only once.
        pub(super) dmabuf_warned: Cell<bool>,
        /// Current nested output size (device px); read by every coordinate mapping.
        pub(super) out_size: Cell<(f64, f64)>,
        /// Last (logical w, logical h, scale) pushed to the compositor by the resize poll.
        pub(super) applied: Cell<(i32, i32, i32)>,
        /// The frame-pump timeout — a VIEW concern, live only while mapped.
        pub(super) pump_source: RefCell<Option<glib::SourceId>>,
        /// The resize-poll timeout — a VIEW concern, live only while mapped.
        pub(super) resize_source: RefCell<Option<glib::SourceId>>,
        /// A compositor-startup error message, if `spawn_headless` failed in `constructed`.
        pub(super) startup_error: RefCell<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WaylandPane {
        const NAME: &'static str = "KstWaylandPane";
        type Type = super::WaylandPane;
        type ParentType = gtk::Widget;

        fn new() -> Self {
            Self {
                picture: gtk::Picture::new(),
                handle: RefCell::new(None),
                input_tx: RefCell::new(None),
                release_tx: RefCell::new(None),
                present_dmabuf: Cell::new(present_dmabuf_preferred()),
                dmabuf_warned: Cell::new(false),
                out_size: Cell::new((INIT_OUT_W as f64, INIT_OUT_H as f64)),
                applied: Cell::new((0, 0, 0)),
                pump_source: RefCell::new(None),
                resize_source: RefCell::new(None),
                startup_error: RefCell::new(None),
            }
        }
    }

    impl ObjectImpl for WaylandPane {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPS.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("wayland-socket")
                        .read_only()
                        .build(),
                    glib::ParamSpecString::builder("control-socket-path")
                        .read_only()
                        .build(),
                ]
            })
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "wayland-socket" => self
                    .handle
                    .borrow()
                    .as_ref()
                    .map(|h| h.socket_name())
                    .to_value(),
                "control-socket-path" => self
                    .handle
                    .borrow()
                    .as_ref()
                    .and_then(|h| h.control_socket_path().map(|p| p.display().to_string()))
                    .to_value(),
                other => unimplemented!("unknown property {other}"),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Single child, laid out to fill in `size_allocate`.
            self.picture.set_content_fit(gtk::ContentFit::Contain);
            self.picture.set_parent(&*obj);

            // The pane itself is the interactive, focusable surface.
            obj.set_focusable(true);
            obj.set_can_focus(true);
            obj.set_hexpand(true);
            obj.set_vexpand(true);

            // Spawn the compositor ONCE, bound to the object's whole lifetime.
            match spawn_headless(INIT_OUT_W, INIT_OUT_H) {
                Ok(handle) => {
                    *self.input_tx.borrow_mut() = Some(handle.input_sender());
                    *self.release_tx.borrow_mut() = Some(handle.release_sender());
                    *self.handle.borrow_mut() = Some(handle);
                }
                Err(err) => {
                    let msg = format!("{err:#}");
                    eprintln!("KstWaylandPane: failed to start the compositor: {msg}");
                    *self.startup_error.borrow_mut() = Some(msg);
                }
            }

            self.install_controllers();
        }

        fn dispose(&self) {
            // View teardown (map lifecycle): pause the pumps.
            self.stop_pump();
            self.stop_resize_poll();

            // Object teardown (object lifecycle): destroy the compositor + hosted client.
            // This is the ONLY place (besides `close`) the compositor is torn down.
            if let Some(mut handle) = self.handle.borrow_mut().take() {
                handle.shutdown();
            }
            *self.input_tx.borrow_mut() = None;
            *self.release_tx.borrow_mut() = None;

            // Drop any imported dmabuf texture so no pool slot outlives the compositor.
            self.picture.set_paintable(gdk::Paintable::NONE);

            // Unparent the child so GTK does not warn about a still-parented widget.
            self.picture.unparent();
        }
    }

    impl WidgetImpl for WaylandPane {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            // Defer to the child; `Picture` can shrink, so the pane imposes no big minimum.
            self.picture.measure(orientation, for_size)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            // The single child fills the whole allocation.
            self.picture.allocate(width, height, baseline, None);
        }

        fn map(&self) {
            self.parent_map();
            // VIEW lifecycle: resume presentation and resize tracking. Never touches the
            // compositor, which is already running.
            self.start_pump();
            self.start_resize_poll();
            // Seat keyboard focus mirrors this widget's GTK focus, and unmapping cleared it
            // (`SpikeInput::Focus(false)` → `keyboard.set_focus(None)`). GTK does not re-emit
            // focus-enter for a widget remapped while it still holds focus — an embedder that
            // unparents and reparents the pane hits exactly that — so re-assert it here.
            // Without this the hosted client stays deaf until the next click, silently.
            if self.obj().has_focus()
                && let Some(tx) = self.input_tx.borrow().as_ref()
            {
                let _ = tx.send(SpikeInput::Focus(true));
            }
        }

        fn unmap(&self) {
            // VIEW lifecycle: pause presentation and resize tracking. The compositor and the
            // hosted client keep running with their state intact.
            self.stop_pump();
            self.stop_resize_poll();
            // Drop any imported dmabuf texture so its release closure fires and the pool slot
            // returns to the compositor — a hidden pane must pin no buffer. Harmless for the
            // readback path: the next `map` repaints within one pump tick. This returns a
            // buffer to the pool but does NOT touch the compositor's client state.
            self.picture.set_paintable(gdk::Paintable::NONE);
            self.parent_unmap();
        }
    }

    impl WaylandPane {
        /// Attach the pointer/scroll/keyboard/focus controllers to the pane widget.
        fn install_controllers(&self) {
            let obj = self.obj();
            // One clone of the seat sender shared by every controller (`None` if startup
            // failed — the closures then simply no-op).
            let tx = self.input_tx.borrow().clone();

            // --- Pointer buttons: press/release edges → seat button events. ---
            let click = gtk::GestureClick::new();
            click.set_button(0); // every button; filtered to 1/2/3 below.
            click.connect_pressed(glib::clone!(
                #[weak]
                obj,
                #[strong]
                tx,
                move |gesture, _n, x, y| {
                    obj.grab_focus(); // click-to-focus
                    let Some(tx) = tx.as_ref() else { return };
                    let Some(button) = gtk_button_to_evdev(gesture.current_button()) else {
                        return;
                    };
                    let (ow, oh) = obj.imp().out_size.get();
                    if let Some((ox, oy)) =
                        widget_to_output(x, y, obj.width() as f64, obj.height() as f64, ow, oh)
                    {
                        let _ = tx.send(SpikeInput::Button {
                            x: ox,
                            y: oy,
                            button,
                            pressed: true,
                        });
                    }
                }
            ));
            click.connect_released(glib::clone!(
                #[weak]
                obj,
                #[strong]
                tx,
                move |gesture, _n, x, y| {
                    let Some(tx) = tx.as_ref() else { return };
                    let Some(button) = gtk_button_to_evdev(gesture.current_button()) else {
                        return;
                    };
                    let (ow, oh) = obj.imp().out_size.get();
                    if let Some((ox, oy)) =
                        widget_to_output(x, y, obj.width() as f64, obj.height() as f64, ow, oh)
                    {
                        let _ = tx.send(SpikeInput::Button {
                            x: ox,
                            y: oy,
                            button,
                            pressed: false,
                        });
                    }
                }
            ));
            obj.add_controller(click);

            // --- Pointer motion → seat motion events. ---
            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(glib::clone!(
                #[weak]
                obj,
                #[strong]
                tx,
                move |_c, x, y| {
                    let Some(tx) = tx.as_ref() else { return };
                    let (ow, oh) = obj.imp().out_size.get();
                    if let Some((ox, oy)) =
                        widget_to_output(x, y, obj.width() as f64, obj.height() as f64, ow, oh)
                    {
                        let _ = tx.send(SpikeInput::Motion { x: ox, y: oy });
                    }
                }
            ));
            obj.add_controller(motion);

            // --- Scroll (both axes) → seat axis events. ---
            let scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            scroll.connect_scroll(glib::clone!(
                #[strong]
                tx,
                move |_c, dx, dy| {
                    if let Some(tx) = tx.as_ref() {
                        let _ = tx.send(SpikeInput::Scroll { dx, dy });
                    }
                    glib::Propagation::Proceed
                }
            ));
            obj.add_controller(scroll);

            // --- Keyboard: hardware keycode − 8 → evdev. ---
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(glib::clone!(
                #[strong]
                tx,
                move |_c, _keyval, keycode, _state| {
                    if let (Some(tx), Some(evdev)) = (tx.as_ref(), keycode.checked_sub(8)) {
                        let _ = tx.send(SpikeInput::Key {
                            evdev,
                            pressed: true,
                        });
                    }
                    glib::Propagation::Proceed
                }
            ));
            keys.connect_key_released(glib::clone!(
                #[strong]
                tx,
                move |_c, _keyval, keycode, _state| {
                    if let (Some(tx), Some(evdev)) = (tx.as_ref(), keycode.checked_sub(8)) {
                        let _ = tx.send(SpikeInput::Key {
                            evdev,
                            pressed: false,
                        });
                    }
                }
            ));
            obj.add_controller(keys);

            // --- Focus enter/leave → seat keyboard focus. ---
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(glib::clone!(
                #[strong]
                tx,
                move |_c| {
                    if let Some(tx) = tx.as_ref() {
                        let _ = tx.send(SpikeInput::Focus(true));
                    }
                }
            ));
            focus.connect_leave(glib::clone!(
                #[strong]
                tx,
                move |_c| {
                    if let Some(tx) = tx.as_ref() {
                        let _ = tx.send(SpikeInput::Focus(false));
                    }
                }
            ));
            obj.add_controller(focus);
        }

        /// Start the frame pump (~60 Hz) if not already running. Idempotent.
        pub(super) fn start_pump(&self) {
            if self.pump_source.borrow().is_some() {
                return;
            }
            let obj = self.obj();
            let id = glib::timeout_add_local(
                Duration::from_millis(FRAME_PUMP_MS),
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        obj.imp().pump_tick(&obj);
                        glib::ControlFlow::Continue
                    }
                ),
            );
            *self.pump_source.borrow_mut() = Some(id);
        }

        /// Present one frame: the dmabuf zero-copy path when preferred, else CPU readback.
        ///
        /// In dmabuf-preferred mode it imports the compositor's latest dmabuf slot as a
        /// `GdkDmabufTexture` (zero-copy) and sets it as the `Picture` paintable. If no dmabuf
        /// frame exists yet, the compositor internally fell back to readback, or GTK rejected
        /// the fourcc/modifier, it falls through to [`Self::present_readback`]. In readback mode
        /// it only ever uses the CPU `MemoryTexture` path. Runs on the GTK main thread.
        fn pump_tick(&self, obj: &super::WaylandPane) {
            if self.present_dmabuf.get() {
                let frame = self.handle.borrow().as_ref().and_then(|h| h.latest_dmabuf());
                if let Some(frame) = frame {
                    if let Some(release_tx) = self.release_tx.borrow().clone() {
                        match build_dmabuf_texture(&frame, &obj.display(), release_tx) {
                            Ok(texture) => {
                                self.picture.set_paintable(Some(&texture));
                                return;
                            }
                            Err(err) => {
                                // GTK rejected the buffer (fourcc/modifier). The slot was
                                // already released inside `build_dmabuf_texture`; fall through
                                // to readback. Warn once — this fires at ~60 Hz otherwise.
                                if !self.dmabuf_warned.replace(true) {
                                    eprintln!(
                                        "KstWaylandPane: dmabuf import failed ({err}); \
                                         falling back to readback. Set \
                                         KLAMOTTENKISTE_PRESENT=readback to silence this."
                                    );
                                }
                            }
                        }
                    }
                }
                // No dmabuf frame yet, an internal readback fallback, or a failed import:
                // try the CPU readback slot (empty in a healthy dmabuf run, populated if the
                // compositor itself fell back to readback).
            }
            self.present_readback();
        }

        /// Present the latest CPU-readback frame as a `MemoryTexture`, if one exists.
        fn present_readback(&self) {
            let frame = self.handle.borrow().as_ref().and_then(|h| h.latest_frame());
            if let Some(frame) = frame {
                let bytes = glib::Bytes::from(&frame.bytes);
                let texture = gdk::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                );
                self.picture.set_paintable(Some(&texture));
            }
        }

        /// Pause the frame pump if running. Does NOT touch the compositor. Idempotent.
        pub(super) fn stop_pump(&self) {
            if let Some(id) = self.pump_source.borrow_mut().take() {
                id.remove();
            }
        }

        /// Start the allocation → nested-output resize poll if not running. Idempotent.
        pub(super) fn start_resize_poll(&self) {
            if self.resize_source.borrow().is_some() {
                return;
            }
            let obj = self.obj();
            let id = glib::timeout_add_local(
                Duration::from_millis(RESIZE_POLL_MS),
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        let imp = obj.imp();
                        let sample = (obj.width(), obj.height(), obj.scale_factor());
                        if sample == imp.applied.get() {
                            return glib::ControlFlow::Continue;
                        }
                        let Some((width, height)) =
                            requested_output(sample.0, sample.1, sample.2)
                        else {
                            return glib::ControlFlow::Continue;
                        };
                        // Record BEFORE sending: the coordinate mapping and the next
                        // comparison must describe what we asked for.
                        imp.applied.set(sample);
                        imp.out_size.set((width as f64, height as f64));
                        if let Some(tx) = imp.input_tx.borrow().as_ref() {
                            let _ = tx.send(SpikeInput::Resize { width, height });
                        }
                        glib::ControlFlow::Continue
                    }
                ),
            );
            *self.resize_source.borrow_mut() = Some(id);
        }

        /// Pause the resize poll if running. Idempotent.
        pub(super) fn stop_resize_poll(&self) {
            if let Some(id) = self.resize_source.borrow_mut().take() {
                id.remove();
            }
        }
    }
}

glib::wrapper! {
    /// A reusable GTK4 widget embedding a hosted Wayland application.
    ///
    /// See the [module docs](self) for the two-lifecycle model and the embedder contract:
    /// hiding/unmapping preserves the hosted app; only `dispose`/[`WaylandPane::close`] tears
    /// it down.
    pub struct WaylandPane(ObjectSubclass<imp::WaylandPane>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl WaylandPane {
    /// Construct a pane, spawning its nested compositor immediately.
    ///
    /// The compositor starts in `constructed`; if startup fails the widget still builds but
    /// renders nothing — inspect [`WaylandPane::startup_error`].
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// The nested `WAYLAND_DISPLAY` socket name a client should connect to, if running.
    pub fn wayland_socket(&self) -> Option<String> {
        self.imp().handle.borrow().as_ref().map(|h| h.socket_name())
    }

    /// The path of the per-instance control socket (screenshot/click/type/resize), if running.
    pub fn control_socket_path(&self) -> Option<PathBuf> {
        self.imp()
            .handle
            .borrow()
            .as_ref()
            .and_then(|h| h.control_socket_path().map(|p| p.to_path_buf()))
    }

    /// A clone of the seat-input sender, for an embedder that wants to inject input directly.
    pub fn input_sender(&self) -> Option<Sender<SpikeInput>> {
        self.imp().input_tx.borrow().clone()
    }

    /// The compositor-startup error message, if `spawn_headless` failed at construction.
    pub fn startup_error(&self) -> Option<String> {
        self.imp().startup_error.borrow().clone()
    }

    /// Whether the hosted compositor is still running (not yet closed/disposed).
    pub fn is_running(&self) -> bool {
        self.imp().handle.borrow().is_some()
    }

    /// Explicitly tear the compositor and its hosted client down NOW.
    ///
    /// Equivalent to what `dispose` does, but callable by the embedder on group-close/app-exit
    /// without dropping the widget. Idempotent. After this the pane renders nothing; hiding or
    /// unmapping the pane is NOT this — it preserves the hosted app.
    pub fn close(&self) {
        let imp = self.imp();
        imp.stop_pump();
        imp.stop_resize_poll();
        if let Some(mut handle) = imp.handle.borrow_mut().take() {
            handle.shutdown();
        }
        *imp.input_tx.borrow_mut() = None;
        *imp.release_tx.borrow_mut() = None;
        // Drop any imported dmabuf texture so no pool slot outlives the compositor.
        imp.picture.set_paintable(gtk::gdk::Paintable::NONE);
    }
}

impl Default for WaylandPane {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_maps_centre_to_output_centre() {
        let (ox, oy) = widget_to_output(960.0, 600.0, 1920.0, 1200.0, 1280.0, 800.0).unwrap();
        assert!((ox - 640.0).abs() < 1e-6, "ox={ox}");
        assert!((oy - 400.0).abs() < 1e-6, "oy={oy}");
    }

    #[test]
    fn contain_exact_fit_is_identity() {
        let (ox, oy) = widget_to_output(100.0, 200.0, 1280.0, 800.0, 1280.0, 800.0).unwrap();
        assert!((ox - 100.0).abs() < 1e-6);
        assert!((oy - 200.0).abs() < 1e-6);
    }

    #[test]
    fn pillarbox_and_letterbox_bars_are_rejected() {
        // 2000×800 widget, 1280×800 output → 360 px pillarbox bars.
        assert!(widget_to_output(100.0, 400.0, 2000.0, 800.0, 1280.0, 800.0).is_none());
        assert!(widget_to_output(1000.0, 400.0, 2000.0, 800.0, 1280.0, 800.0).is_some());
        // 1280×1000 widget, 1280×800 output → 100 px letterbox bars.
        assert!(widget_to_output(640.0, 50.0, 1280.0, 1000.0, 1280.0, 800.0).is_none());
        assert!(widget_to_output(640.0, 500.0, 1280.0, 1000.0, 1280.0, 800.0).is_some());
    }

    #[test]
    fn coordinates_follow_the_current_runtime_output_size() {
        // After a resize the pane maps 1:1 into the NEW output, not a fixed constant.
        let (ox, oy) = widget_to_output(450.0, 150.0, 900.0, 300.0, 900.0, 300.0).unwrap();
        assert!((ox - 450.0).abs() < 1e-6, "ox={ox}");
        assert!((oy - 150.0).abs() < 1e-6, "oy={oy}");
        // Same pane, HiDPI output (2× device pixels): centre still maps to centre.
        let (ox, oy) = widget_to_output(450.0, 150.0, 900.0, 300.0, 1800.0, 600.0).unwrap();
        assert!((ox - 900.0).abs() < 1e-6, "ox={ox}");
        assert!((oy - 300.0).abs() < 1e-6, "oy={oy}");
    }

    #[test]
    fn output_follows_pane_and_scale_and_is_guarded() {
        assert_eq!(requested_output(900, 300, 1), Some((900, 300)));
        assert_eq!(requested_output(900, 300, 2), Some((1800, 600)));
        assert_eq!(requested_output(0, 300, 1), None);
        assert_eq!(requested_output(900, 0, 1), None);
        assert_eq!(requested_output(900, 300, 0), None);
        assert_eq!(requested_output(4, 4, 1), Some((MIN_OUT_PX, MIN_OUT_PX)));
        assert_eq!(
            requested_output(i32::MAX, i32::MAX, 8),
            Some((MAX_OUT_PX, MAX_OUT_PX))
        );
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(gtk_button_to_evdev(1), Some(0x110));
        assert_eq!(gtk_button_to_evdev(2), Some(0x112));
        assert_eq!(gtk_button_to_evdev(3), Some(0x111));
        assert_eq!(gtk_button_to_evdev(9), None);
    }
}
