//! Backend initialisation: the GLES renderer, the nested output, and the dmabuf
//! protocol state.
//!
//! The default path is HEADLESS (`init_headless_backend`): it opens a DRM render
//! node, builds a headless EGL context from a gbm device, creates a `GlesRenderer`,
//! and renders into an offscreen GLES renderbuffer — no window, no winit. The
//! Output/dmabuf-global setup that follows is shared with the legacy winit path via
//! `finish_backend_setup`. The old winit nested-window entry point (`init_backend`)
//! is preserved behind `#[cfg(feature = "backend_winit")]` but not built by default.

/// What:     Grouped `use` of the headless building blocks: gbm device + Fourcc, the
///           EGL display/context/device, the GLES renderer and its offscreen types,
///           the dmabuf import traits, output types, transform/size geometry, and the
///           dmabuf protocol state.
/// Why:      Everything `init_headless_backend`, `finish_backend_setup`, and
///           `HeadlessBackend` reference.
use smithay::{
    backend::{
        allocator::{
            dmabuf::{AsDmabuf, Dmabuf},
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
            Allocator, Buffer as _, Fourcc, Modifier,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            gles::{GlesError, GlesRenderbuffer, GlesRenderer, GlesTarget},
            Bind, ImportDma, ImportEgl, Offscreen,
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::DisplayHandle,
    utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform},
    wayland::dmabuf::{
        DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufState,
    },
};

/// What:     `use anyhow::{anyhow, Context, Result};`. Error helpers.
/// Why:      The backend init functions return `Result` and annotate failures.
use anyhow::{anyhow, Context, Result};

/// What:     `use tracing::{info, warn};`. Structured log macros.
/// Why:      Report the chosen render node, dmabuf version, and hardware-acceleration.
use tracing::{info, warn};

/// What:     `use std::{fs::File, os::fd::AsRawFd, path::PathBuf};`. Owned file handle,
///           the raw-fd accessor for a dmabuf plane, and a path.
/// Why:      Opening the DRM render node yields a `File` used as the gbm fd; exporting a
///           dmabuf plane's borrowed fd into the plain `DmabufFrame` needs `AsRawFd`.
use std::{fs::File, os::fd::AsRawFd, path::PathBuf};

/// What:     `use crossbeam_channel::{Receiver, Sender};`. The two halves of the
///           slot-release channel.
/// Why:      The consumer sends a freed pool-slot id; the render loop drains it before the
///           next bind and returns that slot to the free pool.
use crossbeam_channel::{Receiver, Sender};

/// What:     `use crate::state::BackendPieces;`. The carrier struct for the built pieces.
/// Why:      Both init paths return `BackendPieces`.
use crate::state::BackendPieces;

/// Milli-hertz refresh rate reported for the nested output (60.000 Hz).
///
/// What:     `pub const OUTPUT_REFRESH_MHZ: i32 = 60_000;`. Smithay reports refresh in
///           millihertz, so 60 Hz is 60000.
/// Why:      Named so the magic number is not repeated at the mode-construction sites.
pub const OUTPUT_REFRESH_MHZ: i32 = 60_000;

/// The concrete render backend the compositor state carries.
///
/// What:     A type alias that resolves to the headless backend by default, or the
///           winit graphics backend when `backend_winit` is enabled.
/// Why:      Lets `Compositor`/`BackendPieces` name one backend type while the winit
///           path stays a compile-time opt-in.
#[cfg(not(feature = "backend_winit"))]
pub type RenderBackend = HeadlessBackend;

/// What:     `pub type RenderBackend = WinitGraphicsBackend<GlesRenderer>;` (winit build).
/// Why:      Preserve the original backend type when the legacy feature is on.
#[cfg(feature = "backend_winit")]
pub type RenderBackend = smithay::backend::winit::WinitGraphicsBackend<GlesRenderer>;

/// Number of dmabuf render targets in the rotating pool.
///
/// What:     `const DMABUF_POOL_SIZE: usize = 3;`. Small triple-buffer.
/// Why:      One slot the render loop is drawing into, one the consumer is sampling, one
///           spare in flight — enough that a prompt consumer never stalls the renderer, and
///           small enough that the GPU memory cost stays negligible.
const DMABUF_POOL_SIZE: usize = 3;

/// Which present path the headless backend composites through.
///
/// What:     `pub enum PresentMode { Dmabuf, Readback }`. Selected once at startup from the
///           `KLAMOTTENKISTE_PRESENT` env var (`dmabuf` is the default; `readback` forces
///           the legacy CPU path). Also the value the backend falls back to if the dmabuf
///           pool cannot be allocated or bound on this driver.
/// Why:      A runtime toggle (no rebuild) so a driver that rejects the dmabuf render target
///           can still run via the `glReadPixels` readback the widget already knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PresentMode {
    /// Composite into a dmabuf-backed render target and export it for zero-copy import.
    Dmabuf,
    /// Composite into a plain GLES renderbuffer and hand the widget CPU-readback frames.
    Readback,
}

