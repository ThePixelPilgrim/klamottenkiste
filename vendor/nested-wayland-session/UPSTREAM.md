# Upstream attribution

This directory vendors a fork verbatim from:

- **Upstream repo:** https://github.com/Aquaticat/Monochromatic
- **Subpath:** `package/cli/nested-wayland-session`
- **Cloned commit SHA:** `6c4fc2941c3ac966040be3c96acb347574ba4b7a`
- **Vendored on:** 2026-07-23

The crate is `monochromatic-nested-wayland-session` (library name
`nested_wayland_session`), licensed LGPL-3.0-or-later. Its `LICENSES/`
directory and license notices are retained unchanged.

## Local modifications to the copied `Cargo.toml`

- Removed the trailing empty `[workspace]` table so this crate can be a
  member of the kabelsalat workspace instead of its own workspace root.

## Build notes

Task 1 keeps the `smithay` feature set exactly as upstream (including
`backend_winit`); feature stripping is Task 2.

### System library status

Task 1 (winit path) built with no missing system libraries. Task 2
switched to a **headless EGL/GLES backend over a DRM render node**, which
links against `libgbm` via smithay's `gbm-sys` (`#[link(name="gbm")]`).
This requires the **devel** package that provides the bare `libgbm.so`
linker symlink — the runtime `mesa-libgbm` (`libgbm.so.1`) alone is not
enough. On Fedora:

```
sudo dnf install mesa-libgbm-devel
```

Symptom if missing: `cargo check` passes but `cargo build`/`cargo run`
fails at link with `rust-lld: error: unable to find library -lgbm`.
Installed on the target 2026-07-23; full `cargo build --bin spike` links
and the spike comes up headless (advertises a live `wayland-N` socket on
`/dev/dri/renderD128`).

### Runtime backend selection

The headless backend picks a DRM render node in this order:
`$KABELSALAT_DRM_RENDER_NODE`, else the first `/dev/dri/renderD*`, else
`/dev/dri/renderD128`.
