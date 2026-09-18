use srw_capture::macos::snapshot::{encode_png, snapshot_window_rgba};
use srw_core::blit::chunk_blit;
use srw_core::protocol::WindowId;
use srw_transport::peer::HostPeer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

const BLIT_HZ_INTERVAL: Duration = Duration::from_millis(100); // ~10 Hz

/// Spawns the 10 Hz snapshot→diff→PNG→chunks loop for one transient.
/// Returns the stop flag; the thread exits on stop, on two consecutive
/// snapshot failures (window died), or on send failure.
pub fn start_blit(
    window_id: WindowId,
    peer: Arc<HostPeer>,
    rt: tokio::runtime::Handle,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("blit-{window_id}"))
        .spawn(move || {
            let mut last_rgba: Option<Vec<u8>> = None;
            let mut seq: u32 = 0;
            let mut failures = 0u32;
            while !flag.load(Ordering::Relaxed) {
                let started = std::time::Instant::now();
                match snapshot_window_rgba(window_id) {
                    Ok(img) => {
                        failures = 0;
                        // Resend only when pixels changed — this is what makes hover-highlight track.
                        if last_rgba.as_deref() != Some(img.data.as_slice()) {
                            match encode_png(&img) {
                                Ok(png) => {
                                    seq += 1;
                                    for msg in chunk_blit(window_id, seq, &png) {
                                        if rt.block_on(peer.send(&msg)).is_err() {
                                            info!("blit {window_id}: send failed, stopping");
                                            return;
                                        }
                                    }
                                    debug!("blit {window_id}: seq {seq}, {} bytes", png.len());
                                }
                                Err(e) => warn!("blit {window_id}: png encode: {e}"),
                            }
                            last_rgba = Some(img.data);
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        if failures >= 2 {
                            debug!("blit {window_id}: window gone ({e}); stopping");
                            return; // tracker's Closed event handles the announce
                        }
                    }
                }
                if let Some(rest) = BLIT_HZ_INTERVAL.checked_sub(started.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
        });
    if let Err(e) = spawned {
        warn!("blit {window_id}: failed to spawn blit thread: {e}");
    }
    stop
}
