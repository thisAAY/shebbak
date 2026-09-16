use anyhow::Result;
use srw_core::pixels::BgraFrame;
use srw_transport::codec::H264Encoder;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::{info, warn};
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// Per-window pipeline: capture callback → bounded channel (latest-wins) →
/// encoder thread → write_sample. Returns the sender for the capture callback
/// and a shutdown closure.
pub struct Pipeline {
    pub frame_tx: SyncSender<BgraFrame>,
    shutdown_tx: SyncSender<()>,
}

impl Pipeline {
    pub fn start(track: Arc<TrackLocalStaticSample>, rt: Handle, label: String) -> Result<Self> {
        // Depth 2: if the encoder falls behind, drop frames at the door.
        let (frame_tx, frame_rx): (SyncSender<BgraFrame>, Receiver<BgraFrame>) = sync_channel(2);
        let (shutdown_tx, shutdown_rx) = sync_channel::<()>(1);

        std::thread::Builder::new().name(format!("encode-{label}")).spawn(move || {
            let mut encoder = match H264Encoder::new() {
                Ok(e) => e,
                Err(e) => {
                    warn!("{label}: encoder init failed: {e}");
                    return;
                }
            };
            let mut sent: u64 = 0;
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    info!("{label}: pipeline shutdown");
                    return;
                }
                let frame = match frame_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                };
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
        })?;

        Ok(Self { frame_tx, shutdown_tx })
    }

    /// Latest-wins push for the capture callback.
    pub fn push(&self, frame: BgraFrame) {
        match self.frame_tx.try_send(frame) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub fn stop(&self) {
        let _ = self.shutdown_tx.try_send(());
    }
}
