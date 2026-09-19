#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use srw_capture::macos::{list::list_windows, permissions, stream::SckCapture};
    use srw_capture::WindowCapture;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if !permissions::check_screen_recording() {
        eprintln!("Screen Recording permission missing.");
        eprintln!(
            "Grant it in System Settings > Privacy & Security > Screen Recording, then re-run."
        );
        std::process::exit(1);
    }

    let arg = std::env::args().nth(1);
    let windows = list_windows()?;
    let entry = match arg.and_then(|s| s.parse::<u32>().ok()) {
        Some(id) => windows
            .into_iter()
            .find(|w| w.info.id == id)
            .expect("window id not found"),
        None => {
            eprintln!("usage: capture_probe <cgid>   (run list_windows for ids)");
            std::process::exit(2);
        }
    };

    // Capture at 2x the point size (retina assumption is fine for a probe), rounded to even.
    let w = ((entry.info.width as u32 * 2) / 2) * 2;
    let h = ((entry.info.height as u32 * 2) / 2) * 2;
    println!(
        "capturing '{}' ({}) at {}x{}px, 5 seconds...",
        entry.info.title, entry.info.id, w, h
    );

    let count = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let (c2, b2) = (count.clone(), bytes.clone());
    let mut cap = SckCapture::new(entry.info.id, w, h, 30)?;
    cap.start(Box::new(move |frame, _meta| {
        c2.fetch_add(1, Ordering::Relaxed);
        b2.store(frame.data.len() as u64, Ordering::Relaxed);
        // Cheap liveness signal: checksum of the first row.
        if c2.load(Ordering::Relaxed) % 30 == 0 {
            let sum: u64 = frame.data[..frame.width as usize * 4]
                .iter()
                .map(|&b| b as u64)
                .sum();
            println!(
                "frame {}: first-row checksum {}",
                c2.load(Ordering::Relaxed),
                sum
            );
        }
    }))?;

    std::thread::sleep(std::time::Duration::from_secs(5));
    cap.stop();
    let n = count.load(Ordering::Relaxed);
    println!(
        "captured {} frames in 5s (~{} fps), frame size {} bytes",
        n,
        n / 5,
        bytes.load(Ordering::Relaxed)
    );
    if n == 0 {
        anyhow::bail!("no frames captured");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
