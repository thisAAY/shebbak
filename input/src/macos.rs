use anyhow::{anyhow, bail, Result};
use accessibility_sys::{
    kAXCloseButtonAttribute, kAXErrorSuccess, kAXFrontmostAttribute, kAXPressAction,
    kAXRaiseAction, kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementPerformAction, AXUIElementRef,
    AXUIElementSetAttributeValue,
};
use core_foundation::array::CFArray;
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use srw_core::protocol::{MouseAction, MouseButton, WindowId};
use std::collections::HashMap;
use std::thread::sleep;
use std::time::Duration;

use crate::InputSink;

// Private but ubiquitous (used by yabai, Hammerspoon, etc.): CGWindowID of an AX window.
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, out: *mut u32) -> AXError;
}

pub struct AxInput {
    pids: HashMap<WindowId, i32>,
}

impl AxInput {
    pub fn new(pids: HashMap<WindowId, i32>) -> Self {
        Self { pids }
    }

    /// Find the AXUIElement for a CGWindowID within its owning app.
    /// Returns (app, window) AX elements, both retained — caller must CFRelease both.
    fn ax_window(&self, window_id: WindowId) -> Result<(AXUIElementRef, AXUIElementRef)> {
        let pid = *self
            .pids
            .get(&window_id)
            .ok_or_else(|| anyhow!("no pid known for window {window_id}"))?;
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                bail!("AXUIElementCreateApplication failed for pid {pid}");
            }
            let mut windows_ref: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(
                app,
                CFString::from_static_string(kAXWindowsAttribute).as_concrete_TypeRef(),
                &mut windows_ref,
            );
            if err != kAXErrorSuccess {
                CFRelease(app as CFTypeRef);
                bail!("AXWindows copy failed (err {err}) — Accessibility permission?");
            }
            let windows: CFArray<*const std::ffi::c_void> =
                CFArray::wrap_under_create_rule(windows_ref as _);
            for i in 0..windows.len() {
                let el = *windows.get(i).unwrap() as AXUIElementRef;
                let mut cgid: u32 = 0;
                if _AXUIElementGetWindow(el, &mut cgid) == kAXErrorSuccess && cgid == window_id {
                    // Retain el beyond `windows`' lifetime — `windows` releases its
                    // elements when dropped at the end of this scope.
                    CFRetain(el as CFTypeRef);
                    return Ok((app, el));
                }
            }
            CFRelease(app as CFTypeRef);
            bail!("no AX window matching CGWindowID {window_id} in pid {pid}");
        }
    }

    fn raise(&self, window_id: WindowId) -> Result<()> {
        unsafe {
            let (app, win) = self.ax_window(window_id)?;
            // Make the app frontmost, then raise the specific window.
            let err = AXUIElementSetAttributeValue(
                app,
                CFString::from_static_string(kAXFrontmostAttribute).as_concrete_TypeRef(),
                CFBoolean::true_value().as_concrete_TypeRef() as CFTypeRef,
            );
            if err != kAXErrorSuccess {
                tracing::warn!("set frontmost failed: {err}");
            }
            let err = AXUIElementPerformAction(
                win,
                CFString::from_static_string(kAXRaiseAction).as_concrete_TypeRef(),
            );
            CFRelease(win as CFTypeRef);
            CFRelease(app as CFTypeRef);
            if err != kAXErrorSuccess {
                bail!("AXRaise failed (err {err})");
            }
        }
        // Give WindowServer a beat to complete the focus change before the click lands.
        sleep(Duration::from_millis(60));
        Ok(())
    }
}

impl InputSink for AxInput {
    fn mouse(
        &mut self,
        window_id: WindowId,
        screen_x: f64,
        screen_y: f64,
        button: MouseButton,
        action: MouseAction,
    ) -> Result<()> {
        // Focus-then-click: only raise on Down (Up follows a Down that already raised).
        if action == MouseAction::Down {
            if let Err(e) = self.raise(window_id) {
                tracing::error!("raise failed for window {window_id}: {e:#}");
                return Err(e);
            }
        }
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
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("CGEventSource creation failed"))?;
        let event = CGEvent::new_mouse_event(
            source,
            event_type,
            CGPoint::new(screen_x, screen_y),
            cg_button,
        )
        .map_err(|_| anyhow!("CGEvent creation failed"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn close_window(&mut self, window_id: WindowId) -> Result<()> {
        unsafe {
            let (app, win) = self.ax_window(window_id)?;
            let mut btn_ref: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(
                win,
                CFString::from_static_string(kAXCloseButtonAttribute).as_concrete_TypeRef(),
                &mut btn_ref,
            );
            if err != kAXErrorSuccess || btn_ref.is_null() {
                CFRelease(win as CFTypeRef);
                CFRelease(app as CFTypeRef);
                bail!("close button lookup failed (err {err})");
            }
            let err = AXUIElementPerformAction(
                btn_ref as AXUIElementRef,
                CFString::from_static_string(kAXPressAction).as_concrete_TypeRef(),
            );
            CFRelease(btn_ref);
            CFRelease(win as CFTypeRef);
            CFRelease(app as CFTypeRef);
            if err != kAXErrorSuccess {
                bail!("AXPress on close button failed (err {err})");
            }
        }
        Ok(())
    }
}
