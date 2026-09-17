use anyhow::{Context, Result};
use srw_capture::macos::ax_watch::AxWatcher;
use srw_capture::macos::list::PidSnapshotSource;
use srw_capture::macos::stream::SckCapture;
use srw_capture::WindowCapture;
use srw_core::model::SnapshotWindow;
use srw_core::protocol::{HostMessage, WindowId, WindowKind};
use srw_core::tracker::{AppTracker, TrackerEvent, WindowSnapshotSource};
use srw_transport::peer::HostPeer;
use srw_transport::signalling::serve_one_offer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

use crate::pipeline::Pipeline;

const SIGNAL_PORT: u16 = 9009;
const RECONCILE_MS: u64 = 500;

/// Per-window runtime state. `Track` windows (Normal/Sheet) get a media track
/// + encoder pipeline + capture; `Transient` windows are announced but their
/// pixels ride the control channel as blits (Task 15 fills in `stop`, the
/// blit loop's shutdown flag — this task only ever inserts `None`).
enum WindowRuntime {
    Track { pipeline: Arc<Pipeline>, capture: Option<Box<dyn WindowCapture>>, track_id: String },
    Blit { stop: Option<Arc<AtomicBool>> },
}

type RuntimeMap = HashMap<WindowId, WindowRuntime>;

/// A track_id-keyed index of live pipelines, shared with the `on_pli` hook so
/// a PLI on any track can find its encoder without walking `RuntimeMap`
/// (whose key is `WindowId`, not `track_id`).
type PliPipelineMap = Arc<Mutex<HashMap<String, Arc<Pipeline>>>>;

/// Points -> even pixel dimension at the host's display scale. SCStream
/// requires even width/height; `.max(2)` keeps a degenerate 0/1px window from
/// producing an invalid (zero) capture size.
fn even_px(points: f64, scale: f64) -> u32 {
    (((points * scale) as u32) & !1).max(2)
}

/// Best-effort LAN-facing IPv4 address for the printed connection hint. Binds
/// a UDP socket toward a public address and reads back the local address the
/// kernel picked for that route — no packet is actually sent (UDP `connect`
/// only selects a route), so this works even without connectivity as long as
/// a default route exists.
fn local_lan_ip() -> Option<std::net::IpAddr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

