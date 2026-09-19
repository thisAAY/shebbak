//! Dock icon control shared by the coordinator (embedded Shebbak SVG) and
//! helpers (host-supplied PNG, badged with the Shebbak mark so a mirror
//! tile is distinguishable from a locally running copy of the same app).

use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSGraphicsContext, NSImage,
};
use objc2_foundation::{NSData, NSDictionary, NSInteger, NSPoint, NSRect, NSSize};

/// Shebbak's own icon (a full-bleed squircle — it needs no backing plate
/// when drawn as a badge). Used full-size by the coordinator and helpers
/// without a host icon, and as the corner badge on mirrored apps' icons.
pub const SHEBBAK_ICON_SVG: &[u8] = include_bytes!("../../assets/shebbak-icon.svg");

/// Badge canvas geometry, in pixels: bottom-right corner, macOS badge
/// convention (top-right belongs to notification badges).
const CANVAS_SIDE: f64 = 256.0;
const BADGE_SIDE: f64 = 100.0;
const BADGE_MARGIN: f64 = 10.0;

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

/// Composite the host app's icon with the Shebbak badge in the bottom-right
/// corner, returning a 256 px PNG. `None` only when the host bytes don't
/// parse as an image (caller falls back to setting them raw / doing
/// nothing). An unparseable or empty `badge_bytes` degrades to a host-only
/// render — the badge is an embedded constant we control, and the host
/// icon must still show if it ever breaks.
pub fn badge_icon(host_bytes: &[u8], badge_bytes: &[u8]) -> Option<Vec<u8>> {
    if host_bytes.is_empty() {
        return None;
    }
    let host_data = NSData::with_bytes(host_bytes);
    let host = NSImage::initWithData(NSImage::alloc(), &host_data)?;
    let badge = if badge_bytes.is_empty() {
        None
    } else {
        let badge_data = NSData::with_bytes(badge_bytes);
        NSImage::initWithData(NSImage::alloc(), &badge_data)
    };

    unsafe {
        let full = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(CANVAS_SIDE, CANVAS_SIDE),
        );
        let mut host_rect = full;
        let host_cg = host.CGImageForProposedRect_context_hints(&mut host_rect, None, None)?;
        // For a vector (SVG) rep the proposed rect guides rasterization; the
        // draw below fixes the final geometry either way.
        let badge_cg = badge.and_then(|b| {
            let mut r = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(BADGE_SIDE, BADGE_SIDE));
            b.CGImageForProposedRect_context_hints(&mut r, None, None)
        });

        let side_px = CANVAS_SIDE as NSInteger;
        let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
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
        let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
        let cg = context.CGContext();
        // Self-allocated rep buffers aren't guaranteed zero-filled; clear
        // before compositing so stale memory can't show through the
        // transparent corners of rounded app icons (same as app_identity).
        objc2_core_graphics::CGContext::clear_rect(Some(&cg), full);
        objc2_core_graphics::CGContext::draw_image(Some(&cg), full, Some(&host_cg));
        if let Some(badge_cg) = badge_cg {
            // CG origin is bottom-left, so y = margin IS the bottom edge.
            let badge_rect = NSRect::new(
                NSPoint::new(CANVAS_SIDE - BADGE_SIDE - BADGE_MARGIN, BADGE_MARGIN),
                NSSize::new(BADGE_SIDE, BADGE_SIDE),
            );
            objc2_core_graphics::CGContext::draw_image(Some(&cg), badge_rect, Some(&badge_cg));
        }
        let data = rep
            .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_icon_bytes() -> Vec<u8> {
        std::fs::read(
            "/System/Library/CoreServices/CoreTypes.bundle/Contents/Resources/GenericApplicationIcon.icns",
        )
        .expect("system generic app icon exists")
    }

    /// (width, height) from the PNG IHDR, after checking the magic bytes.
    fn png_dims(png: &[u8]) -> (u32, u32) {
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "png magic"
        );
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        (w, h)
    }

    #[test]
    fn badge_icon_composites_a_256px_png() {
        let png = badge_icon(&system_icon_bytes(), SHEBBAK_ICON_SVG).expect("composites");
        assert_eq!(png_dims(&png), (256, 256));
    }

    #[test]
    fn badge_actually_changes_the_pixels() {
        // An unparseable badge degrades to a host-only render (the badge is
        // an embedded constant we control; the host icon must still show if
        // it ever breaks) — which doubles as the unbadged baseline here.
        let host = system_icon_bytes();
        let with_badge = badge_icon(&host, SHEBBAK_ICON_SVG).expect("badged");
        let without = badge_icon(&host, &[]).expect("host-only render");
        assert_eq!(png_dims(&without), (256, 256));
        assert_ne!(with_badge, without, "badge must change the output");
    }

    #[test]
    fn badge_icon_rejects_unparseable_host_bytes() {
        assert!(badge_icon(&[1, 2, 3], SHEBBAK_ICON_SVG).is_none());
        assert!(badge_icon(&[], SHEBBAK_ICON_SVG).is_none());
    }
}
