//! Frame-geometry metadata and the pointer-coordinate mapping derived
//! from it (SCK `includesChildWindows` letterboxing — see the 2026-09-19
//! native-child-window-compositing spec).

use crate::model::WindowInfo;

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
    /// `SCStreamFrameInfo.scaleFactor`. Its exact semantics are unverified
    /// beyond the Task-1 gate run, where it read 2.0 and was consistent
    /// with either "output pixels per content point" or "display backing
    /// scale" — the run couldn't separate the two readings (both the main
    /// display's backing scale and the host's `--scale` were 2.0). The
    /// `cs ≈ 1.0` early return in `mapping_for_frame` removes the mapping's
    /// dependence on this field in the common (non-letterboxed) state.
    pub scale_factor: Option<f64>,
    /// `SCStreamFrameInfo.boundingRect` as (x, y, width, height). Observed
    /// frame-local and identical to `content_rect` on macOS 26 (see the
    /// findings doc) — kept for diagnostics, unused by the mapping math.
    pub bounding_rect: Option<(f64, f64, f64, f64)>,
}

/// Scale tolerance for treating two mappings as equal. even_px rounding
/// skews the 1:1 scale by up to ~1/point; real letterboxing changes it by
/// 10%+, so 0.5% cleanly separates noise from signal.
const SCALE_TOL: f64 = 5e-3;
/// Offset tolerance in window points — half a point is invisible for clicking.
const OFFSET_TOL: f64 = 0.5;

/// Affine pointer transform from mirror-local points to window-local
/// points: `window = offset + scale * mirror`. Identity in the 1:1 state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputMapping {
    pub scale_x: f64,
    pub scale_y: f64,
    pub offset_x: f64,
    pub offset_y: f64,
}

impl InputMapping {
    pub const IDENTITY: Self = Self {
        scale_x: 1.0,
        scale_y: 1.0,
        offset_x: 0.0,
        offset_y: 0.0,
    };

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.offset_x + x * self.scale_x,
            self.offset_y + y * self.scale_y,
        )
    }

    pub fn approx_eq(&self, other: &Self) -> bool {
        (self.scale_x - other.scale_x).abs() <= SCALE_TOL
            && (self.scale_y - other.scale_y).abs() <= SCALE_TOL
            && (self.offset_x - other.offset_x).abs() <= OFFSET_TOL
            && (self.offset_y - other.offset_y).abs() <= OFFSET_TOL
    }

    pub fn is_identity(&self) -> bool {
        self.approx_eq(&Self::IDENTITY)
    }
}

/// Whether a newly derived mapping is worth sending to the client: identity
/// is implicit at open (never sent first), and every real change is sent
/// exactly once — subsequent frames producing the same mapping (within
/// tolerance) stay quiet.
pub fn should_send(last: Option<&InputMapping>, next: &InputMapping) -> bool {
    match last {
        Some(prev) => !next.approx_eq(prev),
        None => !next.is_identity(),
    }
}

