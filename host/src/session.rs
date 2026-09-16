use anyhow::{Context, Result};
use srw_capture::macos::list::{CgSnapshotSource, WindowListEntry};
use srw_capture::macos::stream::SckCapture;
use srw_capture::WindowCapture;
use srw_core::model::window_local_to_screen;
use srw_core::protocol::{ClientMessage, HostMessage};
use srw_core::tracker::{WindowSnapshotSource, WindowTracker};
use srw_input::macos::AxInput;
use srw_input::InputSink;
use srw_transport::peer::HostPeer;
use srw_transport::signalling::serve_one_offer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

use crate::pipeline::Pipeline;

const SIGNAL_PORT: u16 = 9009;
const TRACKER_HZ: u64 = 10;

/// Serve exactly one client session with the two chosen windows.
/// Returns when the peer disconnects.
pub async fn run_session(windows: &[WindowListEntry; 2], scale: f64) -> Result<()> {
    info!("waiting for a client offer on http://127.0.0.1:{SIGNAL_PORT}/offer ...");
    let (offer, responder) = tokio::task::spawn_blocking(move || serve_one_offer(SIGNAL_PORT))
        .await
        .context("signalling task panicked")??;

    let peer = HostPeer::new().await?;

    // Tracks first, then answer, so the SDP carries them.
    let mut tracks = Vec::new();
    for w in windows.iter() {
        let track_id = format!("win-{}", w.info.id);
        tracks.push(peer.add_track(&track_id).await?);
    }
    let answer = peer.answer(offer).await?;
    tokio::task::spawn_blocking(move || responder.respond(&answer))
        .await
        .context("responder task panicked")??;

    let disconnected = Arc::new(AtomicBool::new(false));
    {
        let d = disconnected.clone();
        peer.on_disconnect(move || d.store(true, Ordering::SeqCst));
    }

    // Tracker + input state.
    let tracker = Arc::new(Mutex::new(WindowTracker::new(
        windows.iter().map(|w| w.info.clone()).collect(),
        &[windows[0].info.id, windows[1].info.id],
    )));
    let pids: HashMap<u32, i32> = windows.iter().map(|w| (w.info.id, w.pid)).collect();
    let input = Arc::new(Mutex::new(AxInput::new(pids)));

    // Client → host messages: input delivery on a blocking thread.
    {
        let tracker = tracker.clone();
        let input = input.clone();
        peer.on_client_message(move |msg| {
            let tracker = tracker.clone();
            let input = input.clone();
            // AX + CGEvent calls block; hop off the webrtc callback thread.
            std::thread::spawn(move || match msg {
                ClientMessage::MouseInput { window_id, x, y, button, action } => {
                    let geo = tracker.lock().unwrap().geometry(window_id).cloned();
                    match geo {
                        Some(win) => {
                            let (sx, sy) = window_local_to_screen(&win, x, y);
                            if let Err(e) =
                                input.lock().unwrap().mouse(window_id, sx, sy, button, action)
                            {
                                warn!("input delivery failed for window {window_id}: {e}");
                            }
                        }
                        None => warn!("input for unknown window {window_id}"),
                    }
                }
                ClientMessage::CloseWindow { window_id } => {
                    if let Err(e) = input.lock().unwrap().close_window(window_id) {
                        warn!("close window {window_id} failed: {e}");
                    }
                }
            });
        });
    }

    // Wait for the data channel to open by retrying the first sends.
    // Announce each window, then start captures.
    let rt = tokio::runtime::Handle::current();
    let mut pipelines: HashMap<u32, (Arc<Pipeline>, Box<dyn WindowCapture>)> = HashMap::new();
    for (i, w) in windows.iter().enumerate() {
        let track_id = format!("win-{}", w.info.id);
        let opened = HostMessage::WindowOpened {
            window_id: w.info.id,
            title: w.info.title.clone(),
            x: w.info.x,
            y: w.info.y,
            width: w.info.width,
            height: w.info.height,
            track_id: track_id.clone(),
        };
        // Retry until the control channel opens (or the peer dies).
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if disconnected.load(Ordering::SeqCst) {
                anyhow::bail!("peer disconnected during setup");
            }
            match peer.send(&opened).await {
                Ok(()) => break,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e).context("data channel never opened"),
            }
        }

        // Capture at pixel size (points × host scale), rounded down to even.
        let px_w = ((w.info.width * scale) as u32) & !1;
        let px_h = ((w.info.height * scale) as u32) & !1;
        let pipeline = Arc::new(Pipeline::start(tracks[i].clone(), rt.clone(), track_id.clone())?);
        let mut capture: Box<dyn WindowCapture> =
            Box::new(SckCapture::new(w.info.id, px_w, px_h, 30)?);
        let push_pipe = pipeline.clone();
        capture.start(Box::new(move |frame| {
            // Latest-wins: drop when the encoder is behind.
            push_pipe.push(frame);
        }))?;
        info!("sharing '{}' ({}) at {}x{}px", w.info.title, w.info.id, px_w, px_h);
        pipelines.insert(w.info.id, (pipeline, capture));
    }

    // Tracker loop: 10 Hz until disconnect.
    let mut source = CgSnapshotSource;
    let mut interval = tokio::time::interval(Duration::from_millis(1000 / TRACKER_HZ));
    while !disconnected.load(Ordering::SeqCst) {
        interval.tick().await;
        let snapshot = tokio::task::block_in_place(|| source.snapshot());
        let events = tracker.lock().unwrap().diff(snapshot);
        for ev in events {
            if let HostMessage::WindowClosed { window_id } = &ev {
                if let Some((pipeline, mut capture)) = pipelines.remove(window_id) {
                    capture.stop();
                    pipeline.stop();
                    info!("window {window_id} closed on host; capture stopped");
                }
            }
            if let Err(e) = peer.send(&ev).await {
                warn!("event send failed: {e}");
            }
        }
    }

    error!("peer disconnected; tearing down session");
    for (_, (pipeline, mut capture)) in pipelines.drain() {
        capture.stop();
        pipeline.stop();
    }
    peer.close().await;
    Ok(())
}
