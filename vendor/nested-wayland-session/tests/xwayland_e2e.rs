//! Xwayland readiness gate, binary 2 of 2: the end-to-end "done" signal
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
//! One test per binary (the display-advertisement half lives in `xwayland.rs`),
//! so `KLAMOTTENKISTE_PRESENT` can be set directly at test start the way
//! `lifecycle.rs` does it, and no `--test-threads=1` caveat is needed. The
//! small helpers are deliberately duplicated between the two files: this crate
//! keeps its integration binaries self-contained. Shared constants are kept
//! identical.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nested_wayland_session::{Frame, HeadlessHandle, spawn_headless};

/// Nested output size, matching `lifecycle.rs`.
const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
/// How long to wait for `x11_display()` to become `Some`.
const DISPLAY_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for the client's own `mapped` / `exposed` lines, i.e. for
/// it to reach the X server at all.
const CLIENT_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for the client's magenta fill to reach the composited
/// frame. The client binary is pre-built by the gate command and spawned
/// directly, so no compile can happen inside this window.
const PAINT_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for an injected input event to echo on client stdout.
const INPUT_WAIT: Duration = Duration::from_secs(10);
/// How long to wait after shutdown for the Xwayland child to be reaped.
const REAP_WAIT: Duration = Duration::from_secs(5);
/// How many trailing stderr lines to quote in a failure message.
const STDERR_TAIL_LINES: usize = 10;

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

/// Path of the pre-built `x11-echo` binary, derived from this test executable's
/// own location: `target/<profile>/deps/xwayland_e2e-<hash>` → its parent's
/// parent is `target/<profile>/`, where cargo puts workspace bins. Spawning it
/// directly (rather than via `cargo run`) keeps the client a plain child of
/// this process, so kill-on-drop actually kills the client and not a cargo
/// wrapper.
fn x11_echo_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe() failed");
    let bin = exe
        .parent()
        .and_then(Path::parent)
        .map(|profile_dir| profile_dir.join("x11-echo"))
        .expect("test executable has no target/<profile>/deps/ ancestry");
    assert!(
        bin.is_file(),
        "x11-echo binary not found at {} — build it first: `cargo build -p x11-echo`",
        bin.display()
    );
    bin
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

/// True when the frame's center pixel is `#FF00FF` within ±2 per channel
/// (frames are upright, tightly packed RGBA8; `stride == width * 4`).
fn center_is_magenta(frame: &Frame) -> bool {
    let x = (frame.width / 2) as usize;
    let y = (frame.height / 2) as usize;
    let idx = y * frame.stride + x * 4;
    let Some(px) = frame.bytes.get(idx..idx + 4) else {
        return false;
    };
    let close = |a: u8, want: u8| a.abs_diff(want) <= 2;
    close(px[0], 0xff) && close(px[1], 0x00) && close(px[2], 0xff)
}

/// Send one control command line and return the response line, or an error
/// string (same helper as `lifecycle.rs`).
fn control_request(path: &Path, line: &str) -> Result<String, String> {
    let stream =
        UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
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

/// Kill-on-drop wrapper so a panicking assertion never leaks the client.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Drain client stdout on a thread into a channel so a wedged client cannot
/// deadlock the test.
fn spawn_line_reader(child: &mut Child) -> Receiver<String> {
    let stdout = child.stdout.take().expect("client stdout must be piped");
    let (tx, rx) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Drain client stderr on a thread into a shared buffer, so every failure
/// message can quote what the client complained about. Without this the red
/// state is undiagnosable: an X11 connect error would vanish into `/dev/null`
/// and look exactly like a compositor bug.
fn spawn_stderr_drain(child: &mut Child) -> Arc<Mutex<String>> {
    let stderr = child.stderr.take().expect("client stderr must be piped");
    let buf = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&buf);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let Ok(mut guard) = sink.lock() else { break };
            guard.push_str(&line);
            guard.push('\n');
        }
    });
    buf
}

/// The last `STDERR_TAIL_LINES` lines the client wrote to stderr.
fn stderr_tail(buf: &Arc<Mutex<String>>) -> String {
    let text = match buf.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return "<no output>".to_string();
    }
    lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..].join("\n")
}

/// Outcome of waiting for a line on the client's stdout. `ClientGone` is the
/// important one: the channel disconnecting means the reader thread saw EOF,
/// i.e. the client process exited — a very different fault from a timeout with
/// the client still alive and simply not receiving anything.
#[derive(Debug, PartialEq, Eq)]
enum LineWait {
    Matched,
    TimedOut,
    ClientGone,
}

/// Wait until a line matching `pred` arrives, the deadline passes, or the
/// client exits.
fn wait_for_line(
    rx: &Receiver<String>,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> LineWait {
    let deadline = Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return LineWait::TimedOut;
        };
        match rx.recv_timeout(remaining) {
            Ok(line) if pred(&line) => return LineWait::Matched,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => return LineWait::TimedOut,
            Err(RecvTimeoutError::Disconnected) => return LineWait::ClientGone,
        }
    }
}

/// Require a client stdout line starting with `want`, panicking with a message
/// that distinguishes "timed out, client alive" (`on_timeout` explains what
/// that implicates) from "client exited".
fn expect_client_line(
    rx: &Receiver<String>,
    timeout: Duration,
    stderr: &Arc<Mutex<String>>,
    want: &str,
    on_timeout: &str,
) {
    match wait_for_line(rx, timeout, |line| line.starts_with(want)) {
        LineWait::Matched => {}
        LineWait::TimedOut => panic!(
            "x11-echo never printed `{want}` within {timeout:?} — {on_timeout}\n\
             client stderr (last {STDERR_TAIL_LINES} lines):\n{}",
            stderr_tail(stderr)
        ),
        LineWait::ClientGone => panic!(
            "x11-echo exited before printing `{want}` (stdout reached EOF) — the client \
             process is gone, so nothing downstream can be concluded about the compositor\n\
             client stderr (last {STDERR_TAIL_LINES} lines):\n{}",
            stderr_tail(stderr)
        ),
    }
}