/// One dmabuf render target in the pool.
///
/// What:     `struct DmabufSlot { buffer: GbmBuffer, dmabuf: Dmabuf, in_flight: bool }`.
///           `buffer` is the gbm buffer object kept alive so its backing storage lives;
///           `dmabuf` is the exported handle bound as a render target (and described to the
///           consumer); `in_flight` is `true` between the moment the slot is handed out via
///           `latest_dmabuf` and the moment the consumer releases it.
/// Why:      A slot that is `in_flight` must not be re-bound for rendering, or the consumer
///           would sample a torn frame.
struct DmabufSlot {
    /// The gbm buffer object, kept alive so the exported dmabuf's storage stays valid.
    ///
    /// Never read after construction — held only so the backing bo is not destroyed while
    /// the exported dmabuf (and the consumer's import of it) is still in use.
    #[allow(dead_code)]
    buffer: GbmBuffer,
    /// The exported dmabuf, bound as a render target and described to the consumer.
    dmabuf: Dmabuf,
    /// `true` while handed out to the consumer and not yet released.
    in_flight: bool,
}

/// A headless GLES backend: a renderer plus its render targets (dmabuf pool + fallback rbo).
///
/// What:     `pub struct HeadlessBackend { renderer, buffer, size, present_mode, allocator,
///           render_fourcc, render_modifiers, pool, current, next_slot, release_tx,
///           release_rx }`. Owns the renderer (which in turn owns the EGL context, display,
///           and the gbm device behind it), the fallback offscreen renderbuffer, and — in
///           `dmabuf` mode — a `GbmAllocator` plus a small pool of dmabuf render targets.
/// Why:      Mirrors the small slice of `WinitGraphicsBackend`'s surface the render /
///           readback / resize code uses (`renderer`, `bind`, `submit`, `window_size`),
///           plus the dmabuf export/release seam the GTK host imports from.
pub struct HeadlessBackend {
    /// The GLES renderer over the headless EGL context.
    renderer: GlesRenderer,
    /// The fallback offscreen renderbuffer (used in `readback` mode; always allocated).
    buffer: GlesRenderbuffer,
    /// The current framebuffer size in physical pixels.
    size: Size<i32, Physical>,
    /// The active present path (dmabuf export or CPU readback).
    present_mode: PresentMode,
    /// Allocator for the dmabuf pool, kept for reallocation on `resize`.
    allocator: GbmAllocator<File>,
    /// The FourCC every pool target is allocated with (ARGB8888).
    render_fourcc: Fourcc,
    /// The modifier set the pool is allocated with (a render-capable set, or Linear).
    render_modifiers: Vec<Modifier>,
    /// The rotating pool of dmabuf render targets (empty in `readback` mode).
    pool: Vec<DmabufSlot>,
    /// The pool slot bound by the most recent `bind` (cleared by `export_current`).
    current: Option<usize>,
    /// Round-robin cursor used only when every slot is in flight (backpressure fallback).
    next_slot: usize,
    /// Sending half of the slot-release channel (cloned out to the consumer).
    release_tx: Sender<u64>,
    /// Receiving half: drained before each dmabuf bind to return released slots to the pool.
    release_rx: Receiver<u64>,
}

