//! kabelsalat embedded-browser spike — Phase 0, Task 4 (S3 real GTK input → seat).
//!
//! Starts the vendored nested Wayland compositor headlessly (rendering into an offscreen
//! GLES renderbuffer), presents its composited frames in a GTK4 `gtk::Picture`, and — new
//! in this task — translates real GTK pointer/scroll/keyboard/focus events on that
//! `Picture` into compositor seat events delivered to the hosted client.
//!
//! Presentation (from Task 3A): each frame is read back to CPU RGBA on the compositor
//! thread, handed across to the GTK main thread, wrapped in a `gdk::MemoryTexture`, and
//! painted. Point a real Wayland client at the printed nested socket to prove the pipe:
//!
//! ```text
//! WAYLAND_DISPLAY=<printed socket> foot
//! ```
//!
//! Input (this task): GTK event controllers on the `Picture` forward each event over a
//! `crossbeam-channel` `Sender<SpikeInput>` (obtained from `HeadlessHandle::input_sender`)
//! to the compositor thread, which drains and applies them to the seat via `input.rs`.
//! Widget-local coordinates are inverted through the `ContentFit::Contain` letterbox into
//! compositor OUTPUT pixels by [`widget_to_output`]. GTK button numbers map to `BTN_*`
//! evdev codes; GTK/X11 hardware keycodes map to evdev by subtracting the 8 offset.
//!
//! The spike's OWN GTK window uses the ambient `WAYLAND_DISPLAY` (e.g. `wayland-1`);
//! `spawn_headless` only creates a new listening socket and never mutates the process env.
//! `spawn_headless` also binds a control Unix socket (path printed at startup, override
//! with `KABELSALAT_SPIKE_CONTROL`) so the `type`/`click`/`screenshot` API can drive the
//! SAME seat code path the GTK controllers feed — the automated seat-injection proof.
//!
//! Verification: set `KABELSALAT_SPIKE_DUMP=<path.png>` to dump one readback PNG after the
//! 60th presented frame (or ~2 s), logging `dumped <path>`.

use std::cell::{Cell, RefCell};
use std::env;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::{gdk, gio, glib};
use libadwaita as adw;

use adw::prelude::*;

use klamottenkiste::compositor::input::SpikeInput;
use klamottenkiste::compositor::{screenshot, spawn_headless};

/// INITIAL compositor output size passed to `spawn_headless`. It is only a starting value:
/// the first pane-size poll (see [`RESIZE_POLL_MS`]) replaces it with the real allocation.
const OUT_W: f64 = 1280.0;
/// See [`OUT_W`].
const OUT_H: f64 = 800.0;

/// How often the pane's allocation is sampled, in milliseconds.
///
/// `gtk::Picture` has no resize signal, and a window drag emits a flood of allocations;
/// reallocating the GLES renderbuffer and reconfiguring the hosted client on each one would
/// thrash. Polling on a timeout and acting only when the sampled triple actually changed IS
/// the debounce — at most one resize per tick, and none while the size is steady.
const RESIZE_POLL_MS: u64 = 150;

/// Smallest nested output edge, in device pixels (below this the client is pointless).
const MIN_OUT_PX: i32 = 16;
/// Largest nested output edge, in device pixels (guards against an absurd allocation).
const MAX_OUT_PX: i32 = 16384;

/// Invert a `ContentFit::Contain` letterbox: widget-local point → compositor output pixels.
///
/// The `gtk::Picture` scales the composited image to fit inside its allocation while
/// preserving aspect ratio, centring it with letterbox/pillarbox bars. This undoes that:
/// it computes the on-screen image rectangle (aspect-fit inside `widget_w × widget_h`),
/// rejects points that land in the bars (returns `None`), and scales inside-rect
/// coordinates back to `out_w × out_h` output pixels.
///
/// Pure and side-effect-free so it can be eyeballed and unit-tested.
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

    // Contain uses the smaller of the two axis scales so the whole image fits.
    let scale = (widget_w / out_w).min(widget_h / out_h);
    let disp_w = out_w * scale;
    let disp_h = out_h * scale;

    // The image rect is centred: equal bars on opposite sides.
    let off_x = (widget_w - disp_w) / 2.0;
    let off_y = (widget_h - disp_h) / 2.0;

    // Coordinates inside the displayed image rect.
    let inside_x = wx - off_x;
    let inside_y = wy - off_y;
    if inside_x < 0.0 || inside_y < 0.0 || inside_x > disp_w || inside_y > disp_h {
        // Point fell in the letterbox bars — outside the client surface.
        return None;
    }

    // Scale the inside-rect point back up to output pixels (clamp against fp edge slop).
    let ox = (inside_x / scale).clamp(0.0, out_w);
    let oy = (inside_y / scale).clamp(0.0, out_h);
    Some((ox, oy))
}

