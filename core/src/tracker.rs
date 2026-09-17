use crate::model::WindowInfo;
use crate::protocol::{HostMessage, WindowId};
use std::collections::{HashMap, HashSet};

/// Anything that can produce the current on-screen window list.
/// The macOS implementation (CGWindowList) lives in the capture crate;
/// tests feed synthetic snapshots.
pub trait WindowSnapshotSource {
    fn snapshot(&mut self) -> Vec<WindowInfo>;
}

/// Polls snapshots and diffs them into protocol events for watched windows.
pub struct WindowTracker {
    last: HashMap<WindowId, WindowInfo>,
    watched: HashSet<WindowId>,
}

impl WindowTracker {
    pub fn new(initial: Vec<WindowInfo>, watched: &[WindowId]) -> Self {
        let watched: HashSet<WindowId> = watched.iter().copied().collect();
        let last = initial
            .into_iter()
            .filter(|w| watched.contains(&w.id))
            .map(|w| (w.id, w))
            .collect();
        Self { last, watched }
    }

    pub fn diff(&mut self, snapshot: Vec<WindowInfo>) -> Vec<HostMessage> {
        let now: HashMap<WindowId, WindowInfo> = snapshot
            .into_iter()
            .filter(|w| self.watched.contains(&w.id))
            .map(|w| (w.id, w))
            .collect();

        let mut events = Vec::new();
        for (id, prev) in &self.last {
            match now.get(id) {
                None => {
                    events.push(HostMessage::WindowClosed { window_id: *id });
                    self.watched.remove(id);
                }
                Some(cur) => {
                    if (cur.width, cur.height) != (prev.width, prev.height) {
                        events.push(HostMessage::WindowResized {
                            window_id: *id,
                            width: cur.width,
                            height: cur.height,
                        });
                    }
                    if cur.title != prev.title {
                        events.push(HostMessage::WindowTitleChanged {
                            window_id: *id,
                            title: cur.title.clone(),
                        });
                    }
                }
            }
        }
        self.last = now;
        events
    }

    pub fn geometry(&self, id: WindowId) -> Option<&WindowInfo> {
        self.last.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HostMessage;

    fn win(id: u32, title: &str, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
        WindowInfo { id, title: title.into(), x, y, width: w, height: h }
    }

    fn tracker() -> WindowTracker {
        WindowTracker::new(
            vec![win(1, "one", 0.0, 0.0, 100.0, 100.0), win(2, "two", 50.0, 50.0, 200.0, 200.0)],
            &[1, 2],
        )
    }

    #[test]
    fn no_change_emits_nothing() {
        let mut t = tracker();
        let events = t.diff(vec![
            win(1, "one", 0.0, 0.0, 100.0, 100.0),
            win(2, "two", 50.0, 50.0, 200.0, 200.0),
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn resize_title_each_emit() {
        let mut t = tracker();
        let events = t.diff(vec![
            win(1, "renamed", 10.0, 0.0, 100.0, 150.0),
            win(2, "two", 50.0, 50.0, 200.0, 200.0),
        ]);
        assert!(events.contains(&HostMessage::WindowResized { window_id: 1, width: 100.0, height: 150.0 }));
        assert!(events.contains(&HostMessage::WindowTitleChanged { window_id: 1, title: "renamed".into() }));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn missing_window_emits_closed_once() {
        let mut t = tracker();
        let events = t.diff(vec![win(2, "two", 50.0, 50.0, 200.0, 200.0)]);
        assert_eq!(events, vec![HostMessage::WindowClosed { window_id: 1 }]);
        // Next diff: window 1 stays gone, no repeat.
        let events = t.diff(vec![win(2, "two", 50.0, 50.0, 200.0, 200.0)]);
        assert!(events.is_empty());
    }

    #[test]
    fn unwatched_windows_are_ignored() {
        let mut t = tracker();
        let events = t.diff(vec![
            win(1, "one", 0.0, 0.0, 100.0, 100.0),
            win(2, "two", 50.0, 50.0, 200.0, 200.0),
            win(99, "noise", 5.0, 5.0, 10.0, 10.0),
        ]);
        assert!(events.is_empty());
        assert!(t.geometry(99).is_none());
    }

    #[test]
    fn geometry_reflects_latest_diff() {
        let mut t = tracker();
        t.diff(vec![
            win(1, "one", 10.0, 20.0, 100.0, 100.0),
            win(2, "two", 50.0, 50.0, 200.0, 200.0),
        ]);
        let g = t.geometry(1).unwrap();
        assert_eq!((g.x, g.y), (10.0, 20.0));
    }
}