impl HeadlessBackend {
    /// Borrow the renderer mutably.
    ///
    /// What:     `pub fn renderer(&mut self) -> &mut GlesRenderer`. Mirrors
    ///           `WinitGraphicsBackend::renderer`.
    /// Why:      shm/dmabuf format queries and dmabuf import go through the renderer.
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    /// The current framebuffer size in physical pixels.
    ///
    /// What:     `pub fn window_size(&self) -> Size<i32, Physical>`. Mirrors
    ///           `WinitGraphicsBackend::window_size`.
    /// Why:      Render and readback size their regions from this.
    pub fn window_size(&self) -> Size<i32, Physical> {
        self.size
    }

    /// The active present path.
    ///
    /// What:     `pub fn present_mode(&self) -> PresentMode`.
    /// Why:      The redraw timer publishes either `latest_dmabuf` (Dmabuf) or `latest_frame`
    ///           via CPU readback (Readback); it branches on this.
    pub fn present_mode(&self) -> PresentMode {
        self.present_mode
    }

    /// A clone of the slot-release sender for the consumer.
    ///
    /// What:     `pub fn release_sender(&self) -> Sender<u64>`.
    /// Why:      `spawn_headless` hands this to `HeadlessHandle` so the GTK host can signal
    ///           when it has finished sampling a dmabuf slot.
    pub fn release_sender(&self) -> Sender<u64> {
        self.release_tx.clone()
    }

