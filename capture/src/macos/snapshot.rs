use anyhow::{anyhow, Result};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
    create_image, kCGWindowImageBestResolution, kCGWindowImageBoundsIgnoreFraming,
    kCGWindowListOptionIncludingWindow,
};
use srw_core::pixels::RgbaImage;

/// Snapshot one window at native (retina) resolution with alpha.
///
/// Uses `CGWindowListCreateImage`, which returns BGRA premultiplied pixels;
/// this un-premultiplies them so the resulting [`RgbaImage`] composites
/// correctly once decoded on the client (see [`RgbaImage`] docs).
pub fn snapshot_window_rgba(window_id: u32) -> Result<RgbaImage> {
    // CGRectNull → "tight bounds of the listed windows".
    let null_rect = CGRect::new(
        &CGPoint::new(f64::INFINITY, f64::INFINITY),
        &CGSize::new(0.0, 0.0),
    );
    let image = create_image(
        null_rect,
        kCGWindowListOptionIncludingWindow,
        window_id,
        kCGWindowImageBoundsIgnoreFraming | kCGWindowImageBestResolution,
    )
    .ok_or_else(|| anyhow!("CGWindowListCreateImage returned null for window {window_id}"))?;

    let width = image.width() as u32;
    let height = image.height() as u32;
    let bpr = image.bytes_per_row();
    let data = image.data(); // CFData, BGRA premultiplied, 32bpp little-endian
    let src = data.bytes();
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for row in 0..height as usize {
        let line = &src[row * bpr..row * bpr + width as usize * 4];
        for px in line.as_chunks::<4>().0 {
            let (b, g, r, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3]);
            // Un-premultiply so PNG alpha composites correctly on the client.
            let (r, g, b) = if a == 0 || a == 255 {
                (r as u8, g as u8, b as u8)
            } else {
                (
                    ((r * 255) / a as u32).min(255) as u8,
                    ((g * 255) / a as u32).min(255) as u8,
                    ((b * 255) / a as u32).min(255) as u8,
                )
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Ok(RgbaImage {
        width,
        height,
        data: out,
    })
}

/// RGBA → PNG bytes (what goes over the wire).
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        // Speed over ratio: these are 10 Hz UI snapshots on LAN.
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header()?;
        w.write_image_data(&img.data)?;
    }
    Ok(out)
}
