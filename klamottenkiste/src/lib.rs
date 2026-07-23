//! Embed a Wayland application inside a GTK4 widget.
//!
//! `klamottenkiste` hosts a Wayland client in a nested [Smithay] compositor and presents its
//! composited output inside a GTK4 widget, translating GTK input events into the nested
//! compositor's own seat. The hosted application believes it is talking to a normal Wayland
//! compositor; the embedder gets a widget it can drop into any layout.
//!
//! It also exposes a per-instance control channel (screenshot / click / key / type / resize)
//! that reaches **only** the hosted client — never the host seat — which makes it useful for
//! driving embedded apps programmatically.
//!
//! # Status
//!
//! Early. The embedding pipeline is proven end-to-end (real Chromium: popups, clipboard both
//! directions, keyboard with the host layout, layout reflow on resize, CDP attach), but that
//! was proven as a *standalone application*. Packaging it as a reusable widget — and above all
//! running **several instances in one process** — is the work in progress. See `docs/`.
//!
//! # Licence
//!
//! LGPL-3.0-or-later. The vendored nested compositor under `vendor/nested-wayland-session` is a
//! fork of `monochromatic-nested-wayland-session` and retains its upstream attribution.
//!
//! [Smithay]: https://github.com/Smithay/smithay

#![deny(missing_docs)]

/// Re-export of the vendored nested compositor.
///
/// Exposed while the widget API is still being shaped: the demo and the embedder currently
/// reach through to `spawn_headless`, the control socket, and the input types directly. As the
/// widget API firms up, the surface here should shrink to whatever the widget genuinely needs
/// to hand back to callers.
pub use nested_wayland_session as compositor;