    /// Bind the active render target, returning the renderer and its framebuffer.
    ///
    /// What:     `pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>),
    ///           GlesError>`. In `dmabuf` mode it drains the release channel, picks a free
    ///           pool slot (round-robin fallback under backpressure), records it as
    ///           `current`, and binds that slot's dmabuf. In `readback` mode it binds the
    ///           fallback renderbuffer, exactly as before. The returned `GlesTarget` borrows
    ///           the target (not the renderer), so the renderer is handed back alongside it.
    /// Why:      Mirrors `WinitGraphicsBackend::bind`, letting `render.rs`/`screenshot.rs`
    ///           call `state.backend.bind()` unchanged across both present modes.
    pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>), GlesError> {
        if self.present_mode == PresentMode::Dmabuf && !self.pool.is_empty() {
            // Return any slots the consumer finished sampling to the free pool.
            while let Ok(id) = self.release_rx.try_recv() {
                if let Some(slot) = self.pool.get_mut(id as usize) {
                    slot.in_flight = false;
                }
            }

            // Prefer a free slot; under backpressure (all in flight) fall back to a
            // round-robin slot so the renderer never stalls. The strict "not reused until
            // released" contract holds in the normal case where a slot is free.
            let idx = self
                .pool
                .iter()
                .position(|slot| !slot.in_flight)
                .unwrap_or_else(|| {
                    let i = self.next_slot % self.pool.len();
                    warn!("dmabuf pool exhausted (all slots in flight); reusing slot {i}");
                    i
                });
            self.next_slot = (idx + 1) % self.pool.len();
            self.current = Some(idx);

            let target = self.renderer.bind(&mut self.pool[idx].dmabuf)?;
            return Ok((&mut self.renderer, target));
        }

        let target = self.renderer.bind(&mut self.buffer)?;
        Ok((&mut self.renderer, target))
    }

    /// Finish the GPU work for the just-rendered dmabuf slot and export a description of it.
    ///
    /// What:     `pub fn export_current(&mut self) -> Option<crate::app::DmabufFrame>`. In
    ///           `dmabuf` mode, for the slot the last `bind` selected: run `glFinish` so all
    ///           compositing writes are complete before the fd is sampled elsewhere, mark the
    ///           slot `in_flight`, and return its plain description (size, FourCC, modifier,
    ///           per-plane fd/offset/stride, slot id). Returns `None` in `readback` mode or
    ///           if no slot was bound.
    /// Why:      SYNC CHOICE — this first correct version uses `glFinish` (a full CPU/GPU
    ///           barrier) rather than a GLES fence the consumer waits on. It is the bluntest
    ///           possible sync, but it is exactly what makes the exported dmabuf safe to
    ///           sample the instant `latest_dmabuf` returns it, and it still removes the
    ///           `glReadPixels` roundtrip that the readback path pays every frame (a device
    ///           readback + row flip + full-frame memcpy). A pipelined fence is a later
    ///           optimisation; correctness first.
    pub fn export_current(&mut self) -> Option<crate::app::DmabufFrame> {
        if self.present_mode != PresentMode::Dmabuf {
            return None;
        }
        let idx = self.current.take()?;

        // Block until the GPU has finished compositing into this slot. Only then are the
        // dmabuf's pixels safe for another importer (the GTK GL context) to sample.
        let _ = self.renderer.with_context(|gl| unsafe { gl.Finish() });

        let slot = self.pool.get_mut(idx)?;
        slot.in_flight = true;
        let dmabuf = &slot.dmabuf;

        let planes: Vec<crate::app::DmabufPlane> = dmabuf
            .handles()
            .zip(dmabuf.offsets())
            .zip(dmabuf.strides())
            .map(|((fd, offset), stride)| crate::app::DmabufPlane {
                fd: fd.as_raw_fd(),
                offset,
                stride,
            })
            .collect();

        let format = dmabuf.format();
        Some(crate::app::DmabufFrame {
            width: self.size.w as u32,
            height: self.size.h as u32,
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            planes,
            buffer_id: idx as u64,
        })
    }

    /// Present the composited frame.
    ///
    /// What:     `pub fn submit(&mut self, _damage: Option<&[Rectangle<i32, Physical>]>)
    ///           -> Result<(), GlesError>`. A no-op for the headless backend (there is no
    ///           on-screen surface to swap); the composited pixels live in the offscreen
    ///           target until a readback copies them out or the consumer imports the dmabuf.
    /// Why:      Mirrors `WinitGraphicsBackend::submit` so the redraw call site is
    ///           unchanged.
    pub fn submit(&mut self, _damage: Option<&[Rectangle<i32, Physical>]>) -> Result<(), GlesError> {
        Ok(())
    }

    /// Reallocate the render targets at a new size.
    ///
    /// What:     `pub fn resize(&mut self, width: i32, height: i32) -> Result<()>`.
    ///           Reallocates the fallback renderbuffer and, in `dmabuf` mode, the whole
    ///           dmabuf pool at the new size. Any stale release signals for the old pool are
    ///           drained and dropped; the fresh slots all start free.
    /// Why:      The `resize` control command changes the nested screen size; the winit
    ///           path did this by asking winit for a new inner size.
    pub fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        let region: Size<i32, BufferCoord> = (width, height).into();
        let buffer = self
            .renderer
            .create_buffer(Fourcc::Argb8888, region)
            .map_err(|err| anyhow!("allocating the offscreen renderbuffer failed: {err:?}"))?;
        self.buffer = buffer;
        self.size = (width, height).into();

        if self.present_mode == PresentMode::Dmabuf {
            // Drop stale release signals for the pool we are about to replace.
            while self.release_rx.try_recv().is_ok() {}
            let pool = allocate_dmabuf_pool(
                &mut self.allocator,
                width as u32,
                height as u32,
                self.render_fourcc,
                &self.render_modifiers,
            )
            .context("reallocating the dmabuf pool on resize")?;
            self.pool = pool;
            self.current = None;
            self.next_slot = 0;
        }
        Ok(())
    }
}

