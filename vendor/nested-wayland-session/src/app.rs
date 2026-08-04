//! Program orchestration: build the event loop, wire every source, and run.
//!
//! The compositor is HEADLESS — there is no winit window and no winit event loop.
//! Redraw is driven by a fixed ~60 Hz calloop timer that calls `render::redraw`
//! (which composites, submits, and sends frame callbacks). Two entry points share the
//! wiring: `run` hosts one child process and blocks until it exits (the standalone
//! binary), and `spawn_headless` starts the compositor on its own thread, advertises a
//! Wayland socket, and hosts no child of its own (external clients connect via
//! `WAYLAND_DISPLAY`).

/// What:     Grouped `use` of the memory-import trait, the calloop event loop + timer,
///           the loop handle, and the wayland display.
/// Why:      `run`, `spawn_headless`, and the redraw timer reference these.
use smithay::{
    backend::renderer::ImportMemWl,
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle, LoopSignal,
        },
        wayland_server::Display,
    },
};

/// What:     `use anyhow::{anyhow, Context, Result};`. Error helpers.
/// Why:      `run`/`spawn_headless` return `Result` and annotate setup failures.
use anyhow::{anyhow, Context, Result};

/// What:     `use std::{sync::{mpsc, Arc, Mutex}, thread, time::Duration};`. Std blocks.
/// Why:      `spawn_headless` runs the loop on a thread and hands the socket name +
///           shared-frame slot back over a channel; the redraw timer is scheduled by a
///           `Duration` and the latest readback is published behind `Arc<Mutex<_>>`.
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// Process-global monotonic counter making each headless instance's default
/// control-socket path unique.
///
/// What:     `static INSTANCE_SEQ: AtomicU64`. Bumped once per `spawn_headless`.
/// Why:      The default control-socket path used to be keyed on the PID alone
///           (`kabelsalat-spike-control-<pid>.sock`). Two or more headless
///           instances in ONE process share a PID, so they collided: each
///           `bind_listener` unlinks the pre-existing file and re-binds, silently
///           orphaning the earlier instances' listeners. A per-instance sequence
///           number appended to the PID makes the default path unique so N
///           instances coexist. (Set `KABELSALAT_SPIKE_CONTROL` to force one path
///           only when you intend a single instance.)
static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// What:     Grouped `use` of our own modules' items.
/// Why:      Orchestration calls into backend, child, control, render, and state.
use crate::{
    backend::init_headless_backend,
    capture::{self, CaptureRequest, CaptureResult, CaptureSender},
    child::{register_exit_poll, spawn_child},
    cli::Config,
    clipboard,
    control::{self, ControlHandle},
    input::{drain_input, SpikeInput},
    render::{read_frame_rgba, redraw},
    state::Compositor,
};

/// What:     `use crossbeam_channel::Sender;`. The sending half of the input queue.
/// Why:      `HeadlessHandle` hands a clone of it to the GTK host.
use crossbeam_channel::Sender;

/// What:     `use tracing::warn;`. The structured warning macro.
/// Why:      The redraw timer reports a failed readback as a dropped frame instead of
///           panicking the compositor thread over it.
use tracing::warn;

/// One composited frame read back to CPU memory, shared across threads.
///
/// What:     `pub struct Frame { pub bytes: Vec<u8>, pub width: u32, pub height: u32,
///           pub stride: usize }`. Upright RGBA8 pixels (memory order R, G, B, A;
///           `stride == width * 4`) plus dimensions. `#[derive(Clone)]` so the GTK thread
///           can take an owned snapshot out of the shared slot.
/// Why:      The compositor thread produces frames; the GTK host consumes them. A plain
///           owned struct is the simplest thing to hand across the thread boundary.
#[derive(Clone)]
pub struct Frame {
    /// Upright RGBA8 pixels, tightly packed (`stride == width * 4`).
    pub bytes: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: usize,
}

