use crate::protocol::{WindowId, WindowKind};
use std::collections::HashMap;

/// A shared window's identity and geometry in host screen points (top-left origin).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxRole { Window, Sheet, Unknown }

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotWindow {
    pub info: WindowInfo,
    pub pid: i32,
    pub layer: i64,
    pub on_screen: bool,
    pub ax_role: AxRole,
    pub minimized: bool,
}

/// Translate window-local points to host screen points.
pub fn window_local_to_screen(win: &WindowInfo, local_x: f64, local_y: f64) -> (f64, f64) {
    (win.x + local_x, win.y + local_y)
}

/// A `WindowOpened` announcement paired with its track binding key.
///
/// `info.x`/`info.y` are unused (0.0) here — the client's window manager owns
/// mirror placement; `offset` (host points, relative to `parent_id`'s mirror)
/// is what a Sheet/Transient mirror is actually positioned from.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedWindow {
    pub info: WindowInfo,
    pub kind: WindowKind,
    pub parent_id: Option<WindowId>,
    pub offset: (f64, f64),
    pub track_id: String,
}

/// Buffers whichever half arrives first — the `WindowOpened` announcement or the
/// media track — and emits the pair once both are present (spec: track↔window binding).
pub struct TrackBinder<T> {
    announcements: HashMap<String, OpenedWindow>,
    tracks: HashMap<String, T>,
}

impl<T> TrackBinder<T> {
    pub fn new() -> Self {
        Self { announcements: HashMap::new(), tracks: HashMap::new() }
    }

    pub fn on_announcement(&mut self, a: OpenedWindow) -> Option<(OpenedWindow, T)> {
        match self.tracks.remove(&a.track_id) {
            Some(t) => Some((a, t)),
            None => {
                self.announcements.insert(a.track_id.clone(), a);
                None
            }
        }
    }

    pub fn on_track(&mut self, track_id: String, t: T) -> Option<(OpenedWindow, T)> {
        match self.announcements.remove(&track_id) {
            Some(a) => Some((a, t)),
            None => {
                self.tracks.insert(track_id, t);
                None
            }
        }
    }
}

impl<T> Default for TrackBinder<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32) -> WindowInfo {
        WindowInfo { id, title: "t".into(), x: 100.0, y: 50.0, width: 800.0, height: 600.0 }
    }

    #[test]
    fn local_to_screen_offsets_by_window_origin() {
        let w = win(1);
        assert_eq!(window_local_to_screen(&w, 0.0, 0.0), (100.0, 50.0));
        assert_eq!(window_local_to_screen(&w, 10.5, 20.25), (110.5, 70.25));
    }

    #[test]
    fn binder_announcement_then_track() {
        let mut b: TrackBinder<&'static str> = TrackBinder::new();
        let ann = OpenedWindow { info: win(1), kind: WindowKind::Normal, parent_id: None, offset: (0.0, 0.0), track_id: "win-1".into() };
        assert!(b.on_announcement(ann.clone()).is_none());
        let out = b.on_track("win-1".into(), "track").unwrap();
        assert_eq!(out.0, ann);
        assert_eq!(out.1, "track");
    }

    #[test]
    fn binder_track_then_announcement() {
        let mut b: TrackBinder<&'static str> = TrackBinder::new();
        assert!(b.on_track("win-1".into(), "track").is_none());
        let ann = OpenedWindow { info: win(1), kind: WindowKind::Normal, parent_id: None, offset: (0.0, 0.0), track_id: "win-1".into() };
        let out = b.on_announcement(ann.clone()).unwrap();
        assert_eq!(out.0, ann);
        assert_eq!(out.1, "track");
    }

    #[test]
    fn binder_unrelated_ids_do_not_pair() {
        let mut b: TrackBinder<&'static str> = TrackBinder::new();
        assert!(b.on_track("win-1".into(), "track").is_none());
        let ann = OpenedWindow { info: win(2), kind: WindowKind::Normal, parent_id: None, offset: (0.0, 0.0), track_id: "win-2".into() };
        assert!(b.on_announcement(ann).is_none());
    }

    #[test]
    fn binder_pair_is_consumed() {
        let mut b: TrackBinder<&'static str> = TrackBinder::new();
        let ann = OpenedWindow { info: win(1), kind: WindowKind::Normal, parent_id: None, offset: (0.0, 0.0), track_id: "win-1".into() };
        b.on_announcement(ann.clone());
        assert!(b.on_track("win-1".into(), "t1").is_some());
        // Second track with the same id has no pending announcement left.
        assert!(b.on_track("win-1".into(), "t2").is_none());
    }
}
