//! Routes depacketized video access units from the tokio `read_track`
//! tasks to the owning helper's writer channel, and tracks per-track
//! decode progress (reported upstream by helpers) for the PLI stall loop.

use crate::ipc::DownFrame;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// No successful decode in this long ⇒ the track is stalled and the
/// coordinator keeps requesting keyframes (same threshold as the M2
/// single-process client used).
const STALL_AFTER: Duration = Duration::from_millis(1200);

#[derive(Default)]
struct Inner {
    routes: HashMap<String, Sender<DownFrame>>,
    /// Last helper-reported successful decode per track; absent = never.
    progress: HashMap<String, Instant>,
}

pub struct VideoRouter {
    inner: Mutex<Inner>,
}

impl VideoRouter {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn set_route(&self, track_id: &str, tx: Sender<DownFrame>) {
        self.inner
            .lock()
            .unwrap()
            .routes
            .insert(track_id.to_string(), tx);
    }

    pub fn clear_route(&self, track_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.routes.remove(track_id);
        inner.progress.remove(track_id);
    }

    /// Sends the AU to the owning helper. `false` means dropped: no route
    /// yet (window not announced) or the helper's writer is gone. Dropped
    /// AUs are safe — the PLI stall loop forces a fresh IDR once a route
    /// exists.
    pub fn forward(&self, track_id: &str, au: Vec<u8>) -> bool {
        let inner = self.inner.lock().unwrap();
        match inner.routes.get(track_id) {
            Some(tx) => tx
                .send(DownFrame::Video {
                    track_id: track_id.to_string(),
                    au,
                })
                .is_ok(),
            None => false,
        }
    }

    pub fn mark_progress(&self, track_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .progress
            .insert(track_id.to_string(), Instant::now());
    }

    /// Respawn path: force the next `stalled()` poll to be true so the PLI
    /// loop immediately requests an IDR for the fresh decoder.
    pub fn clear_progress(&self, track_id: &str) {
        self.inner.lock().unwrap().progress.remove(track_id);
    }

    pub fn stalled(&self, track_id: &str) -> bool {
        match self.inner.lock().unwrap().progress.get(track_id) {
            None => true,
            Some(t) => t.elapsed() > STALL_AFTER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrouted_aus_are_dropped() {
        let r = VideoRouter::new();
        assert!(!r.forward("win-1", vec![1, 2, 3]));
    }

    #[test]
    fn routed_aus_reach_the_helper_channel() {
        let r = VideoRouter::new();
        let (tx, rx) = std::sync::mpsc::channel();
        r.set_route("win-1", tx);
        assert!(r.forward("win-1", vec![7]));
        match rx.try_recv().unwrap() {
            DownFrame::Video { track_id, au } => {
                assert_eq!(track_id, "win-1");
                assert_eq!(au, vec![7]);
            }
            other => panic!("unexpected {other:?}"),
        }
        r.clear_route("win-1");
        assert!(!r.forward("win-1", vec![8]));
    }

    #[test]
    fn forward_to_a_dead_helper_reports_unrouted() {
        let r = VideoRouter::new();
        let (tx, rx) = std::sync::mpsc::channel();
        r.set_route("win-1", tx);
        drop(rx); // helper writer thread gone
        assert!(!r.forward("win-1", vec![7]));
    }

    #[test]
    fn stall_state_tracks_progress() {
        let r = VideoRouter::new();
        // Nothing decoded yet: stalled (drives the PLI-on-join behavior).
        assert!(r.stalled("win-1"));
        r.mark_progress("win-1");
        assert!(!r.stalled("win-1"));
        r.clear_progress("win-1");
        assert!(r.stalled("win-1"), "cleared progress re-arms the PLI loop");
    }
}