/// One plane of a dmabuf-backed render target, described in plain POD terms.
///
/// What:     `pub struct DmabufPlane { pub fd: RawFd, pub offset: u32, pub stride: u32 }`.
///           `fd` is a BORROWED raw file descriptor: the compositor keeps the owning
///           `Dmabuf` alive (the slot stays `in_flight`) for as long as the consumer
///           holds the matching `DmabufFrame`, so the number is valid until the consumer
///           releases the slot. The importer must `dup` it if it needs to outlive that.
/// Why:      Hands the GTK side exactly what `zwp_linux_dmabuf`/`gdk::DmabufTextureBuilder`
///           want (fd + offset + stride per plane) without any smithay type crossing the
///           crate boundary.
#[derive(Clone)]
pub struct DmabufPlane {
    /// Borrowed DRM-prime file descriptor for this plane (owned by the compositor's slot).
    pub fd: std::os::fd::RawFd,
    /// Byte offset of this plane within its fd.
    pub offset: u32,
    /// Row stride in bytes for this plane.
    pub stride: u32,
}

/// A handle to the compositor's current dmabuf-backed render target, for zero-copy import.
///
/// What:     `pub struct DmabufFrame { width, height, fourcc: u32, modifier: u64, planes:
///           Vec<DmabufPlane>, buffer_id: u64 }`. A plain owned description (no smithay
///           types) of the pool slot the last frame was composited into. `fourcc` is the
///           DRM FourCC as a raw `u32`; `modifier` the DRM modifier as a raw `u64`.
///           `buffer_id` identifies the pool slot so the consumer can release it via
///           [`HeadlessHandle::release_dmabuf`] once it has finished sampling.
/// Why:      The GTK host imports this as a `GdkDmabufTexture` (zero-copy) instead of the
///           CPU readback the `Frame` path does. A slot handed out here is NOT reused for
///           rendering until it is released, so its planes stay stable while sampled.
#[derive(Clone)]
pub struct DmabufFrame {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// DRM FourCC of the target (e.g. `DRM_FORMAT_ARGB8888`) as a raw `u32`.
    pub fourcc: u32,
    /// DRM format modifier as a raw `u64`.
    pub modifier: u64,
    /// One entry per plane (fd/offset/stride).
    pub planes: Vec<DmabufPlane>,
    /// Opaque pool-slot id, echoed back to `release_dmabuf` when sampling is done.
    pub buffer_id: u64,
}

/// Target frame interval for the redraw timer (~60 Hz).
///
/// What:     `const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);`.
/// Why:      Drives the render loop at roughly 60 frames per second without a window's
///           vsync to pace it.
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// Register the fixed-rate redraw timer on the event loop.
///
/// What:     `pub fn register_redraw_timer(loop_handle: &LoopHandle<'static,
///           Compositor>)`. Inserts a calloop `Timer` that fires immediately and then
///           reschedules every `FRAME_INTERVAL`, calling `redraw` each time.
/// Why:      Headless rendering has no winit `Redraw` event to react to, so a timer
///           drives the render + frame-callback loop that keeps clients animating.
pub fn register_redraw_timer(loop_handle: &LoopHandle<'static, Compositor>) {
    loop_handle
        .insert_source(Timer::immediate(), |_, _, state: &mut Compositor| {
            // What:     `redraw(state);`. Composite one frame and send frame callbacks. In
            //           `dmabuf` present mode `redraw` also finishes the GPU work, exports the
            //           slot's dmabuf, and publishes it into `state.latest_dmabuf` — no CPU
            //           readback happens on this path at all.
            // Why:      The per-tick render work (submit + frame callbacks keep clients drawing).
            redraw(state);

            // What:     Only in `readback` present mode: read the composited frame back to CPU
            //           memory and publish it to `latest_frame`. In `dmabuf` mode this whole
            //           block is skipped — the `glReadPixels` roundtrip is exactly the cost the
            //           dmabuf path removes.
            // Why:      The GTK host pulls whichever slot matches the active present mode. This
            //           readback runs on the compositor thread, where the GLES context is
            //           current — the only place `read_frame_rgba` may be called.
            if state.backend.present_mode() == crate::backend::PresentMode::Readback {
                // What:     Publish a successful readback; on failure keep the frame already
                //           published and log the drop.
                // Why:      `read_frame_rgba` is fallible, and a GPU hiccup here must not
                //           unwind this timer callback: that would tear down the event loop,
                //           the display and every hosted client over one dropped frame. The
                //           host keeps presenting the last good frame until the next tick.
                match read_frame_rgba(state) {
                    Ok((bytes, width, height, stride)) => {
                        if let Ok(mut slot) = state.latest_frame.lock() {
                            *slot = Some(Frame {
                                bytes,
                                width,
                                height,
                                stride,
                            });
                        }
                    }
                    Err(err) => warn!("readback present: dropping one frame ({err:#})"),
                }
            }

            // What:     `TimeoutAction::ToDuration(FRAME_INTERVAL)`. Reschedule the timer.
            // Why:      Keep rendering at ~60 Hz until the loop stops.
            TimeoutAction::ToDuration(FRAME_INTERVAL)
        })
        .expect("failed to register the redraw timer");
}

