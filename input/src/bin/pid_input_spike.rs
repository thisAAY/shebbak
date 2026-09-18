//! Throwaway M2 spike: can CGEventPostToPid deliver input without activating the app?
//! usage: pid_input_spike <pid> click <screen_x> <screen_y>
//!        pid_input_spike <pid> key <cg_key_code>          (e.g. 0 = 'a')
//!        pid_input_spike <pid> move <screen_x> <screen_y>
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, KeyCode};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse()?;
    let cmd = args[2].as_str();
    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource"))?;
    let _ = KeyCode::COMMAND; // silence unused import on some paths

    match cmd {
        "click" => {
            let x: f64 = args[3].parse()?;
            let y: f64 = args[4].parse()?;
            let p = CGPoint::new(x, y);
            let down = CGEvent::new_mouse_event(
                src.clone(),
                CGEventType::LeftMouseDown,
                p,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("event"))?;
            down.post_to_pid(pid);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let up =
                CGEvent::new_mouse_event(src, CGEventType::LeftMouseUp, p, CGMouseButton::Left)
                    .map_err(|_| anyhow::anyhow!("event"))?;
            up.post_to_pid(pid);
            println!("posted click to pid {pid} at ({x},{y})");
        }
        "key" => {
            let code: u16 = args[3].parse()?;
            let down = CGEvent::new_keyboard_event(src.clone(), code, true)
                .map_err(|_| anyhow::anyhow!("event"))?;
            down.post_to_pid(pid);
            std::thread::sleep(std::time::Duration::from_millis(30));
            let up = CGEvent::new_keyboard_event(src, code, false)
                .map_err(|_| anyhow::anyhow!("event"))?;
            up.post_to_pid(pid);
            println!("posted key {code} to pid {pid}");
        }
        "move" => {
            let x: f64 = args[3].parse()?;
            let y: f64 = args[4].parse()?;
            let mv = CGEvent::new_mouse_event(
                src,
                CGEventType::MouseMoved,
                CGPoint::new(x, y),
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("event"))?;
            mv.post_to_pid(pid);
            println!("posted move to pid {pid} at ({x},{y})");
        }
        _ => {
            eprintln!("usage: pid_input_spike <pid> click|key|move ...");
            std::process::exit(2);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
