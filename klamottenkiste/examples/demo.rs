//! Minimal single-pane demo: one [`WaylandPane`] filling an `adw::ApplicationWindow`.
//!
//! Everything that used to live here by hand — spawning the compositor, the frame pump, the
//! input controllers, the coordinate mapping, and the resize poll — now lives inside the
//! widget. The example just builds a window around one pane and prints the sockets so you can
//! point a client at it:
//!
//! ```text
//! WAYLAND_DISPLAY=<printed socket> foot
//! ```
//!
//! Run: `cargo run --example demo`

use gtk::glib;
use libadwaita as adw;

use adw::prelude::*;

use klamottenkiste::WaylandPane;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.kabelsalat.klamottenkiste.Demo")
        .build();

    app.connect_activate(|app| {
        // The whole embedding pipeline in one widget. The compositor spawns as the widget is
        // constructed, before it is ever mapped.
        let pane = WaylandPane::new();

        if let Some(socket) = pane.wayland_socket() {
            println!("nested wayland socket: {socket}");
            println!("point a client at it, e.g. WAYLAND_DISPLAY={socket} foot");
        }
        if let Some(control) = pane.control_socket_path() {
            println!("control socket: {}", control.display());
        }
        if let Some(err) = pane.startup_error() {
            eprintln!("compositor failed to start: {err}");
        }

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("klamottenkiste demo")
            .default_width(1280)
            .default_height(800)
            .content(&pane)
            .build();
        window.present();
    });

    app.run()
}