/// Build the compositor state, wire the shared sources, and return the loop + state.
///
/// What:     `fn build(width: u32, height: u32) -> Result<(EventLoop<'static,
///           Compositor>, Compositor)>`. Creates the event loop and display, initialises
///           the headless backend, constructs the state, advertises shm formats, and
///           registers the redraw timer.
/// Why:      The wiring common to `run` and `spawn_headless`; each adds its own extras
///           (a hosted child / a control socket) afterward.
fn build(width: u32, height: u32) -> Result<(EventLoop<'static, Compositor>, Compositor)> {
    // What:     Create the calloop event loop carrying `Compositor` as shared data.
    // Why:      Drives Wayland clients, the redraw timer, and the child/control sources.
    let mut event_loop: EventLoop<'static, Compositor> =
        EventLoop::try_new().context("creating the calloop event loop")?;

    // What:     Create the wayland-server display and take a handle before it is moved.
    // Why:      Backend init registers the output and dmabuf globals through the handle.
    let display: Display<Compositor> = Display::new().context("creating the Wayland display")?;
    let display_handle = display.handle();

    // What:     Build the headless GLES backend, output, and dmabuf state.
    // Why:      Set up the GPU render path (no window) before constructing the state.
    let pieces = init_headless_backend(&display_handle, width, height)
        .context("initialising the headless backend")?;

    // What:     Construct the full state (consuming the display and backend pieces).
    // Why:      Produce the value the event loop carries.
    let mut state = Compositor::new(&mut event_loop, display, pieces);

    // What:     Advertise exactly the shm formats the GLES renderer can upload.
    // Why:      Any shm client (e.g. a cursor theme) then uses a supported format.
    let shm_formats = state.backend.renderer().shm_formats();
    state.shm_state.update_formats(shm_formats);

    // What:     Register the ~60 Hz redraw timer.
    // Why:      Drive rendering without a winit event loop.
    let loop_handle = event_loop.handle();
    register_redraw_timer(&loop_handle);

    Ok((event_loop, state))
}

/// Build the headless compositor, host one child, and run until it exits.
///
/// What:     `pub fn run(config: Config) -> Result<i32>`. Consumes the parsed config,
///           returns the hosted client's exit code (or an error if setup failed).
/// Why:      The entry point the standalone binary invokes.
pub fn run(config: Config) -> Result<i32> {
    // What:     Shared build (event loop + state).
    // Why:      Same wiring `spawn_headless` uses.
    let (mut event_loop, mut state) = build(config.width as u32, config.height as u32)?;

    let loop_handle = event_loop.handle();

    // What:     Bind the control socket when requested.
    // Why:      Enable the screenshot/input/resize control API only when asked.
    if let Some(socket_path) = &config.control_socket {
        control::start(&loop_handle, socket_path)?;
    }

    // What:     Insert the periodic child-exit poll.
    // Why:      Stop the loop when the hosted client exits.
    register_exit_poll(&loop_handle);

    // What:     Launch the one hosted client, connected to our socket.
    // Why:      Start the app the fixture hosts.
    spawn_child(&mut state, &config.child_command)?;

    // What:     Run the loop until `loop_signal.stop()` (child exit / quit). The
    //           post-dispatch callback drains any queued real-GTK input into the seat.
    // Why:      The program's main blocking loop; draining here applies input on the
    //           thread that owns the seat, within a frame of it being enqueued.
    event_loop
        .run(None, &mut state, |state| {
            drain_input(state);
            // SPIKE SCOPE: one clipboard-bridge step per iteration (see crate::clipboard).
            clipboard::service(state);
        })
        .context("event loop failed")?;

    // What:     Return the hosted app's exit code, or 0.
    // Why:      Propagate the app's exit status.
    Ok(state.child_exit_code.unwrap_or(0))
}

