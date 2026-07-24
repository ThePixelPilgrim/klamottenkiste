//! Widget-gate harness: prove N=3 nested-compositor instances coexist in ONE process.
//!
//! This is the isolation gate for turning the nested compositor into an embeddable widget:
//! it must be possible to run several independent instances inside a single host process,
//! each hosting its own Wayland client with no cross-talk. This harness proves exactly that,
//! HEADLESSLY — it opens **no GTK window** (a real window would steal the user's keyboard and
//! mouse focus), driving everything through `spawn_headless` and the compositor's own CPU
//! readback (`HeadlessHandle::latest_frame` + `screenshot::write_rgba_png`).
//!
//! What it does:
//!   1. Calls `spawn_headless(800, 600)` THREE times in this process, collecting 3 handles.
//!   2. Asserts the three nested Wayland socket names are DISTINCT and the three control
//!      socket paths are DISTINCT (the per-PID control path used to collide — see the fix in
//!      `vendor/nested-wayland-session/src/app.rs`).
//!   3. Launches three `foot` clients, one per nested socket, each printing a DISTINCT string
//!      `PANE-<i>`, so a rendered frame visibly identifies which instance produced it.
//!   4. Waits for them to paint, then for EACH instance reads `latest_frame()` and writes a
//!      PNG to a distinct path under a scratch dir, and prints the three paths.
//!   5. Kills every `foot` child and exits; the whole run is time-bounded.
//!
//! Run: `cargo run --example multi_headless`

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use klamottenkiste::compositor::{screenshot, spawn_headless};

/// Number of coexisting instances to prove.
const N: usize = 3;
/// Per-instance compositor output size.
const OUT_W: u32 = 800;
const OUT_H: u32 = 600;
/// Hard wall-clock budget for the whole run, so nothing lingers.
const OVERALL_BUDGET: Duration = Duration::from_secs(30);
/// How long to let the `foot` clients paint before reading frames back.
const PAINT_WAIT: Duration = Duration::from_secs(6);

/// A pixel buffer is considered "painted" if it is not a single flat colour (a black or
/// uniform frame means the client has not drawn yet). Returns the count of distinct-ish rows.
fn looks_painted(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let first = &bytes[0..4];
    // Any pixel differing from the top-left pixel means real content was composited.
    bytes.chunks_exact(4).any(|px| px != first)
}

fn main() {
    let start = Instant::now();

    // Scratch dir for the PNGs.
    let scratch = std::env::temp_dir().join(format!("klamottenkiste-multigate-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("creating scratch dir");

    // 1. THREE headless instances in this ONE process.
    eprintln!("spawning {N} headless nested-compositor instances in one process (pid {})...", std::process::id());
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let h = spawn_headless(OUT_W, OUT_H)
            .unwrap_or_else(|e| panic!("instance {i}: spawn_headless failed: {e:#}"));
        eprintln!(
            "  instance {i}: socket = {:?}  control = {:?}",
            h.socket_name(),
            h.control_socket_path()
        );
        handles.push(h);
    }

    // 2. DISTINCT sockets + DISTINCT control paths.
    let socket_names: Vec<String> = handles.iter().map(|h| h.socket_name()).collect();
    let control_paths: Vec<PathBuf> = handles
        .iter()
        .map(|h| h.control_socket_path().expect("control socket path present").to_path_buf())
        .collect();
    {
        let mut s = socket_names.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), N, "nested socket names must be distinct, got {socket_names:?}");
        let mut c = control_paths.clone();
        c.sort();
        c.dedup();
        assert_eq!(c.len(), N, "control socket paths must be distinct, got {control_paths:?}");
    }
    eprintln!("OK: {N} distinct nested sockets and {N} distinct control sockets");

    // 3. One `foot` client per nested socket, each printing a distinct PANE-<i>.
    let mut children: Vec<Child> = Vec::with_capacity(N);
    for (i, sock) in socket_names.iter().enumerate() {
        let script = format!(
            "clear; tput civis 2>/dev/null; printf 'PANE-{i}'; sleep 999"
        );
        let child = Command::new("foot")
            .env("WAYLAND_DISPLAY", sock)
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("instance {i}: launching foot failed: {e}"));
        eprintln!("  launched foot pid {} -> WAYLAND_DISPLAY={sock} (PANE-{i})", child.id());
        children.push(child);
    }

    // 4. Wait for paint, then read each instance's frame back and write a PNG.
    eprintln!("waiting up to {:?} for clients to paint...", PAINT_WAIT);
    let paint_deadline = Instant::now() + PAINT_WAIT;
    // Poll until every instance has a painted (non-flat) frame, or the deadline passes.
    loop {
        let all_painted = handles
            .iter()
            .all(|h| h.latest_frame().map(|f| looks_painted(&f.bytes)).unwrap_or(false));
        if all_painted || Instant::now() >= paint_deadline || start.elapsed() >= OVERALL_BUDGET {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let mut png_paths = Vec::with_capacity(N);
    for (i, h) in handles.iter().enumerate() {
        let path = scratch.join(format!("pane-{i}.png"));
        match h.latest_frame() {
            Some(f) => {
                screenshot::write_rgba_png(&f.bytes, f.width, f.height, f.stride, &path)
                    .unwrap_or_else(|e| panic!("instance {i}: writing PNG failed: {e:#}"));
                let painted = looks_painted(&f.bytes);
                eprintln!(
                    "  instance {i}: wrote {} ({}x{}, painted={})",
                    path.display(),
                    f.width,
                    f.height,
                    painted
                );
            }
            None => {
                eprintln!("  instance {i}: NO FRAME available (latest_frame() == None)");
            }
        }
        png_paths.push(path);
    }

    // 5. Print the three PNG paths on their own lines for the harness reader.
    println!("=== PNG PATHS ===");
    for p in &png_paths {
        println!("{}", p.display());
    }

    // Cleanup: kill every foot child.
    for (i, mut c) in children.into_iter().enumerate() {
        let _ = c.kill();
        let _ = c.wait();
        eprintln!("  killed foot for instance {i}");
    }
    eprintln!("done in {:?}", start.elapsed());
    // Do not join the compositor threads; the process exits, tearing them down.
}
