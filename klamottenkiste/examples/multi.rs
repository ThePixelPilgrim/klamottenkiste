//! Three [`WaylandPane`]s side by side in ONE window, each with a HIDE/SHOW toggle.
//!
//! This exercises the reusable widget in a real layout and makes the state-preservation
//! contract visible: the toggle button hides/shows its pane WITHOUT dropping the
//! `WaylandPane` reference (the containing box keeps it alive). Hiding a pane unmaps it —
//! pausing its frame pump — but leaves its nested compositor and hosted client running with
//! all their state. Toggling back shows the same live client, unchanged.
//!
//! Point one client at each pane's printed socket, e.g.:
//!
//! ```text
//! WAYLAND_DISPLAY=<pane 0 socket> foot
//! ```
//!
//! Then hide/show pane 0: the `foot` (or Chromium, etc.) keeps running the whole time.
//!
//! Run: `cargo run --example multi`

use gtk::glib;
use libadwaita as adw;

use adw::prelude::*;

use klamottenkiste::WaylandPane;

/// How many panes to place side by side.
const N: usize = 3;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.kabelsalat.klamottenkiste.Multi")
        .build();

    app.connect_activate(|app| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_homogeneous(true);

        for i in 0..N {
            // Each pane spawns its own independent nested compositor at construction.
            let pane = WaylandPane::new();
            match pane.wayland_socket() {
                Some(socket) => println!("pane {i}: WAYLAND_DISPLAY={socket}"),
                None => eprintln!(
                    "pane {i}: compositor failed to start: {}",
                    pane.startup_error().unwrap_or_default()
                ),
            }

            // A column: [ HIDE/SHOW toggle ] over [ the pane ].
            let column = gtk::Box::new(gtk::Orientation::Vertical, 4);

            let toggle = gtk::ToggleButton::with_label(&format!("Pane {i}: shown"));
            toggle.set_active(true);
            // Keeping a ref to `pane` in the closure is the point: the hosted client stays
            // alive across every hide/show because the reference is never dropped.
            toggle.connect_toggled(glib::clone!(
                #[weak]
                pane,
                move |btn| {
                    let shown = btn.is_active();
                    pane.set_visible(shown); // hide = unmap (pauses pump); NOT a teardown
                    btn.set_label(&format!(
                        "Pane {i}: {}",
                        if shown { "shown" } else { "hidden" }
                    ));
                }
            ));

            column.append(&toggle);
            pane.set_vexpand(true);
            pane.set_hexpand(true);
            column.append(&pane);
            row.append(&column);
        }

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("klamottenkiste — three panes")
            .default_width(1600)
            .default_height(700)
            .content(&row)
            .build();
        window.present();
    });

    app.run()
}
