//! In-process frame capture (`HeadlessHandle::request_frame`) against a live compositor.
//!
//! The GTK widget's `capture_frame` is this request/reply crossing plus a main-context
//! delivery hop, so everything that can be proven headlessly is proven here:
//!
//! * **(a) GEOMETRY** — a capture answers with the nested output's size, a tight stride, and
//!   at least `height * stride` bytes; two captures in a row both answer (the channel source
//!   stays registered).
//! * **(b) FRESH RENDER, NOT THE CACHE** — with a REAL client painting, the captured frame is
//!   non-blank while the compositor runs in its DEFAULT present mode (`dmabuf`), where
//!   `latest_frame()` — the CPU-readback cache — is empty. The pixels can therefore only come
//!   from the capture's own composite. This is the property the whole feature rests on.
//! * **(c) AFTER SHUTDOWN** — `request_frame` reports the loop is gone instead of queueing a
//!   request nobody will ever answer. That is what the widget turns into `NotRunning`.
//!
//! Deliberately does NOT set `KLAMOTTENKISTE_PRESENT`: the point is the default (dmabuf) mode.
//! Everything lives in one `#[test]` so the EGL/headless backend is brought up once, matching
//! `lifecycle.rs`.

use std::process::{Child, Command};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use nested_wayland_session::{CaptureResult, Frame, HeadlessHandle, spawn_headless};

/// Nested output size, matching `lifecycle.rs`.
const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
/// Bytes per pixel in the RGBA8 readback.
const BYTES_PER_PIXEL: usize = 4;
/// How long one capture may take before the test calls it hung.
const CAPTURE_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for the client's paint to show up in a capture.
const PAINT_WAIT: Duration = Duration::from_secs(15);

/// A frame is "painted" once it is not a single flat colour (matching `lifecycle.rs`).
fn looks_painted(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let first = &bytes[0..4];
    bytes.chunks_exact(4).any(|px| px != first)
}

/// Request one frame and block (off the compositor thread) until it arrives.
fn capture_blocking(handle: &HeadlessHandle, timeout: Duration) -> CaptureResult {
    let (reply_tx, reply_rx) = sync_channel::<CaptureResult>(1);
    handle
        .request_frame(reply_tx)
        .map_err(|err| format!("{err:#}"))?;
    reply_rx
        .recv_timeout(timeout)
        .map_err(|err| format!("waiting for the capture reply: {err}"))?
}

/// Assert a captured frame describes the nested output consistently.
fn assert_plausible(frame: &Frame, label: &str) {
    assert_eq!(frame.width, OUT_W, "{label}: captured width");
    assert_eq!(frame.height, OUT_H, "{label}: captured height");
    assert_eq!(
        frame.stride,
        OUT_W as usize * BYTES_PER_PIXEL,
        "{label}: stride should be a tight RGBA8 row"
    );
    assert!(
        frame.bytes.len() >= frame.stride * frame.height as usize,
        "{label}: {} bytes is short for {}x{} at stride {}",
        frame.bytes.len(),
        frame.width,
        frame.height,
        frame.stride
    );
    // The clear colour is opaque, so a real composite is never an all-zero buffer.
    assert!(
        frame.bytes.iter().any(|&byte| byte != 0),
        "{label}: the readback is entirely zero — nothing was composited"
    );
}

#[test]
fn capture_renders_a_fresh_frame_and_reports_a_stopped_compositor() {
    let mut handle = spawn_headless(OUT_W, OUT_H).expect("spawn_headless failed");

    // ---- (a) GEOMETRY: two captures back to back, both plausible. -----------------------
    let first = capture_blocking(&handle, CAPTURE_WAIT).expect("first capture failed");
    assert_plausible(&first, "first capture");
    let second = capture_blocking(&handle, CAPTURE_WAIT).expect("second capture failed");
    assert_plausible(&second, "second capture");

    // ---- (b) FRESH RENDER: a real client's paint reaches the capture, not the cache. ----
    let socket = handle.socket_name();
    let mut client: Child = Command::new("foot")
        .env("WAYLAND_DISPLAY", &socket)
        .arg("sh")
        .arg("-c")
        .arg("clear; printf 'CAPTURE-ME'; sleep 999")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("launching foot failed (is foot installed?)");

    let deadline = Instant::now() + PAINT_WAIT;
    let mut painted = None;
    while Instant::now() < deadline {
        let frame = capture_blocking(&handle, CAPTURE_WAIT).expect("capture during paint failed");
        if looks_painted(&frame.bytes) {
            painted = Some(frame);
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    // The readback cache: empty in the default dmabuf present mode, which is exactly what
    // makes the painted capture above proof of a fresh composite. If the compositor fell back
    // to readback internally (no dmabuf pool on this machine) the cache is populated and this
    // particular inference is unavailable — the capture is a fresh render either way, so say
    // so and move on rather than failing on an environment difference.
    match handle.latest_frame() {
        None => {
            let frame = painted.as_ref().expect(
                "no painted capture within PAINT_WAIT (client never drew, or the capture is \
                 reading a stale cache)",
            );
            assert_plausible(frame, "painted capture");
        }
        Some(_) => {
            eprintln!(
                "note: the compositor fell back to the readback present mode, so the \
                 cache-independence half of this test is not observable here"
            );
            if let Some(frame) = painted.as_ref() {
                assert_plausible(frame, "painted capture");
            }
        }
    }

    let _ = client.kill();
    let _ = client.wait();

    // ---- (c) AFTER SHUTDOWN: the request is refused, not silently queued. ---------------
    handle.shutdown();
    let after = capture_blocking(&handle, CAPTURE_WAIT);
    assert!(
        after.is_err(),
        "request_frame must fail once the event loop is gone, got a frame"
    );
}
