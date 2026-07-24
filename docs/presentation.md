# Presentation: how a composited frame reaches the GTK `Picture`

`WaylandPane` (`klamottenkiste/src/widget.rs`) embeds a headless nested Smithay compositor and
presents its output in an internal `gtk::Picture`. There are **two present paths**, selected
once at startup by the `KLAMOTTENKISTE_PRESENT` environment variable. Both are driven by the
same ~60 Hz frame pump, which runs **only while the widget is mapped** (see the two-lifecycle
model in the `widget.rs` module docs).

| `KLAMOTTENKISTE_PRESENT` | Path | Texture | Copies |
| --- | --- | --- | --- |
| `dmabuf` (default, or unset) | Zero-copy dmabuf import | `GdkDmabufTexture` | none (GPU-resident) |
| `readback` | CPU readback | `gdk::MemoryTexture` | `glReadPixels` + row-flip + memcpy, every frame |

The same variable is read by **both** the compositor backend (which decides what it composites
into and publishes) and the widget (which decides what it imports), so they always agree.

## Readback path (`readback`) — the fallback

The compositor composites into a plain GLES renderbuffer, reads it back to CPU memory
(`render::read_frame_rgba`: `glReadPixels`, then flip bottom-up → upright), and publishes an
owned `Frame { bytes, width, height, stride }` via `HeadlessHandle::latest_frame()`. Each pump
tick wraps those bytes in a `gdk::MemoryTexture` (`R8g8b8a8`) and sets it as the paintable.
This is the original, always-correct path; it is unchanged by phase B and is the safety net for
any driver where the dmabuf import is rejected.

## Dmabuf path (`dmabuf`) — zero-copy, the default

The compositor composites into a **rotating pool of dmabuf-backed render targets** (pool size
3, a triple buffer; see `vendor/nested-wayland-session/src/backend.rs`). After compositing a
frame it runs `glFinish` (a full GPU barrier, so the pixels are complete before anyone else
samples the fd), marks that pool slot `in_flight`, and publishes a plain
`DmabufFrame { width, height, fourcc: u32, modifier: u64, planes: [{ fd, offset, stride }],
buffer_id }` via `HeadlessHandle::latest_dmabuf()`. The `fd`s are **borrowed** — the compositor
keeps the owning `Dmabuf` alive as long as the slot stays `in_flight`.

Each pump tick, the widget (`imp::WaylandPane::pump_tick` → `build_dmabuf_texture`):

1. Builds a `gdk::DmabufTextureBuilder`, setting `display`, `width`, `height`, `fourcc`,
   `modifier`, `n_planes`, and per plane `set_fd` / `set_offset` / `set_stride`.
2. Calls `build_with_release_func(closure)` where `closure` returns the slot to the compositor
   (`release_dmabuf(buffer_id)`) — see the release contract below.
3. Sets the resulting `gdk::Texture` as the `Picture` paintable. No pixels are copied; GTK
   imports the dmabuf into its own renderer (EGL/Vulkan) and samples it directly.

### Buffer / release design (the hazard and how it is handled)

GTK4 does **not** take ownership of the fds passed to `DmabufTextureBuilder`. The caller must
keep them valid until GTK is finished with the texture; GTK signals that by running the
`build_with_release_func` closure when the texture is finalized. We wire that closure straight
to `HeadlessHandle::release_dmabuf(buffer_id)`, which returns the pool slot to the free set. So:

```
compositor renders slot N → glFinish → marks N in_flight → publishes DmabufFrame{buffer_id=N}
        │
widget imports N as a GdkDmabufTexture, release closure captures N
        │
… GTK samples slot N's fds (still in_flight, fds valid) …
        │
widget drops the texture (new frame set, or unmap/close)
        │
GTK finalizes the texture → runs the closure → release_dmabuf(N)
        │
compositor drains the release channel before its next bind → slot N returns to the free pool
```