/// Map a GTK pointer button number to its Linux `BTN_*` evdev code.
///
/// GTK numbers buttons 1=primary, 2=middle, 3=secondary; evdev uses `BTN_LEFT`=0x110,
/// `BTN_MIDDLE`=0x112, `BTN_RIGHT`=0x111. Unknown buttons return `None` (ignored).
fn gtk_button_to_evdev(button: u32) -> Option<u32> {
    match button {
        1 => Some(0x110), // BTN_LEFT
        2 => Some(0x112), // BTN_MIDDLE
        3 => Some(0x111), // BTN_RIGHT
        _ => None,
    }
}

/// Nested output size (DEVICE pixels) to request for a pane allocation, or `None` to skip.
///
/// The hosted app should lay out for the pane it is actually shown in, at the monitor's
/// native pixel density: `logical size x scale_factor`. That makes the client re-flow AND
/// removes the upscaling blur, because the readback then matches the widget 1:1.
///
/// Returns `None` for a not-yet-allocated or nonsensical widget (GTK reports 0 before the
/// first allocation); otherwise clamps each edge into [`MIN_OUT_PX`]..=[`MAX_OUT_PX`].
///
/// Pure and side-effect-free so the guard rails can be unit-tested.
fn requested_output(widget_w: i32, widget_h: i32, scale_factor: i32) -> Option<(i32, i32)> {
    if widget_w <= 0 || widget_h <= 0 || scale_factor <= 0 {
        return None;
    }

    // i64 intermediate: a huge allocation times a scale factor could overflow i32 before
    // the clamp ever ran.
    let width = (widget_w as i64 * scale_factor as i64).clamp(MIN_OUT_PX as i64, MAX_OUT_PX as i64);
    let height =
        (widget_h as i64 * scale_factor as i64).clamp(MIN_OUT_PX as i64, MAX_OUT_PX as i64);
    Some((width as i32, height as i32))
}

/// Parse a `KABELSALAT_SPIKE_WINDOW=WxH` override into a GTK window default size.
///
/// Verification aid: the reflow proof needs the spike started at two deterministic pane
/// shapes (e.g. `900x300`, the "beneath the terminal" shape, and `1600x900`) without a
/// human dragging the window. Invalid or non-positive specs return `None` (ignored).
fn parse_window_size(spec: &str) -> Option<(i32, i32)> {
    let (w, h) = spec.trim().split_once(['x', 'X'])?;
    let w: i32 = w.trim().parse().ok()?;
    let h: i32 = h.trim().parse().ok()?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((w, h))
}

