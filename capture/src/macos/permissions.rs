// CGPreflightScreenCaptureAccess / CGRequestScreenCaptureAccess (macOS 10.15+)
// and AXIsProcessTrusted are C functions; declare them directly.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// True if Screen Recording permission is granted. Triggers the system prompt
/// (once per app per boot) when not yet granted.
pub fn check_screen_recording() -> bool {
    unsafe {
        if CGPreflightScreenCaptureAccess() {
            true
        } else {
            CGRequestScreenCaptureAccess()
        }
    }
}

/// True if Accessibility permission is granted. Does not prompt (the prompt
/// API needs a CFDictionary; printing instructions is our M1 UX).
pub fn check_accessibility() -> bool {
    unsafe { AXIsProcessTrusted() }
}
