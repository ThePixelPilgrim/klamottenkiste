//! Screenshot capture: render one frame and write the framebuffer as a PNG.
//!
//! The shared readback primitive now lives in `render::read_frame_rgba` (which binds the
//! offscreen framebuffer, composites the current frame, copies it to CPU memory, and
//! returns upright RGBA8). This module only turns that buffer into a PNG file: `capture`
//! reads one frame and writes it; `write_rgba_png` is the reusable encoder (also used by
//! the GTK spike's debug dump). Keeping the readback in `render.rs` means orientation and
//! pixel-format handling live in exactly one place.

/// What:     `use std::path::Path;`. Borrowed filesystem path.
/// Why:      `capture` / `write_rgba_png` write to a caller-provided path.
use std::path::Path;

/// What:     `use anyhow::{Context, Result};`. Error helpers.
/// Why:      The functions return `Result` and annotate each fallible step.
use anyhow::{Context, Result};

/// What:     Grouped `use` of the in-process PNG encoder pieces from the `image` crate.
/// Why:      `write_rgba_png` encodes the readback buffer as a PNG without a separate
///           encoder module.
use image::{codecs::png::PngEncoder, ColorType, ImageEncoder};

/// What:     `use crate::{render::{read_frame_rgba, BYTES_PER_PIXEL}, state::Compositor};`.
/// Why:      `capture` reads a frame through the shared readback; the PNG writer sizes rows
///           with the shared bytes-per-pixel constant.
use crate::{
    render::{read_frame_rgba, BYTES_PER_PIXEL},
    state::Compositor,
};

/// Render the current frame and write it to `path` as a single PNG.
///
/// What:     `pub fn capture(state: &mut Compositor, path: &Path) -> Result<()>`. Reads one
///           frame via `render::read_frame_rgba` and encodes it synchronously (a single
///           screenshot is not on a hot path).
/// Why:      The `screenshot` control command's implementation.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// function capture(state, path): void { ... }
/// ```
pub fn capture(state: &mut Compositor, path: &Path) -> Result<()> {
    // What:     Read one upright RGBA frame (bytes, width, height, stride).
    // Why:      `read_frame_rgba` already flips to upright and reports the format. A GPU
    //           failure becomes this command's error — the control socket answers `err ...`
    //           and the compositor keeps running, rather than unwinding the event loop.
    let (pixels, width, height, stride) =
        read_frame_rgba(state).context("rendering the frame to screenshot")?;

    // What:     Encode the upright buffer to `path`.
    // Why:      `read_frame_rgba` returns top-down pixels, so no further flip is needed.
    write_rgba_png(&pixels, width, height, stride, path)
        .with_context(|| format!("writing screenshot to {}", path.display()))
}

/// Write upright RGBA8 `pixels` to `path` as a PNG.
///
/// What:     `pub fn write_rgba_png(pixels: &[u8], width: u32, height: u32, stride: usize,
///           path: &Path) -> Result<()>`. Encodes the buffer as an RGBA8 PNG via the
///           `image` crate. When `stride` exceeds the tight row width the rows are repacked
///           first; the common case (`stride == width * 4`) encodes without a copy.
/// Why:      Shared by the `screenshot` command and the GTK spike's `KABELSALAT_SPIKE_DUMP`
///           verification dump; the pixels are expected already upright (top-down).
pub fn write_rgba_png(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    path: &Path,
) -> Result<()> {
    // What:     The tightly packed row width in bytes.
    // Why:      PNG encoding wants contiguous RGBA rows.
    let tight = width as usize * BYTES_PER_PIXEL;

    // What:     Create the destination file.
    // Why:      Write the upright PNG to disk.
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let encoder = PngEncoder::new(file);

    if stride == tight {
        // What:     Encode the buffer directly.
        // Why:      Tightly packed rows need no repacking.
        encoder
            .write_image(pixels, width, height, ColorType::Rgba8.into())
            .context("encoding the screenshot PNG")?;
    } else {
        // What:     Repack each strided row into a tight buffer, then encode.
        // Why:      The `image` encoder assumes no inter-row padding.
        let mut packed = vec![0u8; tight * height as usize];
        for row in 0..height as usize {
            let src = &pixels[row * stride..row * stride + tight];
            packed[row * tight..(row + 1) * tight].copy_from_slice(src);
        }
        encoder
            .write_image(&packed, width, height, ColorType::Rgba8.into())
            .context("encoding the screenshot PNG")?;
    }

    Ok(())
}