/// Allocate a fresh pool of dmabuf render targets at the given size.
///
/// What:     `fn allocate_dmabuf_pool(allocator, width, height, fourcc, modifiers) ->
///           Result<Vec<DmabufSlot>>`. Creates `DMABUF_POOL_SIZE` gbm buffer objects with
///           the given format/modifiers and exports each as a `Dmabuf`; keeps both alive per
///           slot, all starting free.
/// Why:      Shared by initial setup and `resize`.
fn allocate_dmabuf_pool(
    allocator: &mut GbmAllocator<File>,
    width: u32,
    height: u32,
    fourcc: Fourcc,
    modifiers: &[Modifier],
) -> Result<Vec<DmabufSlot>> {
    let mut pool = Vec::with_capacity(DMABUF_POOL_SIZE);
    for i in 0..DMABUF_POOL_SIZE {
        let buffer = allocator
            .create_buffer(width, height, fourcc, modifiers)
            .with_context(|| format!("allocating dmabuf pool slot {i}"))?;
        let dmabuf = buffer
            .export()
            .with_context(|| format!("exporting dmabuf pool slot {i}"))?;
        pool.push(DmabufSlot {
            buffer,
            dmabuf,
            in_flight: false,
        });
    }
    Ok(pool)
}

/// Read the requested present mode from the environment.
///
/// What:     `fn present_mode_from_env() -> PresentMode`. `KLAMOTTENKISTE_PRESENT=readback`
///           forces the CPU path; anything else (including unset) is `Dmabuf`.
/// Why:      Runtime selection with no rebuild, per the fallback requirement.
fn present_mode_from_env() -> PresentMode {
    match std::env::var("KLAMOTTENKISTE_PRESENT") {
        Ok(value) if value.eq_ignore_ascii_case("readback") => PresentMode::Readback,
        _ => PresentMode::Dmabuf,
    }
}

/// Pick the ARGB8888 modifier set the GLES renderer can render into as a dmabuf target.
///
/// What:     `fn render_modifiers_for_argb8888(renderer) -> Vec<Modifier>`. Collects every
///           modifier the EGL display advertises as a *render* format for ARGB8888; if the
///           driver advertises none, falls back to `[Linear]` (broadly importable).
/// Why:      Allocating with a render-capable modifier is what lets `GlesRenderer::bind`
///           build an EGLImage + FBO over the dmabuf and composite into it.
fn render_modifiers_for_argb8888(renderer: &GlesRenderer) -> Vec<Modifier> {
    let mut modifiers: Vec<Modifier> = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .filter(|format| format.code == Fourcc::Argb8888)
        .map(|format| format.modifier)
        .collect();
    if modifiers.is_empty() {
        modifiers.push(Modifier::Linear);
    }
    modifiers
}

/// Open a usable DRM render node as a read/write `File`.
///
/// What:     `fn open_render_node() -> Result<File>`. Tries, in order: the
///           `KABELSALAT_DRM_RENDER_NODE` env override, every `renderD*` node under
///           `/dev/dri`, then `/dev/dri/renderD128` as a last resort. Returns the first
///           node that opens.
/// Why:      The headless EGL display is built from a gbm device wrapping this fd; the
///           exact render node varies by machine, so probe rather than hard-code.
fn open_render_node() -> Result<File> {
    // What:     Collect candidate node paths, most-specific first.
    // Why:      An explicit override wins; otherwise enumerate what the machine has.
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(path) = std::env::var("KABELSALAT_DRM_RENDER_NODE") {
        candidates.push(PathBuf::from(path));
    }

    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        let mut nodes: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        candidates.extend(nodes);
    }

    candidates.push(PathBuf::from("/dev/dri/renderD128"));

    // What:     Try each candidate; return the first that opens read/write.
    // Why:      gbm/EGL need a writable render node.
    for path in &candidates {
        match File::options().read(true).write(true).open(path) {
            Ok(file) => {
                info!("headless EGL: using DRM render node {}", path.display());
                return Ok(file);
            }
            Err(err) => {
                warn!("headless EGL: cannot open {}: {err}", path.display());
            }
        }
    }

    Err(anyhow!(
        "no usable DRM render node found (set KABELSALAT_DRM_RENDER_NODE to one)"
    ))
}

