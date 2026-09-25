# Headless display (`klamottenkiste-display`)

Date: 2026-09-25
Status: **draft, in progress**
- Section 1: approved.
- Section 2: proposed, awaiting review.
- Sections 3 onwards: not yet written.

Context: this is spec 1 of 3 for kabelsalat's remote display pane. The
decision record is kabelsalat
`docs/superpowers/specs/2026-09-25-remote-display-design.md`, and the research
is under kabelsalat `docs/research/2026-09-25-*`.

## Goal

A static, GTK-free binary that runs klamottenkiste's compositor on a server
with no GPU and no viewer. It:
- hosts one app;
- keeps it running at full pace whether anyone watches or not;
- gives the local session screenshots, synthetic input and low-CPU video
  recording through a control socket.

A network viewer stream is spec 2 and plugs in later as one more consumer.

## Starting point (verified 2026-09-25, v0.2.1+1)

- The compositor, `vendor/nested-wayland-session`, is already GTK-free. It
  runs on its own thread (`spawn_headless` → `HeadlessHandle`) and has a
  line-based control socket. GTK lives only in `klamottenkiste/src/widget.rs`.
- Apps are already never throttled by the widget: frame callbacks come from a
  fixed ~60 Hz calloop timer, even while the pane is unmapped.
- **Gaps for a server:**
  - Rendering requires an EGL/GBM render node.
  - Every frame is a full redraw (buffer age 0), even though damage is
    tracked.
  - The control socket serves one connection at a time, text only, at the
    socket path `$TMPDIR/kabelsalat-spike-control-<pid>-<seq>.sock`.
  - Xwayland is a stub (`x11_display()` returns `None`).
  - xkb data is missing on minimal hosts.

## 1. Architecture and process model (approved)

- **Binary:** `klamottenkiste-display`, built from the klamottenkiste repo on
  top of the existing compositor crate. It is statically linked with musl
  for x86_64 and aarch64. kabelsalat deploys it as
  `~/.local/share/kabelsalat/bin/klamottenkiste-display-<version>` under its
  version rules.
- **One display = one process = one app:**
  `klamottenkiste-display run --name <id> -- <app argv…>`.
  - If the app exits, the display stays up so the app can be relaunched.
    `stop` ends the display.
  - kabelsalat runs displays inside its remote tmux server as session
    `ksd-<group-uuid>`, so they survive disconnects.
- **Sockets:** one directory per display, mode 0700.
  - The directory is `$XDG_RUNTIME_DIR/klamottenkiste/<name>/`, falling back
    to `/tmp/klamottenkiste-<uid>/<name>/` because ssh sessions may lack
    `XDG_RUNTIME_DIR`.
  - `wayland.sock` is for the app, set as `WAYLAND_DISPLAY=<absolute path>`.
  - `control.sock` is the control API and accepts any number of concurrent
    connections.
- **Pacing is decoupled from rendering:**
  - Apps get frame callbacks at 60 Hz always, never throttled by consumers.
  - Compositing happens **only on demand**: when at least one consumer exists
    (a screenshot, a recording, later a viewer) *and* there is damage.
  - With no consumer there is no compositing, so an idle display costs
    almost nothing.
  - Consumers share one composited frame at the highest rate any of them
    asks for.
- **Renderer:** Smithay's pixman renderer, behind a feature flag. It is the
  only renderer in the static build, so the build has no EGL or GBM. The GTK
  widget keeps GLES/dmabuf.
- **Incremental damage:** real buffer-age-based damage replaces the full
  redraw. This is also the input for damage-driven recording and, later, for
  tiles.
- **Client subcommands** of the same binary, each talking to `control.sock`:
  - `screenshot <file.png>`
  - `click` / `key` / `type` / `resize`
  - `record …` (Section 3)
  - `status`
  - `stop`

  A display is found by `--name` or by `KLAMOTTENKISTE_DISPLAY`, which
  kabelsalat exports into the remote sessions.
- **Changes to the compositor crate:**
  1. a pixman renderer path;
  2. on-demand, damage-driven compositing;
  3. a multi-connection control socket with binary streams;
  4. configurable socket paths;
  5. embedded xkb data.

## 2. Xwayland (proposed, awaiting review)

- **Goal:** turn the existing harness
  (`2026-07-30-xwayland-test-harness-design.md`, with `tests/xwayland.rs`,
  `tests/xwayland_e2e.rs` and `test-clients/x11-echo`) green, with no changes
  to the tests.
- **Implementation:** Smithay's built-in `xwayland` feature and `X11Wm`, in
  process. Rejected alternative: xwayland-satellite, a separate process and
  binary. kabelsalat's local notes also record a satellite clipboard wedge.
- **Xwayland is a host dependency.**
  - `run --x11` starts it. It is opt-in because an idle Xwayland costs about
    20–30 MB.
  - If it is missing, the display refuses to start and names the package
    (Fedora: `xorg-x11-server-Xwayland`).
  - `DISPLAY` is passed to the app, written to `<dir>/x11-display`, and
    reported by `status`.
- **Window placement:**
  - X11 windows float at the geometry they request, and override-redirect
    windows sit exactly where they ask.
  - Wayland toplevels stay maximized.
  - Reason: the Android emulator has a separate frameless toolbar window
    beside its main window, and maximizing everything would stack it over
    the screen.
  - This is a risk point to validate against a real emulator.
- **Input and clipboard:**
  - Control-API input reaches X11 windows through the same seat, with
    click-to-focus.
  - The text clipboard is bridged X11 ⇄ Wayland if Smithay's selection
    support makes it cheap. Otherwise it goes out of v1 into known-issues.
- **Tests:**
  1. the existing harness;
  2. new: two floating X11 windows at their requested positions plus an
     override-redirect popup, checked by pixel sampling;
  3. manual: the real Android emulator (with KVM): main window, toolbar,
     a menu, and `adb` from the session.

## 3. Control API (not yet written)

To cover:
- concurrent connections;
- binary streams;
- `record` as a handle, where the connection's lifetime is the recording,
  with `record out.webm [-- cmd]` and `record -` (stdout);
- WebM live mode, so a killed recorder leaves a playable file;
- VP8 by default and `--codec vp9`, MP4 only via a host `ffmpeg`;
- damage-driven variable frame rate, `--fps` (default 10), `--scale`,
  `--max`.

## Later sections (not yet written)

Static build and deployment (musl, pixman, libvpx and xkb data), error
handling, and testing, including CPU measurements for recording on a real
host.
