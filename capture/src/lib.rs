use srw_core::mapping::FrameMeta;
use srw_core::pixels::BgraFrame;

/// Platform seam: produce BGRA frames (plus per-frame geometry metadata)
/// for one window.
pub trait WindowCapture: Send {
    fn start(
        &mut self,
        on_frame: Box<dyn Fn(BgraFrame, FrameMeta) + Send + Sync>,
    ) -> anyhow::Result<()>;
    /// Change output dimensions of a running capture (SCStream.updateConfiguration).
    fn reconfigure(&mut self, width_px: u32, height_px: u32) -> anyhow::Result<()>;
    fn stop(&mut self);
}

#[cfg(target_os = "macos")]
pub mod macos;
