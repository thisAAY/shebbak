//! Dock icon control shared by the coordinator (embedded Shebbak SVG) and
//! helpers (host-supplied PNG).

use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

/// Set the app's Dock icon from raw image bytes (PNG or SVG — NSImage
/// sniffs the format; SVG needs macOS 11+). No-op off the main thread or
/// if the data doesn't parse (the generic icon stays, per spec).
pub fn set_dock_icon(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    unsafe { app.setApplicationIconImage(Some(&image)) };
}
