//! Reproduction harness: does a hide/show (unparent/reparent) cycle kill keyboard input?
//!
//! Mirrors how kabelsalat's `sync_browser_pane` swaps the pane: the pane competes for GTK
//! focus with a second focusable widget (standing in for the VTE terminal), then gets
//! `set_visible(false)` + unparented, then reparented + `set_visible(true)`.
//!
//! It prints the window's focus widget at each phase and pauses between them, so an
//! outside driver can inject `type ...` through the control socket and screenshot the
//! result before and after the cycle.
//!
//! Run: cargo run --example focus_cycle    (client from $FOCUS_CYCLE_CLIENT, default foot)

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use klamottenkiste::WaylandPane;
use libadwaita as adw;

fn focus_name(window: &adw::ApplicationWindow) -> String {
    match gtk::prelude::GtkWindowExt::focus(window) {
        Some(w) => format!("{} ({:?})", w.type_().name(), w.widget_name()),
        None => "<none>".to_string(),
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.kabelsalat.klamottenkiste.FocusCycle")
        .build();

    app.connect_activate(|app| {
        let pane = WaylandPane::new();

        let socket = pane.wayland_socket().unwrap_or_default();
        println!("nested wayland socket: {socket}");
        if let Some(control) = pane.control_socket_path() {
            println!("control socket: {}", control.display());
        }
        if let Some(err) = pane.startup_error() {
            eprintln!("compositor failed to start: {err}");
        }

        // A competing focusable widget, standing in for kabelsalat's VTE terminal.
        let terminal_stand_in = gtk::TextView::new();
        terminal_stand_in.set_widget_name("terminal-stand-in");
        terminal_stand_in
            .buffer()
            .set_text("competing focusable widget\n");

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&terminal_stand_in));
        paned.set_end_child(Some(&pane));
        paned.set_position(300);

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("focus cycle")
            .default_width(1280)
            .default_height(800)
            .content(&paned)
            .build();
        window.present();

        // Spawn the hosted client into the nested socket.
        let client = std::env::var("FOCUS_CYCLE_CLIENT").unwrap_or_else(|_| "foot".to_string());
        let mut parts = client.split_whitespace();
        let program = parts.next().unwrap_or("foot").to_string();
        let args: Vec<String> = parts.map(str::to_string).collect();
        // The client logs its own protocol traffic, so focus enter/leave is observable.
        let client_log = std::fs::File::create("/tmp/fc_client.log").ok();
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .env("WAYLAND_DISPLAY", &socket)
            .env("WAYLAND_DEBUG", "1");
        if let Some(log) = client_log {
            cmd.stderr(log);
        }
        let child = cmd.spawn();
        match child {
            Ok(_) => println!("spawned client: {client}"),
            Err(err) => eprintln!("could not spawn {program}: {err}"),
        }

        // Scripted phases. Each step prints where GTK focus sits.
        let step = Rc::new(RefCell::new(0u32));
        let pane = Rc::new(pane);
        let paned = Rc::new(paned);
        let window = Rc::new(window);
        glib::timeout_add_seconds_local(3, move || {
            let mut n = step.borrow_mut();
            *n += 1;
            match *n {
                // Give the client time to map, then report the steady state.
                2 => {
                    // Stand in for the user clicking into the pane: the widget's own
                    // GestureClick does exactly this (`grab_focus`), so the pane HOLDS
                    // GTK focus when the hide/show cycle later unparents it.
                    println!("GRAB: pane.grab_focus() (stands in for a user click)");
                    let got = pane.grab_focus();
                    println!(
                        "  grab_focus returned {got}, focus = {}",
                        focus_name(&window)
                    );
                }
                3 => println!(
                    "PHASE A (client mapped, before cycle): focus = {}",
                    focus_name(&window)
                ),
                // Pause here so the driver can inject keys for the "before" sample.
                5 => {
                    println!("HIDE: set_visible(false) + unparent (as sync_browser_pane does)");
                    pane.set_visible(false);
                    paned.set_end_child(gtk::Widget::NONE);
                    println!("  focus after hide = {}", focus_name(&window));
                }
                7 => {
                    println!("SHOW: reparent + set_visible(true)");
                    pane.set_visible(true);
                    paned.set_end_child(Some(pane.as_ref()));
                    paned.set_position(300);
                    println!("  focus after show = {}", focus_name(&window));
                }
                9 => {
                    println!("PHASE B (after cycle): focus = {}", focus_name(&window));
                    println!("READY: inject keys now and compare with PHASE A");
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        });
    });

    app.run()
}
