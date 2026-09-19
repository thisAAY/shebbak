mod pipeline;
mod session;

use anyhow::Result;
use srw_capture::macos::list::list_apps;
use srw_capture::macos::permissions;
use std::io::Write;
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

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

    // List apps and pick one or more to share, by pid. `SRW_SHARE_PIDS`
    // (comma-separated pids) skips the interactive prompt for scripting
    // and reconnect testing.
    let pids: Vec<i32> = if let Ok(raw) = std::env::var("SRW_SHARE_PIDS") {
        let pids: Vec<i32> = raw
            .split(',')
            .map(|s| -> Result<i32> { Ok(s.trim().parse()?) })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(
            !pids.is_empty(),
            "SRW_SHARE_PIDS must list at least one pid"
        );
        pids
    } else {
        let apps = list_apps()?;
        println!("{:<4} {:<8} {:<24} windows", "idx", "pid", "app");
        for (i, a) in apps.iter().enumerate() {
            println!(
                "{:<4} {:<8} {:<24} {}",
                i,
                a.pid,
                a.app_name,
                a.window_titles.join(" | ")
            );
        }
        print!("app indices to share (comma-separated): ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let pids: Vec<i32> = line
            .trim()
            .split(',')
            .map(|s| -> Result<i32> {
                let idx: usize = s.trim().parse()?;
                anyhow::ensure!(idx < apps.len(), "index {idx} out of range");
                Ok(apps[idx].pid)
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(!pids.is_empty(), "pick at least one app");
        pids
    };

    // Host display scale for capture pixel size. M1: assume the main display's
    // scale; 2.0 on retina Macs. Read it from CGDisplay.
    let scale = main_display_scale();
    info!("host display scale: {scale}");

    // Session loop: serve one client at a time; on disconnect, wait for the next.
    loop {
        if let Err(e) = session::run_session(&pids, scale).await {
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
