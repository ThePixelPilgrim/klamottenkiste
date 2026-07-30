//! X11 guinea-pig for the Xwayland readiness harness
//! (`docs/superpowers/specs/2026-07-30-xwayland-test-harness-design.md`).
//!
//! Connects to `$DISPLAY`, maps one window, fills it `#FF00FF` on every Expose,
//! and prints one flushed line per observed event so the spawning test can
//! assert on stdout: `mapped`, `exposed`, `key-press <keycode>`,
//! `button-press <n>`.

use std::io::Write;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Rectangle, WindowClass,
};

/// `#FF00FF` as a truecolor pixel value. Xwayland's default root visual is
/// 24-bit truecolor where the pixel is `0x00RRGGBB`, so this holds without a
/// visual lookup.
const MAGENTA: u32 = 0x00ff_00ff;

/// Initial window size; the compositor's X11 WM is expected to reconfigure the
/// window (the nested session maps its single app fullscreen), tracked via
/// ConfigureNotify.
const INIT_W: u16 = 640;
const INIT_H: u16 = 480;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];

    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        INIT_W,
        INIT_H,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::KEY_PRESS
                    | EventMask::BUTTON_PRESS
                    | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(MAGENTA))?;
    conn.map_window(win)?;
    conn.flush()?;

    let stdout = std::io::stdout();
    let mut size = (INIT_W, INIT_H);
    loop {
        let event = conn.wait_for_event()?;
        let mut out = stdout.lock();
        match event {
            Event::MapNotify(_) => writeln!(out, "mapped")?,
            Event::ConfigureNotify(e) => size = (e.width, e.height),
            Event::Expose(_) => {
                conn.poly_fill_rectangle(
                    win,
                    gc,
                    &[Rectangle {
                        x: 0,
                        y: 0,
                        width: size.0,
                        height: size.1,
                    }],
                )?;
                conn.flush()?;
                writeln!(out, "exposed")?;
            }
            Event::KeyPress(e) => writeln!(out, "key-press {}", e.detail)?,
            Event::ButtonPress(e) => writeln!(out, "button-press {}", e.detail)?,
            _ => {}
        }
        out.flush()?;
    }
}
