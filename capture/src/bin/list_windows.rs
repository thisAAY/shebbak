#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    let windows = srw_capture::macos::list::list_windows()?;
    println!(
        "{:<4} {:<8} {:<7} {:<24} {:<40} bounds",
        "idx", "cgid", "pid", "app", "title"
    );
    for (i, w) in windows.iter().enumerate() {
        println!(
            "{:<4} {:<8} {:<7} {:<24} {:<40} ({}, {}) {}x{}",
            i,
            w.info.id,
            w.pid,
            w.app_name,
            w.info.title,
            w.info.x,
            w.info.y,
            w.info.width,
            w.info.height
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
