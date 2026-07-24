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
            EventLoop, LoopHandle,
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
    child::{register_exit_poll, spawn_child},
    cli::Config,
    clipboard, control,
    input::{drain_input, SpikeInput},
    render::{read_frame_rgba, redraw},
    state::Compositor,
};

/// What:     `use crossbeam_channel::Sender;`. The sending half of the input queue.
/// Why:      `HeadlessHandle` hands a clone of it to the GTK host.
use crossbeam_channel::Sender;

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
            // What:     `redraw(state);`. Composite one frame and send frame callbacks.
            // Why:      The per-tick render work (submit + frame callbacks keep clients drawing).
            redraw(state);

            // What:     Read the composited frame back and publish it to the shared slot.
            // Why:      The GTK host thread pulls the latest frame from here to present it.
            //           This runs on the compositor thread, where the GLES context is
            //           current — the only place `read_frame_rgba` may be called.
            let (bytes, width, height, stride) = read_frame_rgba(state);
            if let Ok(mut slot) = state.latest_frame.lock() {
                *slot = Some(Frame {
                    bytes,
                    width,
                    height,
                    stride,
                });
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
/// What:     `pub struct HeadlessHandle { socket_name: String, thread:
///           Option<JoinHandle<Result<()>>> }`. Owns the compositor thread and remembers
///           the Wayland socket it advertised.
/// Why:      Lets a host process (e.g. the spike) learn the socket to point clients at,
///           and join the thread when done.
pub struct HeadlessHandle {
    /// The advertised `WAYLAND_DISPLAY` socket name (e.g. `wayland-1`).
    socket_name: String,
    /// The latest composited frame the compositor thread published, if any.
    latest_frame: Arc<Mutex<Option<Frame>>>,
    /// A clone of the real-GTK-input sender: enqueue `SpikeInput` for the seat.
    input_tx: Sender<SpikeInput>,
    /// The bound control-socket path (for the `type`/`click`/`screenshot` API), if any.
    control_socket_path: Option<std::path::PathBuf>,
    /// SPIKE SCOPE: text the hosted client copied, waiting to go onto the host clipboard.
    clipboard_from_nested: crossbeam_channel::Receiver<String>,
    /// SPIKE SCOPE: text the host clipboard holds, to be offered to the hosted client.
    clipboard_to_nested: Sender<String>,
    /// The running compositor thread, taken on `join`.
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

    /// A clone of the sender for enqueuing real GTK input into the compositor seat.
    ///
    /// What:     `pub fn input_sender(&self) -> Sender<SpikeInput>`. Cheap clone of the
    ///           crossbeam sending half.
    /// Why:      The GTK host attaches event controllers that each `send` a `SpikeInput`;
    ///           the compositor loop drains and applies them to the seat.
    pub fn input_sender(&self) -> Sender<SpikeInput> {
        self.input_tx.clone()
    }

    /// The path of the bound control socket, if one was started.
    ///
    /// What:     `pub fn control_socket_path(&self) -> Option<&std::path::Path>`.
    /// Why:      Lets the host print it / drive the `type`/`click`/`screenshot` control
    ///           API (the seat-injection proof path) alongside the GTK controllers.
    pub fn control_socket_path(&self) -> Option<&std::path::Path> {
        self.control_socket_path.as_deref()
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
}

/// What the compositor thread hands back once it is listening.
///
/// What:     `struct Ready { ... }`. The socket name, the shared frame slot, the input
///           sender, and (SPIKE SCOPE) the two clipboard-bridge channel halves.
/// Why:      A named struct instead of a five-field tuple keeps `spawn_headless` readable.
struct Ready {
    /// The advertised `WAYLAND_DISPLAY` socket name.
    socket_name: String,
    /// The shared slot the redraw timer publishes frames into.
    latest_frame: Arc<Mutex<Option<Frame>>>,
    /// The sending half of the real-GTK-input queue.
    input_tx: Sender<SpikeInput>,
    /// SPIKE SCOPE: text the hosted client copied.
    clipboard_from_nested: crossbeam_channel::Receiver<String>,
    /// SPIKE SCOPE: text observed on the host clipboard.
    clipboard_to_nested: Sender<String>,
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
            //           reported like any other startup failure.
            // Why:      Headless mode hosted no child and never bound a control socket; the
            //           spike needs one to inject input through the seat over Unix.
            {
                let loop_handle = event_loop.handle();
                if let Err(err) = control::start(&loop_handle, &control_socket_thread_path) {
                    let _ = ready_tx.send(Err(anyhow!("starting the control socket failed: {err:#}")));
                    return Err(err);
                }
            }

            // What:     Send the advertised socket name, a clone of the shared frame slot,
            //           and a clone of the input sender back to the caller.
            // Why:      Unblocks `spawn_headless` so it can return the handle; the cloned
            //           `Arc` lets the GTK host read frames, and the sender lets it enqueue
            //           real GTK input into the seat.
            let ready = Ready {
                socket_name: state.socket_name.to_string_lossy().into_owned(),
                latest_frame: Arc::clone(&state.latest_frame),
                input_tx: state.input_tx.clone(),
                // SPIKE SCOPE: the two halves the GTK host needs to bridge clipboards.
                clipboard_from_nested: state.clipboard.to_host_rx.clone(),
                clipboard_to_nested: state.clipboard.from_host_tx.clone(),
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
            input_tx: ready.input_tx,
            control_socket_path: Some(control_socket_path),
            clipboard_from_nested: ready.clipboard_from_nested,
            clipboard_to_nested: ready.clipboard_to_nested,
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
