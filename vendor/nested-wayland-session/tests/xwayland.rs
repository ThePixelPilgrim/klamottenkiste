//! Xwayland readiness gate
//! (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`).
//!
//! Both tests are `#[ignore]`d: red until Xwayland support lands, run explicitly via
//!
//! ```sh
//! cargo build -p x11-echo
//! cargo test -p monochromatic-nested-wayland-session --test xwayland -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1`: two tests share one binary, each spawns its own
//! compositor; serializing avoids GPU/EGL contention and makes the one-time
//! env write below race-free in practice.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

use nested_wayland_session::{Frame, HeadlessHandle, spawn_headless};

/// Nested output size, matching `lifecycle.rs`.
const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
/// How long to wait for `x11_display()` to become `Some`.
const DISPLAY_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for the client's magenta fill to reach the composited
/// frame. Generous: on a cold target dir `cargo run -p x11-echo` may compile
/// first even though the gate command pre-builds it.
const PAINT_WAIT: Duration = Duration::from_secs(30);
/// How long to wait for an injected input event to echo on client stdout.
const INPUT_WAIT: Duration = Duration::from_secs(10);
/// How long to wait after shutdown for the Xwayland child to be reaped.
const REAP_WAIT: Duration = Duration::from_secs(5);

/// Pin the backend to CPU-readback frames so `latest_frame()` is populated
/// (the default `dmabuf` mode leaves it empty — see `lifecycle.rs`).
fn init_readback() {
    static READBACK: Once = Once::new();
    READBACK.call_once(|| {
        // SAFETY: runs at most once, before the calling test spawns its
        // compositor thread; the gate is documented to run with
        // `--test-threads=1`, so no other thread is reading the environment.
        unsafe {
            std::env::set_var("KLAMOTTENKISTE_PRESENT", "readback");
        }
    });
}

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

/// Wait until a line matching `pred` arrives or the deadline passes.
fn wait_for_line(rx: &Receiver<String>, timeout: Duration, pred: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        match rx.recv_timeout(remaining) {
            Ok(line) if pred(&line) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// PIDs of direct children of this process whose comm is `Xwayland`
/// (includes zombies — an unreaped child stays in `/proc` in state Z).
fn xwayland_children() -> Vec<u32> {
    let my_pid = std::process::id();
    let mut pids = Vec::new();
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
        // Format: "pid (comm) state ppid ..." — comm may contain spaces,
        // so split on the *last* ')'.
        let Some(open) = stat.find('(') else { continue };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let ppid = stat[close + 2..]
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if comm == "Xwayland" && ppid == my_pid {
            pids.push(pid);
        }
    }
    pids
}

/// Fast first gate: the compositor advertises an X11 display at all.
#[test]
#[ignore = "red until Xwayland support lands"]
fn x11_display_advertised() {
    require_xwayland_binary();
    init_readback();

    let mut handle = spawn_headless(OUT_W, OUT_H).expect("spawn_headless failed");
    let display = wait_for_x11_display(&handle, DISPLAY_WAIT);
    handle.shutdown();

    assert!(
        display.is_some(),
        "Xwayland support not implemented yet: x11_display() returned None \
         (waited {DISPLAY_WAIT:?})"
    );
}

/// The "done" signal: X11 client maps, composites, receives input, tears down.
#[test]
#[ignore = "red until Xwayland support lands"]
fn x11_client_end_to_end() {
    require_xwayland_binary();
    init_readback();

    let mut handle = spawn_headless(OUT_W, OUT_H).expect("spawn_headless failed");
    let display = wait_for_x11_display(&handle, DISPLAY_WAIT)
        .expect("Xwayland support not implemented yet: x11_display() returned None");

    // Spawn the guinea pig. `cargo run -p` rather than CARGO_BIN_EXE_*: that
    // env var is only generated for bins of the package under test.
    let mut client = KillOnDrop(
        Command::new(env!("CARGO"))
            .args(["run", "--quiet", "-p", "x11-echo"])
            .env("DISPLAY", &display)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("launching x11-echo via cargo failed"),
    );
    let lines = spawn_line_reader(&mut client.0);

    // Compositor -> screen: the X11 window was WM-mapped and composited.
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
        "composited frame never showed the client's magenta fill within {PAINT_WAIT:?} \
         (any frame published at all: {saw_any_frame}) — X11 window likely not mapped"
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
    assert!(
        wait_for_line(&lines, INPUT_WAIT, |l| l.starts_with("key-press ")),
        "client never reported key-press within {INPUT_WAIT:?} after `key a tap` \
         — X11 keyboard focus/routing missing"
    );
    let center = format!("click {} {}", OUT_W / 2, OUT_H / 2);
    assert_eq!(
        control_request(&control, &center).as_deref(),
        Ok("ok"),
        "control socket rejected `{center}`"
    );
    assert!(
        wait_for_line(&lines, INPUT_WAIT, |l| l.starts_with("button-press ")),
        "client never reported button-press within {INPUT_WAIT:?} after `{center}` \
         — X11 pointer routing missing"
    );

    // Teardown: no stray Xwayland process survives shutdown.
    drop(client);
    handle.shutdown();
    let deadline = Instant::now() + REAP_WAIT;
    let mut leftover = xwayland_children();
    while !leftover.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(150));
        leftover = xwayland_children();
    }
    assert!(
        leftover.is_empty(),
        "Xwayland child process(es) survived shutdown: {leftover:?}"
    );
}
