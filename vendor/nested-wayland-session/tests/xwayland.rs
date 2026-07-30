//! Xwayland readiness gate, binary 1 of 2: display advertisement
//! (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`).
//!
//! `#[ignore]`d: red until Xwayland support lands. Run the whole gate with
//!
//! ```sh
//! cargo build -p x11-echo
//! cargo test -p monochromatic-nested-wayland-session --test xwayland --test xwayland_e2e \
//!     --no-fail-fast -- --ignored
//! ```
//!
//! `--no-fail-fast`: cargo stops after the first failing test *binary*, and
//! while the gate is red both are expected to fail — without it the second
//! binary never runs.
//!
//! One test per binary (the end-to-end half lives in `xwayland_e2e.rs`), so
//! `KLAMOTTENKISTE_PRESENT` can be set directly at test start the way
//! `lifecycle.rs` does it, and no `--test-threads=1` caveat is needed.

use std::thread;
use std::time::{Duration, Instant};

use nested_wayland_session::{HeadlessHandle, spawn_headless};

/// Nested output size, matching `lifecycle.rs`.
const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
/// How long to wait for `x11_display()` to become `Some`.
const DISPLAY_WAIT: Duration = Duration::from_secs(10);

/// A missing Xwayland binary is a broken environment, not a red feature.
fn require_xwayland_binary() {
    let found = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join("Xwayland").is_file()))
        .unwrap_or(false);
    assert!(
        found,
        "Xwayland binary not found on PATH — install it (Fedora: xorg-x11-server-Xwayland)"
    );
}

/// Poll `x11_display()` until it is `Some` or the deadline passes.
fn wait_for_x11_display(handle: &HeadlessHandle, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(display) = handle.x11_display() {
            return Some(display);
        }
        thread::sleep(Duration::from_millis(150));
    }
    None
}

/// Fast first gate: the compositor advertises an X11 display at all.
#[test]
#[ignore = "red until Xwayland support lands"]
fn x11_display_advertised() {
    // Pin the backend to CPU-readback frames so `latest_frame()` is populated
    // (the default `dmabuf` mode leaves it empty — see `lifecycle.rs`). This
    // integration binary runs exactly one test, so there is no concurrent
    // reader of the variable.
    // SAFETY: single-threaded at this point (no compositor thread spawned yet).
    unsafe {
        std::env::set_var("KLAMOTTENKISTE_PRESENT", "readback");
    }

    require_xwayland_binary();

    let mut handle = spawn_headless(OUT_W, OUT_H).expect("spawn_headless failed");
    let display = wait_for_x11_display(&handle, DISPLAY_WAIT);
    handle.shutdown();

    assert!(
        display.is_some(),
        "Xwayland support not implemented yet: x11_display() returned None \
         (waited {DISPLAY_WAIT:?})"
    );
}