/// Derive the client-side pointer transform for one frame.
///
/// `win` is the tracked window's rect in host screen points (top-left
/// origin). In the 1:1 state this returns (approximately) the identity;
/// while SCK letterboxes a scaled-down union rect into the fixed output
/// frame, it returns the affine map from mirror-local points to
/// window-local points.
///
/// Unit/orientation assumptions are pinned by the Task-1 gate findings
/// (docs/superpowers/plans/2026-09-19-native-compositing-metadata-findings.md):
/// contentRect is in output-frame points, top-left anchored; contentScale
/// is the union→frame shrink factor; boundingRect is frame-local (it
/// duplicated contentRect in every observed frame) and carries no
/// screen-space union origin.
pub fn mapping_for_frame(meta: &FrameMeta, win: &WindowInfo) -> InputMapping {
    let (Some((cr_x, cr_y, cr_w, _)), Some(cs), Some(sf)) =
        (meta.content_rect, meta.content_scale, meta.scale_factor)
    else {
        return InputMapping::IDENTITY; // metadata absent → assume 1:1
    };
    if cs <= 0.0 || sf <= 0.0 || win.width <= 0.0 || win.height <= 0.0 {
        return InputMapping::IDENTITY; // degenerate metadata → fail open
    }
    if (cs - 1.0).abs() <= SCALE_TOL {
        return InputMapping::IDENTITY; // no letterboxing ⇒ frame is 1:1 with the window
    }
    // Union origin relative to the window origin. The metadata does not
    // carry it (boundingRect is frame-local), so it comes from how macOS
    // anchors sheets: horizontally centered on the window, hanging down
    // from its top edge — centered horizontal overhang, no vertical
    // offset. A popover overflowing one edge asymmetrically would be off
    // by half its overhang while letterboxed (accepted: menus never
    // overflow on Tahoe; sheets are centered).
    let union_w = cr_w / cs;
    let rel_x = -((union_w - win.width).max(0.0)) / 2.0;
    let rel_y = 0.0;
    let frame_w_pts = meta.width_px as f64 / sf;
    let frame_h_pts = meta.height_px as f64 / sf;
    InputMapping {
        scale_x: frame_w_pts / (win.width * cs),
        scale_y: frame_h_pts / (win.height * cs),
        offset_x: rel_x - cr_x / cs,
        offset_y: rel_y - cr_y / cs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowInfo;

    fn win_800x600() -> WindowInfo {
        WindowInfo {
            id: 1,
            title: "w".into(),
            x: 100.0,
            y: 50.0,
            width: 800.0,
            height: 600.0,
        }
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    #[test]
    fn one_to_one_state_is_identity() {
        // Retina 2x, frame exactly window-sized, content fills the frame,
        // bounding rect == window rect.
        let meta = FrameMeta {
            width_px: 1600,
            height_px: 1200,
            content_rect: Some((0.0, 0.0, 800.0, 600.0)),
            content_scale: Some(1.0),
            scale_factor: Some(2.0),
            bounding_rect: Some((0.0, 0.0, 800.0, 600.0)), // frame-local, == content_rect
        };
        let m = mapping_for_frame(&meta, &win_800x600());
        assert!(m.is_identity(), "expected identity, got {m:?}");
        let (x, y) = m.apply(123.0, 456.0);
        close(x, 123.0);
        close(y, 456.0);
    }

    fn win_500x400() -> WindowInfo {
        WindowInfo {
            id: 1,
            title: "w".into(),
            x: 100.0,
            y: 100.0,
            width: 500.0,
            height: 400.0,
        }
    }

    #[test]
    fn letterboxed_sheet_maps_clicks_back_to_window_points() {
        // Print-sheet shape (per the Task-1 gate findings): a 500x400
        // window whose sheet makes the union 800x500 — wider than the
        // window (centered overhang) and hanging below it. SCK shrinks by
        // min(500/800, 400/500) = 0.625: content fills the frame width
        // (500 x 312.5 pts), top-left anchored, bars at the bottom.
        let cs = 0.625;
        let meta = FrameMeta {
            width_px: 1000,
            height_px: 800,
            content_rect: Some((0.0, 0.0, 500.0, 312.5)),
            content_scale: Some(cs),
            scale_factor: Some(2.0),
            bounding_rect: Some((0.0, 0.0, 500.0, 312.5)), // frame-local, == content_rect
        };
        let m = mapping_for_frame(&meta, &win_500x400());
        close(m.scale_x, 1.6); // 500 / (500 * 0.625)
        close(m.scale_y, 1.6);
        close(m.offset_x, -150.0); // union 800 wide, centered: 150 pts overhang each side
        close(m.offset_y, 0.0); // sheet hangs down; union top == window top
        assert!(!m.is_identity());

        // Mirror center-x maps back to window center-x (centered overhang).
        let (wx, _) = m.apply(250.0, 0.0);
        close(wx, 250.0);
        // A point 200 mirror-pts down is 320 union-pts down; union top is
        // the window top, so window-local y = 320.
        let (_, wy) = m.apply(0.0, 200.0);
        close(wy, 320.0);
    }

    #[test]
    fn gate_replica_print_sheet_values() {
        // The exact values logged in the Task-1 gate run (TextEdit,
        // 500x400pt window, print sheet open) — pins the formula to
        // observed reality rather than a constructed example.
        let cs = 0.641_025_662_422_180_2;
        let meta = FrameMeta {
            width_px: 1000,
            height_px: 800,
            content_rect: Some((0.0, 0.0, 500.000_016_689_300_54, 380.128_217_816_352_84)),
            content_scale: Some(cs),
            scale_factor: Some(2.0),
            bounding_rect: Some((0.0, 0.0, 500.000_016_689_300_54, 380.128_217_816_352_84)),
        };
        let m = mapping_for_frame(&meta, &win_500x400());
        let approx = |a: f64, b: f64| assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        approx(m.scale_x, 1.0 / cs); // ~1.56
        approx(m.scale_y, 1.0 / cs);
        approx(m.offset_x, -140.0); // union ~780 wide, centered on the 500-pt window
        approx(m.offset_y, 0.0);
        // Mirror center-x still lands on window center-x.
        let (wx, _) = m.apply(250.0, 0.0);
        approx(wx, 250.0);
    }

    #[test]
    fn missing_or_degenerate_metadata_is_identity() {
        let no_meta = FrameMeta {
            width_px: 1600,
            height_px: 1200,
            ..Default::default()
        };
        assert!(mapping_for_frame(&no_meta, &win_800x600()).is_identity());

        let zero_scale = FrameMeta {
            width_px: 1600,
            height_px: 1200,
            content_rect: Some((0.0, 0.0, 800.0, 600.0)),
            content_scale: Some(0.0),
            scale_factor: Some(2.0),
            bounding_rect: None,
        };
        assert!(mapping_for_frame(&zero_scale, &win_800x600()).is_identity());
    }

    #[test]
    fn even_px_rounding_stays_within_identity_tolerance() {
        // even_px(800.5 * 2) = 1600 px -> 800 frame pts vs 800.5 window pts:
        // a 0.06% scale skew must still read as identity.
        let mut win = win_800x600();
        win.width = 800.5;
        let meta = FrameMeta {
            width_px: 1600,
            height_px: 1200,
            content_rect: Some((0.0, 0.0, 800.0, 600.0)),
            content_scale: Some(1.0),
            scale_factor: Some(2.0),
            bounding_rect: Some((0.0, 0.0, 800.0, 600.0)),
        };
        assert!(mapping_for_frame(&meta, &win).is_identity());
    }

    #[test]
    fn cs_exactly_one_is_identity_even_with_mismatched_size() {
        // cs == 1.0 exactly should hit the early return regardless of what
        // the rest of the metadata says — even a frame/window size that
        // would otherwise produce a non-identity affine (a stale or
        // corrupt sf reading, say) must not leak through.
        let meta = FrameMeta {
            width_px: 3000, // wildly mismatched vs. an 800x600 window at any sf
            height_px: 2000,
            content_rect: Some((0.0, 0.0, 800.0, 600.0)),
            content_scale: Some(1.0),
            scale_factor: Some(2.0),
            bounding_rect: Some((0.0, 0.0, 800.0, 600.0)),
        };
        let m = mapping_for_frame(&meta, &win_800x600());
        assert!(m.is_identity(), "expected identity, got {m:?}");
    }

    #[test]
    fn approx_eq_tolerances() {
        let a = InputMapping::IDENTITY;
        let mut b = InputMapping::IDENTITY;
        b.offset_x = 0.4; // under the 0.5 pt offset tolerance
        assert!(a.approx_eq(&b));
        b.offset_x = 0.6; // over it
        assert!(!a.approx_eq(&b));
        let mut c = InputMapping::IDENTITY;
        c.scale_x = 1.2; // an actual letterbox-sized change
        assert!(!a.approx_eq(&c));
    }

    #[test]
    fn should_send_no_previous_send() {
        assert!(!should_send(None, &InputMapping::IDENTITY));
        let mut letterboxed = InputMapping::IDENTITY;
        letterboxed.scale_x = 1.6;
        assert!(should_send(None, &letterboxed));
    }

    #[test]
    fn should_send_against_previous_send() {
        let mut prev = InputMapping::IDENTITY;
        prev.scale_x = 1.6;
        let mut differs = prev;
        differs.scale_x = 1.8; // beyond SCALE_TOL of prev
        assert!(should_send(Some(&prev), &differs));
        let mut within_tol = prev;
        within_tol.offset_x += 0.1; // under OFFSET_TOL
        assert!(!should_send(Some(&prev), &within_tol));
    }
}
