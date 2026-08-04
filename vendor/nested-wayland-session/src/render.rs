//! Rendering the hosted window into the nested offscreen framebuffer.
//!
//! One `redraw` call binds the backend's framebuffer (a headless GLES renderbuffer),
//! composites the space (the one hosted window) into it with Smithay's `render_output`
//! helper, submits the frame, and sends frame-callbacks so the client draws its next
//! frame. `redraw` is driven by a fixed ~60 Hz calloop timer (see `app.rs`).

/// What:     `use std::time::Duration;`. A span of time.
/// Why:      Frame callbacks report elapsed time; `Duration::ZERO` is the throttle hint.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// // Duration ~ a millisecond count.
/// ```
use std::time::Duration;

/// What:     `use anyhow::{Result, anyhow};`. The crate's error alias and its constructor.
/// Why:      `read_frame_rgba` reports GPU failures to its caller instead of panicking the
///           compositor thread; every step it takes talks to a driver that may say no.
use anyhow::{Result, anyhow};

/// What:     Grouped `use` of the render-element type, the GLES renderer, the
///           `render_output` helper, and the `Rectangle` geometry type.
/// Why:      Everything `redraw` references.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// import { WaylandSurfaceRenderElement, GlesRenderer, renderOutput, Rectangle } from "smithay";
/// ```
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer, ExportMem},
    },
    desktop::space::render_output,
    utils::{Buffer, Rectangle},
};

/// What:     `use crate::state::Compositor;`. Our state type.
/// Why:      `redraw` operates on `&mut Compositor`.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// import { Compositor } from "./state";
/// ```
use crate::state::Compositor;

/// Dark grey clear colour (RGBA, 0..=1) behind the hosted window.
///
/// What:     `const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];`. A fixed-size array
///           of four 32-bit floats (`f32`; sibling `f64` is 64-bit). Order is
///           red, green, blue, alpha.
/// Why:      A neutral background makes the hosted window's own drawing obvious in
///           screenshots; the app is fullscreen so it is usually fully covered anyway.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// const CLEAR_COLOR = [0.1, 0.1, 0.1, 1.0];
/// ```
pub const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];

/// Number of bytes per pixel in the read-back framebuffer (RGBA8).
///
/// What:     `pub const BYTES_PER_PIXEL: usize = 4;`. `usize` because it multiplies a
///           pixel count into a byte count.
/// Why:      Shared by the readback stride computation and the PNG writer in
///           `screenshot.rs`.
pub const BYTES_PER_PIXEL: usize = 4;

