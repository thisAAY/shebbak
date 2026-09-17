use anyhow::{anyhow, Result};
use core_graphics::event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use srw_core::protocol::{MouseAction, MouseButton, WindowId};
use std::collections::HashMap;

use crate::macos::{ax_close_window, ax_resize_window};
use crate::InputSink;

pub struct PidInput {
    pids: HashMap<WindowId, i32>,
}

impl PidInput {
    pub fn new(pids: HashMap<WindowId, i32>) -> Self {
        Self { pids }
    }

    fn pid(&self, window_id: WindowId) -> Result<i32> {
        self.pids
            .get(&window_id)
            .copied()
            .ok_or_else(|| anyhow!("no pid known for window {window_id}"))
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("CGEventSource creation failed"))
    }
}

impl InputSink for PidInput {
    fn mouse(
        &mut self,
        window_id: WindowId,
        screen_x: f64,
        screen_y: f64,
        button: MouseButton,
        action: MouseAction,
    ) -> Result<()> {
        let pid = self.pid(window_id)?;
        let (event_type, cg_button) = match (button, action) {
            (MouseButton::Left, MouseAction::Down) => {
                (CGEventType::LeftMouseDown, CGMouseButton::Left)
            }
            (MouseButton::Left, MouseAction::Up) => {
                (CGEventType::LeftMouseUp, CGMouseButton::Left)
            }
            (MouseButton::Right, MouseAction::Down) => {
                (CGEventType::RightMouseDown, CGMouseButton::Right)
            }
            (MouseButton::Right, MouseAction::Up) => {
                (CGEventType::RightMouseUp, CGMouseButton::Right)
            }
        };
        let ev = CGEvent::new_mouse_event(
            Self::source()?,
            event_type,
            CGPoint::new(screen_x, screen_y),
            cg_button,
        )
        .map_err(|_| anyhow!("CGEvent creation failed"))?;
        ev.post_to_pid(pid);
        Ok(())
    }

    fn mouse_move(&mut self, window_id: WindowId, screen_x: f64, screen_y: f64) -> Result<()> {
        let pid = self.pid(window_id)?;
        let ev = CGEvent::new_mouse_event(
            Self::source()?,
            CGEventType::MouseMoved,
            CGPoint::new(screen_x, screen_y),
            CGMouseButton::Left,
        )
        .map_err(|_| anyhow!("CGEvent creation failed"))?;
        ev.post_to_pid(pid);
        Ok(())
    }

    fn key(&mut self, window_id: WindowId, key_code: u16, down: bool, flags: u64) -> Result<()> {
        let pid = self.pid(window_id)?;
        let ev = CGEvent::new_keyboard_event(Self::source()?, key_code, down)
            .map_err(|_| anyhow!("CGEvent creation failed"))?;
        ev.set_flags(CGEventFlags::from_bits_truncate(flags));
        ev.post_to_pid(pid);
        Ok(())
    }

    fn focus(&mut self, _window_id: WindowId) -> Result<()> {
        Ok(()) // pid targeting needs no host-side focus; events carry their target
    }

    fn resize_window(&mut self, window_id: WindowId, width: f64, height: f64) -> Result<()> {
        ax_resize_window(&self.pids, window_id, width, height)
    }

    fn close_window(&mut self, window_id: WindowId) -> Result<()> {
        ax_close_window(&self.pids, window_id)
    }

    fn set_pid_map(&mut self, pids: HashMap<WindowId, i32>) {
        self.pids = pids;
    }
}