fn main() -> glib::ExitCode {
    // Start the headless compositor on its own thread and learn its socket. Any
    // backend/EGL startup failure is surfaced here before GTK starts.
    let handle = match spawn_headless(OUT_W as u32, OUT_H as u32) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("spike: failed to start the headless compositor: {err:#}");
            return glib::ExitCode::FAILURE;
        }
    };

    let socket = handle.socket_name();
    println!("nested wayland socket: {socket}");
    println!("point a client at it, e.g. WAYLAND_DISPLAY={socket} foot");
    if let Some(control) = handle.control_socket_path() {
        println!("control socket: {}", control.display());
        println!(
            "  seat-injection proof, e.g.: printf 'type INJECTED-42\\n' | socat - UNIX-CONNECT:{}",
            control.display()
        );
    }

    // Optional verification dump path (written once, after the pipe is proven live).
    let dump_path: Option<PathBuf> = env::var_os("KABELSALAT_SPIKE_DUMP").map(PathBuf::from);

    // The GTK application. Its own window connects to the ambient WAYLAND_DISPLAY — we do
    // NOT set WAYLAND_DISPLAY to the nested socket for this process.
    let app = adw::Application::builder()
        .application_id("org.kabelsalat.Spike")
        .build();

    // Share the compositor handle into the activate closure (Rc: single-threaded GTK).
    let handle = Rc::new(handle);

    app.connect_activate(move |app| {
        // The pane the composited frames are painted into and that receives real input.
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        // Make the pane focusable and able to take keyboard focus on click.
        picture.set_can_focus(true);
        picture.set_focusable(true);

        // The channel into the compositor seat; each controller clones its own sender.
        let input_tx = handle.input_sender();

        // CURRENT nested output size in device pixels. The nested resolution is no longer a
        // compile-time constant — it follows the pane — so every coordinate mapping must
        // read it at event time. `Rc<Cell<..>>` because GTK is single-threaded and the
        // resize poll below is the only writer.
        let out_size: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((OUT_W, OUT_H)));

        // --- Pointer buttons (GestureClick): press+release edges → seat button events. ---
        let click = gtk::GestureClick::new();
        // 0 = listen for every button (we filter to 1/2/3 ourselves).
        click.set_button(0);
        {
            let tx = input_tx.clone();
            let pic = picture.clone();
            let out_size = Rc::clone(&out_size);
            click.connect_pressed(move |gesture, _n_press, x, y| {
                // A click grabs keyboard focus for the pane.
                pic.grab_focus();
                let Some(button) = gtk_button_to_evdev(gesture.current_button()) else {
                    return;
                };
                let (out_w, out_h) = out_size.get();
                if let Some((ox, oy)) =
                    widget_to_output(x, y, pic.width() as f64, pic.height() as f64, out_w, out_h)
                {
                    let _ = tx.send(SpikeInput::Button {
                        x: ox,
                        y: oy,
                        button,
                        pressed: true,
                    });
                }
            });
        }
        {
            let tx = input_tx.clone();
            let pic = picture.clone();
            let out_size = Rc::clone(&out_size);
            click.connect_released(move |gesture, _n_press, x, y| {
                let Some(button) = gtk_button_to_evdev(gesture.current_button()) else {
                    return;
                };
                let (out_w, out_h) = out_size.get();
                if let Some((ox, oy)) =
                    widget_to_output(x, y, pic.width() as f64, pic.height() as f64, out_w, out_h)
                {
                    let _ = tx.send(SpikeInput::Button {
                        x: ox,
                        y: oy,
                        button,
                        pressed: false,
                    });
                }
            });
        }
        picture.add_controller(click);

        // --- Pointer motion (EventControllerMotion) → seat motion events. ---
        let motion = gtk::EventControllerMotion::new();
        {
            let tx = input_tx.clone();
            let pic = picture.clone();
            let out_size = Rc::clone(&out_size);
            motion.connect_motion(move |_controller, x, y| {
                let (out_w, out_h) = out_size.get();
                if let Some((ox, oy)) =
                    widget_to_output(x, y, pic.width() as f64, pic.height() as f64, out_w, out_h)
                {
                    let _ = tx.send(SpikeInput::Motion { x: ox, y: oy });
                }
            });
        }
        picture.add_controller(motion);

        // --- Scroll (EventControllerScroll, both axes) → seat axis events. ---
        let scroll =
            gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        {
            let tx = input_tx.clone();
            scroll.connect_scroll(move |_controller, dx, dy| {
                let _ = tx.send(SpikeInput::Scroll { dx, dy });
                glib::Propagation::Proceed
            });
        }
        picture.add_controller(scroll);

        // --- Keyboard (EventControllerKey): hardware keycode − 8 → evdev. ---
        let keys = gtk::EventControllerKey::new();
        {
            let tx = input_tx.clone();
            keys.connect_key_pressed(move |_controller, _keyval, keycode, _state| {
                // GTK/X11 keycode = evdev + 8; guard against the impossible < 8.
                if let Some(evdev) = keycode.checked_sub(8) {
                    let _ = tx.send(SpikeInput::Key {
                        evdev,
                        pressed: true,
                    });
                }
                glib::Propagation::Proceed
            });
        }
        {
            let tx = input_tx.clone();
            keys.connect_key_released(move |_controller, _keyval, keycode, _state| {
                if let Some(evdev) = keycode.checked_sub(8) {
                    let _ = tx.send(SpikeInput::Key {
                        evdev,
                        pressed: false,
                    });
                }
            });
        }
        picture.add_controller(keys);

        // --- Focus (EventControllerFocus): enter/leave → seat keyboard focus. ---
        let focus = gtk::EventControllerFocus::new();
        {
            let tx = input_tx.clone();
            focus.connect_enter(move |_controller| {
                let _ = tx.send(SpikeInput::Focus(true));
            });
        }
        {
            let tx = input_tx.clone();
            focus.connect_leave(move |_controller| {
                let _ = tx.send(SpikeInput::Focus(false));
            });
        }
        picture.add_controller(focus);

        // Startup pane shape. `KABELSALAT_SPIKE_WINDOW=WxH` makes the reflow proof
        // reproducible without dragging a window by hand.
        let (win_w, win_h) = env::var("KABELSALAT_SPIKE_WINDOW")
            .ok()
            .as_deref()
            .and_then(parse_window_size)
            .unwrap_or((1280, 800));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("kabelsalat spike")
            .default_width(win_w)
            .default_height(win_h)
            .content(&picture)
            .build();
        window.present();

        // --- Pane size -> nested output resolution. -------------------------------------
        //
        // THE point of this: the nested output must BE the pane, so the hosted app lays out
        // for the real space (short and wide "beneath the terminal", say) instead of being
        // laid out for a fixed 1280x800 screen and then scaled down by `ContentFit::Contain`.
        //
        // Polled rather than signalled: `gtk::Picture` has no resize signal, `scale_factor`
        // changes independently of the allocation, and a drag would otherwise fire a resize
        // per allocation. Sampling every RESIZE_POLL_MS and comparing against the last
        // APPLIED triple is both the trigger and the debounce.
        {
            let pic = picture.clone();
            let tx = input_tx.clone();
            let out_size = Rc::clone(&out_size);
            // Last (logical w, logical h, scale) actually pushed to the compositor.
            let applied: Rc<Cell<(i32, i32, i32)>> = Rc::new(Cell::new((0, 0, 0)));
            glib::timeout_add_local(Duration::from_millis(RESIZE_POLL_MS), move || {
                let sample = (pic.width(), pic.height(), pic.scale_factor());
                if sample == applied.get() {
                    return glib::ControlFlow::Continue;
                }
                let Some((width, height)) = requested_output(sample.0, sample.1, sample.2) else {
                    // Not allocated yet (or nonsense) — keep the current output and retry.
                    return glib::ControlFlow::Continue;
                };

                // Record BEFORE sending: the coordinate mapping and the next comparison must
                // both describe what we asked for, even if the send races a shutdown.
                applied.set(sample);
                out_size.set((width as f64, height as f64));
                let _ = tx.send(SpikeInput::Resize { width, height });
                eprintln!(
                    "spike: pane {}x{} @{}x -> nested output {width}x{height}",
                    sample.0, sample.1, sample.2
                );
                glib::ControlFlow::Continue
            });
        }

        // ---------------------------------------------------------------------------
        // SPIKE SCOPE: text-only clipboard bridge between the nested seat and the HOST
        // GTK clipboard. The compositor half lives in
        // `nested_wayland_session::clipboard`; this half owns the `gdk::Clipboard`,
        // which may only be touched from the GTK main thread. The two halves only ever
        // exchange `String`s over crossbeam channels.
        // ---------------------------------------------------------------------------

        // LOOP GUARD: the last text this bridge itself saw/placed on the HOST clipboard.
        // Direction 1 (nested -> host) writes it after calling `set_text`, so direction
        // 2's poll recognises its own echo and does not push it back into the nested
        // compositor (which would ping-pong the selection back and forth forever).
        let last_host_text: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // Direction 1, NESTED -> HOST: the hosted client copied something; put it on the
        // host clipboard. Polled rather than signalled because the channel is the only
        // thing crossing the thread boundary.
        {
            let from_nested = handle.clipboard_from_nested();
            let last_host_text = Rc::clone(&last_host_text);
            glib::timeout_add_local(Duration::from_millis(100), move || {
                // Collapse a burst to the newest value; older ones are already stale.
                let mut newest: Option<String> = None;
                while let Ok(text) = from_nested.try_recv() {
                    newest = Some(text);
                }

                if let Some(text) = newest {
                    if let Some(display) = gdk::Display::default() {
                        display.clipboard().set_text(&text);
                        eprintln!("spike: clipboard nested -> host ({} bytes)", text.len());
                        *last_host_text.borrow_mut() = Some(text);
                    }
                }

                glib::ControlFlow::Continue
            });
        }

        // Direction 2, HOST -> NESTED: poll the host clipboard and forward changes. A
        // modest poll is deliberate for the spike: it needs no ownership tracking and
        // cannot re-enter, unlike the `changed` signal.
        {
            let to_nested = handle.clipboard_to_nested();
            let last_host_text = Rc::clone(&last_host_text);
            glib::timeout_add_local(Duration::from_millis(500), move || {
                let Some(display) = gdk::Display::default() else {
                    return glib::ControlFlow::Continue;
                };

                let to_nested = to_nested.clone();
                let last_host_text = Rc::clone(&last_host_text);
                display.clipboard().read_text_async(
                    None::<&gio::Cancellable>,
                    move |result| {
                        // SPIKE SCOPE: log every poll outcome, including failures. A read
                        // can legitimately fail (nothing on the clipboard, or the host
                        // compositor withholding the offer from an unfocused window), and
                        // silence there is indistinguishable from a broken bridge.
                        let text = match result {
                            Ok(Some(text)) => text,
                            Ok(None) => {
                                eprintln!("spike: clipboard poll: host clipboard has no text");
                                return;
                            }
                            Err(err) => {
                                eprintln!("spike: clipboard poll failed: {err}");
                                return;
                            }
                        };
                        let text = text.to_string();
                        if text.is_empty() {
                            return;
                        }
                        // LOOP GUARD: unchanged, or our own echo from direction 1.
                        if last_host_text.borrow().as_deref() == Some(text.as_str()) {
                            return;
                        }
                        eprintln!("spike: clipboard poll: host text changed");
                        eprintln!("spike: clipboard host -> nested ({} bytes)", text.len());
                        *last_host_text.borrow_mut() = Some(text.clone());
                        let _ = to_nested.send(text);
                    },
                );

                glib::ControlFlow::Continue
            });
        }

        // Per-activation state for the frame pump and the one-shot dump.
        let handle = Rc::clone(&handle);
        let dump_path = dump_path.clone();
        let start = Instant::now();
        let frame_count = Rc::new(RefCell::new(0u64));
        let dumped = Rc::new(RefCell::new(false));

        // Frame pump: ~60 Hz on the GTK main thread. Pull the latest readback, wrap it in
        // a GdkMemoryTexture, and present it.
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if let Some(frame) = handle.latest_frame() {
                let bytes = glib::Bytes::from(&frame.bytes);
                let texture = gdk::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                );
                picture.set_paintable(Some(&texture));

                *frame_count.borrow_mut() += 1;
                let count = *frame_count.borrow();

                // One-shot verification dump: reuse the crate's PNG writer on the upright
                // RGBA bytes we already hold on this thread.
                if !*dumped.borrow() {
                    if let Some(path) = &dump_path {
                        if count >= 60 || start.elapsed().as_secs_f64() >= 2.0 {
                            match screenshot::write_rgba_png(
                                &frame.bytes,
                                frame.width,
                                frame.height,
                                frame.stride,
                                path,
                            ) {
                                Ok(()) => println!("dumped {}", path.display()),
                                Err(err) => eprintln!("spike: dump failed: {err:#}"),
                            }
                            *dumped.borrow_mut() = true;
                        }
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    });

    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_maps_centre_to_output_centre() {
        // Widget wider than the 16:10 output → pillarbox bars left/right.
        let (ox, oy) = widget_to_output(960.0, 600.0, 1920.0, 1200.0, OUT_W, OUT_H).unwrap();
        assert!((ox - 640.0).abs() < 1e-6, "ox={ox}");
        assert!((oy - 400.0).abs() < 1e-6, "oy={oy}");
    }

    #[test]
    fn contain_exact_fit_is_identity() {
        // Widget exactly the output size: mapping is 1:1.
        let (ox, oy) = widget_to_output(100.0, 200.0, OUT_W, OUT_H, OUT_W, OUT_H).unwrap();
        assert!((ox - 100.0).abs() < 1e-6);
        assert!((oy - 200.0).abs() < 1e-6);
    }

    #[test]
    fn pillarbox_bar_is_rejected() {
        // Widget 2000×800 for a 1280×800 output → scale 1.0, image 1280 wide centred,
        // bars of 360 px each side. x=100 is inside the left bar.
        assert!(widget_to_output(100.0, 400.0, 2000.0, 800.0, OUT_W, OUT_H).is_none());
        // x=1000 lands on the image (bar ends at 360).
        assert!(widget_to_output(1000.0, 400.0, 2000.0, 800.0, OUT_W, OUT_H).is_some());
    }

    #[test]
    fn letterbox_bar_is_rejected() {
        // Widget 1280×1000 for 1280×800 → scale 1.0, 100 px bars top/bottom.
        assert!(widget_to_output(640.0, 50.0, 1280.0, 1000.0, OUT_W, OUT_H).is_none());
        assert!(widget_to_output(640.0, 500.0, 1280.0, 1000.0, OUT_W, OUT_H).is_some());
    }

    #[test]
    fn output_follows_pane_and_scale() {
        assert_eq!(requested_output(900, 300, 1), Some((900, 300)));
        // HiDPI: request native device pixels so the client renders sharp.
        assert_eq!(requested_output(900, 300, 2), Some((1800, 600)));
    }

    #[test]
    fn unallocated_or_absurd_sizes_are_guarded() {
        assert_eq!(requested_output(0, 300, 1), None);
        assert_eq!(requested_output(900, 0, 1), None);
        assert_eq!(requested_output(900, 300, 0), None);
        // Clamped into MIN_OUT_PX..=MAX_OUT_PX rather than passed through.
        assert_eq!(requested_output(4, 4, 1), Some((MIN_OUT_PX, MIN_OUT_PX)));
        assert_eq!(
            requested_output(i32::MAX, i32::MAX, 8),
            Some((MAX_OUT_PX, MAX_OUT_PX))
        );
    }

    #[test]
    fn window_override_parses() {
        assert_eq!(parse_window_size("900x300"), Some((900, 300)));
        assert_eq!(parse_window_size(" 1600X900 "), Some((1600, 900)));
        assert_eq!(parse_window_size("900"), None);
        assert_eq!(parse_window_size("0x300"), None);
        assert_eq!(parse_window_size("wide x tall"), None);
    }

    #[test]
    fn coordinates_follow_the_current_output_size() {
        // After a resize the pane maps 1:1 into the NEW output, not the old 1280x800.
        let (ox, oy) = widget_to_output(450.0, 150.0, 900.0, 300.0, 900.0, 300.0).unwrap();
        assert!((ox - 450.0).abs() < 1e-6, "ox={ox}");
        assert!((oy - 150.0).abs() < 1e-6, "oy={oy}");
        // Same pane, HiDPI output (2x device pixels): the centre still maps to the centre.
        let (ox, oy) = widget_to_output(450.0, 150.0, 900.0, 300.0, 1800.0, 600.0).unwrap();
        assert!((ox - 900.0).abs() < 1e-6, "ox={ox}");
        assert!((oy - 300.0).abs() < 1e-6, "oy={oy}");
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(gtk_button_to_evdev(1), Some(0x110));
        assert_eq!(gtk_button_to_evdev(2), Some(0x112));
        assert_eq!(gtk_button_to_evdev(3), Some(0x111));
        assert_eq!(gtk_button_to_evdev(9), None);
    }
}