/// Build the headless GLES backend, the nested output, and the dmabuf state.
///
/// What:     `pub fn init_headless_backend(display_handle: &DisplayHandle, width: u32,
///           height: u32) -> Result<BackendPieces>`. Opens a render node, builds a
///           headless EGL context from a gbm device, creates the `GlesRenderer` and its
///           offscreen renderbuffer, then delegates the Output/dmabuf setup to
///           `finish_backend_setup`.
/// Why:      The default (no-winit) backend seam the compositor is constructed from.
pub fn init_headless_backend(
    display_handle: &DisplayHandle,
    width: u32,
    height: u32,
) -> Result<BackendPieces> {
    // What:     Open the render node. Duplicate the fd (`try_clone`) so the SAME physical
    //           render node backs two gbm devices: one consumed by the EGL display, one kept
    //           for the dmabuf pool allocator (`EGLDisplay::new` takes the device by value).
    // Why:      `GbmDevice` implements `EGLNativeDisplay`, the input to `EGLDisplay::new`;
    //           the allocator needs its own device to create the render targets we bind.
    let node = open_render_node().context("opening a DRM render node for the headless EGL backend")?;
    let alloc_node = node
        .try_clone()
        .context("duplicating the render node fd for the dmabuf allocator")?;
    let gbm = GbmDevice::new(node).context("creating a gbm device from the render node")?;
    let alloc_gbm =
        GbmDevice::new(alloc_node).context("creating the allocator gbm device from the render node")?;

    // What:     Build the headless EGL display/context and the GLES renderer.
    //           `EGLDisplay::new` and `GlesRenderer::new` are `unsafe` (raw EGL/GL
    //           handles); safety here is the same contract smithay's own headless
    //           example relies on — the display outlives the context, which outlives the
    //           renderer, all owned by the returned backend.
    // Why:      This is the GPU render path with no window.
    let egl_display =
        unsafe { EGLDisplay::new(gbm) }.context("creating the headless EGLDisplay")?;
    let egl_context =
        EGLContext::new(&egl_display).context("creating the headless EGLContext")?;
    let mut renderer =
        unsafe { GlesRenderer::new(egl_context) }.context("creating the headless GlesRenderer")?;

    // What:     Allocate the fallback offscreen renderbuffer at the requested size. Always
    //           allocated so the `readback` present mode (and driver-reject fallback) works
    //           without a rebuild.
    // Why:      `render_output` composites into this in readback mode; in dmabuf mode it is
    //           the safety net if the pool cannot be bound.
    let region: Size<i32, BufferCoord> = (width as i32, height as i32).into();
    let buffer = renderer
        .create_buffer(Fourcc::Argb8888, region)
        .map_err(|err| anyhow!("allocating the offscreen renderbuffer failed: {err:?}"))?;

    // What:     Build the dmabuf pool allocator (RENDERING usage — these are render targets)
    //           and pick a render-capable ARGB8888 modifier set for it.
    // Why:      The pool targets must be allocated with a modifier the renderer can bind an
    //           FBO over, or `bind` fails; RENDERING is the usage that guarantees that.
    let mut allocator = GbmAllocator::new(alloc_gbm, GbmBufferFlags::RENDERING);
    let render_fourcc = Fourcc::Argb8888;
    let render_modifiers = render_modifiers_for_argb8888(&renderer);

    // What:     Choose the present mode from the environment, then — in dmabuf mode — try to
    //           allocate the pool AND test-bind slot 0. Any failure logs a warning and falls
    //           back to `readback` so the compositor still starts on a driver that rejects
    //           the dmabuf render target.
    // Why:      "If a driver rejects the dmabuf path we can fall back without a rebuild."
    let mut present_mode = present_mode_from_env();
    let mut pool: Vec<DmabufSlot> = Vec::new();
    if present_mode == PresentMode::Dmabuf {
        match allocate_dmabuf_pool(&mut allocator, width, height, render_fourcc, &render_modifiers) {
            Ok(mut allocated) => {
                // Test-bind slot 0 in a scoped statement so the returned target (which
                // borrows `allocated`) is dropped at the `;` — before we move the pool.
                let bind_result = renderer.bind(&mut allocated[0].dmabuf).map(|_| ());
                match bind_result {
                    Ok(()) => {
                        let modifier = allocated[0].dmabuf.format().modifier;
                        info!(
                            "dmabuf present: rotating pool of {} ARGB8888 targets ({}x{}), modifier {:?}",
                            allocated.len(),
                            width,
                            height,
                            modifier,
                        );
                        pool = allocated;
                    }
                    Err(err) => {
                        warn!("dmabuf present: renderer rejected the dmabuf target ({err:?}); falling back to readback");
                        present_mode = PresentMode::Readback;
                    }
                }
            }
            Err(err) => {
                warn!("dmabuf present: pool allocation failed ({err:#}); falling back to readback");
                present_mode = PresentMode::Readback;
            }
        }
    } else {
        info!("readback present: CPU glReadPixels path selected via KLAMOTTENKISTE_PRESENT");
    }

    // What:     Shared Output + dmabuf-global setup (identical to the winit path).
    // Why:      Everything downstream of the renderer is presentation-independent.
    let (output, dmabuf_state, dmabuf_global, dmabuf_feedback) =
        finish_backend_setup(display_handle, width as i32, height as i32, &mut renderer)?;

    let (release_tx, release_rx) = crossbeam_channel::unbounded::<u64>();

    let backend = HeadlessBackend {
        renderer,
        buffer,
        size: (width as i32, height as i32).into(),
        present_mode,
        allocator,
        render_fourcc,
        render_modifiers,
        pool,
        current: None,
        next_slot: 0,
        release_tx,
        release_rx,
    };

    Ok(BackendPieces {
        backend,
        output,
        dmabuf_state,
        dmabuf_global,
        dmabuf_feedback,
    })
}

