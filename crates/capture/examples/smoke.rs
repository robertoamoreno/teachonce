//! A real 4-second capture against the live machine, for verifying that the
//! macOS permissions and the screen/window/clipboard paths actually work.
//!
//!     cargo run -p skillrec-capture --example smoke
//!
//! Deliberately an example, not a test: it needs a real screen and real
//! permissions, so it must never run in a headless `cargo test`.
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use skillrec_capture::collector::{Collector, CollectorContext, CollectorHost};
use skillrec_capture::{ActiveWindowCollector, ClipboardCollector, ScreenCollector, permissions};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let report = permissions::report();
    println!("screen recording: {:?}", report.screen_recording);
    println!("accessibility:    {:?}", report.accessibility);

    match skillrec_capture::window::active_window() {
        Some(window) => println!("frontmost: {:?} — {:?}", window.app, window.title),
        None => println!("frontmost: <none visible — Screen Recording is probably denied>"),
    }

    let dir: PathBuf = std::env::temp_dir().join("skillrec-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(ActiveWindowCollector::new(true, true)),
        Box::new(ClipboardCollector::new()),
        Box::new(ScreenCollector::new()),
    ];
    let started = skillrec_core::epoch_ms();
    let host = CollectorHost::start(collectors, tx, dir.clone(), started);

    println!("\ncapturing for 4 seconds — switch apps or copy something…");
    tokio::time::sleep(Duration::from_secs(4)).await;
    host.stop();

    let mut count = 0;
    while let Ok(event) = rx.try_recv() {
        count += 1;
        println!("  [{}] {:?}", event.source, event.payload);
    }
    let frames = std::fs::read_dir(dir.join("frames")).map(|d| d.count()).unwrap_or(0);
    println!("\n{count} events, {frames} frames kept, in {}", dir.display());

    // Prove the context type is exercised too.
    let _ = CollectorContext::new(
        tokio::sync::mpsc::unbounded_channel().0,
        Arc::new(AtomicBool::new(false)),
        dir,
        started,
    );
}
