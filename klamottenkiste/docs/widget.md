# `WaylandPane` — the embeddable widget

`KstWaylandPane` (Rust type `klamottenkiste::WaylandPane`) is a GObject subclass of
`gtk::Widget` that embeds a hosted Wayland application in any GTK4 layout. It owns a headless
nested [Smithay] compositor, presents its composited output in an internal `gtk::Picture`
(`ContentFit::Contain`), and forwards GTK pointer/scroll/keyboard/focus events into the nested
compositor's seat.

The whole embedding pipeline — spawn, frame pump, input translation, coordinate mapping,
resize — lives inside the widget. An embedder just constructs one, drops it into a container,
and (optionally) points a client at its socket.

## API

Construction:

```rust
use klamottenkiste::WaylandPane;
let pane = WaylandPane::new();      // spawns the nested compositor immediately
```

The compositor is spawned **once, in `constructed()`** — before the widget is ever mapped —
and lives for the whole object lifetime.

### Getters / properties

| Rust getter | GObject property | Returns |
|---|---|---|
| `pane.wayland_socket()` | `wayland-socket` (read-only string) | nested `WAYLAND_DISPLAY` name, e.g. `wayland-3` |
| `pane.control_socket_path()` | `control-socket-path` (read-only string) | per-instance control socket path |
| `pane.input_sender()` | — | clone of the seat-input `Sender<SpikeInput>` |
| `pane.startup_error()` | — | compositor-startup error message, if spawn failed |
| `pane.is_running()` | — | `true` until closed/disposed |

Launch a client into a pane:

```rust
if let Some(socket) = pane.wayland_socket() {
    // WAYLAND_DISPLAY=<socket> chromium … (or foot, etc.)
}
```

Drive it programmatically via the control socket (`ping`, `screenshot <path>`, `click`,
`key`, `type`, `resize`) — this reaches **only** the hosted client, never the host seat.

### Teardown

```rust
pane.close();   // explicit: stop the compositor + client now, keep the widget object
```

`close()` is idempotent and does exactly what `dispose` does. Dropping the last reference to
the widget (GObject finalization → `dispose`) tears the compositor down automatically.

## The N-instance result

The isolation gate that unblocked this widget (`examples/multi_headless`, and the
`widget-gate` commit) proved that **N = 3 independent nested compositors coexist in one
process**, each hosting its own isolated Wayland client with no cross-talk: three distinct
nested sockets and three distinct control sockets, three clients rendering three distinct
frames. The bug that had to be fixed was a per-PID control-socket path that collided when
several instances shared a PID; it is now per-instance (a monotonic sequence number appended
to the PID).

`examples/multi` shows the same result through the widget: three `WaylandPane`s side by side
in one `adw::ApplicationWindow`, each with a HIDE/SHOW toggle.

## Lifecycle / shutdown contract

There are **two decoupled lifecycles**. Confusing them is the classic mistake this widget is
designed to avoid.

### (a) Map / visibility — a VIEW concern

`map`/`unmap`, hide/show, detach/re-attach from a container.

* On `map`: the frame pump (~60 Hz `latest_frame()` → `gdk::MemoryTexture` → `Picture`) and
  the allocation→resize poll **start/resume**.
* On `unmap`: both **pause** (no readback while hidden — it saves CPU).
* Pausing them **does not touch the compositor**. No shutdown is done in `unmap` or
  `unrealize`.

### (b) Object lifetime — the COMPOSITOR concern

`construct → dispose`.

* The compositor and its hosted client are spawned once in `constructed()` and torn down once,
  in `dispose()` (final drop) **or** via an explicit `close()`.
* Teardown stops the compositor thread (signals the calloop `LoopSignal`, joins the thread —
  releasing EGL / the render node), stops and joins the control thread, and unlinks the
  control-socket file. No leaked threads, sockets, or fds remain.

### Embedder contract (say it out loud)

> **To hide the pane while keeping its browser alive, keep a reference to the `WaylandPane`
> and detach it from the visible layout (or `set_visible(false)`) — do NOT drop it.** Hiding /
> unmapping preserves the hosted app's page, scroll, form state, and any CDP connection across
> any number of hide/show cycles. **Dropping the last reference (or calling `close()`) is what
> destroys the browser.**

Toggling a pane off and on is a daily action and must be cheap and state-preserving; that is
why unmap only pauses the view.

## Verification

Headless / build-only (no GTK window is opened — a real window would steal focus):

* `cargo build --examples` links `demo`, `multi`, and `multi_headless`.
* `cargo test -p monochromatic-nested-wayland-session` — 31 unit tests + 1 lifecycle
  integration test, 0 doctests.
* `cargo test -p klamottenkiste --lib` — 6 unit tests for the pure widget logic
  (`widget_to_output` against the **runtime** output size, `requested_output`,
  `gtk_button_to_evdev`).

### Xwayland readiness gate

Two `#[ignore]`d integration tests certify Xwayland support
(`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`): red
with a self-explaining message until the feature lands, green when an X11
client maps, composites, and receives input headlessly. Requires an
`Xwayland` binary on `PATH` (Fedora: `xorg-x11-server-Xwayland`).

    cargo build -p x11-echo
    cargo test -p monochromatic-nested-wayland-session --test xwayland -- --ignored --test-threads=1

The lifecycle integration test (`vendor/nested-wayland-session/tests/lifecycle.rs`) asserts,
headlessly:

* **(a) Teardown** — `spawn_headless` + `shutdown()` in a loop of 5: the control-socket file
  exists (and answers `ping`) while running and is removed after each `shutdown`; `shutdown`
  is idempotent; and the process does not accumulate file descriptors (no leaked
  threads/sockets — a leak would also hang the join).
* **(b) State preservation** — `spawn_headless` **once**, connect a real `foot` client, wait
  for a painted frame, then run several pump pause/resume cycles (the headless stand-in for
  unmap/map) **without** `shutdown`. After every cycle the same compositor is still alive: the
  nested socket name is unchanged, the control socket still answers `ping`, and the client is
  still connected and rendering (its frame is still non-blank). Only the final `shutdown` tears
  it down, after which the socket file is gone and `ping` no longer answers.

On-screen verification of the running widget (real Chromium in a pane, hide/show preserving
page state) needs a display and is left to the user.

[Smithay]: https://github.com/Smithay/smithay
