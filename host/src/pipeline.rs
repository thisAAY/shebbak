use anyhow::{Context, Result};
use srw_core::pixels::BgraFrame;
use srw_transport::codec::H264Encoder;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::{info, warn};
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// Single-frame mailbox: push replaces, take blocks with timeout.
///
/// This replaces the M1 `sync_channel(2)` approach, which was not actually
/// latest-wins: with capacity 2, a burst of pushes kept the OLDEST two
/// frames in the channel (a full bounded channel drops the newest arrival),
/// so the encoder could fall behind on stale frames instead of catching up
/// to the latest one.
pub(crate) struct FrameSlot {
    frame: Mutex<Option<BgraFrame>>,
    cv: Condvar,
}

impl FrameSlot {
    pub(crate) fn new() -> Self {
        Self { frame: Mutex::new(None), cv: Condvar::new() }
    }

    /// Replace whatever frame is waiting (if any) and wake one waiter.
    pub(crate) fn push(&self, frame: BgraFrame) {
        *self.frame.lock().unwrap() = Some(frame);
        self.cv.notify_one();
    }

    /// Block up to `timeout` for a frame, returning `None` on timeout.
    /// Always consumes the frame it returns.
    pub(crate) fn take(&self, timeout: Duration) -> Option<BgraFrame> {
        let guard = self.frame.lock().unwrap();
        let (mut guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |f| f.is_none())
            .unwrap();
        guard.take()
    }
}

struct Shared {
    slot: FrameSlot,
    shutdown: AtomicBool,
    idr: AtomicBool,
}

/// Per-window pipeline: capture callback → latest-wins slot → encoder thread
/// → write_sample.
pub struct Pipeline {
    shared: Arc<Shared>,
}

impl Pipeline {
    /// Constructs the encoder BEFORE spawning the thread: an encoder-init
    /// failure is a loud `Err` here, not a silently dead thread and a black
    /// window.
    pub fn start(track: Arc<TrackLocalStaticSample>, rt: Handle, label: String) -> Result<Self> {
        let mut encoder =
            H264Encoder::new().with_context(|| format!("{label}: encoder init"))?;
        let shared = Arc::new(Shared {
            slot: FrameSlot::new(),
            shutdown: AtomicBool::new(false),
            idr: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name(format!("encode-{label}"))
            .spawn(move || {
                let mut sent: u64 = 0;
                while !thread_shared.shutdown.load(Ordering::Relaxed) {
                    let Some(frame) = thread_shared.slot.take(Duration::from_millis(250)) else {
                        continue;
                    };
                    if thread_shared.idr.swap(false, Ordering::Relaxed) {
                        encoder.force_idr();
                    }
                    match encoder.encode_bgra(&frame) {
                        Ok(Some(au)) => {
                            let sample = Sample {
                                data: au.into(),
                                duration: Duration::from_millis(33),
                                ..Default::default()
                            };
                            if let Err(e) = rt.block_on(track.write_sample(&sample)) {
                                warn!("{label}: write_sample: {e}");
                            } else {
                                sent += 1;
                                if sent % 300 == 0 {
                                    info!("{label}: {sent} samples sent");
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => warn!("{label}: encode error (frame dropped): {e}"),
                    }
                }
                info!("{label}: pipeline shutdown");
            })?;
        Ok(Self { shared })
    }

    /// Latest-wins push for the capture callback.
    pub fn push(&self, frame: BgraFrame) {
        self.shared.slot.push(frame);
    }

    /// PLI response: force the next encoded frame to be an IDR.
    pub fn request_idr(&self) {
        self.shared.idr.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8) -> BgraFrame {
        BgraFrame { width: 2, height: 2, data: vec![tag; 16] }
    }

    #[test]
    fn push_twice_take_returns_newest() {
        let slot = FrameSlot::new();
        slot.push(frame(1));
        slot.push(frame(2)); // M1 bug: sync_channel(2) kept the OLDEST two
        let got = slot.take(Duration::from_millis(10)).unwrap();
        assert_eq!(got.data[0], 2);
        assert!(slot.take(Duration::from_millis(10)).is_none()); // consumed
    }

    #[test]
    fn take_times_out_empty() {
        let slot = FrameSlot::new();
        assert!(slot.take(Duration::from_millis(10)).is_none());
    }
}
