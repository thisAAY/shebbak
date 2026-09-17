use accessibility_sys::{
    kAXChildrenAttribute, kAXErrorSuccess, kAXMinimizedAttribute, kAXRoleAttribute,
    kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementRef,
};
use core_foundation::array::CFArray;
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use srw_core::model::AxRole;
use std::collections::HashMap;

// Private but ubiquitous (used by yabai, Hammerspoon, etc.): CGWindowID of an AX window.
// Mirrors the redeclaration in input/src/macos.rs — see that file for the rationale.
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, out: *mut u32) -> AXError;
}

pub struct AxWindowMeta {
    pub role: AxRole,
    pub minimized: bool,
}

/// Copy an AX attribute value. Returns a `CFTypeRef` owned per the create rule —
/// caller must wrap it in the appropriate CF type (which then owns the release).
fn copy_attr(el: AXUIElementRef, name: &'static str) -> Option<CFTypeRef> {
    let mut out: CFTypeRef = std::ptr::null();
    let err = unsafe {
        AXUIElementCopyAttributeValue(
            el,
            CFString::from_static_string(name).as_concrete_TypeRef(),
            &mut out,
        )
    };
    if err == kAXErrorSuccess && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

fn cgid_of(el: AXUIElementRef) -> Option<u32> {
    let mut id: u32 = 0;
    (unsafe { _AXUIElementGetWindow(el, &mut id) } == kAXErrorSuccess).then_some(id)
}

/// CGWindowID -> AX metadata for every AX-visible window (and attached sheet) of `pids`.
pub fn query_ax_meta(pids: &[i32]) -> HashMap<u32, AxWindowMeta> {
    let mut out = HashMap::new();
    for &pid in pids {
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                continue;
            }
            if let Some(windows_ref) = copy_attr(app, kAXWindowsAttribute) {
                let windows: CFArray<*const std::ffi::c_void> =
                    CFArray::wrap_under_create_rule(windows_ref as _);
                for i in 0..windows.len() {
                    let win = *windows.get(i).unwrap() as AXUIElementRef;
                    if let Some(id) = cgid_of(win) {
                        let minimized = copy_attr(win, kAXMinimizedAttribute)
                            .map(|v| {
                                // wrap_under_create_rule takes ownership of `v`'s +1 ref.
                                let b = CFBoolean::wrap_under_create_rule(v as _);
                                bool::from(b)
                            })
                            .unwrap_or(false);
                        out.insert(id, AxWindowMeta { role: AxRole::Window, minimized });
                    }
                    // Attached sheets are AXSheet children of the window, not AXWindows entries.
                    if let Some(children_ref) = copy_attr(win, kAXChildrenAttribute) {
                        let children: CFArray<*const std::ffi::c_void> =
                            CFArray::wrap_under_create_rule(children_ref as _);
                        for j in 0..children.len() {
                            let child = *children.get(j).unwrap() as AXUIElementRef;
                            let is_sheet = copy_attr(child, kAXRoleAttribute)
                                .map(|v| {
                                    let s = CFString::wrap_under_create_rule(v as _);
                                    s == CFString::from_static_string("AXSheet")
                                })
                                .unwrap_or(false);
                            if is_sheet {
                                if let Some(id) = cgid_of(child) {
                                    out.insert(
                                        id,
                                        AxWindowMeta { role: AxRole::Sheet, minimized: false },
                                    );
                                }
                            }
                        }
                        // `children` drops here, releasing each element (including non-sheet
                        // siblings we didn't otherwise retain) per the create rule.
                    }
                }
                // `windows` drops here, releasing each AXUIElementRef per the create rule.
            }
            CFRelease(app as CFTypeRef);
        }
    }
    out
}
