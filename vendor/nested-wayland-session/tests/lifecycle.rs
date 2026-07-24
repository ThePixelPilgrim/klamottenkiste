//! Headless lifecycle leak checks for `HeadlessHandle` (NO GTK window).
//!
//! These back the reusable-widget lifecycle contract without needing GTK: a GTK map/unmap is
//! a widget signal that cannot be fired headlessly, so each phase tests the underlying
//! invariant the widget relies on.
//!
//! * **(a) TEARDOWN** — `spawn_headless` + `shutdown` in a loop must fully release each
//!   instance: the control-socket file is removed every time, the process does not accumulate
//!   file descriptors, and nothing hangs (a leaked compositor/control thread would make the
//!   final `shutdown`/join block).
//! * **(b) STATE-PRESERVATION** — the important one for the corrected design. Spawn ONCE,
//!   connect a real client, then repeatedly do what `unmap`/`map` do to the widget — pause and
//!   resume the frame pump (i.e. stop/resume calling `latest_frame`) — WITHOUT calling
//!   `shutdown`. After every cycle the SAME compositor must still be alive: the nested socket
//!   name is unchanged, the control socket still answers `ping`, and the client is still
//!   connected and rendering (its frame is still non-blank). Only the final `shutdown` tears
//!   it down.
//!
//! Both phases live in one `#[test]` so the EGL/headless backend is never brought up
//! concurrently and the descriptor count is measured against a clean baseline.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use nested_wayland_session::spawn_headless;

/// Per-instance output size for the checks.
const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
/// Teardown iterations.
const TEARDOWN_N: usize = 5;
/// Map/unmap (pump pause/resume) cycles for the state-preservation phase.
const CYCLES: usize = 4;
/// How long to wait for the client's first painted frame.
const PAINT_WAIT: Duration = Duration::from_secs(8);

/// Count this process's open file descriptors (Linux `/proc/self/fd`).
fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

/// A frame is "painted" once it is not a single flat colour.
fn looks_painted(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let first = &bytes[0..4];
    bytes.chunks_exact(4).any(|px| px != first)
}

/// Send one control command line and return the response line, or an error string.
fn control_request(path: &Path, line: &str) -> Result<String, String> {
    let stream = UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writer
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;
    Ok(resp.trim().to_string())
}

#[test]
fn teardown_releases_and_state_survives_pump_pause() {
    // ---- (a) TEARDOWN: spawn + shutdown N times; assert clean release each time. --------
    let fd_baseline = open_fd_count();

    for i in 0..TEARDOWN_N {
        let mut handle = spawn_headless(OUT_W, OUT_H)
            .unwrap_or_else(|e| panic!("teardown iter {i}: spawn_headless failed: {e:#}"));

        let control = handle
            .control_socket_path()
            .expect("control socket path present")
            .to_path_buf();

        // The bound socket file exists while the instance is up, and answers ping.
        assert!(
            control.exists(),
            "iter {i}: control socket {} should exist while running",
            control.display()
        );
        assert_eq!(
            control_request(&control, "ping").as_deref(),
            Ok("ok"),
            "iter {i}: control socket should answer ping while running"
        );

        // Teardown. If the compositor or control thread leaked, this would hang.
        handle.shutdown();

        // The socket file is unlinked by shutdown.
        assert!(
            !control.exists(),
            "iter {i}: control socket {} must be removed after shutdown",
            control.display()
        );
        // Idempotent: a second shutdown (and the Drop that follows) is a no-op.
        handle.shutdown();
    }

    // No unbounded descriptor growth across the spawn/shutdown loop. A small slack absorbs
    // lazily-initialised globals; a genuine per-instance leak would blow past it.
    let fd_after = open_fd_count();
    assert!(
        fd_after <= fd_baseline + 8,
        "fd leak across {TEARDOWN_N} teardown cycles: baseline {fd_baseline}, after {fd_after}"
    );

    // ---- (b) STATE-PRESERVATION: spawn once, pause/resume the pump, stay alive. ----------
    let mut handle =
        spawn_headless(OUT_W, OUT_H).expect("state-preservation: spawn_headless failed");
    let socket = handle.socket_name();
    let control = handle
        .control_socket_path()
        .expect("control socket path present")
        .to_path_buf();

    // Connect a REAL client (its own process) to the nested socket.
    let mut client: Child = Command::new("foot")
        .env("WAYLAND_DISPLAY", &socket)
        .arg("sh")
        .arg("-c")
        .arg("clear; printf 'STATE-KEEP'; sleep 999")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("launching foot failed (is foot installed?)");

    // Wait for the client's first painted frame (this is the pump "running").
    let deadline = Instant::now() + PAINT_WAIT;
    let mut painted = false;
    while Instant::now() < deadline {
        if handle
            .latest_frame()
            .map(|f| looks_painted(&f.bytes))
            .unwrap_or(false)
        {
            painted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(painted, "client never produced a painted frame");

    // Several map/unmap cycles: "unmap" = stop reading frames (pump paused), "map" = read
    // them again (pump resumed). NO shutdown in between.
    for cycle in 0..CYCLES {
        // --- unmap: pause the pump. We simply stop calling latest_frame() for a beat. The
        //     compositor keeps running; nothing about the handle is touched. ---
        std::thread::sleep(Duration::from_millis(200));

        // --- map: resume the pump. ---
        let frame = handle.latest_frame();

        // The SAME compositor is still alive after the cycle:
        assert_eq!(
            handle.socket_name(),
            socket,
            "cycle {cycle}: nested socket name changed — compositor was replaced/restarted"
        );
        assert_eq!(
            control_request(&control, "ping").as_deref(),
            Ok("ok"),
            "cycle {cycle}: control socket stopped answering — compositor died"
        );
        // The client is still connected and rendering: its frame is still non-blank.
        assert!(
            frame.map(|f| looks_painted(&f.bytes)).unwrap_or(false),
            "cycle {cycle}: frame went blank — client was dropped across the pump pause"
        );
    }

    // Only now, the explicit teardown tears it down.
    handle.shutdown();
    assert!(
        !control.exists(),
        "control socket must be gone after the final shutdown"
    );
    // After shutdown the control socket no longer answers.
    assert!(
        control_request(&control, "ping").is_err(),
        "control socket must not answer after shutdown"
    );

    let _ = client.kill();
    let _ = client.wait();
}