/// PIDs of *every* process visible in `/proc` whose comm is `Xwayland`,
/// regardless of parent. Includes zombies (an unreaped child stays in `/proc`
/// in state Z). Parentage is deliberately not filtered: a compositor that
/// double-forks or re-parents its Xwayland would otherwise make the teardown
/// check pass vacuously. The test compares against a snapshot taken before it
/// started anything, so other people's Xwayland servers on the machine are
/// excluded without weakening the check.
fn xwayland_pids() -> BTreeSet<u32> {
    let mut pids = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Format: "pid (comm) state ppid ..." — comm may contain spaces, so
        // split on the *last* ')'. `get` rather than a direct slice: a stat
        // read can come back truncated when the process exits mid-read, and
        // that must skip the entry, not panic.
        let Some(open) = stat.find('(') else { continue };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let (Some(comm), Some(_after)) = (stat.get(open + 1..close), stat.get(close + 2..)) else {
            continue;
        };
        if comm == "Xwayland" {
            pids.insert(pid);
        }
    }
    pids
}

/// The "done" signal: X11 client maps, composites, receives input, tears down.
#[test]
#[ignore = "red until Xwayland support lands"]
fn x11_client_end_to_end() {
    // Pin the backend to CPU-readback frames so `latest_frame()` is populated
    // (the default `dmabuf` mode leaves it empty — see `lifecycle.rs`). This
    // integration binary runs exactly one test, so there is no concurrent
    // reader of the variable.
    // SAFETY: single-threaded at this point (no compositor thread spawned yet).
    unsafe {
        std::env::set_var("KLAMOTTENKISTE_PRESENT", "readback");
    }

    require_xwayland_binary();
    let client_bin = x11_echo_binary();
    // Snapshot before anything of ours exists, so the teardown check below sees
    // every Xwayland this test is responsible for — including the compositor's.
    let pre_existing_xwayland = xwayland_pids();

    let mut handle = spawn_headless(OUT_W, OUT_H).expect("spawn_headless failed");
    let display = wait_for_x11_display(&handle, DISPLAY_WAIT)
        .expect("Xwayland support not implemented yet: x11_display() returned None");

    // Spawn the pre-built guinea pig directly (not `cargo run`): the client is
    // then a real child of this process, and `CARGO_BIN_EXE_*` is unavailable
    // because x11-echo is not a bin of the package under test.
    let mut client = KillOnDrop(
        Command::new(&client_bin)
            .env("DISPLAY", &display)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("launching {} failed: {e}", client_bin.display())),
    );
    let lines = spawn_line_reader(&mut client.0);
    let stderr = spawn_stderr_drain(&mut client.0);

    // Client -> X server: did the guinea pig reach the X server at all? Split
    // out from the pixel check so "never connected/mapped" cannot be misread as
    // "mapped but not composited".
    expect_client_line(
        &lines,
        CLIENT_WAIT,
        &stderr,
        "mapped",
        "the client never saw its window mapped, so it never reached a working X server \
         (or the X11 WM never mapped it)",
    );
    expect_client_line(
        &lines,
        CLIENT_WAIT,
        &stderr,
        "exposed",
        "the window mapped but the client was never asked to paint, so it produced no pixels \
         for the compositor to show",
    );

    // Compositor -> screen: the painted X11 window reached the composited frame.
    let deadline = Instant::now() + PAINT_WAIT;
    let mut magenta = false;
    let mut saw_any_frame = false;
    while Instant::now() < deadline {
        if let Some(frame) = handle.latest_frame() {
            saw_any_frame = true;
            if center_is_magenta(&frame) {
                magenta = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(150));
    }
    assert!(
        magenta,
        "the client reported `mapped` and `exposed`, but no composited frame showed its \
         magenta fill within {PAINT_WAIT:?} (any frame published at all: {saw_any_frame}) \
         — the X11 surface is not being composited into the output\n\
         client stderr (last {STDERR_TAIL_LINES} lines):\n{}",
        stderr_tail(&stderr)
    );

    // Host -> client: injected input reaches the X11 world.
    let control = handle
        .control_socket_path()
        .expect("control socket missing")
        .to_path_buf();
    assert_eq!(
        control_request(&control, "key a tap").as_deref(),
        Ok("ok"),
        "control socket rejected `key a tap`"
    );
    expect_client_line(
        &lines,
        INPUT_WAIT,
        &stderr,
        "key-press ",
        "`key a tap` was accepted by the control socket but never arrived — X11 keyboard \
         focus/routing missing",
    );
    let center = format!("click {} {}", OUT_W / 2, OUT_H / 2);
    assert_eq!(
        control_request(&control, &center).as_deref(),
        Ok("ok"),
        "control socket rejected `{center}`"
    );
    expect_client_line(
        &lines,
        INPUT_WAIT,
        &stderr,
        "button-press ",
        "`click` was accepted by the control socket but never arrived — X11 pointer routing \
         missing",
    );

    // Teardown: no Xwayland process this test caused survives shutdown.
    drop(client);
    handle.shutdown();
    let new_xwayland = || -> Vec<u32> {
        xwayland_pids()
            .difference(&pre_existing_xwayland)
            .copied()
            .collect()
    };
    let deadline = Instant::now() + REAP_WAIT;
    let mut leftover = new_xwayland();
    while !leftover.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(150));
        leftover = new_xwayland();
    }
    assert!(
        leftover.is_empty(),
        "Xwayland process(es) started during this test survived shutdown: {leftover:?}"
    );
}
