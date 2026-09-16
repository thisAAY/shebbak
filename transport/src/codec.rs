use anyhow::{Context, Result};
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

pub struct H264Encoder {
    inner: Encoder,
}

impl H264Encoder {
    pub fn new() -> Result<Self> {
        Ok(Self { inner: Encoder::new().context("create openh264 encoder")? })
    }

    /// Encode one BGRA frame to an Annex-B access unit.
    pub fn encode_bgra(&mut self, frame: &BgraFrame) -> Result<Option<Vec<u8>>> {
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
}
