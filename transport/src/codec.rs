use anyhow::{ensure, Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::Encoder;
use openh264::formats::YUVSource;
use srw_core::pixels::{bgra_to_i420, i420_to_bgra, BgraFrame, I420Frame};

/// Adapter: our I420Frame as an openh264 YUV source.
struct I420Source<'a>(&'a I420Frame);

impl YUVSource for I420Source<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.0.width as usize, self.0.height as usize)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.0.width as usize, self.0.width as usize / 2, self.0.width as usize / 2)
    }
    fn y(&self) -> &[u8] {
        &self.0.y
    }
    fn u(&self) -> &[u8] {
        &self.0.u
    }
    fn v(&self) -> &[u8] {
        &self.0.v
    }
}

/// How often (in encoded frames) to force a fresh IDR, independent of
/// openh264's default of one IDR per session. Without this, a decoder that
/// loses reference state to packet loss never recovers: every subsequent
/// access unit fails to decode for the rest of the session. ~10 s at 30fps
/// backstop; PLI-triggered IDR is the primary recovery path as of M2.
const IDR_INTERVAL_FRAMES: u64 = 300;

pub struct H264Encoder {
    inner: Encoder,
    frame_count: u64,
}

impl H264Encoder {
    pub fn new() -> Result<Self> {
        Ok(Self { inner: Encoder::new().context("create openh264 encoder")?, frame_count: 0 })
    }

    /// Force the next encoded frame to be an IDR (PLI response path).
    pub fn force_idr(&mut self) {
        self.inner.force_intra_frame();
    }

    /// Encode one BGRA frame to an Annex-B access unit.
    pub fn encode_bgra(&mut self, frame: &BgraFrame) -> Result<Option<Vec<u8>>> {
        // bgra_to_i420 asserts even dimensions; validate here so odd-sized
        // frames (e.g. from window capture) surface as an error, not a panic.
        ensure!(
            frame.width.is_multiple_of(2) && frame.height.is_multiple_of(2),
            "encode_bgra requires even dimensions, got {}x{}",
            frame.width,
            frame.height
        );
        if self.frame_count.is_multiple_of(IDR_INTERVAL_FRAMES) {
            self.inner.force_intra_frame();
        }
        self.frame_count += 1;
        let i420 = bgra_to_i420(frame);
        let bitstream = self.inner.encode(&I420Source(&i420)).context("encode frame")?;
        let bytes = bitstream.to_vec();
        if bytes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(bytes))
        }
    }
}

pub struct H264Decoder {
    inner: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self { inner: Decoder::new().context("create openh264 decoder")? })
    }

    /// Decode one Annex-B access unit; None until a picture is available.
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<BgraFrame>> {
        match self.inner.decode(annexb).context("decode access unit")? {
            None => Ok(None),
            Some(yuv) => {
                let (w, h) = yuv.dimensions();
                let (ys, us, vs) = yuv.strides();
                Ok(Some(i420_to_bgra(
                    w as u32,
                    h as u32,
                    yuv.y(),
                    yuv.u(),
                    yuv.v(),
                    ys,
                    us,
                    vs,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use srw_core::pixels::BgraFrame;

    fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> BgraFrame {
        let mut data = Vec::new();
        for _ in 0..width * height {
            data.extend_from_slice(&[b, g, r, 255]);
        }
        BgraFrame { width, height, data }
    }

    #[test]
    fn encode_decode_roundtrip_solid_frame() {
        let mut enc = H264Encoder::new().unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let frame = solid(64, 64, 20, 180, 240); // orange-ish

        // Feed several identical frames; encoders may not emit a decodable
        // picture on the very first call, decoders may need SPS/PPS + IDR.
        let mut decoded = None;
        for _ in 0..10 {
            if let Some(au) = enc.encode_bgra(&frame).unwrap() {
                if let Some(out) = dec.decode(&au).unwrap() {
                    decoded = Some(out);
                    break;
                }
            }
        }
        let out = decoded.expect("no decoded frame after 10 encoded frames");
        assert_eq!((out.width, out.height), (64, 64));
        // Average color must be close to the input (lossy codec: tolerance 12).
        let (mut bs, mut gs, mut rs) = (0i64, 0i64, 0i64);
        for px in out.data.chunks(4) {
            bs += px[0] as i64;
            gs += px[1] as i64;
            rs += px[2] as i64;
        }
        let n = (out.width * out.height) as i64;
        assert!((bs / n - 20).abs() <= 12, "b avg {}", bs / n);
        assert!((gs / n - 180).abs() <= 12, "g avg {}", gs / n);
        assert!((rs / n - 240).abs() <= 12, "r avg {}", rs / n);
    }

    #[test]
    fn encode_bgra_rejects_odd_dimensions_instead_of_panicking() {
        let mut enc = H264Encoder::new().unwrap();
        let frame = solid(63, 64, 10, 20, 30);
        let result = enc.encode_bgra(&frame);
        assert!(result.is_err(), "expected Err for odd-width frame, got {:?}", result.is_ok());
    }

    /// Returns true if the Annex-B access unit contains an IDR slice NAL unit
    /// (nal_unit_type == 5), scanning past both 3- and 4-byte start codes.
    fn au_contains_idr(au: &[u8]) -> bool {
        let mut i = 0;
        while i + 3 <= au.len() {
            let start_len = if au[i..].starts_with(&[0, 0, 0, 1]) {
                Some(4)
            } else if au[i..].starts_with(&[0, 0, 1]) {
                Some(3)
            } else {
                None
            };
            if let Some(len) = start_len {
                let nal_byte_idx = i + len;
                if nal_byte_idx < au.len() {
                    let nal_type = au[nal_byte_idx] & 0x1F;
                    if nal_type == 5 {
                        return true;
                    }
                }
                i += len;
            } else {
                i += 1;
            }
        }
        false
    }

    #[test]
    fn force_idr_emits_idr_on_next_frame() {
        let mut enc = H264Encoder::new().unwrap();
        let frame = solid(64, 64, 20, 180, 240);
        // Warm past the initial IDR.
        for _ in 0..5 { enc.encode_bgra(&frame).unwrap(); }
        enc.force_idr();
        let au = enc.encode_bgra(&frame).unwrap().expect("au after force_idr");
        assert!(au_contains_idr(&au), "frame after force_idr() must contain an IDR NAL");
    }

    #[test]
    fn encoder_emits_periodic_idr_for_loss_recovery() {
        // A decoder that loses reference state to packet loss can only recover
        // once a fresh IDR arrives. openh264's default config emits exactly one
        // IDR per session (at start), so a decoder that misses it is stuck
        // forever. Assert the encoder emits more than one IDR across a run
        // long enough to span the periodic interval.
        let mut enc = H264Encoder::new().unwrap();
        let frame = solid(64, 64, 20, 180, 240);

        let mut idr_aus = 0;
        for _ in 0..400 {
            if let Some(au) = enc.encode_bgra(&frame).unwrap() {
                if au_contains_idr(&au) {
                    idr_aus += 1;
                }
            }
        }

        assert!(
            idr_aus >= 2,
            "expected at least 2 access units containing an IDR (initial + periodic), got {}",
            idr_aus
        );
    }
}
