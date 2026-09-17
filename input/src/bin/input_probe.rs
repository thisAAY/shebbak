#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    // usage: input_probe <cgid> <pid> click <local_x> <local_y> <win_x> <win_y>
    //        input_probe <cgid> <pid> close
    //        input_probe <cgid> <pid> resize <w> <h>
    //        input_probe <cgid> <pid> key <code>
    use srw_core::protocol::{MouseAction, MouseButton};
    use srw_input::macos::AxInput;
    use srw_input::InputSink;
    use std::collections::HashMap;

    let args: Vec<String> = std::env::args().collect();
    let cgid: u32 = args.get(1).and_then(|s| s.parse().ok()).expect("cgid");
    let pid: i32 = args.get(2).and_then(|s| s.parse().ok()).expect("pid");
    let cmd = args.get(3).map(String::as_str).unwrap_or("");

    let mut sink = AxInput::new(HashMap::from([(cgid, pid)]));
    match cmd {
        "click" => {
            let lx: f64 = args[4].parse()?;
            let ly: f64 = args[5].parse()?;
            let wx: f64 = args[6].parse()?;
            let wy: f64 = args[7].parse()?;
            let (sx, sy) = (wx + lx, wy + ly);
            println!("clicking at screen ({sx}, {sy})");
            sink.mouse(cgid, sx, sy, MouseButton::Left, MouseAction::Down)?;
            std::thread::sleep(std::time::Duration::from_millis(50));
            sink.mouse(cgid, sx, sy, MouseButton::Left, MouseAction::Up)?;
        }
        "close" => {
            println!("pressing close button");
            sink.close_window(cgid)?;
        }
        "resize" => {
            let w: f64 = args[4].parse()?;
            let h: f64 = args[5].parse()?;
            println!("resizing to ({w}, {h})");
            sink.resize_window(cgid, w, h)?;
        }
        "key" => {
            let code: u16 = args[4].parse()?;
            println!("focusing then posting key {code}");
            sink.focus(cgid)?;
            sink.key(cgid, code, true, 0)?;
            std::thread::sleep(std::time::Duration::from_millis(30));
            sink.key(cgid, code, false, 0)?;
        }
        _ => {
            eprintln!(
                "usage: input_probe <cgid> <pid> click <lx> <ly> <win_x> <win_y> | close | resize <w> <h> | key <code>"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
