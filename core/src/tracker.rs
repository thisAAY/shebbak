use crate::classify::classify;
use crate::model::{SnapshotWindow, WindowInfo, WindowKind};
use crate::protocol::WindowId;
use std::collections::{HashMap, HashSet};

pub trait WindowSnapshotSource {
    /// Current windows of interest, front-to-back order.
    fn snapshot(&mut self) -> Vec<SnapshotWindow>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrackerEvent {
    Opened {
        window: SnapshotWindow,
    },
    Closed {
        window_id: WindowId,
    },
    Minimized {
        window_id: WindowId,
    },
    Restored {
        window_id: WindowId,
    },
    Resized {
        window_id: WindowId,
        width: f64,
        height: f64,
    },
    TitleChanged {
        window_id: WindowId,
        title: String,
    },
}

struct Tracked {
    info: WindowInfo,
    pid: i32,
    minimized: bool,
    last_unminimized_size: (f64, f64),
}

pub struct AppTracker {
    pids: HashSet<i32>,
    live: HashMap<WindowId, Tracked>,
}

impl AppTracker {
    pub fn new(pids: &[i32]) -> Self {
        Self {
            pids: pids.iter().copied().collect(),
            live: HashMap::new(),
        }
    }

    pub fn diff(&mut self, snapshot: Vec<SnapshotWindow>) -> Vec<TrackerEvent> {
        let ours: Vec<SnapshotWindow> = snapshot
            .into_iter()
            .filter(|w| self.pids.contains(&w.pid))
            .collect();
        // Only Normal windows are tracked and announced. Sheet/Transient
        // windows composite into their parent window's stream natively
        // (SCK includesChildWindows), so they must never open tracks or
        // emit events — classify() is the recognition seam that keeps a
        // menu or sheet from ever being announced as a Normal window.
        let ours: Vec<SnapshotWindow> = ours
            .into_iter()
            .filter(|w| classify(w.layer, w.ax_role) == WindowKind::Normal)
            .collect();
        let now_ids: HashSet<WindowId> = ours.iter().map(|w| w.info.id).collect();
        let mut events = Vec::new();

        // Closes first.
        let gone: Vec<WindowId> = self
            .live
            .keys()
            .copied()
            .filter(|id| !now_ids.contains(id))
            .collect();
        for id in gone {
            self.live.remove(&id);
            events.push(TrackerEvent::Closed { window_id: id });
        }

        // Opens, in front-to-back snapshot order.
        for w in &ours {
            if self.live.contains_key(&w.info.id) {
                continue;
            }
            events.push(TrackerEvent::Opened { window: w.clone() });
            self.live.insert(
                w.info.id,
                Tracked {
                    info: w.info.clone(),
                    pid: w.pid,
                    minimized: false,
                    last_unminimized_size: (w.info.width, w.info.height),
                },
            );
            if w.minimized {
                events.push(TrackerEvent::Minimized {
                    window_id: w.info.id,
                });
                self.live.get_mut(&w.info.id).unwrap().minimized = true;
            }
        }

        // Updates.
        for w in &ours {
            let Some(t) = self.live.get_mut(&w.info.id) else {
                continue;
            };
            let mut just_restored = false;
            if w.minimized != t.minimized {
                t.minimized = w.minimized;
                if w.minimized {
                    events.push(TrackerEvent::Minimized {
                        window_id: w.info.id,
                    });
                } else {
                    events.push(TrackerEvent::Restored {
                        window_id: w.info.id,
                    });
                    just_restored = true;
                    // Emit catch-up Resized if size changed while minimized
                    if (w.info.width, w.info.height) != t.last_unminimized_size {
                        events.push(TrackerEvent::Resized {
                            window_id: w.info.id,
                            width: w.info.width,
                            height: w.info.height,
                        });
                    }
                }
            }
            // Resized: only while not minimized and not immediately after restore
            // (restore already handled catch-up Resized)
            if !t.minimized
                && !w.minimized
                && !just_restored
                && (w.info.width, w.info.height) != (t.info.width, t.info.height)
            {
                events.push(TrackerEvent::Resized {
                    window_id: w.info.id,
                    width: w.info.width,
                    height: w.info.height,
                });
            }
            // TitleChanged: fires regardless of minimized state or kind
            if w.info.title != t.info.title {
                events.push(TrackerEvent::TitleChanged {
                    window_id: w.info.id,
                    title: w.info.title.clone(),
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

    pub fn geometry(&self, id: WindowId) -> Option<&WindowInfo> {
        self.live.get(&id).map(|t| &t.info)
    }
    pub fn pid_of(&self, id: WindowId) -> Option<i32> {
        self.live.get(&id).map(|t| t.pid)
    }
    pub fn pid_map(&self) -> HashMap<WindowId, i32> {
        self.live.iter().map(|(id, t)| (*id, t.pid)).collect()
    }

    /// Stops tracking `pid` (client unsubscribed the app): removes it from
    /// the shared set and forgets its live windows. Returns the forgotten
    /// window ids so the session can tear down their tracks. No `Closed`
    /// events are emitted — the client side that displayed them is gone.
    pub fn remove_pid(&mut self, pid: i32) -> Vec<WindowId> {
        self.pids.remove(&pid);
        let ids: Vec<WindowId> = self
            .live
            .iter()
            .filter(|(_, t)| t.pid == pid)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.live.remove(id);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AxRole, SnapshotWindow, WindowInfo};

    #[allow(clippy::too_many_arguments)]
    fn snap(
        id: u32,
        pid: i32,
        layer: i64,
        role: AxRole,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        title: &str,
        minimized: bool,
    ) -> SnapshotWindow {
        SnapshotWindow {
            info: WindowInfo {
                id,
                title: title.into(),
                x,
                y,
                width: w,
                height: h,
            },
            pid,
            layer,
            on_screen: !minimized,
            ax_role: role,
            minimized,
        }
    }
    fn normal(id: u32, pid: i32) -> SnapshotWindow {
        snap(
            id,
            pid,
            0,
            AxRole::Window,
            10.0,
            20.0,
            800.0,
            600.0,
            "win",
            false,
        )
    }

    #[test]
    fn new_window_of_shared_pid_opens() {
        let mut t = AppTracker::new(&[100]);
        let ev = t.diff(vec![normal(1, 100)]);
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            TrackerEvent::Opened { window } => assert_eq!(window.info.id, 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unshared_pid_is_ignored() {
        let mut t = AppTracker::new(&[100]);
        assert!(t.diff(vec![normal(1, 999)]).is_empty());
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
        let mut min = normal(1, 100);
        min.minimized = true;
        min.on_screen = false;
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
        let mut w = normal(1, 100);
        w.minimized = true;
        w.on_screen = false;
        let ev = t.diff(vec![w]);
        assert!(matches!(ev[0], TrackerEvent::Opened { .. }));
        assert_eq!(ev[1], TrackerEvent::Minimized { window_id: 1 });
    }

    #[test]
    fn resize_and_title_emit_moves_do_not() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let moved_resized = snap(
            1,
            100,
            0,
            AxRole::Window,
            500.0,
            500.0,
            900.0,
            700.0,
            "renamed",
            false,
        );
        let ev = t.diff(vec![moved_resized]);
        assert_eq!(ev.len(), 2); // resize + title, NO move event
        assert!(ev.contains(&TrackerEvent::Resized {
            window_id: 1,
            width: 900.0,
            height: 700.0
        }));
        assert!(ev.contains(&TrackerEvent::TitleChanged {
            window_id: 1,
            title: "renamed".into()
        }));
    }

    #[test]
    fn pid_map_tracks_live_windows() {
        let mut t = AppTracker::new(&[100, 200]);
        t.diff(vec![normal(1, 100), normal(2, 200)]);
        let map = t.pid_map();
        assert_eq!(map.get(&1), Some(&100));
        assert_eq!(map.get(&2), Some(&200));
        t.diff(vec![normal(2, 200)]);
        assert!(!t.pid_map().contains_key(&1));
    }

    #[test]
    fn geometry_reflects_state() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        assert_eq!(t.geometry(1).unwrap().width, 800.0);
        assert!(t.geometry(99).is_none());
    }

    #[test]
    fn resize_while_minimized_emits_resized_on_restore() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        // Minimize
        let mut min = normal(1, 100);
        min.minimized = true;
        min.on_screen = false;
        t.diff(vec![min.clone()]);
        // Resize while minimized
        min.info.width = 900.0;
        min.info.height = 700.0;
        let ev = t.diff(vec![min.clone()]);
        assert!(ev.is_empty(), "no event while minimized");
        // Restore - should emit Restored + catch-up Resized
        let restored = snap(
            1,
            100,
            0,
            AxRole::Window,
            10.0,
            20.0,
            900.0,
            700.0,
            "win",
            false,
        );
        let ev = t.diff(vec![restored]);
        assert_eq!(ev.len(), 2, "expect Restored + Resized, got {:?}", ev);
        assert_eq!(ev[0], TrackerEvent::Restored { window_id: 1 });
        assert_eq!(
            ev[1],
            TrackerEvent::Resized {
                window_id: 1,
                width: 900.0,
                height: 700.0
            }
        );
        // geometry reflects current state
        assert_eq!(t.geometry(1).unwrap().width, 900.0);
    }

    #[test]
    fn title_change_fires_while_minimized() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        // Minimize and change title
        let mut min = normal(1, 100);
        min.minimized = true;
        min.on_screen = false;
        t.diff(vec![min.clone()]);
        min.info.title = "renamed".into();
        let ev = t.diff(vec![min]);
        assert!(
            ev.contains(&TrackerEvent::TitleChanged {
                window_id: 1,
                title: "renamed".into()
            }),
            "TitleChanged must fire while minimized, got {:?}",
            ev
        );
    }

    #[test]
    fn sheet_and_transient_windows_emit_no_events() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let menu = snap(
            2,
            100,
            101,
            AxRole::Unknown,
            40.0,
            60.0,
            200.0,
            300.0,
            "",
            false,
        );
        let sheet = snap(
            3,
            100,
            0,
            AxRole::Sheet,
            40.0,
            60.0,
            400.0,
            200.0,
            "sheet",
            false,
        );
        // Appearing emits nothing and they are never tracked.
        let ev = t.diff(vec![menu.clone(), sheet.clone(), normal(1, 100)]);
        assert!(ev.is_empty(), "no events for transient/sheet, got {ev:?}");
        assert!(t.geometry(2).is_none());
        assert!(t.geometry(3).is_none());
        assert!(!t.pid_map().contains_key(&2));
        // Disappearing emits nothing either (they were never live).
        let ev = t.diff(vec![normal(1, 100)]);
        assert!(ev.is_empty(), "no Closed for transient/sheet, got {ev:?}");
    }

    #[test]
    fn minimized_normal_window_off_screen_stays_tracked() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        let mut min = normal(1, 100);
        min.minimized = true;
        min.on_screen = false;
        let ev = t.diff(vec![min]);
        assert_eq!(ev, vec![TrackerEvent::Minimized { window_id: 1 }]);
        assert!(
            t.geometry(1).is_some(),
            "minimized normal window must stay tracked"
        );
    }

    #[test]
    fn no_duplicate_resized_when_restore_and_resize_coincide() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        // Minimize
        let mut min = normal(1, 100);
        min.minimized = true;
        min.on_screen = false;
        t.diff(vec![min.clone()]);
        // In ONE diff call: restore with size change
        let restored_resized = snap(
            1,
            100,
            0,
            AxRole::Window,
            10.0,
            20.0,
            900.0,
            700.0,
            "win",
            false,
        );
        let ev = t.diff(vec![restored_resized]);
        assert_eq!(
            ev.len(),
            2,
            "expect exactly [Restored, Resized], got {:?}",
            ev
        );
        assert_eq!(ev[0], TrackerEvent::Restored { window_id: 1 });
        assert_eq!(
            ev[1],
            TrackerEvent::Resized {
                window_id: 1,
                width: 900.0,
                height: 700.0
            }
        );
    }

    #[test]
    fn remove_pid_forgets_its_windows_and_returns_their_ids() {
        let mut t = AppTracker::new(&[100, 200]);
        t.diff(vec![normal(1, 100), normal(2, 100), normal(3, 200)]);
        let mut removed = t.remove_pid(100);
        removed.sort_unstable();
        assert_eq!(removed, vec![1, 2]);
        // The other app is untouched.
        assert!(t.geometry(3).is_some());
        assert!(t.geometry(1).is_none());
    }

    #[test]
    fn removed_pid_windows_emit_no_events_afterwards() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        t.remove_pid(100);
        // The window is still on screen on the host, but the pid is no
        // longer shared: no Opened (re-add), no Closed (already forgotten).
        assert!(t.diff(vec![normal(1, 100)]).is_empty());
        assert!(t.diff(vec![]).is_empty());
    }

    #[test]
    fn remove_unknown_pid_is_a_no_op() {
        let mut t = AppTracker::new(&[100]);
        t.diff(vec![normal(1, 100)]);
        assert!(t.remove_pid(999).is_empty());
        assert!(t.geometry(1).is_some());
    }
}
