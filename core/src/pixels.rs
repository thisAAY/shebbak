#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> BgraFrame {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&[b, g, r, 255]);
        }
        BgraFrame { width, height, data }
    }

    fn assert_roundtrip_close(orig: &BgraFrame) {
        let i420 = bgra_to_i420(orig);
        let w = orig.width as usize;
        let back = i420_to_bgra(
            orig.width, orig.height,
            &i420.y, &i420.u, &i420.v,
            w, w / 2, w / 2,
        );
        for (a, b) in orig.data.chunks(4).zip(back.data.chunks(4)) {
            for c in 0..3 {
                assert!(
                    (a[c] as i32 - b[c] as i32).abs() <= 4,
                    "channel {} diverged: {} vs {}", c, a[c], b[c]
                );
            }
        }
    }

    #[test]
    fn plane_sizes_are_correct() {
        let f = solid(16, 8, 0, 0, 0);
        let i = bgra_to_i420(&f);
        assert_eq!(i.y.len(), 16 * 8);
        assert_eq!(i.u.len(), 8 * 4);
        assert_eq!(i.v.len(), 8 * 4);
    }

    #[test]
    fn solid_colors_roundtrip() {
        for (b, g, r) in [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255), (255, 255, 255), (0, 0, 0), (128, 64, 200)] {
            assert_roundtrip_close(&solid(16, 16, b, g, r));
        }
    }

    #[test]
    fn gradient_roundtrips() {
        let (w, h) = (32u32, 32u32);
        let mut data = Vec::new();
        for yy in 0..h {
            for xx in 0..w {
                data.extend_from_slice(&[(xx * 8) as u8, (yy * 8) as u8, ((xx + yy) * 4) as u8, 255]);
            }
        }
        // Gradient chroma is averaged over 2x2, so only check Y-dominant closeness loosely:
        let orig = BgraFrame { width: w, height: h, data };
        let i420 = bgra_to_i420(&orig);
        let back = i420_to_bgra(w, h, &i420.y, &i420.u, &i420.v, w as usize, w as usize / 2, w as usize / 2);
        // Average absolute error across the image must be small.
        let total: i64 = orig.data.chunks(4).zip(back.data.chunks(4))
            .map(|(a, b)| (0..3).map(|c| (a[c] as i64 - b[c] as i64).abs()).sum::<i64>())
            .sum();
        let avg = total as f64 / (w * h * 3) as f64;
        assert!(avg < 6.0, "average channel error too high: {}", avg);
    }

    #[test]
    fn stride_aware_decode_skips_padding() {
        // 2x2 white image with strided planes (stride 4 for Y, 3 for U/V).
        let y = vec![235, 235, 0, 0, 235, 235, 0, 0];
        let u = vec![128, 0, 0];
        let v = vec![128, 0, 0];
        let back = i420_to_bgra(2, 2, &y, &u, &v, 4, 3, 3);
        for px in back.data.chunks(4) {
            for c in 0..3 {
                assert!(px[c] > 250, "expected near-white, got {:?}", px);
            }
        }
    }
}

/// A tightly packed 8-bit BGRA frame.
#[derive(Debug, Clone)]
pub struct BgraFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// A tightly packed 8-bit RGBA image with **unpremultiplied** alpha.
///
/// Unlike [`BgraFrame`] (which backs the live video path), this is the
/// wire format for one-shot window snapshots (e.g. the transient-blit
/// source): RGBA8, row-major, no padding, alpha not premultiplied so it
/// composites correctly once decoded on the client.
#[derive(Debug, Clone, PartialEq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Tightly packed I420 planes.
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// BT.601 limited-range BGRA → I420. Width and height must be even.
pub fn bgra_to_i420(frame: &BgraFrame) -> I420Frame {
    let w = frame.width as usize;
    let h = frame.height as usize;
    assert!(w % 2 == 0 && h % 2 == 0, "bgra_to_i420 requires even dimensions, got {}x{}", w, h);
    assert_eq!(frame.data.len(), w * h * 4);

    let mut y_plane = vec![0u8; w * h];
    let mut u_plane = vec![0u8; (w / 2) * (h / 2)];
    let mut v_plane = vec![0u8; (w / 2) * (h / 2)];

    for yy in 0..h {
        for xx in 0..w {
            let i = (yy * w + xx) * 4;
            let b = frame.data[i] as i32;
            let g = frame.data[i + 1] as i32;
            let r = frame.data[i + 2] as i32;
            y_plane[yy * w + xx] = clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
    }
    // Chroma: average each 2x2 block in RGB space, then convert.
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut rs, mut gs, mut bs) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((cy * 2 + dy) * w + cx * 2 + dx) * 4;
                    bs += frame.data[i] as i32;
                    gs += frame.data[i + 1] as i32;
                    rs += frame.data[i + 2] as i32;
                }
            }
            let (r, g, b) = (rs / 4, gs / 4, bs / 4);
            u_plane[cy * (w / 2) + cx] = clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
            v_plane[cy * (w / 2) + cx] = clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
        }
    }
    I420Frame { width: frame.width, height: frame.height, y: y_plane, u: u_plane, v: v_plane }
}

/// BT.601 limited-range I420 → BGRA, honoring source plane strides.
pub fn i420_to_bgra(
    width: u32,
    height: u32,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
) -> BgraFrame {
    let w = width as usize;
    let h = height as usize;
    let mut data = vec![0u8; w * h * 4];
    for yy in 0..h {
        for xx in 0..w {
            let c = y[yy * y_stride + xx] as i32 - 16;
            let d = u[(yy / 2) * u_stride + xx / 2] as i32 - 128;
            let e = v[(yy / 2) * v_stride + xx / 2] as i32 - 128;
            let r = clamp_u8((298 * c + 409 * e + 128) >> 8);
            let g = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
            let b = clamp_u8((298 * c + 516 * d + 128) >> 8);
            let i = (yy * w + xx) * 4;
            data[i] = b;
            data[i + 1] = g;
            data[i + 2] = r;
            data[i + 3] = 255;
        }
    }
    BgraFrame { width, height, data }
}
