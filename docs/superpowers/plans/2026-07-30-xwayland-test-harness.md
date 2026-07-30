# Xwayland Readiness Test Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A headless `cargo test` gate that is red today and turns green exactly when the nested compositor can host, composite, and route input to an X11 client via Xwayland.

**Architecture:** Three pieces per the approved spec (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`): a stubbed `HeadlessHandle::x11_display()` contract seam in the vendored compositor crate, a tiny `x11-echo` X11 guinea-pig bin crate, and two `#[ignore]`d integration tests that assert display advertisement, magenta-pixel compositing, input echo, and clean teardown.

**Tech Stack:** Rust edition 2024, workspace resolver 3. Test infra mirrors `vendor/nested-wayland-session/tests/lifecycle.rs` (spawn_headless + readback frames + control socket over `UnixStream`). New dev-side dep: `x11rb = "0.13"` (already in `Cargo.lock` at 0.13.2, so no lock churn).

## Global Constraints

- Everything runs headless: no GTK, no display session, no `gtk::init()`.
- The library crate `monochromatic-nested-wayland-session` (lib name `nested_wayland_session`) gains **no new dependencies**; `x11rb` lives only in the new `x11-echo` bin crate.
- Both harness tests carry `#[ignore = "red until Xwayland support lands"]` so plain `cargo test` stays green.
- Env var `KLAMOTTENKISTE_PRESENT=readback` must be set before any `spawn_headless` call (readback frames feed `latest_frame()`); in a multi-test binary this write must go through `std::sync::Once` and the gate is run with `--test-threads=1`. *(Superseded by final review: the gate ships as two single-test binaries, so the write is a plain `set_var` at test start and `--test-threads=1` is gone — see the spec's "Amended after final review" note.)*
- A missing `Xwayland` binary on `PATH` is an environment error: fail with install hint `Fedora: xorg-x11-server-Xwayland`, never a silent skip.
- Doc-comment style in the vendored crate uses the existing `What:` / `Why:` convention — follow it.
- Workspace license is LGPL-3.0-or-later; new crate inherits via `license.workspace = true`.
- Commit subjects follow the repo's `<Area>: <imperative summary>` style (e.g. `Spec: …`); use the `Harness:` prefix.

---

### Task 1: Contract seam `HeadlessHandle::x11_display()` stub

**Files:**
- Modify: `vendor/nested-wayland-session/src/app.rs` (inside `impl HeadlessHandle`, after `control_socket_path()` around line 390)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub fn x11_display(&self) -> Option<String>` on `nested_wayland_session::HeadlessHandle`, stub-returning `None`. Task 3's tests poll exactly this method.

**Note on TDD:** This task intentionally has no "assert it returns `None`" unit test — such a test would turn red the moment the feature lands, inverting the gate. The failing tests for this seam are Task 3's harness tests; this task's verification is compilation.

- [ ] **Step 1: Add the stub method**

In `vendor/nested-wayland-session/src/app.rs`, inside `impl HeadlessHandle`, directly after the `control_socket_path` method:

```rust
    /// `DISPLAY` name (e.g. `:2`) of this compositor's Xwayland server.
    ///
    /// What:     `pub fn x11_display(&self) -> Option<String>`. Returns `None` while
    ///           Xwayland support is unimplemented or the X server is not yet ready.
    /// Why:      The contract seam of the Xwayland readiness harness
    ///           (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`):
    ///           `tests/xwayland.rs` polls this and goes green exactly when a real
    ///           display name is returned. Filling this in is the Xwayland feature
    ///           branch's final step.
    pub fn x11_display(&self) -> Option<String> {
        None
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p monochromatic-nested-wayland-session`
Expected: clean (warnings-free; the method is `pub`, so no dead-code warning).

- [ ] **Step 3: Run the crate's existing tests to confirm nothing broke**

Run: `cargo test -p monochromatic-nested-wayland-session`
Expected: all existing unit tests + `lifecycle` pass as before.

- [ ] **Step 4: Commit**

```bash
git add vendor/nested-wayland-session/src/app.rs
git commit -m "Harness: stub HeadlessHandle::x11_display seam"
```

---

### Task 2: `x11-echo` guinea-pig client crate

**Files:**
- Create: `test-clients/x11-echo/Cargo.toml`
- Create: `test-clients/x11-echo/src/main.rs`
- Modify: `Cargo.toml` (workspace root — add member)

**Interfaces:**
- Consumes: `$DISPLAY` env var (set by the spawning test).
- Produces: a bin named `x11-echo`, runnable as `cargo run -p x11-echo`. Wire protocol on stdout, one flushed line per event: `mapped`, `exposed`, `key-press <keycode>`, `button-press <n>`. Window fills `#FF00FF` on every Expose. Task 3 spawns it and parses these exact strings.

**Note on TDD:** An X11 client cannot be unit-tested without an X server — which is the very thing that doesn't exist yet. Verification here is compilation; behavioral verification is Task 3's end-to-end test (and, today, its well-defined red failure).

- [ ] **Step 1: Register the workspace member**

Edit root `Cargo.toml`, first line block:

```toml
[workspace]
members = ["klamottenkiste", "vendor/nested-wayland-session", "test-clients/x11-echo"]
resolver = "3"
```

- [ ] **Step 2: Create the crate manifest**

`test-clients/x11-echo/Cargo.toml`:

```toml
[package]
name = "x11-echo"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
publish = false
description = "X11 guinea-pig for the Xwayland readiness harness: paints magenta, echoes input events on stdout."

[dependencies]
x11rb = "0.13"
```

- [ ] **Step 3: Write the client**

`test-clients/x11-echo/src/main.rs`:

```rust
//! X11 guinea-pig for the Xwayland readiness harness
//! (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`).
//!
//! Connects to `$DISPLAY`, maps one window, fills it `#FF00FF` on every Expose,
//! and prints one flushed line per observed event so the spawning test can
//! assert on stdout: `mapped`, `exposed`, `key-press <keycode>`,
//! `button-press <n>`.

use std::io::Write;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Rectangle, WindowClass,
};

/// `#FF00FF` as a truecolor pixel value. Xwayland's default root visual is
/// 24-bit truecolor where the pixel is `0x00RRGGBB`, so this holds without a
/// visual lookup.
const MAGENTA: u32 = 0x00ff_00ff;

/// Initial window size; the compositor's X11 WM is expected to reconfigure the
/// window (the nested session maps its single app fullscreen), tracked via
/// ConfigureNotify.
const INIT_W: u16 = 640;
const INIT_H: u16 = 480;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];

    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        INIT_W,
        INIT_H,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::KEY_PRESS
                    | EventMask::BUTTON_PRESS
                    | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(MAGENTA))?;
    conn.map_window(win)?;
    conn.flush()?;

    let stdout = std::io::stdout();
    let mut size = (INIT_W, INIT_H);
    loop {
        let event = conn.wait_for_event()?;
        let mut out = stdout.lock();
        match event {
            Event::MapNotify(_) => writeln!(out, "mapped")?,
            Event::ConfigureNotify(e) => size = (e.width, e.height),
            Event::Expose(_) => {
                conn.poly_fill_rectangle(
                    win,
                    gc,
                    &[Rectangle {
                        x: 0,
                        y: 0,
                        width: size.0,
                        height: size.1,
                    }],
                )?;
                conn.flush()?;
                writeln!(out, "exposed")?;
            }
            Event::KeyPress(e) => writeln!(out, "key-press {}", e.detail)?,
            Event::ButtonPress(e) => writeln!(out, "button-press {}", e.detail)?,
            _ => {}
        }
        out.flush()?;
    }
}
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p x11-echo`
Expected: clean build. (Do not try to run it — there is no X server to connect to; running it now exits with a connect error, which is correct behavior.)

- [ ] **Step 5: Verify the workspace still builds and tests pass**

Run: `cargo test --workspace`
Expected: same pass/ignore counts as before this task (the new crate has no tests).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock test-clients/x11-echo
git commit -m "Harness: add x11-echo X11 guinea-pig client"
```

