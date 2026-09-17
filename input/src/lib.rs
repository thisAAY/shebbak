use srw_core::protocol::{MouseAction, MouseButton, WindowId};
use std::collections::HashMap;

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

    fn mouse_move(&mut self, window_id: WindowId, screen_x: f64, screen_y: f64) -> anyhow::Result<()>;

    fn key(&mut self, window_id: WindowId, key_code: u16, down: bool, flags: u64) -> anyhow::Result<()>;

    /// Client-side mirror focus changed; make this window the host input target.
    fn focus(&mut self, window_id: WindowId) -> anyhow::Result<()>;

    /// Client-driven resize (bidirectional size sync), width/height in host points.
    fn resize_window(&mut self, window_id: WindowId, width: f64, height: f64) -> anyhow::Result<()>;

    fn close_window(&mut self, window_id: WindowId) -> anyhow::Result<()>;

    /// Tracker's live window→pid map (replaces M1's static constructor map).
    fn set_pid_map(&mut self, pids: HashMap<WindowId, i32>);
}

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod macos_pid;