/// Build the nested output and the dmabuf protocol state from a ready renderer.
///
/// What:     `fn finish_backend_setup(display_handle: &DisplayHandle, width: i32,
///           height: i32, renderer: &mut GlesRenderer) -> Result<(Output, DmabufState,
///           DmabufGlobal, Option<DmabufFeedback>)>`. Creates the `wl_output` global,
///           sets its mode/transform, queries the render node for dmabuf v4 feedback
///           (falling back to v3), and binds the legacy `wl_drm` EGL path.
/// Why:      The Output/dmabuf construction is identical for the headless and winit
///           paths, so it lives in one place both call.
fn finish_backend_setup(
    display_handle: &DisplayHandle,
    width: i32,
    height: i32,
    renderer: &mut GlesRenderer,
) -> Result<(Output, DmabufState, DmabufGlobal, Option<DmabufFeedback>)> {
    // What:     The output's video mode at the requested resolution.
    // Why:      Describe the nested screen's resolution and refresh to clients.
    let mode = Mode {
        size: (width, height).into(),
        refresh: OUTPUT_REFRESH_MHZ,
    };

    // What:     Create the output object (0x0 mm physical size for a virtual screen).
    // Why:      The one screen the fixture presents.
    let output = Output::new(
        "nested".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Monochromatic".into(),
            model: "NestedWaylandSession".into(),
        },
    );

    // What:     Register the `wl_output` global (kept alive by the `Output` itself).
    // Why:      Advertise the screen to clients.
    let _global = output.create_global::<crate::state::Compositor>(display_handle);

    // What:     Make the mode current with the identity transform and origin position.
    // Why:      Render the composited frame top-down directly into the target's memory, so the
    //           raw FBO handed to GTK as a dmabuf is already upright (row 0 = top). GL's
    //           framebuffer origin is bottom-left, but `render_output` bakes the output
    //           transform into its projection: with `Transform::Normal` the scene is written so
    //           that buffer memory row 0 is the top of the image — exactly what GTK/DRM sample
    //           top-down. The old `Flipped180` produced bottom-up memory, which forced the
    //           readback to flip on the CPU and left the zero-copy dmabuf upside-down. With the
    //           identity transform the readback needs no flip either (see `render::read_frame_rgba`).
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    // What:     Walk renderer -> EGL context -> EGL display -> the DRM render node.
    // Why:      dmabuf v4 feedback needs the render node's device id.
    let render_node = EGLDevice::device_for_display(renderer.egl_context().display())
        .and_then(|device| device.try_get_render_node());

    let mut dmabuf_state = DmabufState::new();

    // What:     Prefer dmabuf v4 modifier feedback when the render node is known; else v3.
    // Why:      Never fail if the render node cannot be determined.
    let (dmabuf_global, dmabuf_feedback) = match render_node {
        Ok(Some(node)) => {
            let formats = renderer.dmabuf_formats();
            let feedback = DmabufFeedbackBuilder::new(node.dev_id(), formats)
                .build()
                .context("building dmabuf v4 default feedback failed")?;
            let global = dmabuf_state
                .create_global_with_default_feedback::<crate::state::Compositor>(
                    display_handle,
                    &feedback,
                );
            info!("dmabuf: advertising v4 with modifier feedback");
            (global, Some(feedback))
        }
        _ => {
            warn!("dmabuf: no render node available, falling back to v3");
            let formats = renderer.dmabuf_formats();
            let global = dmabuf_state
                .create_global::<crate::state::Compositor>(display_handle, formats);
            (global, None)
        }
    };

    // What:     Bind the legacy EGL `wl_drm` path (from `ImportEgl`).
    // Why:      Mesa accepts either dmabuf v4 OR this wl_drm path for acceleration.
    if renderer.bind_wl_display(display_handle).is_ok() {
        info!("EGL hardware-acceleration (wl_drm) enabled");
    }

    Ok((output, dmabuf_state, dmabuf_global, dmabuf_feedback))
}

