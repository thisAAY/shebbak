#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use srw_capture::macos::{ax_meta::query_ax_meta, list::snapshot_windows};
    use std::collections::HashSet;
    use std::thread::sleep;
    use std::time::Duration;

    let pid: i32 = std::env::args()
        .nth(1)
        .expect("usage: ax_probe <pid>")
        .parse()
        .expect("pid must be an integer");
    let pids: HashSet<i32> = [pid].into_iter().collect();
    let pid_list = [pid];

    println!("{:<8} {:<6} {:<10} {:<8} {:<9} title", "id", "layer", "on_screen", "role", "minimized");
    loop {
        let mut wins = match snapshot_windows(&pids) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("snapshot failed: {e}");
                sleep(Duration::from_millis(500));
                continue;
            }
        };
        let meta = query_ax_meta(&pid_list);
        for w in &mut wins {
            if let Some(m) = meta.get(&w.info.id) {
                w.ax_role = m.role;
                w.minimized = m.minimized;
            }
        }
        for w in &wins {
            println!(
                "{:<8} {:<6} {:<10} {:<8?} {:<9} {}",
                w.info.id, w.layer, w.on_screen, w.ax_role, w.minimized, w.info.title
            );
        }
        println!("---");
        sleep(Duration::from_millis(500));
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
