//! A minimal single-app nested Wayland compositor for GUI testing.
//!
//! This library owns everything except process startup: argument parsing, the
//! compositor state and its protocol handlers, the winit backend and render loop,
//! and the hosted-child lifecycle. Keeping it all in the library (with the binary a
//! thin shell over `run`) lets the display-independent pieces (argument parsing) be
//! unit-tested without opening a window. See the module docs for each part.

/// What:     `pub mod app;`. Declares the `app` module from `src/app.rs`.
/// Why:      Holds `run`, the whole-program orchestration entry.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as app from "./app";
/// ```
pub mod app;

/// What:     `pub mod backend;`. Declares the winit/EGL/dmabuf backend init module.
/// Why:      Builds the nested window, GLES renderer, output, and dmabuf state.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as backend from "./backend";
/// ```
pub mod backend;

/// What:     `pub mod capture;`. Declares the in-process frame-capture module.
/// Why:      Lets an embedding host ask the compositor thread for a freshly rendered frame
///           as bytes (no PNG, no file) — the control socket's `screenshot <path>` command
///           writes a file instead, which is pure overhead for an in-process consumer.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as capture from "./capture";
/// ```
pub mod capture;

/// What:     `pub mod child;`. Declares the hosted-client lifecycle module.
/// Why:      Spawns the app and stops the loop on its exit.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as child from "./child";
/// ```
pub mod child;

/// What:     `pub mod clipboard;`. Declares the SPIKE-scope text clipboard bridge.
/// Why:      Carries the `wl_data_device` clipboard selection across the nested<->host
///           boundary (text mime types only) so the embedder's clipboard and the hosted
///           app's clipboard are the same clipboard.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as clipboard from "./clipboard";
/// ```
pub mod clipboard;

/// What:     `pub mod control;`. Declares the Unix-socket control API module.
/// Why:      Parses control commands and runs them (screenshot, input, resize, quit).
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as control from "./control";
/// ```
pub mod control;

/// What:     `pub mod dnd;`. Declares the compositor-originated drag-and-drop module.
/// Why:      Drives a server-side `text/uri-list` drag toward the hosted app so the app's
///           INBOUND file-drop path can be tested deterministically without a file manager.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as dnd from "./dnd";
/// ```
pub mod dnd;

/// What:     `pub mod input;`. Declares the synthetic input-injection module.
/// Why:      Turns click/key/type commands into seat events.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as input from "./input";
/// ```
pub mod input;

/// What:     `pub mod keymap;`. Declares the US-layout keycode tables.
/// Why:      Maps characters and key names to evdev keycodes; display-independent.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as keymap from "./keymap";
/// ```
pub mod keymap;

/// What:     `pub mod layout_keymap;`. Declares the layout-aware character map module.
/// Why:      `keymap`'s table is US-only; the seat advertises the HOST layout, so synthetic
///           typing must reverse-map characters through the same compiled xkb keymap the
///           client was handed (finding D7). `keymap` remains the fallback.
pub mod layout_keymap;

/// What:     `pub mod protocol;`. Declares the control-protocol parsing module.
/// Why:      Parses request lines and formats response lines; display-independent.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as protocol from "./protocol";
/// ```
pub mod protocol;

/// What:     `pub mod screenshot;`. Declares the framebuffer-readback module.
/// Why:      Renders a frame and encodes it as a PNG.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as screenshot from "./screenshot";
/// ```
pub mod screenshot;

/// What:     `pub mod cli;`. Declares the argument-parsing module.
/// Why:      Turns raw arguments into a validated `Config`; display-independent and
///           unit-tested.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as cli from "./cli";
/// ```
pub mod cli;

/// What:     `pub mod handler;`. Declares the Wayland protocol handler module tree.
/// Why:      Implements the compositor/xdg-shell/shm/seat/dmabuf behaviour.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as handler from "./handler";
/// ```
pub mod handler;

/// What:     `pub mod render;`. Declares the rendering module.
/// Why:      Composites the hosted window into the nested framebuffer each frame.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as render from "./render";
/// ```
pub mod render;

/// What:     `pub mod state;`. Declares the central-state module.
/// Why:      Defines `Compositor`, the value the event loop carries.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export * as state from "./state";
/// ```
pub mod state;

/// What:     `pub use app::{run, spawn_headless, HeadlessHandle};`. Re-export the entry
///           points at the crate root.
/// Why:      Hosts call `nested_wayland_session::spawn_headless` (and the standalone
///           binary calls `run`) without knowing the module layout.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export { run, spawn_headless, HeadlessHandle } from "./app";
/// ```
pub use app::{run, spawn_headless, DmabufFrame, DmabufPlane, Frame, HeadlessHandle};

/// What:     `pub use capture::{CaptureRequest, CaptureResult};`. Re-export the capture
///           request/reply types at the crate root.
/// Why:      A host driving [`HeadlessHandle::request_frame`] needs the reply type without
///           knowing the module layout, exactly as it gets `Frame` from here.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export { CaptureRequest, CaptureResult } from "./capture";
/// ```
pub use capture::{CaptureRequest, CaptureResult};

/// What:     `pub use cli::{parse_args, Config};`. Re-export the parser and its output.
/// Why:      The binary and tests use these directly from the crate root.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// export { parseArgs, Config } from "./cli";
/// ```
pub use cli::{parse_args, Config};

/// What:     `pub mod xkb_host;`. Declares the host keyboard-layout resolver.
/// Why:      The nested seat must advertise the HOST's xkb layout, not Smithay's US
///           default, or every key that differs between the two layouts mistranslates.
pub mod xkb_host;