This is what prevents the compositor from drawing into a buffer GTK is still sampling: a slot
is not re-bound for rendering until it has been released. The release closure is
`FnOnce + Send + 'static` and captures only a crossbeam `Sender<u64>` (obtained from
`HeadlessHandle::release_sender()`) plus the `u64` slot id, so it marshals safely no matter
which thread GTK finalizes the texture on.

**Send-safety / build failure.** If `build_with_release_func` fails (GTK rejects the
fourcc/modifier), GTK never runs the closure, so `build_dmabuf_texture` releases the slot
itself — otherwise a rejected frame would pin its slot `in_flight` forever and shrink the pool
by one each tick. On repeated failure the widget warns **once** and falls through to the
readback slot (which is empty in a healthy dmabuf run, so the honest fix on such a driver is
`KLAMOTTENKISTE_PRESENT=readback`).

**Pool sizing.** The pool is 3. In steady state the widget holds at most the current paintable
(1) plus possibly one texture whose release is still in flight (1), leaving a free slot for the
renderer. If GTK ever holds textures longer and all three slots are `in_flight`, the compositor
falls back to round-robin reuse (with a `warn!`) rather than stalling; because the hosted
client's content is stable between its own frames this degrades gracefully, but a persistently
slow consumer would want a larger pool. This has not been observed in the headless proof.

### Lifecycle interaction

The dmabuf path obeys the same two lifecycle rules as the readback path:

- The pump runs **only while mapped**; `unmap` stops it.
- On `unmap` the widget also clears the `Picture` paintable
  (`set_paintable(gdk::Paintable::NONE)`), which drops any imported dmabuf texture so its
  release closure fires and the slot returns to the compositor — **a hidden pane pins no
  buffer**. This returns a buffer to the pool but does not touch the hosted client's state; the
  next `map` repaints within one pump tick.
- The compositor is untouched by map/unmap. Only `close()` / `dispose` tears it down; both also
  drop the paintable and clear the release sender so no pool slot outlives the compositor.

## Verification

- **`cargo test --workspace`** — widget unit tests, compositor tests, and the
  `tests/lifecycle.rs` integration test (which pins `readback` mode and proves the client
  survives pump pause/resume) all pass.
- **`cargo run --example dmabuf_import_check`** — the headless GTK-import proof. It calls
  `gtk::init()` (connects to the session display, opens **no** window → no focus steal), spawns
  a nested compositor with a `foot` client, imports `latest_dmabuf()` via
  `DmabufTextureBuilder`, and `Texture::download()`s the pixels back off-screen. It asserts the
  content is non-blank, has the expected dark-background / bright-text signature, and matches a
  CPU-readback reference of the same frame by luminance. This proves GTK **accepted** the
  fourcc/modifier and **sampled** the buffer. Writes `/tmp/klamottenkiste_dmabuf_import.png`
  (the import) and `/tmp/klamottenkiste_readback_ref.png` (the readback) for a visual check.

### Known issue: vertical flip (open, for the on-screen visual pass)

The import proof found that the imported dmabuf is **pixel-identical to the readback but
vertically flipped** (per-pixel luminance L1: `0.00` when the reference is flipped, `~19` when
not). Root cause: GLES renders bottom-up. `read_frame_rgba` flips the readback to upright
before publishing, but the dmabuf export (`backend::export_current`) hands GTK the **raw**
bottom-up FBO, and GTK samples a DRM `ARGB8888` buffer top-down. So a mapped widget on the
dmabuf path would show the hosted app upside-down.

The correct fix keeps the widget and its input coordinate mapping present-mode-agnostic (a
widget-side visual flip would desync clicks, which map through the shared `widget_to_output`):
make the **compositor** produce an upright dmabuf — e.g. render the pool target with a flipped
projection, or flip at export — so both present paths deliver upright pixels. That is a
compositor-side change (phase A surface) and is left as the one item for the user's on-screen
visual confirmation.