/// Legacy winit nested-window backend (preserved, off by default).
///
/// What:     Everything below is compiled only with `#[cfg(feature = "backend_winit")]`.
///           It builds a nested winit window + its GLES renderer via
///           `smithay::backend::winit::init_from_attributes`, then reuses
///           `finish_backend_setup` for the Output/dmabuf state.
/// Why:      Keep the original winit entry point as cfg-gated source rather than deleting
///           it, so the upstream diff stays small.
#[cfg(feature = "backend_winit")]
mod winit_backend {
    use super::{finish_backend_setup, BackendPieces};
    use anyhow::Result;
    use smithay::{
        backend::{
            renderer::gles::GlesRenderer,
            winit::{self, WinitEventLoop},
        },
        reexports::{
            wayland_server::DisplayHandle,
            winit::{dpi::PhysicalSize, window::WindowAttributes},
        },
    };

    /// Build the winit backend, the nested output, and the dmabuf state.
    ///
    /// What:     `pub fn init_backend(display_handle: &DisplayHandle, width: u32, height:
    ///           u32) -> Result<(BackendPieces, WinitEventLoop)>`.
    /// Why:      The original nested-window entry point.
    pub fn init_backend(
        display_handle: &DisplayHandle,
        width: u32,
        height: u32,
    ) -> Result<(BackendPieces, WinitEventLoop)> {
        let attributes = WindowAttributes::default()
            .with_title("nested-wayland-session")
            .with_inner_size(PhysicalSize::new(width, height));

        let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)
            .map_err(|err| anyhow::anyhow!("winit backend init failed: {err}"))?;

        let (output, dmabuf_state, dmabuf_global, dmabuf_feedback) =
            finish_backend_setup(display_handle, width as i32, height as i32, backend.renderer())?;

        Ok((
            BackendPieces {
                backend,
                output,
                dmabuf_state,
                dmabuf_global,
                dmabuf_feedback,
            },
            winit,
        ))
    }
}

#[cfg(feature = "backend_winit")]
pub use winit_backend::init_backend;
