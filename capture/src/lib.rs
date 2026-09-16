use srw_core::pixels::BgraFrame;

/// Platform seam: produce BGRA frames for one window.
pub trait WindowCapture: Send {
    fn start(&mut self, on_frame: Box<dyn Fn(BgraFrame) + Send + Sync>) -> anyhow::Result<()>;
    fn stop(&mut self);
}

#[cfg(target_os = "macos")]
pub mod macos;