/// Composite the current frame and read it back to CPU memory as upright RGBA8.
///
/// Signature: `read_frame_rgba(state: &mut Compositor) -> Result<(Vec<u8>, u32 /*w*/,
/// u32 /*h*/, usize /*stride*/)>`. Binds the offscreen renderbuffer, composites the space
/// into it with `render_output` (exactly as `redraw` does), and copies the framebuffer to CPU
/// memory via the renderer's `ExportMem` primitive.
///
/// FALLIBLE ON PURPOSE. Every step here talks to the GPU, and each one fails for reasons that
/// are neither bugs nor unrecoverable: an EGL context lost to a GPU reset, a driver that
/// refuses an `Abgr8888` `ReadPixels`, a target that could not be bound. This function runs on
/// the compositor thread inside `event_loop.run`, so a panic would unwind out of the event
/// loop and drop the display and the listening socket with it — one failed screenshot would
/// kill every hosted client. Callers turn the error into their own failure instead:
/// [`crate::capture::capture`] into a `CaptureResult`, [`crate::screenshot::capture`] into the
/// control command's `Result`, and the redraw timer into a dropped frame plus a warning.
///
/// It binds `backend.bind_readback()`, NOT `backend.bind()`: a readback composites a frame
/// nobody presents, so it must not claim a dmabuf pool slot (see `HeadlessBackend::bind_readback`).
///
/// Pixel format: the bytes are RGBA8 (memory order R, G, B, A per pixel), which maps to
/// `gdk::MemoryFormat::R8g8b8a8`. The readback requests `Fourcc::Abgr8888`; DRM fourccs name
/// channels most- to least-significant within a little-endian 32-bit word, so `Abgr8888` is
/// byte order R, G, B, A.
///
/// Orientation: the nested `Output` uses `Transform::Normal`, so `render_output` composites
/// the frame top-down directly into the target's memory (row 0 = top). `copy_framebuffer`
/// hands those rows back in the same order, so the readback is already upright and this
/// function returns it without a flip — the same top-down memory the zero-copy dmabuf path
/// exports. The returned buffer is tightly packed (`stride == width * 4`).
///
/// MUST be called on the compositor thread, where the GLES context is current (e.g. from the
/// redraw timer, right after `redraw`). It is the single readback primitive shared by the GTK
/// presentation path, the in-process capture, and the `screenshot` control command, so
/// orientation and format handling live in one place.
pub fn read_frame_rgba(state: &mut Compositor) -> Result<(Vec<u8>, u32, u32, usize)> {
    // What:     Framebuffer size and unsigned dimensions.
    // Why:      Sets the readback region and the returned image size.
    let size = state.backend.window_size();
    let width = size.w as u32;
    let height = size.h as u32;

    // What:     The composited pixels as `copy_framebuffer` returns them (top-down, upright).
    // Why:      Filled inside the bind scope, then returned as-is (the `Transform::Normal`
    //           output already wrote the frame top-down, so no CPU flip is needed).
    let mut upright: Vec<u8> = Vec::new();

    // What:     A block scoping the render/readback borrows so they end before the flip.
    // Why:      `bind` borrows the backend mutably; release it after copying pixels out.
    {
        // What:     Bind the offscreen renderbuffer for drawing + readback.
        // Why:      Need the renderer and target to composite and then read back.
        //           `bind_readback`, not `bind`: this frame is never presented, so it must not
        //           take a dmabuf pool slot out of the presentation rotation.
        let (renderer, mut framebuffer) = state
            .backend
            .bind_readback()
            .map_err(|err| anyhow!("binding the framebuffer for readback failed: {err:?}"))?;

        // What:     Composite the current committed client content into the framebuffer.
        // Why:      The readback should reflect the latest frame (age 0 forces a full draw).
        render_output::<_, WaylandSurfaceRenderElement<GlesRenderer>, _, _>(
            &state.output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&state.space],
            &[],
            &mut state.damage_tracker,
            CLEAR_COLOR,
        )
        .map_err(|err| anyhow!("rendering the frame for readback failed: {err:?}"))?;

        // What:     The whole framebuffer in BUFFER coordinates.
        // Why:      `copy_framebuffer` reads a buffer-space region.
        let region: Rectangle<i32, Buffer> = Rectangle::from_size((size.w, size.h).into());

        // What:     Copy the framebuffer into a CPU-readable mapping in `Abgr8888`
        //           (memory order R, G, B, A on little-endian).
        // Why:      Move GPU pixels somewhere readable.
        let mapping = renderer
            .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
            .map_err(|err| anyhow!("copying the framebuffer to CPU memory failed: {err:?}"))?;

        // What:     A read-only byte slice of the mapping, copied into our owned buffer.
        // Why:      Hand back an owned copy so the renderer borrow can end.
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|err| anyhow!("mapping the readback texture failed: {err:?}"))?;
        upright.extend_from_slice(pixels);
    }

    // What:     Row stride in bytes (tightly packed).
    // Why:      Each row is `width` RGBA pixels.
    let stride = width as usize * BYTES_PER_PIXEL;

    // What:     Return the RGBA bytes plus dimensions and stride, unflipped.
    // Why:      The `Transform::Normal` output composites top-down, so `copy_framebuffer`
    //           already handed back upright rows — callers (GTK texture, PNG) want top-down.
    return Ok((upright, width, height, stride));
}

