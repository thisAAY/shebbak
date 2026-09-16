use anyhow::{anyhow, Context, Result};
use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferExt, CMSampleBufferSCExt};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::{PixelFormat, SCStreamConfiguration};
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;
use srw_core::pixels::BgraFrame;
use std::sync::{Arc, Once};
use tracing::warn;

use crate::WindowCapture;

static INIT_CORE_GRAPHICS: Once = Once::new();

/// Forces CoreGraphics initialization (calls `CGMainDisplayID` under the
/// hood via the crate's Swift bridge). Without this, headless/agent
/// terminal sessions can hit a `CGS_REQUIRE_INIT` crash the first time
/// ScreenCaptureKit touches CoreGraphics state.
fn ensure_core_graphics_initialized() {
    INIT_CORE_GRAPHICS.call_once(|| unsafe {
        screencapturekit::ffi::sc_initialize_core_graphics();
    });
}

struct FrameHandler {
    width: u32,
    height: u32,
    on_frame: Arc<dyn Fn(BgraFrame) + Send + Sync>,
}

impl SCStreamOutputTrait for FrameHandler {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }
        // Idle/blank/suspended/stopped status buffers carry no new pixels;
        // SCK delivers these routinely (e.g. a static window), not errors.
        if let Some(status) = sample_buffer.frame_status() {
            if !status.has_content() {
                return;
            }
        }
        let Some(pixel_buffer) = sample_buffer.pixel_buffer() else {
            return;
        };
        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            warn!("failed to lock pixel buffer");
            return;
        };
        let buf_w = guard.width();
        let buf_h = guard.height();
        if buf_w != self.width as usize || buf_h != self.height as usize {
            // Size mismatch (e.g. during teardown): drop.
            return;
        }
        let bytes_per_row = guard.bytes_per_row();
        // SAFETY: `src` is read and copied entirely within this call, before
        // the guard (and the lock it holds) is dropped at the end of scope.
        let Some(src) = (unsafe { guard.as_slice() }) else {
            warn!("pixel buffer has no accessible base address");
            return;
        };
        // Row-copy to strip stride padding -> tightly packed BGRA.
        let mut data = Vec::with_capacity(buf_w * buf_h * 4);
        for row in 0..buf_h {
            let start = row * bytes_per_row;
            data.extend_from_slice(&src[start..start + buf_w * 4]);
        }
        (self.on_frame)(BgraFrame { width: self.width, height: self.height, data });
    }
}

pub struct SckCapture {
    window_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    stream: Option<SCStream>,
}

impl SckCapture {
    pub fn new(window_id: u32, width_px: u32, height_px: u32, fps: u32) -> Result<Self> {
        anyhow::ensure!(
            width_px % 2 == 0 && height_px % 2 == 0,
            "capture size must be even, got {width_px}x{height_px}"
        );
        ensure_core_graphics_initialized();
        Ok(Self { window_id, width: width_px, height: height_px, fps, stream: None })
    }
}

impl WindowCapture for SckCapture {
    fn start(&mut self, on_frame: Box<dyn Fn(BgraFrame) + Send + Sync>) -> Result<()> {
        let content = SCShareableContent::get()
            .context("SCShareableContent::get (needs Screen Recording permission)")?;
        let window = content
            .windows()
            .into_iter()
            .find(|w| w.window_id() == self.window_id)
            .ok_or_else(|| anyhow!("window {} not in shareable content", self.window_id))?;

        let filter = SCContentFilter::create().with_window(&window).build();
        let config = SCStreamConfiguration::new()
            .with_width(self.width)
            .with_height(self.height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_fps(self.fps);

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            FrameHandler { width: self.width, height: self.height, on_frame: Arc::from(on_frame) },
            SCStreamOutputType::Screen,
        );
        stream.start_capture().context("start_capture")?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            if let Err(e) = stream.stop_capture() {
                warn!("stop_capture: {e}");
            }
        }
    }
}
