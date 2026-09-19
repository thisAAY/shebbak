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
use std::sync::{Arc, Mutex, Once};
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
    expected: Arc<Mutex<(u32, u32)>>,
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
        tracing::trace!(
            content_rect = ?sample_buffer.content_rect(),
            content_scale = ?sample_buffer.content_scale(),
            scale_factor = ?sample_buffer.scale_factor(),
            bounding_rect = ?sample_buffer.bounding_rect(),
            "frame metadata"
        );
        let Some(pixel_buffer) = sample_buffer.pixel_buffer() else {
            return;
        };
        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            warn!("failed to lock pixel buffer");
            return;
        };
        let buf_w = guard.width();
        let buf_h = guard.height();
        // Read the current expected size on every frame: a reconfigure() can
        // land between two callbacks, and in-flight frames still arriving at
        // the old size must be dropped, not stretched to the new one.
        let (width, height) = *self.expected.lock().unwrap();
        if buf_w != width as usize || buf_h != height as usize {
            // Size mismatch (e.g. during teardown or a live reconfigure): drop.
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
        (self.on_frame)(BgraFrame {
            width,
            height,
            data,
        });
    }
}

pub struct SckCapture {
    window_id: u32,
    expected: Arc<Mutex<(u32, u32)>>,
    fps: u32,
    stream: Option<SCStream>,
}

impl SckCapture {
    pub fn new(window_id: u32, width_px: u32, height_px: u32, fps: u32) -> Result<Self> {
        anyhow::ensure!(
            width_px.is_multiple_of(2) && height_px.is_multiple_of(2),
            "capture size must be even, got {width_px}x{height_px}"
        );
        ensure_core_graphics_initialized();
        Ok(Self {
            window_id,
            expected: Arc::new(Mutex::new((width_px, height_px))),
            fps,
            stream: None,
        })
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

        let (width, height) = *self.expected.lock().unwrap();
        let filter = SCContentFilter::create().with_window(&window).build();
        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_fps(self.fps)
            // Deliberate, not an OS default: child windows (menus, sheets,
            // popovers) composite into this window's frames (macOS 14.2+).
            .with_includes_child_windows(true);

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            FrameHandler {
                expected: self.expected.clone(),
                on_frame: Arc::from(on_frame),
            },
            SCStreamOutputType::Screen,
        );
        stream.start_capture().context("start_capture")?;
        self.stream = Some(stream);
        Ok(())
    }

    fn reconfigure(&mut self, width_px: u32, height_px: u32) -> Result<()> {
        anyhow::ensure!(
            width_px.is_multiple_of(2) && height_px.is_multiple_of(2),
            "capture size must be even, got {width_px}x{height_px}"
        );
        anyhow::ensure!(
            width_px > 0 && height_px > 0,
            "capture size must be nonzero"
        );
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| anyhow!("capture not started"))?;
        let config = SCStreamConfiguration::new()
            .with_width(width_px)
            .with_height(height_px)
            .with_pixel_format(PixelFormat::BGRA)
            .with_fps(self.fps)
            // Deliberate, not an OS default: child windows (menus, sheets,
            // popovers) composite into this window's frames (macOS 14.2+).
            .with_includes_child_windows(true);
        stream
            .update_configuration(&config)
            .map_err(|e| anyhow!("update_configuration: {e}"))?;
        *self.expected.lock().unwrap() = (width_px, height_px);
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
