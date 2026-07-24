//! HEADLESS GTK-import proof for the zero-copy dmabuf present path — NO visible window.
//!
//! What it proves: GTK 4 accepts the compositor's exported dmabuf (its fourcc + modifier) and
//! actually samples real client pixels out of it, entirely off-screen. This is the crux of
//! phase B: if `gdk::DmabufTextureBuilder::build*` returns a texture and `Texture::download()`
//! pulls foot's rendered content back, the zero-copy import works on this driver.
//!
//! How, without stealing focus:
//! 1. `gtk::init()` connects to the session display but opens **no** window.
//! 2. `spawn_headless` starts a nested compositor in the default (`dmabuf`) present mode.
//! 3. A `foot` client draws a known static banner into it.
//! 4. A CPU readback (via the control-socket `screenshot`, loaded back as a `gdk::Texture`)
//!    provides an UPRIGHT reference of the same content.
//! 5. `latest_dmabuf()` is imported via `GdkDmabufTextureBuilder` and `download()`ed.
//! 6. The two are compared by luminance (histogram + per-pixel, both orientations) to confirm
//!    the dmabuf carries the same content — and to report whether it is vertically flipped
//!    relative to the readback (a GL-FBO-origin cosmetic the on-screen widget must account for).
//!
//! Both PNGs are written to `/tmp` for the user's later eyeball check. Exit code 0 = proof
//! passed, 1 = failed/blocked (with the exact reason printed). Everything is timeout-bound and
//! the foot child is always reaped.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};

use nested_wayland_session::{spawn_headless, DmabufFrame};

const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
const PAINT_WAIT: Duration = Duration::from_secs(12);
const REF_PNG: &str = "/tmp/klamottenkiste_readback_ref.png";
const DMABUF_PNG: &str = "/tmp/klamottenkiste_dmabuf_import.png";

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    // Make sure we are on the dmabuf path regardless of the caller's environment.
    // SAFETY: single-threaded here — no compositor/GTK thread spawned yet.
    unsafe {
        std::env::set_var("KLAMOTTENKISTE_PRESENT", "dmabuf");
    }

    // 1. Connect to the session display; opens NO window (no focus steal).
    if let Err(err) = gtk::init() {
        eprintln!("BLOCKED: gtk::init() failed ({err}); cannot run the headless GTK-import proof");
        return 1;
    }
    let display = match gdk::Display::default() {
        Some(d) => d,
        None => {
            eprintln!("BLOCKED: no default gdk::Display after gtk::init()");
            return 1;
        }
    };

    // 2. Nested compositor in dmabuf present mode.
    let handle = match spawn_headless(OUT_W, OUT_H) {
        Ok(h) => h,
        Err(err) => {
            eprintln!("BLOCKED: spawn_headless failed: {err:#}");
            return 1;
        }
    };
    let socket = handle.socket_name();
    let control = match handle.control_socket_path() {
        Some(p) => p.to_path_buf(),
        None => {
            eprintln!("BLOCKED: no control socket path");
            return 1;
        }
    };

    // 3. A real foot client drawing a known static banner.
    let mut client: Child = match Command::new("foot")
        .env("WAYLAND_DISPLAY", &socket)
        .arg("sh")
        .arg("-c")
        .arg("clear; printf 'FOOTPROOF-DMABUF'; sleep 999")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("BLOCKED: launching foot failed ({err}); is foot installed?");
            return 1;
        }
    };

    let exit = proof(&handle, &display, &control);

    let _ = client.kill();
    let _ = client.wait();
    // Handle drops here → compositor torn down.
    drop(handle);
    exit
}