---

### Task 3: The gate — `tests/xwayland.rs` + docs

**Files:**
- Create: `vendor/nested-wayland-session/tests/xwayland.rs`
- Modify: `klamottenkiste/docs/widget.md` (testing section, after the existing test-command documentation around lines 106–113)

**Interfaces:**
- Consumes: `nested_wayland_session::{spawn_headless, Frame, HeadlessHandle}`; `HeadlessHandle::x11_display()` (Task 1); `handle.latest_frame()`, `handle.control_socket_path()`, `handle.shutdown()`; `cargo run -p x11-echo` and its stdout protocol (Task 2); control-socket wire format (`"ok"` / `"err …"` single-line responses).
- Produces: the runnable gate. Green condition = Xwayland support done.

- [ ] **Step 1: Write the failing tests**

`vendor/nested-wayland-session/tests/xwayland.rs`:

```rust
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
use std::sync::mpsc::{Receiver, channel};
use std::sync::Once;
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
        let Some(close) = stat.rfind(')') else { continue };
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
    let display = wait_for_x11_display(&handle, DISPLAY_WAIT).expect(
        "Xwayland support not implemented yet: x11_display() returned None",
    );

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
```

- [ ] **Step 2: Verify it compiles and default runs stay green**

Run: `cargo test -p monochromatic-nested-wayland-session --test xwayland`
Expected: compiles; output ends with `test result: ok. 0 passed; 0 failed; 2 ignored`.

