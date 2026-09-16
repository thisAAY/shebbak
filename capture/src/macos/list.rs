use anyhow::Result;
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::window::{
    kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    CGWindowListCopyWindowInfo,
};
use srw_core::model::WindowInfo;
use srw_core::tracker::WindowSnapshotSource;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct WindowListEntry {
    pub info: WindowInfo,
    pub pid: i32,
    pub app_name: String,
}

fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    dict.find(CFString::new(key))
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
}

fn dict_f64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<f64> {
    dict.find(CFString::new(key))
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_f64())
}

fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    dict.find(CFString::new(key))
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

// `CFDictionary<CFString, CFType>` (unlike the void-pointer-keyed variant) isn't a
// `ConcreteCFType`, so `CFType::downcast` can't produce one. The nested
// `kCGWindowBounds` value is always a dictionary per the CGWindowList contract, so
// wrap its raw ref directly instead of downcasting.
fn dict_dict(
    dict: &CFDictionary<CFString, CFType>,
    key: &str,
) -> Option<CFDictionary<CFString, CFType>> {
    dict.find(CFString::new(key))
        .map(|v| unsafe { CFDictionary::wrap_under_get_rule(v.as_CFTypeRef() as CFDictionaryRef) })
}

/// On-screen, layer-0 windows with real bounds, front-to-back order.
pub fn list_windows() -> Result<Vec<WindowListEntry>> {
    let raw = unsafe {
        CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )
    };
    if raw.is_null() {
        anyhow::bail!("CGWindowListCopyWindowInfo returned null (no WindowServer session?)");
    }
    let array: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_create_rule(raw) };

    let mut out = Vec::new();
    for dict in array.iter() {
        let layer = dict_i64(&dict, "kCGWindowLayer").unwrap_or(-1);
        if layer != 0 {
            continue; // skip menu bar, dock, overlays
        }
        let id = match dict_i64(&dict, "kCGWindowNumber") {
            Some(n) => n as u32,
            None => continue,
        };
        let pid = dict_i64(&dict, "kCGWindowOwnerPID").unwrap_or(0) as i32;
        let app_name = dict_string(&dict, "kCGWindowOwnerName").unwrap_or_default();
        let title = dict_string(&dict, "kCGWindowName").unwrap_or_default();
        // Bounds is a nested dictionary {X, Y, Width, Height} in screen points, top-left origin.
        let bounds = match dict_dict(&dict, "kCGWindowBounds") {
            Some(b) => b,
            None => continue,
        };
        let (x, y, w, h) = match (
            dict_f64(&bounds, "X"),
            dict_f64(&bounds, "Y"),
            dict_f64(&bounds, "Width"),
            dict_f64(&bounds, "Height"),
        ) {
            (Some(x), Some(y), Some(w), Some(h)) => (x, y, w, h),
            _ => continue,
        };
        if w < 50.0 || h < 50.0 {
            continue; // skip tiny utility windows
        }
        out.push(WindowListEntry {
            info: WindowInfo { id, title, x, y, width: w, height: h },
            pid,
            app_name,
        });
    }
    Ok(out)
}

/// Real snapshot source for the tracker.
pub struct CgSnapshotSource;

impl WindowSnapshotSource for CgSnapshotSource {
    fn snapshot(&mut self) -> Vec<WindowInfo> {
        match list_windows() {
            Ok(entries) => entries.into_iter().map(|e| e.info).collect(),
            Err(e) => {
                warn!("window snapshot failed: {e}");
                Vec::new()
            }
        }
    }
}
