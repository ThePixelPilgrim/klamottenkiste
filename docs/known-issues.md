# Known issues / deferred polish

Verified findings not yet fixed. None blocks the current use; each has a diagnosed cause.

## KI-1. Embedded content renders physically smaller than a native window (output scale not advertised)

**Symptom (observed on-screen 2026-07-24):** an embedded Chrome renders dimensionally *smaller*
than the same Chrome run natively on the host — content and UI are physically smaller, though
crisp (not blurry) and at the correct resolution. Present in both `dmabuf` and `readback` present
modes, so it is a compositor-side sizing issue, not a presentation-path one. Not disruptive.

**Cause:** the nested compositor drives the output *mode* to `widget_allocation × scale_factor`
(native device pixels — that is what keeps it crisp), but advertises the `wl_output` at **scale 1**
and does not implement fractional scaling. So the hosted client computes `devicePixelRatio = 1`
and lays everything out at 1 CSS px = 1 device px. A native window on a HiDPI / fractionally-scaled
display (Fedora/GNOME defaults to fractional, e.g. 1.25–1.5×) instead gets the real scale and lays
out at 1 CSS px = N device px — so native content is N× physically larger than in the pane.

**Fix (Wayland-native, for the finer-UI phase):**
- Integer displays: advertise `wl_output.scale = round(host scale_factor)` on the nested output
  (the widget already knows `scale_factor()` — it uses it for the pixel sizing).
- Fractional displays (the actual target): implement `wp_fractional_scale_v1` (+ `wp_viewporter`)
  so the client receives the exact fractional scale and renders at correct physical size. The fork
  does not implement these protocols yet.

**Why crispness is already fine:** the buffer *is* allocated at native device pixels, so there is
no upscaling blur — the only thing wrong is the logical scale the client is told to lay out for.

## KI-2. Cursor never changes shape

The hosted client's `CursorImageStatus` is discarded (`handler.rs::cursor_image()` is a no-op).
Advertise `wp_cursor_shape_v1` and forward the shape to the GTK widget's `set_cursor`. (Carried
over from the kabelsalat spike finding D1.)

## KI-3. Injected `key <single-char>` uses the US table

`type` is layout-aware (resolves via the seat xkb keymap); `key <single-char>` still routes
through the US `char_to_key` table, so `key y` on a `de` seat presses the US `y`. Modifier/named
keys are position-based and correct. Settle the intended semantics before exposing `key` as an
automation primitive. (Carried over from spike finding D7.)

## KI-4. Present-path perf refinement

The dmabuf path uses `glFinish` (a full GPU barrier) before handing a buffer to GTK — correct, and
it removes the per-frame `glReadPixels` + memcpy, but not pipelined. A consumer-waited GLES/EGL
fence would let the compositor keep working while GTK samples. See `docs/presentation.md`.
