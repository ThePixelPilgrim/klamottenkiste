//! `WaylandPane::capture_frame` on a real GTK main context (NO window is opened).
//!
//! The pixel path itself is covered headlessly by the compositor crate's `tests/capture.rs`;
//! what only a GTK-level test can show is the widget half of the contract:
//!
//! * a running pane's callback fires **on the main context** with plausible upright RGBA8, and
//! * a closed pane's callback still fires — with [`CaptureError::NotRunning`] — instead of
//!   being dropped silently.
//!
//! Both phases share one `#[test]`, so GTK is initialised once and only one nested compositor
//! is brought up.
//!
//! REQUIREMENTS: a GTK display AND a usable EGL render node. The pane brings up a real nested
//! compositor (`backend::init_headless_backend`), so there is no display-only mode. Only the
//! missing display is treated as "not applicable" and skipped, matching how `gtk::init` is
//! handled elsewhere in this workspace; a display that is present but cannot bring the
//! compositor up is a FAILURE, exactly as `spawn_headless` is in the compositor crate's own
//! tests.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use klamottenkiste::{CaptureError, CapturedFrame, WaylandPane};

/// What a `capture_frame` callback hands back.
type Outcome = Result<CapturedFrame, CaptureError>;

/// A slot a callback drops its outcome into, shared with the test body.
type Slot = Rc<RefCell<Option<Outcome>>>;

/// How long a capture may take before the test calls it lost.
const CAPTURE_WAIT: Duration = Duration::from_secs(10);
/// Main-context iteration interval while waiting.
const PUMP_MS: u64 = 2;
/// Bytes per pixel in the RGBA8 frame.
const BYTES_PER_PIXEL: usize = 4;

/// Iterate the GTK main context until the callback filled `slot`, or the deadline passes.
///
/// Deliberately non-blocking (`iteration(false)`): the callback is delivered by a main-context
/// source, so it can only arrive if the context is iterated — and the deadline must hold even
/// when no source is ready.
fn pump_until_filled(slot: &Slot, timeout: Duration) -> Option<Outcome> {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        context.iteration(false);
        if let Some(outcome) = slot.borrow_mut().take() {
            return Some(outcome);
        }
        std::thread::sleep(Duration::from_millis(PUMP_MS));
    }
    None
}

#[test]
fn capture_frame_delivers_pixels_and_reports_a_closed_pane() {
    if gtk::init().is_err() {
        eprintln!(
            "skipping: no GTK display available (this test needs a display AND a render node)"
        );
        return;
    }

    let pane = WaylandPane::new();
    if let Some(err) = pane.startup_error() {
        panic!("the pane's compositor failed to start: {err}");
    }
    assert!(pane.is_running(), "a freshly built pane should be running");

    // ---- (a) RUNNING: the callback lands on the main context with a usable frame. -------
    let slot: Slot = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&slot);
    pane.capture_frame(move |outcome| {
        *sink.borrow_mut() = Some(outcome);
    });
    assert!(
        slot.borrow().is_none(),
        "capture_frame must not invoke its callback synchronously"
    );

    let frame = pump_until_filled(&slot, CAPTURE_WAIT)
        .expect("the capture callback never fired for a running pane")
        .expect("capturing a frame from a running pane failed");

    assert!(
        frame.width > 0 && frame.height > 0,
        "captured frame has area"
    );
    assert_eq!(
        frame.stride,
        frame.width as usize * BYTES_PER_PIXEL,
        "stride should be a tight RGBA8 row"
    );
    assert!(
        frame.rgba.len() >= frame.stride * frame.height as usize,
        "{} bytes is short for {}x{} at stride {}",
        frame.rgba.len(),
        frame.width,
        frame.height,
        frame.stride
    );
    // No client is ever attached here, so the frame is the compositor's clear colour — which
    // is OPAQUE. That is the check: every pixel's alpha byte is 0xff, which an untouched
    // (all-zero) buffer fails and which pins the documented RGBA byte order. Deliberately NOT
    // "some byte is non-zero": the clear colour is 0.1 grey, so that holds for a frame nothing
    // was ever composited into and would prove nothing here.
    assert!(
        frame
            .rgba
            .chunks_exact(BYTES_PER_PIXEL)
            .all(|px| px[3] == 0xff),
        "the captured buffer has transparent pixels — it is not a composited RGBA8 frame"
    );

    // ---- (b) CLOSED: the callback still fires, with NotRunning. -------------------------
    pane.close();
    assert!(!pane.is_running(), "close() should stop the compositor");

    let slot: Slot = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&slot);
    pane.capture_frame(move |outcome| {
        *sink.borrow_mut() = Some(outcome);
    });

    let outcome = pump_until_filled(&slot, CAPTURE_WAIT)
        .expect("the capture callback was dropped silently for a closed pane");
    // `{outcome:?}` straight through: `CapturedFrame`'s hand-written `Debug` prints geometry
    // only, so the failure message is readable and no `map` dance is needed to build one.
    assert!(
        matches!(outcome, Err(CaptureError::NotRunning)),
        "a closed pane must report NotRunning, got {outcome:?}"
    );
}
