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
        allocator::{gbm::GbmDevice, Fourcc},
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

/// What:     `use std::{fs::File, path::PathBuf};`. Owned file handle and path.
/// Why:      Opening the DRM render node yields a `File` used as the gbm fd.
use std::{fs::File, path::PathBuf};

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

/// A headless GLES backend: a renderer plus the offscreen renderbuffer it draws into.
///
/// What:     `pub struct HeadlessBackend { renderer: GlesRenderer, buffer:
///           GlesRenderbuffer, size: Size<i32, Physical> }`. Owns the renderer (which
///           in turn owns the EGL context, display, and the gbm device behind it) and
///           the offscreen render target sized to the nested output.
/// Why:      Mirrors the small slice of `WinitGraphicsBackend`'s surface the render /
///           readback / resize code uses (`renderer`, `bind`, `submit`, `window_size`),
///           so those call sites stay backend-agnostic.
pub struct HeadlessBackend {
    /// The GLES renderer over the headless EGL context.
    renderer: GlesRenderer,
    /// The offscreen renderbuffer every frame is composited into.
    buffer: GlesRenderbuffer,
    /// The current framebuffer size in physical pixels.
    size: Size<i32, Physical>,
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

    /// Bind the offscreen renderbuffer, returning the renderer and its framebuffer.
    ///
    /// What:     `pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>),
    ///           GlesError>`. The returned `GlesTarget` borrows the renderbuffer (not the
    ///           renderer), so the renderer can be handed back alongside it.
    /// Why:      Mirrors `WinitGraphicsBackend::bind`, letting `render.rs`/`screenshot.rs`
    ///           call `state.backend.bind()` unchanged.
    pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>), GlesError> {
        let framebuffer = self.renderer.bind(&mut self.buffer)?;
        Ok((&mut self.renderer, framebuffer))
    }

    /// Present the composited frame.
    ///
    /// What:     `pub fn submit(&mut self, _damage: Option<&[Rectangle<i32, Physical>]>)
    ///           -> Result<(), GlesError>`. A no-op for the headless backend (there is no
    ///           on-screen surface to swap); the composited pixels live in the offscreen
    ///           renderbuffer until a readback copies them out.
    /// Why:      Mirrors `WinitGraphicsBackend::submit` so the redraw call site is
    ///           unchanged.
    pub fn submit(&mut self, _damage: Option<&[Rectangle<i32, Physical>]>) -> Result<(), GlesError> {
        Ok(())
    }

    /// Reallocate the offscreen renderbuffer at a new size.
    ///
    /// What:     `pub fn resize(&mut self, width: i32, height: i32) -> Result<()>`.
    ///           Creates a fresh renderbuffer at the requested size and swaps it in.
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
        Ok(())
    }
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
    // What:     Open the render node and wrap it in a gbm device.
    // Why:      `GbmDevice` implements `EGLNativeDisplay`, the input to `EGLDisplay::new`.
    let node = open_render_node().context("opening a DRM render node for the headless EGL backend")?;
    let gbm = GbmDevice::new(node).context("creating a gbm device from the render node")?;

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

    // What:     Allocate the offscreen renderbuffer at the requested size.
    // Why:      `render_output` composites into this instead of a window framebuffer.
    let region: Size<i32, BufferCoord> = (width as i32, height as i32).into();
    let buffer = renderer
        .create_buffer(Fourcc::Argb8888, region)
        .map_err(|err| anyhow!("allocating the offscreen renderbuffer failed: {err:?}"))?;

    // What:     Shared Output + dmabuf-global setup (identical to the winit path).
    // Why:      Everything downstream of the renderer is presentation-independent.
    let (output, dmabuf_state, dmabuf_global, dmabuf_feedback) =
        finish_backend_setup(display_handle, width as i32, height as i32, &mut renderer)?;

    let backend = HeadlessBackend {
        renderer,
        buffer,
        size: (width as i32, height as i32).into(),
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

    // What:     Make the mode current with a Y-flip transform and origin position.
    // Why:      GL's framebuffer origin is bottom-left, opposite Wayland's top-left, so
    //           the readback path expects the flip (matching the original winit setup).
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
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
