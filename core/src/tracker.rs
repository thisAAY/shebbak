use crate::classify::{classify, parent_for, parent_offset};
use crate::model::{SnapshotWindow, WindowInfo};
use crate::protocol::{WindowId, WindowKind};
use std::collections::{HashMap, HashSet};

pub trait WindowSnapshotSource {
    /// Current windows of interest, front-to-back order.
    fn snapshot(&mut self) -> Vec<SnapshotWindow>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrackerEvent {
    Opened { window: SnapshotWindow, kind: WindowKind, parent_id: Option<WindowId>, offset_x: f64, offset_y: f64 },
    Closed { window_id: WindowId },
    Minimized { window_id: WindowId },
    Restored { window_id: WindowId },
    Resized { window_id: WindowId, width: f64, height: f64 },
    TitleChanged { window_id: WindowId, title: String },
}

struct Tracked { info: WindowInfo, pid: i32, kind: WindowKind, minimized: bool, last_unminimized_size: (f64, f64) }

pub struct AppTracker {
    pids: HashSet<i32>,
    live: HashMap<WindowId, Tracked>,
}

impl AppTracker {
    pub fn new(pids: &[i32]) -> Self {
        Self { pids: pids.iter().copied().collect(), live: HashMap::new() }
    }

    pub fn diff(&mut self, snapshot: Vec<SnapshotWindow>) -> Vec<TrackerEvent> {
        let ours: Vec<SnapshotWindow> =
            snapshot.into_iter().filter(|w| self.pids.contains(&w.pid)).collect();
        let now_ids: HashSet<WindowId> = ours.iter().map(|w| w.info.id).collect();
        let mut events = Vec::new();

        // Closes first.
        let gone: Vec<WindowId> = self.live.keys().copied().filter(|id| !now_ids.contains(id)).collect();
        for id in gone {
            self.live.remove(&id);
            events.push(TrackerEvent::Closed { window_id: id });
        }

        // Opens, in front-to-back snapshot order.
        for w in &ours {
            if self.live.contains_key(&w.info.id) { continue; }
            let kind = classify(w.layer, w.ax_role);
            let (parent_id, ox, oy) = if kind == WindowKind::Normal {
                (None, 0.0, 0.0)
            } else {
                match parent_for(w, &ours) {
                    Some(p) => {
                        let (ox, oy) = parent_offset(&p.info, &w.info);
                        (Some(p.info.id), ox, oy)
                    }
                    None => (None, 0.0, 0.0),
                }
            };
            events.push(TrackerEvent::Opened {
                window: w.clone(), kind, parent_id, offset_x: ox, offset_y: oy,
            });
            self.live.insert(w.info.id, Tracked {
                info: w.info.clone(), pid: w.pid, kind, minimized: false,
                last_unminimized_size: (w.info.width, w.info.height),
            });
            if w.minimized {
                events.push(TrackerEvent::Minimized { window_id: w.info.id });
                self.live.get_mut(&w.info.id).unwrap().minimized = true;
            }
        }

        // Updates.
        for w in &ours {
            let Some(t) = self.live.get_mut(&w.info.id) else { continue; };
            if w.minimized != t.minimized {
                t.minimized = w.minimized;
                if w.minimized {
                    events.push(TrackerEvent::Minimized { window_id: w.info.id });
                } else {
                    events.push(TrackerEvent::Restored { window_id: w.info.id });
                    // Emit catch-up Resized if size changed while minimized
                    if (w.info.width, w.info.height) != t.last_unminimized_size && t.kind != WindowKind::Transient {
                        events.push(TrackerEvent::Resized {
                            window_id: w.info.id, width: w.info.width, height: w.info.height,
                        });
                    }
                }
            }
            // Resized: only while not minimized, not for Transient
            if t.kind != WindowKind::Transient && !t.minimized && !w.minimized {
                if (w.info.width, w.info.height) != (t.info.width, t.info.height) {
                    events.push(TrackerEvent::Resized {
                        window_id: w.info.id, width: w.info.width, height: w.info.height,
                    });
                }
            }
            // TitleChanged: fires regardless of minimized state or kind
            if w.info.title != t.info.title {
                events.push(TrackerEvent::TitleChanged {
                    window_id: w.info.id, title: w.info.title.clone(),
                });
            }
            // Update tracked state and last_unminimized_size
            t.info = w.info.clone();
            if !w.minimized {
                t.last_unminimized_size = (w.info.width, w.info.height);
            }
        }

        events
    }

