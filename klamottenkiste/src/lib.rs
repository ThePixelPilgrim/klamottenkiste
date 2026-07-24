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

mod widget;

/// The reusable GTK4 widget: drop a hosted Wayland app into any layout.
///
/// See [`widget`] (its module docs) for the two-lifecycle model — map/visibility vs. object
/// lifetime — and the embedder contract for hiding a pane without killing its browser.
pub use widget::WaylandPane;

/// Re-export of the vendored nested compositor.
///
/// The widget builds on this internally; it stays public because an embedder still needs the
/// input event type ([`compositor::input::SpikeInput`]) to drive a pane's
/// [`WaylandPane::input_sender`], and the multi-instance gate example drives `spawn_headless`
/// and the control socket directly.
pub use nested_wayland_session as compositor;
