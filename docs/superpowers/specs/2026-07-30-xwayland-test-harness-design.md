# Xwayland readiness test harness

Date: 2026-07-30
Status: approved

## Purpose

klamottenkiste's nested compositor currently hosts native Wayland clients only.
Downstream (kabelsalat) wants to embed X11-only applications — concretely the
Android SDK emulator UI, whose bundled Qt talks X11 — which requires the nested
Smithay compositor to run an Xwayland server and act as its X11 window manager.

This spec defines a **testing harness** that certifies that work: a headless
`cargo test` gate that is red today and turns green exactly when Xwayland
support is done. It does **not** design the Xwayland implementation itself.

"Done" means, per the harness: an X11 client connects, its window is mapped and
composited into the nested compositor's output, synthetic keyboard and pointer
input reaches it, and teardown leaves no stray Xwayland process.

## Constraints

- Runs under plain `cargo test` with no display or desktop session (bare CI
  runner / SSH). The GTK widget layer is not involved.
- Tests are `#[ignore]`d so normal `cargo test` runs stay green while the
  feature is unimplemented; the gate is run by name.
- Requires an `Xwayland` binary on `PATH` at run time. A missing binary is an
  environment error (fail with an install hint, e.g. Fedora's
  `xorg-x11-server-Xwayland`), not a red feature.

## Architecture

Three pieces, all in the klamottenkiste repo:

### 1. Contract seam: `HeadlessHandle::x11_display()`

`vendor/nested-wayland-session` gains one public method:

```rust
impl HeadlessHandle {
    /// DISPLAY name (e.g. ":2") of this compositor's Xwayland server,
    /// or None while Xwayland support is unimplemented / not yet ready.
    pub fn x11_display(&self) -> Option<String>;
}
```

It ships as a stub returning `None` in the same commit as the harness. This is
the only new API the harness introduces; frame readback (`latest_frame()`),
input injection and the control socket already exist and are proven by
`tests/lifecycle.rs`. The Xwayland implementation fills in this method as its
final step; the harness then goes green with no test changes.

### 2. Guinea-pig client: `test-clients/x11-echo`

A new workspace **bin crate** (dependency: `x11rb`), kept out of the library's
dependency tree. Behavior:

- Connect to `$DISPLAY`, create one window, map it.
- Fill the window `#FF00FF` (magenta) on every Expose.
- Print one line per event to stdout, flushed immediately:
  `mapped`, `exposed`, `key-press <keycode>`, `button-press <n>`.

Magenta is chosen because nothing else in the compositor's output (black clear
color, no decorations, single hosted window) can produce it accidentally. The
integration test spawns the client via `cargo run -p x11-echo` — the same
spirit as `lifecycle.rs` spawning system `foot`, but with deterministic pixels
and an input-echo channel `foot` cannot provide.

### 3. The gate: `vendor/nested-wayland-session/tests/xwayland.rs`

Both tests `#[ignore]`, run via:

```
cargo test -p monochromatic-nested-wayland-session --test xwayland -- --ignored
```

Both use `KLAMOTTENKISTE_PRESENT=readback` (CPU-readback frames, as in
`lifecycle.rs`) and deadline-polling helpers copied from that test's style.

**`x11_display_advertised`** — fast first gate:
1. Assert `Xwayland` is on `PATH`; panic with an install hint otherwise.
2. `spawn_headless`, poll `x11_display()` for up to 10 s.
3. On timeout, fail with: `Xwayland support not implemented yet:
   x11_display() returned None` — red is self-explaining.

**`x11_client_end_to_end`** — the "done" signal:
1. Spawn compositor, obtain `DISPLAY` via `x11_display()`.
2. Spawn `x11-echo` with that `DISPLAY`; capture its stdout.
3. Wait until `latest_frame()`'s center pixel is magenta (tolerance ±2 per
   channel, deadline-polled). This proves the X11 window was WM-mapped and
   composited — not merely connected.
4. Inject `key a tap` and `click <center-x> <center-y>` over the control
   socket.
5. Wait for matching `key-press` and `button-press` lines on the client's
   stdout. This proves seat focus and input routing reach the X11 world.
6. `shutdown()`; assert the Xwayland child process is reaped (no zombie, no
   surviving process), mirroring the fd-leak discipline of `lifecycle.rs`.

Steps 3 and 5 test the two directions of the embedding contract
independently: compositor→screen and host→client. Either can break without
the other.

## Error handling

- All waits are polls with explicit deadlines (10 s default); every timeout
  message states which stage timed out and what was observed instead.
- Client stdout is read on a thread with line buffering so a wedged client
  cannot deadlock the test.
- Compositor and client processes are killed on test panic (RAII guards, as
  in `lifecycle.rs`).

## Out of scope

- The Xwayland implementation itself (Smithay `xwayland` feature, `XwmHandler`,
  X11 WM wiring). Smithay 0.7.0 ships the module; enabling it is the feature
  branch's job.
- Multi-window / override-redirect X11 semantics beyond "one window maps".
  The compositor's single-app model is a known constraint the implementation
  must reconcile; the harness certifies only the single-window case kabelsalat
  needs.
- X11 clipboard, cursor shapes, HiDPI.
- CI setup (the repo has none) and any kabelsalat-side changes.

## Acceptance

- On main today: both tests compile, are skipped by default, and fail with the
  self-explaining "not implemented yet" message when run with `--ignored`.
- When Xwayland support lands: both tests pass with `--ignored`, and the
  feature branch's final commit removes the `#[ignore]` attributes.