/// The proof body, with the foot child owned by the caller for guaranteed cleanup.
fn proof(
    handle: &nested_wayland_session::HeadlessHandle,
    display: &gdk::Display,
    control: &Path,
) -> i32 {
    // 4. Wait until foot has painted, using the CPU readback (control `screenshot`) as both the
    //    "is it painted yet?" probe and the upright reference. Loading the PNG back as a
    //    gdk::Texture and downloading it puts the reference in the SAME pixel format GTK hands
    //    us for the dmabuf download (B8G8R8A8, premultiplied), so the two are directly
    //    comparable by luminance.
    let deadline = Instant::now() + PAINT_WAIT;
    let mut reference: Option<(Vec<u8>, i32, i32)> = None;
    while Instant::now() < deadline {
        match control_request(control, &format!("screenshot {REF_PNG}")) {
            Ok(resp) if resp == "ok" => {
                if let Some(px) = load_png_default(REF_PNG) {
                    if looks_painted(&px.0) {
                        reference = Some(px);
                        break;
                    }
                }
            }
            Ok(other) => eprintln!("note: screenshot control returned {other:?}"),
            Err(err) => eprintln!("note: screenshot control error: {err}"),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let (ref_px, ref_w, ref_h) = match reference {
        Some(r) => r,
        None => {
            eprintln!("FAILED: foot never produced a painted readback frame within the timeout");
            return 1;
        }
    };
    println!("readback reference: {ref_w}x{ref_h}, painted, saved to {REF_PNG}");

    // 5. Import the latest dmabuf slot as a GdkDmabufTexture and download its pixels.
    let frame = match handle.latest_dmabuf() {
        Some(f) => f,
        None => {
            eprintln!(
                "FAILED: latest_dmabuf() is None in dmabuf present mode — the compositor \
                 published no dmabuf frame (did it internally fall back to readback?)."
            );
            return 1;
        }
    };
    println!(
        "dmabuf frame: {}x{}, fourcc={} (0x{:08x}), modifier=0x{:016x}, {} plane(s), slot #{}",
        frame.width,
        frame.height,
        fourcc_str(frame.fourcc),
        frame.fourcc,
        frame.modifier,
        frame.planes.len(),
        frame.buffer_id,
    );

    let texture = match build_dmabuf_texture(&frame, display, handle.release_sender()) {
        Ok(t) => t,
        Err(err) => {
            eprintln!(
                "FAILED: GTK rejected the dmabuf (fourcc/modifier not importable on this \
                 driver): {err}. This is the honest negative result — the on-screen path \
                 must use KLAMOTTENKISTE_PRESENT=readback here."
            );
            return 1;
        }
    };
    println!("GTK ACCEPTED the dmabuf: built a {}x{} texture", texture.width(), texture.height());

    // download() forces GTK to import the dmabuf into its GL/Vulkan renderer and read the
    // pixels back — this is the moment the buffer is actually SAMPLED. If the modifier were
    // wrong or the fds unreadable, this is where it would fail/garble.
    let dw = texture.width();
    let dh = texture.height();
    let stride = (dw as usize) * 4;
    let mut dmabuf_px = vec![0u8; stride * dh as usize];
    texture.download(&mut dmabuf_px, stride);

    // Release the pool slot now that we have sampled it.
    handle.release_dmabuf(frame.buffer_id);

    if let Err(err) = texture.save_to_png(DMABUF_PNG) {
        eprintln!("note: saving the dmabuf PNG failed: {err}");
    } else {
        println!("dmabuf import saved to {DMABUF_PNG}");
    }

    // 6. Verify the dmabuf carries real, matching content.
    if !looks_painted(&dmabuf_px) {
        eprintln!("FAILED: the downloaded dmabuf is a single flat colour — GTK sampled nothing");
        return 1;
    }

    let (dark_frac, bright_frac) = luminance_extremes(&dmabuf_px);
    println!(
        "dmabuf luminance profile: {:.1}% dark (background), {:.1}% bright (text)",
        dark_frac * 100.0,
        bright_frac * 100.0,
    );
    if dark_frac < 0.30 || bright_frac <= 0.0 {
        eprintln!(
            "FAILED: dmabuf content does not look like the foot banner \
             (expected a mostly-dark frame with a minority of bright text pixels)"
        );
        return 1;
    }

    // Compare to the readback reference by luminance histogram (orientation-invariant).
    if dw != ref_w || dh != ref_h {
        eprintln!(
            "note: dmabuf {dw}x{dh} vs reference {ref_w}x{ref_h} differ in size; \
             skipping per-pixel orientation check"
        );
    }
    let hist_l1 = histogram_l1(&dmabuf_px, &ref_px);
    println!("luminance histogram L1 distance vs readback reference: {hist_l1:.4} (0 = identical)");
    if hist_l1 > 0.30 {
        eprintln!(
            "FAILED: dmabuf luminance histogram diverges from the readback of the same frame \
             (L1={hist_l1:.4}) — GTK sampled *something*, but not the expected content"
        );
        return 1;
    }

    // Orientation: does the dmabuf match the upright reference as-is, or vertically flipped?
    if dw == ref_w && dh == ref_h {
        let upright = per_pixel_luma_l1(&dmabuf_px, &ref_px, dw, dh, false);
        let flipped = per_pixel_luma_l1(&dmabuf_px, &ref_px, dw, dh, true);
        println!("per-pixel luma L1 — upright: {upright:.2}, vertically-flipped: {flipped:.2}");
        if flipped + 1.0 < upright {
            println!(
                "ORIENTATION: the dmabuf is vertically FLIPPED vs the readback (GL-FBO origin). \
                 The on-screen widget/readback flips; the dmabuf path presents the raw FBO, so \
                 the user's visual check should expect a top-bottom flip until phase C adds one."
            );
        } else {
            println!("ORIENTATION: the dmabuf matches the readback upright (no flip needed).");
        }
    }

    println!(
        "\nPASS: GTK imported the compositor dmabuf zero-copy and sampled foot's content \
         off-screen (no window opened). PNGs: {DMABUF_PNG} (import) and {REF_PNG} (readback)."
    );
    0
}

/// Build a `GdkDmabufTexture` from a compositor slot, releasing the slot when GTK is done.
///
/// Mirrors the widget's private `build_dmabuf_texture` exactly (kept in sync by hand): the
/// borrowed fds stay valid because the release closure returns `frame.buffer_id` to the
/// compositor only once GTK finishes with the texture.
fn build_dmabuf_texture(
    frame: &DmabufFrame,
    display: &gdk::Display,
    release_tx: crossbeam_channel::Sender<u64>,
) -> Result<gdk::Texture, glib::Error> {
    let mut builder = gdk::DmabufTextureBuilder::new()
        .set_display(display)
        .set_width(frame.width)
        .set_height(frame.height)
        .set_fourcc(frame.fourcc)
        .set_modifier(frame.modifier)
        .set_n_planes(frame.planes.len() as u32)
        .set_premultiplied(false);
    for (index, plane) in frame.planes.iter().enumerate() {
        let idx = index as u32;
        // SAFETY: fd valid until we send buffer_id back through release_tx (release closure).
        builder = unsafe { builder.set_fd(idx, plane.fd) }
            .set_offset(idx, plane.offset)
            .set_stride(idx, plane.stride);
    }
    let buffer_id = frame.buffer_id;
    let release_on_drop = release_tx.clone();
    // SAFETY: all planes set; release closure keeps fds alive past every GTK read.
    let result = unsafe {
        builder.build_with_release_func(move || {
            let _ = release_on_drop.send(buffer_id);
        })
    };
    if result.is_err() {
        let _ = release_tx.send(buffer_id);
    }
    result
}

/// Load a PNG as a gdk::Texture and download it in the default (B8G8R8A8) format.
fn load_png_default(path: &str) -> Option<(Vec<u8>, i32, i32)> {
    let texture = gdk::Texture::from_filename(path).ok()?;
    let w = texture.width();
    let h = texture.height();
    let stride = (w as usize) * 4;
    let mut px = vec![0u8; stride * h as usize];
    texture.download(&mut px, stride);
    Some((px, w, h))
}

/// Luminance of a B8G8R8A8 pixel (bytes: B, G, R, A).
#[inline]
fn luma(px: &[u8]) -> f64 {
    0.114 * px[0] as f64 + 0.587 * px[1] as f64 + 0.299 * px[2] as f64
}

/// A frame "looks painted" when it is not a single flat colour.
fn looks_painted(px: &[u8]) -> bool {
    if px.len() < 8 {
        return false;
    }
    let first = &px[0..4];
    px.chunks_exact(4).any(|c| c != first)
}

/// Fraction of near-black and near-white pixels (background vs text signature).
fn luminance_extremes(px: &[u8]) -> (f64, f64) {
    let n = (px.len() / 4).max(1) as f64;
    let mut dark = 0.0;
    let mut bright = 0.0;
    for c in px.chunks_exact(4) {
        let l = luma(c);
        if l < 40.0 {
            dark += 1.0;
        } else if l > 160.0 {
            bright += 1.0;
        }
    }
    (dark / n, bright / n)
}

/// Normalised 8-bin luminance histogram of a B8G8R8A8 buffer.
fn histogram(px: &[u8]) -> [f64; 8] {
    let mut h = [0.0f64; 8];
    let n = (px.len() / 4).max(1) as f64;
    for c in px.chunks_exact(4) {
        let bin = ((luma(c) / 256.0) * 8.0) as usize;
        h[bin.min(7)] += 1.0;
    }
    for b in &mut h {
        *b /= n;
    }
    h
}

/// L1 distance between the two buffers' luminance histograms (0 = identical, 2 = disjoint).
fn histogram_l1(a: &[u8], b: &[u8]) -> f64 {
    let ha = histogram(a);
    let hb = histogram(b);
    ha.iter().zip(hb.iter()).map(|(x, y)| (x - y).abs()).sum()
}

/// Mean per-pixel luminance L1 between `a` and `b` (optionally flipping `b` vertically).
fn per_pixel_luma_l1(a: &[u8], b: &[u8], w: i32, h: i32, flip_b: bool) -> f64 {
    let stride = (w as usize) * 4;
    let mut acc = 0.0f64;
    for y in 0..h as usize {
        let by = if flip_b { (h as usize - 1) - y } else { y };
        for x in 0..w as usize {
            let ai = y * stride + x * 4;
            let bi = by * stride + x * 4;
            acc += (luma(&a[ai..ai + 4]) - luma(&b[bi..bi + 4])).abs();
        }
    }
    acc / ((w as f64) * (h as f64)).max(1.0)
}

/// Render a DRM FourCC u32 as its 4-char code (for logging).
fn fourcc_str(code: u32) -> String {
    let b = code.to_le_bytes();
    b.iter()
        .map(|&c| if c.is_ascii_graphic() { c as char } else { '?' })
        .collect()
}

/// Send one control-socket line and return the response line.
fn control_request(path: &Path, line: &str) -> Result<String, String> {
    let stream = UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writer
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;
    Ok(resp.trim().to_string())
}
