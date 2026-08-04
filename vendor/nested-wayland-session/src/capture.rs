//! In-process frame capture: hand a freshly rendered frame to an embedding host.
//!
//! The control socket's `screenshot <path>` command renders a frame and writes a PNG FILE.
//! An embedding host (the GTK widget) wants the same pixels WITHOUT the file: the composited
//! frame as bytes, in process, so it can wrap them in a texture directly. This module is that
//! path.
//!
//! It mirrors `control.rs`'s request/reply shape — a calloop channel the host sends a one-shot
//! [`CaptureRequest`] on, whose handler runs on the MAIN (event-loop) thread and answers over
//! the request's own reply channel — but stays a separate, lightweight channel: the control
//! protocol is a text line in, a text line out, and a frame is neither.
//!
//! Executing on the main thread is required: `render::read_frame_rgba` binds the offscreen
//! framebuffer and reads it back, which may only happen where the GLES context is current.
//! Each request renders a FRESH frame, so it is independent of the present mode — the
//! `latest_frame` readback cache is only populated in `readback` mode, while the default is
//! `dmabuf`.

/// What:     `use std::sync::mpsc::SyncSender;`. The sending half of a bounded channel.
/// Why:      Every request carries the one-shot channel the main thread answers it on,
///           exactly as `ControlRequest` does.
use std::sync::mpsc::SyncSender;

/// What:     `use anyhow::Result;`. The application-level error alias.
/// Why:      Registering the channel as an event source is fallible.
use anyhow::Result;

/// What:     Grouped `use` of the calloop channel (cross-thread source) and the loop handle.
/// Why:      Register the request channel as an event source on the compositor's loop.
use smithay::reexports::calloop::{
    LoopHandle,
    channel::{Event, Sender, channel},
};

/// What:     `use crate::{app::Frame, render::read_frame_rgba, state::Compositor};`.
/// Why:      The handler renders through the shared readback primitive and answers with the
///           same plain `Frame` the readback present path already carries across threads.
use crate::{app::Frame, render::read_frame_rgba, state::Compositor};

/// The outcome of one capture: the composited frame, or a human-readable failure.
///
/// What:     `pub type CaptureResult = Result<Frame, String>;`. A plain `String` error, not
///           `anyhow::Error`, because this value crosses a thread boundary into a host that
///           only ever shows or logs it.
/// Why:      Gives hosts one type to match on without depending on this crate's error stack.
pub type CaptureResult = Result<Frame, String>;

/// The sending half hosts use to queue capture requests onto the compositor's loop.
///
/// What:     `pub type CaptureSender = Sender<CaptureRequest>;`. The calloop channel sender;
///           cloneable and `Send`, and sending wakes the event loop immediately.
/// Why:      Names the type once so `HeadlessHandle` can store it without importing calloop
///           (whose `Sender` would collide with the crossbeam `Sender` used there).
pub type CaptureSender = Sender<CaptureRequest>;

/// A capture forwarded from a host thread to the main thread, with its reply channel.
///
/// What:     `pub struct CaptureRequest { pub reply: SyncSender<CaptureResult> }`. The
///           request carries no parameters — a capture always means "the current output" —
///           only the way to return its result.
/// Why:      Same contract as [`crate::control::ControlRequest`]: the work crosses one way,
///           the answer the other, with no shared state in between.
pub struct CaptureRequest {
    /// One-shot channel the main thread sends the captured frame (or a failure) back on.
    pub reply: SyncSender<CaptureResult>,
}

/// Register the capture channel on the event loop and return its sender.
///
/// What:     `pub fn start(loop_handle: &LoopHandle<Compositor>) -> Result<CaptureSender>`.
///           Inserts the channel as an event source whose callback runs on the main thread
///           for each request, and hands the sending half back to the caller.
/// Why:      One call from `spawn_headless` wires the whole in-process capture path; the
///           returned sender is what [`crate::HeadlessHandle::request_frame`] pushes into.
pub fn start(loop_handle: &LoopHandle<Compositor>) -> Result<CaptureSender> {
    // What:     `let (sender, source) = channel::<CaptureRequest>();`. Create a calloop
    //           cross-thread channel: `sender` (returned to the host) and `source` (the event
    //           source registered on the loop).
    // Why:      The bridge from a host thread to the main thread.
    let (sender, source) = channel::<CaptureRequest>();

    // What:     Register the source; the callback runs on the main thread per message.
    // Why:      Read the framebuffer back where the GLES context is current.
    loop_handle
        .insert_source(source, |event, _, state: &mut Compositor| {
            // What:     `if let Event::Msg(request) = event { ... }`. The channel yields
            //           `Event::Msg(T)` per item (and `Event::Closed` at the end).
            // Why:      Ignore the close notification; there is nothing to do on it.
            if let Event::Msg(request) = event {
                // What:     `let _ = request.reply.send(capture(state));`. Render, read back,
                //           and answer; discard the send error (the host may have hung up).
                // Why:      Deliver the frame to the waiting host.
                let _ = request.reply.send(capture(state));
            }
        })
        .map_err(|err| anyhow::anyhow!("registering the capture channel source failed: {err}"))?;

    // What:     `return Ok(sender);`. Hand the sending half to the caller.
    // Why:      `spawn_headless` stores it in the `HeadlessHandle`.
    return Ok(sender);
}

/// Render one frame and return it as CPU-side pixels (runs on the main thread).
///
/// What:     `pub fn capture(state: &mut Compositor) -> CaptureResult`. Composites a FRESH
///           frame via `render::read_frame_rgba` and wraps the upright RGBA8 bytes in a
///           [`Frame`], reporting a failed render/readback and rejecting a degenerate one (no
///           area, or fewer bytes than the reported geometry needs) with a message.
/// Why:      The single place the in-process capture's semantics live; called from the
///           channel source and directly usable by any other main-thread caller.
pub fn capture(state: &mut Compositor) -> CaptureResult {
    // What:     Read one upright RGBA frame (bytes, width, height, stride), turning a GPU
    //           failure into this capture's error instead of letting it escape.
    // Why:      The shared readback primitive handles orientation and pixel format, and it is
    //           fallible for exactly this call site: we run inside the compositor's event-loop
    //           callback, where an unwind would drop the loop, the display and the Wayland
    //           socket. A failed capture must cost a capture, not the whole session.
    let (bytes, width, height, stride) =
        read_frame_rgba(state).map_err(|err| format!("{err:#}"))?;

    // What:     Reject a zero-area output.
    // Why:      A host would build a texture from it and get a GTK critical instead of a
    //           diagnosable error.
    if width == 0 || height == 0 {
        return Err(format!("the nested output has no area ({width}x{height})"));
    }

    // What:     The byte count the reported geometry implies, computed saturating.
    // Why:      An overflow here must not wrap into a passing comparison.
    let expected = stride.saturating_mul(height as usize);

    // What:     Reject a readback shorter than its own geometry.
    // Why:      Same reason: the host indexes `height * stride` bytes out of this buffer.
    if bytes.len() < expected {
        return Err(format!(
            "short readback: {} bytes for {width}x{height} at stride {stride} (need {expected})",
            bytes.len()
        ));
    }

    // What:     `return Ok(Frame { .. });`. Hand back the owned pixels plus their geometry.
    // Why:      The host wraps exactly this in a texture; no file, no encode, no policy.
    return Ok(Frame {
        bytes,
        width,
        height,
        stride,
    });
}
