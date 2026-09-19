//! Frame-geometry metadata and the pointer-coordinate mapping derived
//! from it (SCK `includesChildWindows` letterboxing — see the 2026-09-19
//! native-child-window-compositing spec).

/// Per-frame geometry attachments read off an SCK sample buffer, in the
/// units SCK delivered them (see
/// docs/superpowers/plans/2026-09-19-native-compositing-metadata-findings.md).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FrameMeta {
    /// Output frame size in pixels (the configured capture size).
    pub width_px: u32,
    pub height_px: u32,
    /// `SCStreamFrameInfo.contentRect` as (x, y, width, height).
    pub content_rect: Option<(f64, f64, f64, f64)>,
    /// `SCStreamFrameInfo.contentScale`.
    pub content_scale: Option<f64>,
    /// `SCStreamFrameInfo.scaleFactor` (display backing scale).
    pub scale_factor: Option<f64>,
    /// `SCStreamFrameInfo.boundingRect` as (x, y, width, height). Observed
    /// frame-local and identical to `content_rect` on macOS 26 (see the
    /// findings doc) — kept for diagnostics, unused by the mapping math.
    pub bounding_rect: Option<(f64, f64, f64, f64)>,
}
