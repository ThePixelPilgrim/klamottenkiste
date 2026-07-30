# Xwayland readiness test harness

Date: 2026-07-30
Status: approved (amended after final review, 2026-07-30)

## Amended after final review (2026-07-30)

Implementation review changed three things from the approved design; the body
below is updated to match, and this note records the deltas:

1. **The client is spawned directly, not via `cargo run`.** The test locates
   `target/<profile>/x11-echo` relative to its own executable. Via `cargo run`,
   kill-on-drop killed the cargo wrapper rather than the client, and the paint
   deadline had to absorb a possible cold compile.
2. **Two single-test binaries instead of one two-test file.**
   `tests/xwayland.rs` and `tests/xwayland_e2e.rs`. With exactly one test per
   binary, `KLAMOTTENKISTE_PRESENT` can be set with a plain
   `unsafe { std::env::set_var(..) }` at test start — the same safety argument
   `lifecycle.rs` uses — instead of a `std::sync::Once` whose soundness leaned
   on a `--test-threads=1` convention a caller could forget.
3. **The gate command changed accordingly** (both binaries, `--no-fail-fast`,
   no `--test-threads=1`).

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
color, no decorations, single hosted window) can produce it accidentally. It is
the same spirit as `lifecycle.rs` spawning system `foot`, but with
deterministic pixels and an input-echo channel `foot` cannot provide.

The integration test spawns the **pre-built binary directly** from
`target/<profile>/x11-echo`, located relative to the test executable's own path
(`current_exe()`'s grandparent is the profile dir). The gate command builds it
first. Direct spawn, not `cargo run -p x11-echo`: the client is then a real
child of the test process, so kill-on-drop kills the client rather than a cargo
wrapper, and no compile can happen inside a wait deadline. (`CARGO_BIN_EXE_*`
is not an option — cargo only generates it for bins of the package under test.)

### 3. The gate: two single-test binaries

`vendor/nested-wayland-session/tests/xwayland.rs` and
`vendor/nested-wayland-session/tests/xwayland_e2e.rs` — one `#[ignore]`d test
each, run via:

```
cargo build -p x11-echo
cargo test -p monochromatic-nested-wayland-session --test xwayland --test xwayland_e2e \
    --no-fail-fast -- --ignored
```

`--no-fail-fast` because cargo stops after the first failing test binary and,
while the gate is red, both fail.

One test per binary rather than one file with two tests: each can then set
`KLAMOTTENKISTE_PRESENT=readback` (CPU-readback frames, as in `lifecycle.rs`)
with a plain `unsafe { std::env::set_var(..) }` at test start, justified the
same way `lifecycle.rs` justifies it — the binary runs exactly one test, so
there is no concurrent reader. The alternative, a `std::sync::Once` in a
two-test binary, is only sound under `--test-threads=1`, a convention the
caller can silently drop. The cost is a handful of duplicated helpers between
the two files, which matches how this crate keeps its test binaries
self-contained.

Both use deadline-polling helpers copied from `lifecycle.rs`'s style.

**`x11_display_advertised`** — fast first gate:
1. Assert `Xwayland` is on `PATH`; panic with an install hint otherwise.
2. `spawn_headless`, poll `x11_display()` for up to 10 s.
3. On timeout, fail with: `Xwayland support not implemented yet:
   x11_display() returned None` — red is self-explaining.

**`x11_client_end_to_end`** — the "done" signal:
1. Snapshot every `Xwayland` PID visible in `/proc` (for step 7).
2. Spawn compositor, obtain `DISPLAY` via `x11_display()`.
3. Spawn `target/<profile>/x11-echo` with that `DISPLAY`; capture its stdout
   and stderr.
4. Wait for the client's own `mapped`, then `exposed`, lines. This isolates
   "the client never reached a working X server" from "the client painted but
   nothing was composited" — without it, both look like a blank frame.
5. Wait until `latest_frame()`'s center pixel is magenta (tolerance ±2 per
   channel, deadline-polled). This proves the X11 window was WM-mapped and
   composited — not merely connected.
6. Inject `key a tap` and `click <center-x> <center-y>` over the control
   socket, and wait for matching `key-press` and `button-press` lines on the
   client's stdout. This proves seat focus and input routing reach the X11
   world.
7. `shutdown()`; assert that no `Xwayland` PID absent from step 1's snapshot
   survives (no zombie, no surviving process), mirroring the fd-leak discipline
   of `lifecycle.rs`. Parentage is deliberately not part of the filter: a
   compositor that re-parents or double-forks its Xwayland would otherwise make
   the check pass vacuously.

Steps 5 and 6 test the two directions of the embedding contract
independently: compositor→screen and host→client. Either can break without
the other.

## Error handling

- All waits are polls with explicit deadlines (10 s default); every timeout
  message states which stage timed out and what was observed instead.
- Client stdout is read on a thread with line buffering so a wedged client
  cannot deadlock the test. Client **stderr** is drained the same way into a
  shared buffer, and its last ~10 lines are quoted in every failure message
  raised after the client is spawned — otherwise an X11 connect error is
  indistinguishable from a compositor bug.
- Waiting for a client line yields one of three outcomes — matched, timed out,
  or *client gone* (stdout hit EOF) — each with its own message. A dead client
  must never be reported as, say, missing input routing.
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