/// A handle to a headless compositor running on its own thread.
///
/// What:     `pub struct HeadlessHandle { socket_name: String, ..., loop_signal, control,
///           thread }`. Owns the compositor thread + control thread and remembers the
///           Wayland socket it advertised.
/// Why:      Lets a host process (e.g. an embedding GTK widget) learn the socket to point
///           clients at, drive the control API, and tear everything down deterministically.
///
/// # Lifecycle
///
/// The compositor and its hosted client live for as long as this handle does. Presenting
/// frames, forwarding input, and pausing a frame pump (e.g. when an embedding widget is
/// hidden/unmapped) never touch the handle — the compositor keeps running with the client's
/// state intact. Only [`HeadlessHandle::shutdown`] (or dropping the handle) stops the
/// compositor thread, joins the control thread, and unlinks the control socket file.
pub struct HeadlessHandle {
    /// The advertised `WAYLAND_DISPLAY` socket name (e.g. `wayland-1`).
    socket_name: String,
    /// The latest composited frame the compositor thread published, if any (readback path).
    latest_frame: Arc<Mutex<Option<Frame>>>,
    /// The latest dmabuf-backed target the compositor thread published, if any (dmabuf path).
    latest_dmabuf: Arc<Mutex<Option<DmabufFrame>>>,
    /// Signals a pool slot is done being sampled and may be reused for rendering.
    release_tx: Sender<u64>,
    /// A clone of the real-GTK-input sender: enqueue `SpikeInput` for the seat.
    input_tx: Sender<SpikeInput>,
    /// Queues one-shot capture requests onto the compositor thread (see `crate::capture`).
    capture_tx: CaptureSender,
    /// The bound control-socket path (for the `type`/`click`/`screenshot` API), if any.
    control_socket_path: Option<std::path::PathBuf>,
    /// SPIKE SCOPE: text the hosted client copied, waiting to go onto the host clipboard.
    clipboard_from_nested: crossbeam_channel::Receiver<String>,
    /// SPIKE SCOPE: text the host clipboard holds, to be offered to the hosted client.
    clipboard_to_nested: Sender<String>,
    /// Signal used to stop the event loop on `shutdown`/drop. `None` once torn down.
    loop_signal: Option<LoopSignal>,
    /// Joinable handle to the control thread + its socket file. `None` once torn down.
    control: Option<ControlHandle>,
    /// The running compositor thread, taken on `shutdown`/`join`.
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl HeadlessHandle {
    /// The Wayland socket name clients should connect to.
    ///
    /// What:     `pub fn socket_name(&self) -> String`. Returns an owned copy.
    /// Why:      Callers set `WAYLAND_DISPLAY` to this for the clients they launch.
    pub fn socket_name(&self) -> String {
        self.socket_name.clone()
    }

    /// Take a snapshot of the most recently composited frame, if one exists yet.
    ///
    /// What:     `pub fn latest_frame(&self) -> Option<Frame>`. Locks the shared slot and
    ///           clones out the current `Frame` (an owned copy of the RGBA bytes), or
    ///           returns `None` before the first frame (or if the lock is poisoned).
    /// Why:      The GTK host polls this on its own tick to present each new frame; the
    ///           clone hands GTK an owned buffer without holding the compositor's lock.
    pub fn latest_frame(&self) -> Option<Frame> {
        self.latest_frame.lock().ok().and_then(|slot| slot.clone())
    }

    /// Take a snapshot of the most recently composited dmabuf-backed target, if any.
    ///
    /// What:     `pub fn latest_dmabuf(&self) -> Option<DmabufFrame>`. Clones out the plain
    ///           `DmabufFrame` (fds are borrowed — the compositor keeps the underlying
    ///           `Dmabuf` alive until the matching slot is released). `None` before the
    ///           first dmabuf frame, or when the compositor is in `readback` present mode.
    /// Why:      The GTK host imports this as a `GdkDmabufTexture` (zero-copy) instead of the
    ///           CPU readback `latest_frame` does. After sampling, the host MUST call
    ///           [`HeadlessHandle::release_dmabuf`] with `frame.buffer_id` so the slot can be
    ///           reused; until then the render loop will not overwrite it.
    pub fn latest_dmabuf(&self) -> Option<DmabufFrame> {
        self.latest_dmabuf.lock().ok().and_then(|slot| slot.clone())
    }

    /// Signal that a dmabuf pool slot has been fully sampled and may be reused.
    ///
    /// What:     `pub fn release_dmabuf(&self, buffer_id: u64)`. Sends the slot id back to
    ///           the compositor thread over a channel; the render loop drains it before the
    ///           next `bind` and returns that slot to the free pool. Sending after the
    ///           compositor has shut down is a harmless no-op.
    /// Why:      A slot handed out via `latest_dmabuf` is NOT reused for rendering until it
    ///           is released, so the consumer's import stays valid while it samples. This is
    ///           the release half of that contract.
    pub fn release_dmabuf(&self, buffer_id: u64) {
        let _ = self.release_tx.send(buffer_id);
    }

    /// A clone of the sender the consumer uses to release sampled dmabuf slots.
    ///
    /// What:     `pub fn release_sender(&self) -> Sender<u64>`. Cheap clone of the crossbeam
    ///           sending half, for callers that prefer to hold the channel directly.
    /// Why:      Mirrors `input_sender`: lets a host wire the release signal into its own
    ///           frame-callback plumbing without going through `&self` each time.
    pub fn release_sender(&self) -> Sender<u64> {
        self.release_tx.clone()
    }

    /// A clone of the sender for enqueuing real GTK input into the compositor seat.
    ///
    /// What:     `pub fn input_sender(&self) -> Sender<SpikeInput>`. Cheap clone of the
    ///           crossbeam sending half.
    /// Why:      The GTK host attaches event controllers that each `send` a `SpikeInput`;
    ///           the compositor loop drains and applies them to the seat.
    pub fn input_sender(&self) -> Sender<SpikeInput> {
        self.input_tx.clone()
    }

    /// Whether the compositor thread is still running.
    ///
    /// What:     `pub fn is_alive(&self) -> bool`. Asks the THREAD (`JoinHandle::is_finished`),
    ///           not merely whether this handle still holds one: `false` once
    ///           `shutdown`/`join` took it, AND `false` when the thread ended on its own — its
    ///           event loop stopped, its hosted client's exit stopped it, or it panicked.
    /// Why:      Holding a `HeadlessHandle` is not the same as having a compositor. A host that
    ///           keys UI state on "is there a session" (the canonical case is a screenshot
    ///           button's sensitivity) must see a thread that died under it, or it keeps
    ///           offering actions that can only fail. This is a SNAPSHOT, not a lock: the
    ///           thread may exit immediately after the call returns, so a `true` still has to
    ///           be followed by handling the eventual send/reply failure.
    pub fn is_alive(&self) -> bool {
        let Some(thread) = self.thread.as_ref() else {
            return false;
        };
        return !thread.is_finished();
    }

    /// Ask the compositor thread for a freshly rendered frame; the answer arrives on `reply`.
    ///
    /// What:     `pub fn request_frame(&self, reply: mpsc::SyncSender<CaptureResult>) ->
    ///           Result<()>`. Queues a [`CaptureRequest`] onto the compositor's event loop
    ///           (which the send also wakes) and returns immediately — the caller never blocks
    ///           on the readback. Exactly one [`CaptureResult`] is sent on `reply`; if the
    ///           compositor stops before answering, the request (and with it `reply`'s sending
    ///           half) is dropped, so a waiting receiver sees a disconnect rather than hanging.
    ///           `Err` means the request was never queued: the event loop is already gone.
    /// Why:      The in-process alternative to the control socket's `screenshot <path>`: an
    ///           embedding host gets the composited pixels as bytes, with no file and no PNG
    ///           round-trip. The frame is rendered FRESH, so this works in both present modes
    ///           — unlike [`HeadlessHandle::latest_frame`], which stays empty in `dmabuf` mode.
    pub fn request_frame(&self, reply: mpsc::SyncSender<CaptureResult>) -> Result<()> {
        return self
            .capture_tx
            .send(CaptureRequest { reply })
            .map_err(|_| anyhow!("the compositor event loop is gone; no frame can be captured"));
    }

    /// The path of the bound control socket, if one was started.
    ///
    /// What:     `pub fn control_socket_path(&self) -> Option<&std::path::Path>`.
    /// Why:      Lets the host print it / drive the `type`/`click`/`screenshot` control
    ///           API (the seat-injection proof path) alongside the GTK controllers.
    pub fn control_socket_path(&self) -> Option<&std::path::Path> {
        self.control_socket_path.as_deref()
    }

    /// `DISPLAY` name (e.g. `:2`) of this compositor's Xwayland server.
    ///
    /// What:     `pub fn x11_display(&self) -> Option<String>`. Returns `None` while
    ///           Xwayland support is unimplemented or the X server is not yet ready.
    /// Why:      The contract seam of the Xwayland readiness harness
    ///           (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`):
    ///           `tests/xwayland.rs` polls this and goes green exactly when a real
    ///           display name is returned. Filling this in is the Xwayland feature
    ///           branch's final step.
    pub fn x11_display(&self) -> Option<String> {
        None
    }

    /// SPIKE SCOPE: the receiving half for text the hosted client copied.
    ///
    /// What:     `pub fn clipboard_from_nested(&self) -> Receiver<String>`. Cheap clone of
    ///           the crossbeam receiving half.
    /// Why:      The GTK host drains this on its own tick and pushes each value onto the
    ///           host clipboard (`gdk::Clipboard::set_text`). See `crate::clipboard`.
    pub fn clipboard_from_nested(&self) -> crossbeam_channel::Receiver<String> {
        self.clipboard_from_nested.clone()
    }

    /// SPIKE SCOPE: the sending half for text observed on the host clipboard.
    ///
    /// What:     `pub fn clipboard_to_nested(&self) -> Sender<String>`. Cheap clone of the
    ///           crossbeam sending half.
    /// Why:      The GTK host polls its own clipboard and sends changes here; the
    ///           compositor thread turns them into a server-side selection the hosted
    ///           client can paste from.
    pub fn clipboard_to_nested(&self) -> Sender<String> {
        self.clipboard_to_nested.clone()
    }

    /// Block until the compositor thread exits.
    ///
    /// What:     `pub fn join(mut self)`. Consumes the handle and joins the thread,
    ///           discarding its result (a panic in the thread is surfaced by the join).
    /// Why:      Keeps the host process alive while the compositor runs.
    pub fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Stop the compositor, join every thread it owns, and unlink the control socket.
    ///
    /// What:     `pub fn shutdown(&mut self)`. Signals the calloop event loop to stop (and
    ///           wakes it so it observes the stop promptly), joins the compositor thread,
    ///           then stops and joins the control thread and removes its socket file.
    ///           Idempotent: every owned resource is taken behind an `Option`, so a second
    ///           call (or `Drop` after an explicit call) is a no-op.
    /// Why:      This is the ONLY teardown path. An embedding widget calls it on `dispose`
    ///           (or offers an explicit `close()`); hiding/unmapping the widget must NOT.
    ///           After it returns there are no leaked threads, sockets, or fds: the
    ///           compositor thread has exited (releasing its EGL/render-node resources when
    ///           the state drops), the control thread has been woken out of `accept` and
    ///           joined, and the control socket file is gone.
    pub fn shutdown(&mut self) {
        // Stop the event loop and wake it out of any poll wait so it returns now, not at
        // the next unrelated event.
        if let Some(signal) = self.loop_signal.take() {
            signal.stop();
            signal.wakeup();
        }
        // Join the compositor thread; dropping its `Compositor` state releases EGL / the
        // render node and the Wayland listening socket.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Stop and join the control thread, then unlink the control socket file.
        if let Some(control) = self.control.take() {
            control.shutdown();
        }
        self.control_socket_path = None;
    }
}

/// Tear the compositor down when the last handle is dropped.
///
/// What:     `impl Drop for HeadlessHandle`. Calls [`HeadlessHandle::shutdown`].
/// Why:      Binds the compositor's lifetime to the handle's: whoever holds the handle
///           holds the browser. Dropping the last reference (e.g. an embedding widget being
///           finalised) destroys the compositor and its hosted client, with no leaks.
impl Drop for HeadlessHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What the compositor thread hands back once it is listening.
///
/// What:     `struct Ready { ... }`. The socket name, the shared frame slot, the input
///           sender, and (SPIKE SCOPE) the two clipboard-bridge channel halves.
/// Why:      A named struct instead of a five-field tuple keeps `spawn_headless` readable.
struct Ready {
    /// The advertised `WAYLAND_DISPLAY` socket name.
    socket_name: String,
    /// The shared slot the redraw timer publishes readback frames into.
    latest_frame: Arc<Mutex<Option<Frame>>>,
    /// The shared slot the redraw timer publishes dmabuf frames into.
    latest_dmabuf: Arc<Mutex<Option<DmabufFrame>>>,
    /// The sending half of the dmabuf slot-release channel.
    release_tx: Sender<u64>,
    /// The sending half of the real-GTK-input queue.
    input_tx: Sender<SpikeInput>,
    /// The sending half of the one-shot capture-request channel.
    capture_tx: CaptureSender,
    /// SPIKE SCOPE: text the hosted client copied.
    clipboard_from_nested: crossbeam_channel::Receiver<String>,
    /// SPIKE SCOPE: text observed on the host clipboard.
    clipboard_to_nested: Sender<String>,
    /// The event loop's stop signal, so the caller can shut the compositor down.
    loop_signal: LoopSignal,
    /// The control thread's joinable handle (+ socket path), for deterministic teardown.
    control: ControlHandle,
}

/// Start a headless compositor on its own thread and return once it is listening.
///
/// What:     `pub fn spawn_headless(width: u32, height: u32) ->
///           Result<HeadlessHandle>`. Spawns the compositor thread, waits for it to
///           advertise a Wayland socket, and returns a handle exposing the socket name.
///           The compositor hosts no child of its own — external clients connect via
///           `WAYLAND_DISPLAY=<socket_name>`.
/// Why:      The thin embedding seam the spike (and later the GTK host) uses.
pub fn spawn_headless(width: u32, height: u32) -> Result<HeadlessHandle> {
    // What:     A one-shot channel carrying the socket name, the shared frame slot, and a
    //           clone of the input sender (or a startup error) out of the compositor thread.
    // Why:      All three are only known after the state is built inside the thread.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Ready>>();

    // What:     Choose a control-socket path up front (before the thread) so the returned
    //           handle can expose it. Unique per INSTANCE, not just per process.
    // Why:      The seat-injection proof drives the `type`/`click`/`screenshot` control API;
    //           headless mode did not previously start a control socket, so start one here.
    //           The default path formerly used the PID alone, so N instances in ONE process
    //           (same PID) collided — `bind_listener` unlinks the stale file and re-binds,
    //           orphaning the earlier instances. Appending a monotonic per-instance sequence
    //           number keeps each instance's path distinct. `KABELSALAT_SPIKE_CONTROL` still
    //           forces a fixed path (single-instance use only).
    let control_socket_path = std::env::var_os("KABELSALAT_SPIKE_CONTROL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let seq = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!(
                "kabelsalat-spike-control-{}-{}.sock",
                std::process::id(),
                seq
            ))
        });
    let control_socket_thread_path = control_socket_path.clone();

    // What:     Spawn the compositor thread running its own event loop to completion.
    // Why:      The event loop blocks; keeping it off the caller's thread lets the caller
    //           host GTK / launch clients.
    let thread = thread::Builder::new()
        .name("nested-compositor".to_string())
        .spawn(move || -> Result<()> {
            // What:     Build the shared event loop + state.
            // Why:      Same wiring as `run`, minus the hosted child.
            let (mut event_loop, mut state) = match build(width, height) {
                Ok(pair) => pair,
                Err(err) => {
                    // What:     Report the startup failure to the caller and stop.
                    // Why:      `spawn_headless` must not hang if the backend fails.
                    let _ = ready_tx.send(Err(anyhow!("headless compositor startup failed: {err:#}")));
                    return Err(err);
                }
            };

            // What:     Bind the control socket so the host can drive `type`/`click`/
            //           `screenshot` (the seat-injection proof path). Failing to start it is
            //           reported like any other startup failure. The returned handle is kept
            //           so the caller can join the control thread on shutdown.
            // Why:      Headless mode hosted no child and never bound a control socket; the
            //           embedder needs one to inject input through the seat over Unix.
            let control = {
                let loop_handle = event_loop.handle();
                match control::start(&loop_handle, &control_socket_thread_path) {
                    Ok(control) => control,
                    Err(err) => {
                        let _ = ready_tx
                            .send(Err(anyhow!("starting the control socket failed: {err:#}")));
                        return Err(err);
                    }
                }
            };

            // What:     Register the in-process capture channel on the same event loop and
            //           keep its sender. Failing to register it is reported like any other
            //           startup failure, after tearing the control thread back down (it is
            //           already listening at this point and would otherwise be orphaned).
            // Why:      An embedding host needs the composited frame as BYTES on demand; the
            //           control socket only offers `screenshot <path>`, i.e. a file.
            let capture_tx = {
                let loop_handle = event_loop.handle();
                match capture::start(&loop_handle) {
                    Ok(sender) => sender,
                    Err(err) => {
                        control.shutdown();
                        let _ = ready_tx
                            .send(Err(anyhow!("starting the capture channel failed: {err:#}")));
                        return Err(err);
                    }
                }
            };

            // What:     Send the advertised socket name, a clone of the shared frame slot,
            //           the input sender, the loop stop signal, and the control handle back
            //           to the caller.
            // Why:      Unblocks `spawn_headless` so it can return the handle; the cloned
            //           `Arc` lets the GTK host read frames, the sender lets it enqueue real
            //           GTK input into the seat, and the signal + control handle let it tear
            //           the compositor down deterministically.
            let ready = Ready {
                socket_name: state.socket_name.to_string_lossy().into_owned(),
                latest_frame: Arc::clone(&state.latest_frame),
                latest_dmabuf: Arc::clone(&state.latest_dmabuf),
                release_tx: state.backend.release_sender(),
                input_tx: state.input_tx.clone(),
                capture_tx,
                // SPIKE SCOPE: the two halves the GTK host needs to bridge clipboards.
                clipboard_from_nested: state.clipboard.to_host_rx.clone(),
                clipboard_to_nested: state.clipboard.from_host_tx.clone(),
                loop_signal: state.loop_signal.clone(),
                control,
            };
            if ready_tx.send(Ok(ready)).is_err() {
                // What:     The caller dropped the receiver; nothing to host.
                // Why:      Exit cleanly rather than run an unwatched loop.
                return Ok(());
            }

            // What:     Run the event loop forever (until the process ends). The
            //           post-dispatch callback drains real GTK input into the seat.
            // Why:      Keep the compositor alive for external clients; apply queued input
            //           on the seat-owning thread each iteration.
            event_loop
                .run(None, &mut state, |state| {
                    drain_input(state);
                    // SPIKE SCOPE: one clipboard-bridge step per iteration.
                    clipboard::service(state);
                })
                .context("headless event loop failed")?;

            Ok(())
        })
        .context("spawning the compositor thread")?;

    // What:     Wait for the thread to advertise a socket (or fail / die at startup).
    // Why:      Return a handle only once the compositor is actually listening.
    match ready_rx.recv() {
        Ok(Ok(ready)) => Ok(HeadlessHandle {
            socket_name: ready.socket_name,
            latest_frame: ready.latest_frame,
            latest_dmabuf: ready.latest_dmabuf,
            release_tx: ready.release_tx,
            input_tx: ready.input_tx,
            capture_tx: ready.capture_tx,
            control_socket_path: Some(control_socket_path),
            clipboard_from_nested: ready.clipboard_from_nested,
            clipboard_to_nested: ready.clipboard_to_nested,
            loop_signal: Some(ready.loop_signal),
            control: Some(ready.control),
            thread: Some(thread),
        }),
        Ok(Err(err)) => {
            let _ = thread.join();
            Err(err)
        }
        Err(_) => match thread.join() {
            Ok(Ok(())) => Err(anyhow!(
                "compositor thread exited before advertising a socket"
            )),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(anyhow!("compositor thread panicked during startup")),
        },
    }
}
