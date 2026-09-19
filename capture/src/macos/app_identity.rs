//! Shared-app identity for `HostMessage::AppAnnounced`: localized name and
//! a Dock-sized icon PNG, looked up by pid via NSRunningApplication.

use objc2::AnyThread;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSGraphicsContext, NSImage, NSRunningApplication,
};
use objc2_foundation::{NSDictionary, NSInteger, NSPoint, NSRect, NSSize};

/// Raw-PNG budget: AppAnnounced travels on the control data channel whose
/// message limit is 64 KiB, and base64 inflates by 4/3.
pub const MAX_ICON_BYTES: usize = 45_000;

pub struct AppIdentity {
    pub name: String,
    /// Raw PNG bytes, empty when no usable icon was found.
    pub icon_png: Vec<u8>,
}

pub fn app_identity(pid: i32) -> Option<AppIdentity> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let name = app
        .localizedName()
        .map(|n| n.to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    let icon_png = app
        .icon()
        .and_then(|icon| encode_icon_png(&icon))
        .unwrap_or_default();
    Some(AppIdentity { name, icon_png })
}

/// NSImage → PNG at Dock size. Tries 256 px first (Dock renders at most
/// 128 pt @2x), stepping down if the encoding blows the message budget;
/// None if every attempt fails or is oversized (caller falls back to no
/// icon, and the helper keeps a generic one).
fn encode_icon_png(icon: &NSImage) -> Option<Vec<u8>> {
    for side in [256.0_f64, 128.0, 64.0] {
        if let Some(png) = png_at(icon, side) {
            if png.len() <= MAX_ICON_BYTES {
                return Some(png);
            }
        }
    }
    None
}

fn png_at(icon: &NSImage, side: f64) -> Option<Vec<u8>> {
    unsafe {
        // CGImageForProposedRect hands back the closest backing representation
        // as-is; it does not resample. So take whatever CGImage it offers at
        // full size, then draw it down into a bitmap context sized exactly to
        // the target side ourselves, and encode PNG from that bitmap.
        let mut native_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
        let source_cg = icon.CGImageForProposedRect_context_hints(&mut native_rect, None, None)?;

        let side_px = side as NSInteger;
        let target_rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side_px,
            side_px,
            8,
            4,
            true,
            false,
            objc2_app_kit::NSDeviceRGBColorSpace,
            0,
            0,
        )?;
        let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&target_rep)?;
        let cg_context = context.CGContext();
        let draw_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
        objc2_core_graphics::CGContext::draw_image(Some(&cg_context), draw_rect, Some(&source_cg));

        let data = target_rep
            .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_for_nonexistent_pid_is_none() {
        // Large pid far above pid_max; NSRunningApplication returns nil.
        assert!(app_identity(0x7FFF_FF00).is_none());
    }

    #[test]
    fn png_encode_of_a_real_nsimage_yields_png_magic() {
        // A plain empty NSImage of a nonzero size has no bitmap reps, so
        // build one from the system's generic app icon file, which every
        // macOS install has.
        let data = std::fs::read(
            "/System/Library/CoreServices/CoreTypes.bundle/Contents/Resources/GenericApplicationIcon.icns",
        )
        .expect("system generic app icon exists");
        let nsdata = objc2_foundation::NSData::with_bytes(&data);
        let image = NSImage::initWithData(NSImage::alloc(), &nsdata).expect("icns parses");
        let png = encode_icon_png(&image).expect("encodes");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(
            png.len() <= MAX_ICON_BYTES,
            "png {} bytes over budget",
            png.len()
        );
    }
}