- [ ] **Step 3: Run the gate to verify it is red for the right reason**

Run:
```sh
cargo build -p x11-echo
cargo test -p monochromatic-nested-wayland-session --test xwayland -- --ignored --test-threads=1
```
Expected: **2 failed**, and both failure messages contain
`Xwayland support not implemented yet: x11_display() returned None`.
Any other failure (compile error, panic in `spawn_headless`, the PATH
assertion) is a harness bug or missing package — fix before committing.

- [ ] **Step 4: Document the gate**

In `klamottenkiste/docs/widget.md`, append to the testing section (directly after the existing test-command documentation, currently around lines 106–113):

```markdown
### Xwayland readiness gate

Two `#[ignore]`d integration tests certify Xwayland support
(`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`): red
with a self-explaining message until the feature lands, green when an X11
client maps, composites, and receives input headlessly. Requires an
`Xwayland` binary on `PATH` (Fedora: `xorg-x11-server-Xwayland`).

    cargo build -p x11-echo
    cargo test -p monochromatic-nested-wayland-session --test xwayland -- --ignored --test-threads=1
```

- [ ] **Step 5: Commit**

```bash
git add vendor/nested-wayland-session/tests/xwayland.rs klamottenkiste/docs/widget.md
git commit -m "Harness: Xwayland readiness gate tests"
```

---

## Verification after all tasks

Scoped deliberately: workspace-wide `cargo fmt --all -- --check` and
`cargo clippy --workspace` both fail on pre-existing debt that predates this
work (237 `clippy::implicit_return` violations in the vendored crate, plus
unformatted `klamottenkiste/examples/*`), so they cannot serve as a gate here.

1. `rustfmt --edition 2024 --check` on the files this work touches
   (`vendor/nested-wayland-session/tests/xwayland.rs`,
   `vendor/nested-wayland-session/tests/xwayland_e2e.rs`,
   `test-clients/x11-echo/src/main.rs`) — clean.
2. `cargo clippy -p x11-echo` — clean.
3. `cargo test --workspace` — everything green; each Xwayland gate binary
   reports `1 ignored`.
4. The gate command fails exactly 2 tests, both with the "not implemented yet"
   message:

   ```sh
   cargo build -p x11-echo
   cargo test -p monochromatic-nested-wayland-session --test xwayland --test xwayland_e2e \
       --no-fail-fast -- --ignored
   ```
