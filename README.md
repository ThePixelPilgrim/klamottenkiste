# klamottenkiste

Embed a Wayland application inside a GTK4 widget.

`klamottenkiste` hosts a Wayland client in a nested [Smithay](https://github.com/Smithay/smithay)
compositor and presents its composited output inside a GTK4 widget, translating GTK input events
into the nested compositor's own seat. The hosted application believes it is talking to a normal
Wayland compositor; the embedder gets a widget it can drop into any layout.

Each instance also exposes a control channel — `screenshot`, `click`, `key`, `type`, `resize` —
that reaches **only** the hosted client, never the host seat. That makes it a way to drive
embedded GUI applications programmatically without synthesising global input.

A pane can also hand its current frame straight back to the embedder: `WaylandPane::capture_frame`
renders a fresh frame on the compositor thread and calls back on the GTK main context with upright
RGBA8 pixels (ready for `gdk::MemoryTexture`) — no temp file, no PNG round-trip. The library
returns pixels and nothing else; putting them on a clipboard, in a texture or in a file is the
embedder's policy.

## Status: early, but the hard part works

The embedding pipeline is proven end-to-end against real Chromium:

- **xdg_popup** — `<select>` dropdowns, right-click context menus and DevTools render and take input
- **Clipboard** — text copy/paste in both directions between the hosted app and the host clipboard
- **Keyboard** — real typing with the *host's* xkb layout (not a hardcoded US one)
- **Layout reflow** — resizing the pane changes the nested output resolution, so the hosted app
  genuinely re-lays-out rather than being scaled
- **CDP** — Chromium's remote-debugging port is reachable, so Playwright can attach and sees the
  same page the user does

All of that was proven as a **standalone application**. Turning it into a reusable widget is the
work in progress, and the open question is the important one:

> **Can several instances run in one process?** N compositor threads, N EGL contexts on the same
> DRM render node, N nested Wayland sockets, N readback loops. Only one instance has ever been
> tested.

Related: each pane currently does a full-frame **CPU readback** every frame. That was fine for one
pane; at three or four it may not be, which is when zero-copy dmabuf presentation
(`GdkDmabufTextureBuilder`) stops being an optimisation and becomes load-bearing.

## Layout

```
klamottenkiste/                 the widget crate (public API)
  examples/demo.rs              standalone demo: hosts one client in a window
vendor/nested-wayland-session/  the nested compositor (forked, LGPL) — see its UPSTREAM.md
```

## Building

```
cargo build --example demo
cargo run --example demo        # prints a nested WAYLAND_DISPLAY socket + a control socket
```

Then point any Wayland client at the printed socket:

```
WAYLAND_DISPLAY=<socket> foot
WAYLAND_DISPLAY=<socket> google-chrome --ozone-platform=wayland --new-window <url>
```

### System dependencies

- `mesa-libgbm-devel` — the headless EGL backend links `libgbm`; the runtime `mesa-libgbm`
  package alone is not enough (`cargo check` passes but linking fails with
  `unable to find library -lgbm`). On Fedora: `sudo dnf install mesa-libgbm-devel`.
- A DRM render node (`/dev/dri/renderD*`). Override with `KABELSALAT_DRM_RENDER_NODE`.

## Origin

Extracted from a spike in [kabelsalat](https://github.com/ThePixelPilgrim/kabelsalat), which wanted a
per-tab-group embedded browser pane. The embedding machinery is generally useful on its own, so it
lives here; kabelsalat consumes it as a library.

## Licence

LGPL-3.0-or-later. `vendor/nested-wayland-session` is a fork of
`monochromatic-nested-wayland-session` (from
[Aquaticat/Monochromatic](https://github.com/Aquaticat/Monochromatic)) and retains its upstream
attribution and licence notices — see `vendor/nested-wayland-session/UPSTREAM.md`.
