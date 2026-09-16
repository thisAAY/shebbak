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
use srw_transport::signalling::{serve_one_offer, OfferResponder};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::pipeline::Pipeline;

const SIGNAL_PORT: u16 = 9009;
const TRACKER_HZ: u64 = 10;

type PipelineMap = HashMap<u32, (Arc<Pipeline>, Box<dyn WindowCapture>)>;

/// Work items for the single ordered input-delivery worker (Finding 3): a
/// dedicated thread drains these in arrival order so a mouse Up can never be
/// delivered before its Down, which a spawn-per-message design cannot
/// guarantee (spawned threads race for the input lock).
enum InputWork {
    Message(ClientMessage),
    Shutdown,
}

/// Serve exactly one client session with the two chosen windows.
/// Returns when the peer disconnects.
pub async fn run_session(windows: &[WindowListEntry; 2], scale: f64) -> Result<()> {
    info!("waiting for a client offer on http://127.0.0.1:{SIGNAL_PORT}/offer ...");
    let (offer, responder) = tokio::task::spawn_blocking(move || serve_one_offer(SIGNAL_PORT))
        .await
        .context("signalling task panicked")??;

    let peer = HostPeer::new().await?;

    // Tracker + input state, and the ordered input-delivery worker, all set
    // up before any fallible step so the teardown below is reachable no
    // matter how the session body below ends.
    let tracker = Arc::new(Mutex::new(WindowTracker::new(
        windows.iter().map(|w| w.info.clone()).collect(),
        &[windows[0].info.id, windows[1].info.id],
    )));
    let pids: HashMap<u32, i32> = windows.iter().map(|w| (w.info.id, w.pid)).collect();
    let input = Arc::new(Mutex::new(AxInput::new(pids)));

    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputWork>();
    let input_worker = {
        let tracker = tracker.clone();
        let input = input.clone();
        std::thread::Builder::new().name("input-worker".into()).spawn(move || {
            for work in input_rx {
                match work {
                    InputWork::Message(ClientMessage::MouseInput {
                        window_id,
                        x,
                        y,
                        button,
                        action,
                    }) => {
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
                    InputWork::Message(ClientMessage::CloseWindow { window_id }) => {
                        if let Err(e) = input.lock().unwrap().close_window(window_id) {
                            warn!("close window {window_id} failed: {e}");
                        }
                    }
                    InputWork::Shutdown => break,
                }
            }
            info!("input worker exiting");
        })?
    };
    {
        // Client → host messages: non-blocking hand-off to the ordered worker.
        let input_tx = input_tx.clone();
        peer.on_client_message(move |msg| {
            let _ = input_tx.send(InputWork::Message(msg));
        });
    }

    let mut pipelines: PipelineMap = HashMap::new();
    let outcome =
        run_session_body(&peer, offer, responder, windows, scale, &tracker, &mut pipelines).await;

    // Unconditional teardown (Finding 2): whatever happened above — a clean
    // disconnect or an error partway through setup — stop every pipeline
    // that got started, shut the input worker down in order, and close the
    // peer, before propagating the outcome.
    for (_, (pipeline, mut capture)) in pipelines.drain() {
        capture.stop();
        pipeline.stop();
    }
    let _ = input_tx.send(InputWork::Shutdown);
    if let Err(e) = input_worker.join() {
        warn!("input worker panicked: {e:?}");
    }
    peer.close().await;

    outcome
}

/// The fallible core of a session: negotiate, announce + start captures, then
/// run the tracker loop until disconnect. Every early exit here leaves
/// `pipelines` holding only what was actually started, which the caller
/// always tears down.
async fn run_session_body(
    peer: &HostPeer,
    offer: RTCSessionDescription,
    responder: OfferResponder,
    windows: &[WindowListEntry; 2],
    scale: f64,
    tracker: &Arc<Mutex<WindowTracker>>,
    pipelines: &mut PipelineMap,
) -> Result<()> {
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

    // Wait for the data channel to open by retrying the first sends.
    // Announce each window, then start captures.
    let rt = tokio::runtime::Handle::current();
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
    Ok(())
}
