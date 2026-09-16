mod pipeline;
mod session;

use anyhow::Result;
use srw_capture::macos::list::{list_windows, WindowListEntry};
use srw_capture::macos::permissions;
use std::io::Write;
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into()),
    ).init();

    // Permissions: fail loudly with directions (spec: tear down loudly).
    if !permissions::check_screen_recording() {
        eprintln!("Screen Recording permission is required.");
        eprintln!("Grant it: System Settings > Privacy & Security > Screen Recording > enable your terminal, then re-run.");
        std::process::exit(1);
    }
    if !permissions::check_accessibility() {
        eprintln!("Accessibility permission is required for input injection.");
        eprintln!("Grant it: System Settings > Privacy & Security > Accessibility > enable your terminal, then re-run.");
        std::process::exit(1);
    }

    // List and pick two windows.
    let windows = list_windows()?;
    println!("{:<4} {:<8} {:<24} {:<40}", "idx", "cgid", "app", "title");
    for (i, w) in windows.iter().enumerate() {
        println!("{:<4} {:<8} {:<24} {:<40}", i, w.info.id, w.app_name, w.info.title);
    }
    let pick = |prompt: &str| -> Result<usize> {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let idx: usize = line.trim().parse()?;
        anyhow::ensure!(idx < windows.len(), "index out of range");
        Ok(idx)
    };
    let a = pick("first window index: ")?;
    let b = pick("second window index: ")?;
    anyhow::ensure!(a != b, "pick two different windows");
    let chosen: [WindowListEntry; 2] = [windows[a].clone(), windows[b].clone()];

    // Host display scale for capture pixel size. M1: assume the main display's
    // scale; 2.0 on retina Macs. Read it from CGDisplay.
    let scale = main_display_scale();
    info!("host display scale: {scale}");

    // Session loop: serve one client at a time; on disconnect, wait for the next.
    loop {
        if let Err(e) = session::run_session(&chosen, scale).await {
            eprintln!("session ended: {e:#}");
        }
        println!("waiting for next client...");
        // A fast-failing session (e.g. a junk POST that makes serve_one_offer
        // return Err) would otherwise spin the loop at full CPU and can fail
        // to rebind port 9009 (EADDRINUSE) on back-to-back attempts.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn main_display_scale() -> f64 {
    use core_graphics::display::CGDisplay;
    // `CGDisplayPixelsWide`/`pixels_wide()` already returns *points* on Retina
    // displays (it tracks the logical resolution, not the backing pixel
    // count), so pairing it with `bounds().size.width` (also points) always
    // yields a ratio of 1. The actual pixels-per-point scale only shows up by
    // comparing the display mode's pixel dimensions against its point
    // dimensions.
    match CGDisplay::main().display_mode() {
        Some(mode) => {
            let points = mode.width() as f64;
            if points > 0.0 {
                (mode.pixel_width() as f64 / points).round().max(1.0)
            } else {
                2.0
            }
        }
        None => 2.0,
    }
}
