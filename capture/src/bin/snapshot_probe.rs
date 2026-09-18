#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use srw_capture::macos::snapshot::{encode_png, snapshot_window_rgba};
    use std::time::Instant;

    let mut args = std::env::args().skip(1);
    let window_id: u32 = args
        .next()
        .expect("usage: snapshot_probe <window_id> [out.png]")
        .parse()
        .expect("window_id must be an integer");
    let out_path = args
        .next()
        .unwrap_or_else(|| "snapshot_probe_last.png".to_string());

    println!(
        "{:<4} {:>9} {:>10} {:>12} alpha",
        "iter", "ms", "png_bytes", "wxh"
    );

    let mut last_png: Option<Vec<u8>> = None;
    for i in 0..20 {
        let t0 = Instant::now();
        let img = snapshot_window_rgba(window_id)?;
        let png_bytes = encode_png(&img)?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let mut has_black_pixel = false;
        let mut has_nonblack_pixel = false;
        let mut has_partial_alpha = false;
        let mut min_alpha = 255u8;
        let mut max_alpha = 0u8;
        for px in img.data.as_chunks::<4>().0 {
            let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
            if r == 0 && g == 0 && b == 0 {
                has_black_pixel = true;
            } else {
                has_nonblack_pixel = true;
            }
            if a > 0 && a < 255 {
                has_partial_alpha = true;
            }
            min_alpha = min_alpha.min(a);
            max_alpha = max_alpha.max(a);
        }
        let _ = has_black_pixel;

        println!(
            "{:<4} {:>9.2} {:>10} {:>12} nonblack={} partial_alpha={} alpha_range=[{},{}]",
            i,
            elapsed_ms,
            png_bytes.len(),
            format!("{}x{}", img.width, img.height),
            has_nonblack_pixel,
            has_partial_alpha,
            min_alpha,
            max_alpha,
        );

        last_png = Some(png_bytes);
    }

    if let Some(png_bytes) = last_png {
        std::fs::write(&out_path, &png_bytes)?;
        println!("wrote last PNG to {out_path}");
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
