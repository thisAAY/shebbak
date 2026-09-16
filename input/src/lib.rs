use srw_core::protocol::{MouseAction, MouseButton, WindowId};

/// Platform seam: deliver input to a specific host window.
pub trait InputSink: Send {
    fn mouse(
        &mut self,
        window_id: WindowId,
        screen_x: f64,
        screen_y: f64,
        button: MouseButton,
        action: MouseAction,
    ) -> anyhow::Result<()>;

    fn close_window(&mut self, window_id: WindowId) -> anyhow::Result<()>;
}

#[cfg(target_os = "macos")]
pub mod macos;