/// Composite the hosted window into the nested framebuffer and present one frame.
///
/// What:     `pub fn redraw(state: &mut Compositor)`. Mutably borrows the whole state;
///           internally it borrows several disjoint fields (backend, output, space,
///           damage tracker) at once, which Rust allows because they are distinct
///           fields.
/// Why:      The single place that turns committed client buffers into a presented
///           frame and asks the client for its next one.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// function redraw(state) { ... }
/// ```
///
/// @example
/// ```ts
/// redraw(state); // called on each ~60 Hz calloop timer tick
/// ```
pub fn redraw(state: &mut Compositor) {
    // What:     `let size = state.backend.window_size();`. The framebuffer size as
    //           `Size<i32, Physical>`.
    // Why:      The whole framebuffer is treated as damaged each frame (age 0).
    let size = state.backend.window_size();

    // What:     `let damage = Rectangle::from_size(size);`. A rectangle covering the
    //           whole framebuffer, `Rectangle<i32, Physical>`.
    // Why:      Passed to `submit` as the region that changed.
    let damage = Rectangle::from_size(size);

    // What:     A nested block `{ ... }` scoping the render borrows so they end before
    //           `submit` is called.
    // Why:      `bind` mutably borrows the backend; releasing that borrow at the block's
    //           end lets `submit` (also `&mut backend`) run afterwards.
    {
        // What:     `let (renderer, mut framebuffer) = state.backend.bind().unwrap();`.
        //           `bind()` returns `Result<(&mut GlesRenderer, Framebuffer), _>`;
        //           `.unwrap()` panics on bind failure. `renderer` is the GLES renderer;
        //           `framebuffer` is the target we draw into (`mut` because
        //           `render_output` borrows it mutably).
        // Why:      Get the drawing surface and renderer for this frame.
        let (renderer, mut framebuffer) = state.backend.bind().unwrap();

        // What:     `render_output::<_, WaylandSurfaceRenderElement<GlesRenderer>, _, _>(
        //           &state.output, renderer, &mut framebuffer, 1.0, 0, [&state.space],
        //           &[], &mut state.damage_tracker, CLEAR_COLOR).unwrap();`. The turbofish
        //           pins the render-element type (surfaces from Wayland clients). The
        //           arguments are: output, renderer, framebuffer, scale `1.0`, buffer age
        //           `0` (force full redraw), the spaces to draw `[&state.space]`, extra
        //           custom elements `&[]` (none), the damage tracker, and the clear
        //           colour. `.unwrap()` panics on a rendering error.
        // Why:      Composite the hosted window onto the framebuffer.
        //
        // In TS you'd write (pseudocode):
        // ```ts
        // renderOutput(output, renderer, framebuffer, 1.0, 0, [space], [], damageTracker, CLEAR_COLOR);
        // ```
        render_output::<_, WaylandSurfaceRenderElement<GlesRenderer>, _, _>(
            &state.output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&state.space],
            &[],
            &mut state.damage_tracker,
            CLEAR_COLOR,
        )
        .unwrap();
    }

    // What:     In `dmabuf` present mode, finish the GPU work for the slot just composited,
    //           export its plain dmabuf description, and publish it into `latest_dmabuf` for
    //           the GTK host to import zero-copy. In `readback` mode `export_current` returns
    //           `None` and this is a no-op (the timer does the CPU readback instead).
    // Why:      This is the whole point of phase A: hand out a dmabuf-backed target that was
    //           composited with `render_output` (toplevel + ALL popups), with no glReadPixels.
    if let Some(frame) = state.backend.export_current() {
        if let Ok(mut slot) = state.latest_dmabuf.lock() {
            *slot = Some(frame);
        }
    }

    // What:     `state.backend.submit(Some(&[damage])).unwrap();`. Presents the frame,
    //           telling the parent which region changed. `Some(&[damage])` is a
    //           one-element slice of the whole-framebuffer rectangle. `.unwrap()` panics
    //           on swap failure.
    // Why:      Actually show the composited frame in the nested window.
    state.backend.submit(Some(&[damage])).unwrap();

    // What:     `send_frame_callbacks(state);`. Tell the client its last frame was shown so
    //           it draws the next one, and refresh space/popup bookkeeping.
    // Why:      Shared with the recorder, which needs the same "keep the app animating" step.
    send_frame_callbacks(state);

    // What:     `let _ = state.display_handle.flush_clients();`. Flush queued protocol
    //           events to all clients; `let _ =` discards the `Result` (a flush failure
    //           just means a client disconnected).
    // Why:      Deliver the frame callbacks and configures we just queued. The next frame
    //           is driven by the ~60 Hz calloop timer in `app.rs`, not a winit redraw
    //           request.
    let _ = state.display_handle.flush_clients();
}

/// Send frame callbacks to every mapped window and refresh space/popup bookkeeping.
///
/// What:     `pub fn send_frame_callbacks(state: &mut Compositor)`. Tells each window its
///           last frame was presented (so it draws the next one), then refreshes the space
///           and cleans up dead popups.
/// Why:      Shared by the live redraw and the 60fps recorder: both must keep an animating
///           client producing frames at the intended rate.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// function sendFrameCallbacks(state) { ... }
/// ```
pub fn send_frame_callbacks(state: &mut Compositor) {
    // What:     `let elapsed = state.start_time.elapsed();`. Time since program start.
    // Why:      Frame callbacks carry this timestamp to the client.
    let elapsed = state.start_time.elapsed();

    // What:     `let output = state.output.clone();`. Clone the output handle (cheap,
    //           reference-counted) so the frame-callback closure can own it without
    //           borrowing `state` while `state.space` is also borrowed.
    // Why:      Avoids an overlapping-borrow error between `state.space.elements()` and a
    //           closure that reads `state.output`.
    let output = state.output.clone();

    // What:     `state.space.elements().for_each(|window| { window.send_frame(&output,
    //           elapsed, Some(Duration::ZERO), |_, _| Some(output.clone())); });`. Send a
    //           frame callback to each mapped window. `Some(Duration::ZERO)` is the throttle
    //           hint (draw as fast as possible); the inner closure tells Smithay which
    //           output each surface is on.
    // Why:      Tell the client "your last frame was shown; draw the next one".
    state.space.elements().for_each(|window| {
        window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    });

    // What:     `state.space.refresh();`. Recomputes window/output bookkeeping.
    // Why:      Keep the space's internal state consistent after a frame.
    state.space.refresh();

    // What:     `state.popups.cleanup();`. Drop popups whose surfaces are gone.
    // Why:      Prevent stale popups from lingering.
    state.popups.cleanup();
}