    pub fn geometry(&self, id: WindowId) -> Option<&WindowInfo> { self.live.get(&id).map(|t| &t.info) }
    pub fn pid_of(&self, id: WindowId) -> Option<i32> { self.live.get(&id).map(|t| t.pid) }
    pub fn kind_of(&self, id: WindowId) -> Option<WindowKind> { self.live.get(&id).map(|t| t.kind) }
    pub fn pid_map(&self) -> HashMap<WindowId, i32> {
        self.live.iter().map(|(id, t)| (*id, t.pid)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AxRole, SnapshotWindow, WindowInfo};
    use crate::protocol::WindowKind;

    fn snap(id: u32, pid: i32, layer: i64, role: AxRole, x: f64, y: f64, w: f64, h: f64,
            title: &str, minimized: bool) -> SnapshotWindow {
        SnapshotWindow {
            info: WindowInfo { id, title: title.into(), x, y, width: w, height: h },
            pid, layer, on_screen: !minimized, ax_role: role, minimized,
        }
    }
    fn normal(id: u32, pid: i32) -> SnapshotWindow {
        snap(id, pid, 0, AxRole::Window, 10.0, 20.0, 800.0, 600.0, "win", false)
    }

    #[test]
    fn new_window_of_shared_pid_opens_with_kind() {
        let mut t = AppTracker::new(&[100]);
        let ev = t.diff(vec![normal(1, 100)]);
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            TrackerEvent::Opened { window, kind: WindowKind::Normal, parent_id: None, .. } =>
                assert_eq!(window.info.id, 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unshared_pid_is_ignored() {
        let mut t = AppTracker::new(&[100]);
        assert!(t.diff(vec![normal(1, 999)]).is_empty());
    }

    #[test]
    fn transient_opens_parented_to_frontmost_normal_with_offset() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let menu = snap(2, 100, 101, AxRole::Unknown, 40.0, 60.0, 200.0, 300.0, "", false);
        let ev = t.diff(vec![menu, normal(1, 100)]); // menu frontmost
        match &ev[0] {
            TrackerEvent::Opened { kind: WindowKind::Transient, parent_id: Some(1), offset_x, offset_y, .. } => {
                assert_eq!((*offset_x, *offset_y), (30.0, 40.0)); // 40-10, 60-20
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn gone_window_closes_and_can_reopen() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let ev = t.diff(vec![]);
        assert_eq!(ev, vec![TrackerEvent::Closed { window_id: 1 }]);
        // reappears (e.g. app reopened a doc window with the same CGWindowID)
        let ev = t.diff(vec![normal(1, 100)]);
        assert!(matches!(ev[0], TrackerEvent::Opened { .. }));
    }

    #[test]
    fn minimize_restore_cycle() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let mut min = normal(1, 100); min.minimized = true; min.on_screen = false;
        let ev = t.diff(vec![min.clone()]);
        assert_eq!(ev, vec![TrackerEvent::Minimized { window_id: 1 }]);
        let ev = t.diff(vec![min]); // still minimized: no repeat
        assert!(ev.is_empty());
        let ev = t.diff(vec![normal(1, 100)]);
        assert_eq!(ev, vec![TrackerEvent::Restored { window_id: 1 }]);
    }

    #[test]
    fn born_minimized_opens_then_minimizes() {
        let mut t = AppTracker::new(&[100]);
        let mut w = normal(1, 100); w.minimized = true; w.on_screen = false;
        let ev = t.diff(vec![w]);
        assert!(matches!(ev[0], TrackerEvent::Opened { .. }));
        assert_eq!(ev[1], TrackerEvent::Minimized { window_id: 1 });
    }

    #[test]
    fn resize_and_title_emit_moves_do_not() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let moved_resized = snap(1, 100, 0, AxRole::Window, 500.0, 500.0, 900.0, 700.0, "renamed", false);
        let ev = t.diff(vec![moved_resized]);
        assert_eq!(ev.len(), 2); // resize + title, NO move event
        assert!(ev.contains(&TrackerEvent::Resized { window_id: 1, width: 900.0, height: 700.0 }));
        assert!(ev.contains(&TrackerEvent::TitleChanged { window_id: 1, title: "renamed".into() }));
    }

    #[test]
    fn pid_map_tracks_live_windows() {
        let mut t = AppTracker::new(&[100, 200]);
        t.diff(vec![normal(1, 100), normal(2, 200)]);
        let map = t.pid_map();
        assert_eq!(map.get(&1), Some(&100));
        assert_eq!(map.get(&2), Some(&200));
        t.diff(vec![normal(2, 200)]);
        assert!(t.pid_map().get(&1).is_none());
    }

    #[test]
    fn geometry_and_kind_reflect_state() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        assert_eq!(t.kind_of(1), Some(WindowKind::Normal));
        assert_eq!(t.geometry(1).unwrap().width, 800.0);
        assert_eq!(t.kind_of(99), None);
    }

    #[test]
    fn resize_while_minimized_emits_resized_on_restore() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        // Minimize
        let mut min = normal(1, 100); min.minimized = true; min.on_screen = false;
        t.diff(vec![min.clone()]);
        // Resize while minimized
        min.info.width = 900.0;
        min.info.height = 700.0;
        let ev = t.diff(vec![min.clone()]);
        assert!(ev.is_empty(), "no event while minimized");
        // Restore - should emit Restored + catch-up Resized
        let restored = snap(1, 100, 0, AxRole::Window, 10.0, 20.0, 900.0, 700.0, "win", false);
        let ev = t.diff(vec![restored]);
        assert_eq!(ev.len(), 2, "expect Restored + Resized, got {:?}", ev);
        assert_eq!(ev[0], TrackerEvent::Restored { window_id: 1 });
        assert_eq!(ev[1], TrackerEvent::Resized { window_id: 1, width: 900.0, height: 700.0 });
        // geometry reflects current state
        assert_eq!(t.geometry(1).unwrap().width, 900.0);
    }

    #[test]
    fn title_change_fires_while_minimized_and_for_transients() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        // Minimize and change title
        let mut min = normal(1, 100); min.minimized = true; min.on_screen = false;
        t.diff(vec![min.clone()]);
        min.info.title = "renamed".into();
        let ev = t.diff(vec![min]);
        assert!(ev.contains(&TrackerEvent::TitleChanged { window_id: 1, title: "renamed".into() }),
                "TitleChanged must fire while minimized, got {:?}", ev);
    }

    #[test]
    fn transient_title_change_fires() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let menu = snap(2, 100, 101, AxRole::Unknown, 40.0, 60.0, 200.0, 300.0, "menu", false);
        t.diff(vec![menu, normal(1, 100)]);
        // Transient title change
        let menu_renamed = snap(2, 100, 101, AxRole::Unknown, 40.0, 60.0, 200.0, 300.0, "menu renamed", false);
        let ev = t.diff(vec![menu_renamed, normal(1, 100)]);
        assert!(ev.contains(&TrackerEvent::TitleChanged { window_id: 2, title: "menu renamed".into() }),
                "TitleChanged must fire for transients, got {:?}", ev);
    }
}