fn teardown_runtime(rt: WindowRuntime) {
    match rt {
        WindowRuntime::Track { pipeline, capture, .. } => {
            if let Some(mut c) = capture {
                c.stop();
            }
            pipeline.stop();
        }
        WindowRuntime::Blit { stop } => {
            if let Some(flag) = stop {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Adds a track, starts its encoder pipeline, and starts a capture pushing
/// into it; wires the result into `runtimes` and `pli_pipelines`. On any
/// failure after the pipeline is up, the pipeline is stopped before the error
/// is returned — otherwise its encoder thread (parked on the frame slot)
/// would never see a shutdown signal and would leak for the life of the
/// process (nothing else would hold the `Arc<Pipeline>` to stop it later).
async fn open_track_window(
    peer: &HostPeer,
    rt: &tokio::runtime::Handle,
    scale: f64,
    window: &SnapshotWindow,
    track_id: &str,
    runtimes: &mut RuntimeMap,
    pli_pipelines: &PliPipelineMap,
) -> Result<()> {
    let track = peer.add_track(track_id).await?;
    let pipeline = Arc::new(Pipeline::start(track, rt.clone(), track_id.to_string())?);

    let (pw, ph) = (even_px(window.info.width, scale), even_px(window.info.height, scale));
    let mut capture: Box<dyn WindowCapture> = match SckCapture::new(window.info.id, pw, ph, 30) {
        Ok(c) => Box::new(c),
        Err(e) => {
            pipeline.stop();
            return Err(e);
        }
    };
    let push_pipe = pipeline.clone();
    if let Err(e) = capture.start(Box::new(move |frame| push_pipe.push(frame))) {
        pipeline.stop();
        return Err(e);
    }

    info!(
        "sharing window {} ('{}') at {pw}x{ph}px on {track_id}",
        window.info.id, window.info.title
    );
    pli_pipelines.lock().unwrap().insert(track_id.to_string(), pipeline.clone());
    runtimes.insert(
        window.info.id,
        WindowRuntime::Track { pipeline, capture: Some(capture), track_id: track_id.to_string() },
    );
    Ok(())
}

/// Restarts capture for a window that just came back from minimized, on the
/// SAME pipeline/track it already had (minimize only tore down the capture,
/// not the track — see the `Minimized` reconciler arm). A no-op if the window
/// isn't a live `Track` runtime, already has a capture, or its geometry is no
/// longer known to the tracker (closed out from under us).
fn restart_capture(window_id: WindowId, scale: f64, tracker: &Arc<Mutex<AppTracker>>, runtimes: &mut RuntimeMap) {
    let Some(WindowRuntime::Track { pipeline, capture, .. }) = runtimes.get_mut(&window_id) else {
        return;
    };
    if capture.is_some() {
        return; // already capturing; nothing to restart
    }
    let Some(info) = tracker.lock().unwrap().geometry(window_id).cloned() else {
        warn!("restore window {window_id}: no tracker geometry, leaving capture stopped");
        return;
    };
    let (pw, ph) = (even_px(info.width, scale), even_px(info.height, scale));
    let mut new_capture: Box<dyn WindowCapture> = match SckCapture::new(window_id, pw, ph, 30) {
        Ok(c) => Box::new(c),
        Err(e) => {
            warn!("restore window {window_id}: SckCapture::new failed: {e:#}");
            return;
        }
    };
    let push_pipe = pipeline.clone();
    if let Err(e) = new_capture.start(Box::new(move |frame| push_pipe.push(frame))) {
        warn!("restore window {window_id}: capture start failed: {e:#}");
        return;
    }
    *capture = Some(new_capture);
}

/// Serve exactly one client session, sharing every window of `pids` for as
/// long as the client stays connected. Windows may open/close, minimize/
/// restore, resize, and rename at runtime; tracks renegotiate accordingly.
/// Returns when the peer disconnects.
pub async fn run_session(pids: &[i32], scale: f64) -> Result<()> {
    let hint = local_lan_ip()
        .map(|ip| format!(" (LAN: http://{ip}:{SIGNAL_PORT}/offer — client sets SRW_HOST)"))
        .unwrap_or_default();
    info!("waiting for a client offer on http://127.0.0.1:{SIGNAL_PORT}/offer ...{hint}");
    let (offer, responder) = tokio::task::spawn_blocking(move || serve_one_offer(SIGNAL_PORT))
        .await
        .context("signalling task panicked")??;

    let peer = Arc::new(HostPeer::new().await?);

    // Backlog fix: disconnect hook BEFORE answering, so no state change is missed.
    let disconnected = Arc::new(AtomicBool::new(false));
    {
        let d = disconnected.clone();
        peer.on_disconnect(move || d.store(true, Ordering::SeqCst));
    }

    // Zero tracks at answer time — tracks arrive later via renegotiation, as
    // windows are discovered.
    let answer = peer.answer(offer).await?;
    tokio::task::spawn_blocking(move || responder.respond(&answer))
        .await
        .context("responder task panicked")??;

    let tracker = Arc::new(Mutex::new(AppTracker::new(pids)));
    let mut source = PidSnapshotSource { pids: pids.iter().copied().collect() };

    // PLI → request_idr, via a track_id-keyed pipeline index shared with the reconciler.
    let pli_pipelines: PliPipelineMap = Arc::new(Mutex::new(HashMap::new()));
    {
        let p = pli_pipelines.clone();
        peer.on_pli(move |track_id| {
            if let Some(pipe) = p.lock().unwrap().get(&track_id) {
                pipe.request_idr();
            }
        });
    }

    // Wait for the control channel to actually open by retrying a harmless
    // send (as M1's announce retry did): window_id 0 never exists, and the
    // client ignores unknown ids by design, so this is a no-op probe once it
    // gets through.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if disconnected.load(Ordering::SeqCst) {
            anyhow::bail!("peer disconnected during setup");
        }
        match peer.send(&HostMessage::WindowRestored { window_id: 0 }).await {
            Ok(()) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e).context("data channel never opened"),
        }
    }

    // AX pokes → immediate reconcile. Spawned as the LAST fallible setup
    // step, deliberately: AxWatcher::stop() documents that stopping it
    // immediately after spawn() (before the run loop has actually started)
    // is a no-op that then makes join() hang forever. Every other fallible
    // step above (including the control-channel wait, which can bail on
    // disconnect or a 20s timeout) has already succeeded by the time this
    // runs, so no early return here can race a fresh watcher's startup.
    let (poke_tx, mut poke_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = AxWatcher::spawn(pids.to_vec(), Box::new(move || {
        let _ = poke_tx.send(());
    }))?;
    // `spawn()` only guarantees the watcher thread has SENT its run-loop
    // handle back, not that CFRunLoopRun() has started executing yet (see
    // AxWatcher::stop's doc). If the peer were already disconnected at this
    // exact instant, run_session_body's while-condition would be false on
    // its very first check and return immediately, putting the teardown
    // below's watcher.stop() right back in that no-op/hang race. A short,
    // fixed sleep here — far longer than two back-to-back FFI calls take —
    // makes that race practically impossible without touching AxWatcher
    // itself.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut runtimes: RuntimeMap = HashMap::new();
    let outcome = run_session_body(
        &peer,
        scale,
        &tracker,
        &mut source,
        &mut poke_rx,
        &disconnected,
        &mut runtimes,
        &pli_pipelines,
    )
    .await;

    // Unconditional teardown (M1 Finding 2 discipline): whatever happened
    // above, stop every capture/pipeline that got started, then the watcher,
    // then the peer — in that order, regardless of Ok/Err.
    for (_, rt) in runtimes.drain() {
        teardown_runtime(rt);
    }
    watcher.stop();
    peer.close().await;

    outcome
}

/// The fallible core of a session: reconcile tracker events against
/// `runtimes` until disconnect, on a 2 Hz backstop tick plus immediate AX
/// pokes. Every early exit here leaves `runtimes` holding only what's
/// actually live, which the caller always tears down.
#[allow(clippy::too_many_arguments)]
async fn run_session_body(
    peer: &HostPeer,
    scale: f64,
    tracker: &Arc<Mutex<AppTracker>>,
    source: &mut PidSnapshotSource,
    poke_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    disconnected: &Arc<AtomicBool>,
    runtimes: &mut RuntimeMap,
    pli_pipelines: &PliPipelineMap,
) -> Result<()> {
    let rt = tokio::runtime::Handle::current();
    let mut interval = tokio::time::interval(Duration::from_millis(RECONCILE_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    while !disconnected.load(Ordering::SeqCst) {
        tokio::select! {
            _ = interval.tick() => {}
            Some(()) = poke_rx.recv() => {
                // Debounce a burst of AX notifications into one reconcile pass.
                while poke_rx.try_recv().is_ok() {}
            }
        }

        let snapshot = tokio::task::block_in_place(|| source.snapshot());
        let events = tracker.lock().unwrap().diff(snapshot);
        if events.is_empty() {
            continue;
        }

        let mut tracks_changed = false;
        for ev in events {
            match ev {
                TrackerEvent::Opened { window, kind, parent_id, offset_x, offset_y } => {
                    let track_id = match kind {
                        WindowKind::Normal | WindowKind::Sheet => {
                            let tid = format!("win-{}", window.info.id);
                            match open_track_window(peer, &rt, scale, &window, &tid, runtimes, pli_pipelines)
                                .await
                            {
                                Ok(()) => {
                                    tracks_changed = true;
                                    Some(tid)
                                }
                                Err(e) => {
                                    // Loud failure (backlog: no silent black windows). The
                                    // tracker keeps the window live, so this does NOT retry
                                    // each poll; the window is simply never announced this
                                    // session.
                                    error!(
                                        "window {} pipeline failed, not mirroring it: {e:#}",
                                        window.info.id
                                    );
                                    continue;
                                }
                            }
                        }
                        WindowKind::Transient => {
                            // Task 15 replaces this with the real blit-loop start.
                            runtimes.insert(window.info.id, WindowRuntime::Blit { stop: None });
                            None
                        }
                    };
                    let msg = HostMessage::WindowOpened {
                        window_id: window.info.id,
                        title: window.info.title.clone(),
                        kind,
                        parent_id,
                        offset_x,
                        offset_y,
                        width: window.info.width,
                        height: window.info.height,
                        track_id,
                    };
                    if let Err(e) = peer.send(&msg).await {
                        warn!("send WindowOpened: {e}");
                    }
                }
                TrackerEvent::Closed { window_id } => {
                    if let Some(rt_entry) = runtimes.remove(&window_id) {
                        if let WindowRuntime::Track { ref track_id, .. } = rt_entry {
                            pli_pipelines.lock().unwrap().remove(track_id);
                            if let Err(e) = peer.remove_track(track_id).await {
                                warn!("remove_track: {e}");
                            }
                            tracks_changed = true;
                        }
                        teardown_runtime(rt_entry);
                    }
                    let _ = peer.send(&HostMessage::WindowClosed { window_id }).await;
                }
                TrackerEvent::Minimized { window_id } => {
                    if let Some(WindowRuntime::Track { capture, .. }) = runtimes.get_mut(&window_id) {
                        // Pause: drop the capture, keep the pipeline + track alive.
                        if let Some(mut c) = capture.take() {
                            c.stop();
                        }
                    }
                    let _ = peer.send(&HostMessage::WindowMinimized { window_id }).await;
                }
                TrackerEvent::Restored { window_id } => {
                    restart_capture(window_id, scale, tracker, runtimes);
                    let _ = peer.send(&HostMessage::WindowRestored { window_id }).await;
                }
                TrackerEvent::Resized { window_id, width, height } => {
                    if let Some(WindowRuntime::Track { capture: Some(c), .. }) = runtimes.get_mut(&window_id) {
                        let (pw, ph) = (even_px(width, scale), even_px(height, scale));
                        if let Err(e) = c.reconfigure(pw, ph) {
                            warn!("reconfigure {window_id}: {e}");
                        }
                    }
                    let _ = peer.send(&HostMessage::WindowResized { window_id, width, height }).await;
                }
                TrackerEvent::TitleChanged { window_id, title } => {
                    let _ = peer.send(&HostMessage::WindowTitleChanged { window_id, title }).await;
                }
            }
        }

        if tracks_changed {
            if let Err(e) = peer.renegotiate().await {
                warn!("renegotiation failed: {e:#}");
            }
        }
        // Task 16 adds: input pid-map refresh here.
    }

    error!("peer disconnected; tearing down session");
    Ok(())
}
