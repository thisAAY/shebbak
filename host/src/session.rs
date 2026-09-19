use anyhow::{Context, Result};
use srw_capture::macos::ax_watch::AxWatcher;
use srw_capture::macos::list::PidSnapshotSource;
use srw_capture::macos::stream::SckCapture;
use srw_capture::WindowCapture;
use srw_core::mapping::{mapping_for_frame, FrameMeta, InputMapping};
use srw_core::model::{window_local_to_screen, SnapshotWindow};
use srw_core::protocol::{ClientMessage, HostMessage, WindowId};
use srw_core::tracker::{AppTracker, TrackerEvent, WindowSnapshotSource};
use srw_input::macos::AxInput;
use srw_input::macos_pid::PidInput;
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
const RECONCILE_MS: u64 = 500;

/// Per-window runtime state: a media track plus encoder pipeline and the
/// capture pushing into it. Child windows (menus, sheets, popovers) have
/// no runtime of their own — SCK composites them into the parent's frames.
struct WindowRuntime {
    pipeline: Arc<Pipeline>,
    capture: Option<Box<dyn WindowCapture>>,
    track_id: String,
}

type RuntimeMap = HashMap<WindowId, WindowRuntime>;

/// A track_id-keyed index of live pipelines, shared with the `on_pli` hook so
/// a PLI on any track can find its encoder without walking `RuntimeMap`
/// (whose key is `WindowId`, not `track_id`).
type PliPipelineMap = Arc<Mutex<HashMap<String, Arc<Pipeline>>>>;

/// Capture-callback → mapping-forwarder channel: raw per-window frame
/// metadata, already deduped at the callback so it stays quiet at 30 fps.
type MetaSender = tokio::sync::mpsc::UnboundedSender<(WindowId, FrameMeta)>;

/// Work items for the single ordered input-delivery worker (M1 pattern,
/// extended for v2): a dedicated thread drains these in arrival order so a
/// client's FocusChange→KeyEvent (or mouse Down→Up) ordering is preserved —
/// a spawn-per-message design cannot guarantee that, since spawned threads
/// race for the input lock.
enum InputWork {
    Message(ClientMessage),
    UpdatePids(HashMap<WindowId, i32>),
    Shutdown,
}

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
    if let Some(mut c) = rt.capture {
        c.stop();
    }
    rt.pipeline.stop();
}

/// Adds a track, starts its encoder pipeline, and starts a capture pushing
/// into it; wires the result into `runtimes` and `pli_pipelines`. On any
/// failure after the track/pipeline is up, that state is torn down before
/// the error is returned: the pipeline is stopped (otherwise its encoder
/// thread, parked on the frame slot, would never see a shutdown signal and
/// would leak for the life of the process — nothing else holds the
/// `Arc<Pipeline>` to stop it later), and the track is removed from the peer
/// (otherwise a dead `track_id` m-line lingers in the SDP all session, with
/// no `runtimes` entry for the Closed arm to ever clean it up).
#[allow(clippy::too_many_arguments)]
async fn open_track_window(
    peer: &HostPeer,
    rt: &tokio::runtime::Handle,
    scale: f64,
    window: &SnapshotWindow,
    track_id: &str,
    runtimes: &mut RuntimeMap,
    pli_pipelines: &PliPipelineMap,
    meta_tx: &MetaSender,
) -> Result<()> {
    let track = peer.add_track(track_id).await?;
    // `add_track` isn't a pure constructor: it already registered a sender on
    // the peer connection and spawned its RTCP drain task. Every failure path
    // below this point must remove that track again — otherwise a dead
    // `track_id` m-line lingers in the SDP for the rest of the session (next
    // renegotiate onward), `runtimes` never got an entry to clean it up via
    // the Closed arm, and the drain task leaks.
    let pipeline = match Pipeline::start(track, rt.clone(), track_id.to_string()) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            let _ = peer.remove_track(track_id).await;
            return Err(e);
        }
    };

    let (pw, ph) = (
        even_px(window.info.width, scale),
        even_px(window.info.height, scale),
    );
    let mut capture: Box<dyn WindowCapture> = match SckCapture::new(window.info.id, pw, ph, 30) {
        Ok(c) => Box::new(c),
        Err(e) => {
            pipeline.stop();
            let _ = peer.remove_track(track_id).await;
            return Err(e);
        }
    };
    let push_pipe = pipeline.clone();
    let meta_tx = meta_tx.clone();
    let window_id = window.info.id;
    let last_meta = Mutex::new(FrameMeta::default());
    if let Err(e) = capture.start(Box::new(move |frame, meta| {
        push_pipe.push(frame);
        // Dedup on the raw metadata: the mapping is a pure function of
        // (meta, window width/height), and a resize reconfigures the
        // capture (changing meta.width_px/height_px), so unchanged meta ⇒
        // unchanged mapping. The closure is `Fn`, hence the Mutex cell.
        let mut last = last_meta.lock().unwrap();
        if *last != meta {
            *last = meta;
            let _ = meta_tx.send((window_id, meta));
        }
    })) {
        pipeline.stop();
        let _ = peer.remove_track(track_id).await;
        return Err(e);
    }

    info!(
        "sharing window {} ('{}') at {pw}x{ph}px on {track_id}",
        window.info.id, window.info.title
    );
    pli_pipelines
        .lock()
        .unwrap()
        .insert(track_id.to_string(), pipeline.clone());
    runtimes.insert(
        window.info.id,
        WindowRuntime {
            pipeline,
            capture: Some(capture),
            track_id: track_id.to_string(),
        },
    );
    Ok(())
}

/// Restarts capture for a window that just came back from minimized, on the
/// SAME pipeline/track it already had (minimize only tore down the capture,
/// not the track — see the `Minimized` reconciler arm). A no-op if the window
/// isn't a live `Track` runtime, already has a capture, or its geometry is no
/// longer known to the tracker (closed out from under us).
fn restart_capture(
    window_id: WindowId,
    scale: f64,
    tracker: &Arc<Mutex<AppTracker>>,
    runtimes: &mut RuntimeMap,
    meta_tx: &MetaSender,
) {
    let Some(WindowRuntime {
        pipeline, capture, ..
    }) = runtimes.get_mut(&window_id)
    else {
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
    let meta_tx = meta_tx.clone();
    let last_meta = Mutex::new(FrameMeta::default());
    if let Err(e) = new_capture.start(Box::new(move |frame, meta| {
        push_pipe.push(frame);
        // Dedup on the raw metadata: the mapping is a pure function of
        // (meta, window width/height), and a resize reconfigures the
        // capture (changing meta.width_px/height_px), so unchanged meta ⇒
        // unchanged mapping. The closure is `Fn`, hence the Mutex cell.
        let mut last = last_meta.lock().unwrap();
        if *last != meta {
            *last = meta;
            let _ = meta_tx.send((window_id, meta));
        }
    })) {
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
    let mut source = PidSnapshotSource {
        pids: pids.iter().copied().collect(),
    };

    // Input routing (Task 16). Default comes from the Task-1 spike verdict
    // in docs/m2-input-spike-results.md (`activate`): AxInput
    // (activate-then-post) is the default sink; SRW_INPUT=pid opts into the
    // pid-targeted path, which the spike found unreliable for clicks and
    // menu tracking but is kept available for testing.
    let sink: Box<dyn InputSink> = match std::env::var("SRW_INPUT").as_deref() {
        Ok("pid") => {
            info!("input: pid-targeted (PidInput)");
            Box::new(PidInput::new(HashMap::new()))
        }
        _ => {
            info!("input: activate-then-post (AxInput)");
            Box::new(AxInput::new(HashMap::new()))
        }
    };
    let input = Arc::new(Mutex::new(sink));

    // Ordered input worker (M1 pattern, extended for v2): a single thread
    // drains `input_rx` in arrival order, so the client's
    // FocusChange→KeyEvent (and mouse Down→Up) ordering is preserved end to
    // end — a spawn-per-message design cannot guarantee that.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputWork>();
    let input_worker = {
        let tracker = tracker.clone();
        let input = input.clone();
        std::thread::Builder::new()
            .name("input-worker".into())
            .spawn(move || {
                for work in input_rx {
                    match work {
                        InputWork::UpdatePids(map) => input.lock().unwrap().set_pid_map(map),
                        InputWork::Message(msg) => {
                            let mut sink = input.lock().unwrap();
                            let r = match msg {
                                ClientMessage::MouseInput {
                                    window_id,
                                    x,
                                    y,
                                    button,
                                    action,
                                } => match tracker.lock().unwrap().geometry(window_id).cloned() {
                                    Some(win) => {
                                        let (sx, sy) = window_local_to_screen(&win, x, y);
                                        sink.mouse(window_id, sx, sy, button, action)
                                    }
                                    None => {
                                        warn!("input for unknown window {window_id}");
                                        Ok(())
                                    }
                                },
                                ClientMessage::MouseMove { window_id, x, y } => {
                                    match tracker.lock().unwrap().geometry(window_id).cloned() {
                                        Some(win) => {
                                            let (sx, sy) = window_local_to_screen(&win, x, y);
                                            sink.mouse_move(window_id, sx, sy)
                                        }
                                        None => Ok(()),
                                    }
                                }
                                ClientMessage::KeyEvent {
                                    window_id,
                                    key_code,
                                    down,
                                    flags,
                                } => sink.key(window_id, key_code, down, flags),
                                ClientMessage::FocusChange { window_id } => sink.focus(window_id),
                                ClientMessage::ResizeRequest {
                                    window_id,
                                    width,
                                    height,
                                } => sink.resize_window(window_id, width, height),
                                ClientMessage::CloseRequest { window_id } => {
                                    sink.close_window(window_id)
                                }
                                // Consumed inside HostPeer's dispatch — never reaches here.
                                ClientMessage::SdpAnswer { .. } => Ok(()),
                            };
                            if let Err(e) = r {
                                warn!("input delivery failed: {e:#}");
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

    // Frame-geometry → input-mapping forwarder: capture callbacks push
    // FrameMeta changes here; this turns them into change-triggered
    // HostMessage::InputMapping sends. Identity is implicit at open, so
    // nothing is sent until a mapping first deviates (letterboxing), and
    // the return to 1:1 is sent because it differs from the last send.
    let (meta_tx, mut meta_rx) = tokio::sync::mpsc::unbounded_channel::<(WindowId, FrameMeta)>();
    {
        let tracker = tracker.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            let mut last_sent: HashMap<WindowId, InputMapping> = HashMap::new();
            while let Some((window_id, meta)) = meta_rx.recv().await {
                let Some(win) = tracker.lock().unwrap().geometry(window_id).cloned() else {
                    continue; // window closed under us; nothing to map
                };
                let mapping = mapping_for_frame(&meta, &win);
                let send = match last_sent.get(&window_id) {
                    Some(prev) => !mapping.approx_eq(prev),
                    None => !mapping.is_identity(),
                };
                if send {
                    last_sent.insert(window_id, mapping);
                    let msg = HostMessage::InputMapping {
                        window_id,
                        scale_x: mapping.scale_x,
                        scale_y: mapping.scale_y,
                        offset_x: mapping.offset_x,
                        offset_y: mapping.offset_y,
                    };
                    if let Err(e) = peer.send(&msg).await {
                        warn!("send InputMapping: {e}");
                    }
                }
            }
            // Ends when the last MetaSender clone drops at session teardown.
        });
    }

    // Wait for the control channel to actually open by retrying a harmless
    // send (as M1's announce retry did): window_id 0 never exists, and the
    // client ignores unknown ids by design, so this is a no-op probe once it
    // gets through.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if disconnected.load(Ordering::SeqCst) {
            // Bailing here drops `peer` (an Arc<HostPeer>) without ever
            // reaching the unconditional-teardown block below — close it
            // explicitly first, or every failed connect attempt leaks the
            // ICE/DTLS tasks underneath (main.rs loops right back into
            // serve_one_offer for the next client).
            peer.close().await;
            anyhow::bail!("peer disconnected during setup");
        }
        match peer
            .send(&HostMessage::WindowRestored { window_id: 0 })
            .await
        {
            Ok(()) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                peer.close().await;
                return Err(e).context("data channel never opened");
            }
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
    let watcher = AxWatcher::spawn(
        pids.to_vec(),
        Box::new(move || {
            let _ = poke_tx.send(());
        }),
    )?;
    // `spawn()` only guarantees the watcher thread has SENT its run-loop
    // handle back, not that CFRunLoopRun() has started executing yet (see
    // AxWatcher::stop's doc). If the peer were already disconnected at this
    // exact instant, run_session_body's while-condition would be false on
    // its very first check and return immediately, putting the teardown
    // below's watcher.stop() right back in that no-op/hang race. A short,
    // fixed sleep here — far longer than two back-to-back FFI calls take —
    // makes that race practically impossible without touching AxWatcher
    // itself.
    // TODO: replace this fixed sleep with a real started-signal from
    // AxWatcher::spawn (e.g. send the RunLoopHandle only after
    // CFRunLoopRun() begins, or an explicit "running" rendezvous) so this
    // isn't timing-dependent at all.
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
        &input_tx,
        &meta_tx,
    )
    .await;

    // Unconditional teardown (M1 Finding 2 discipline): whatever happened
    // above, stop every capture/pipeline that got started, then the input
    // worker, then the watcher, then the peer — in that order, regardless of
    // Ok/Err.
    for (_, rt) in runtimes.drain() {
        teardown_runtime(rt);
    }
    let _ = input_tx.send(InputWork::Shutdown);
    // Join on a blocking thread: `JoinHandle::join` blocks the calling
    // thread, and this is a tokio task — blocking it directly would stall
    // the runtime worker until the input thread exits.
    match tokio::task::spawn_blocking(move || input_worker.join()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("input worker panicked: {e:?}"),
        Err(e) => warn!("input worker join task panicked: {e:?}"),
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
    peer: &Arc<HostPeer>,
    scale: f64,
    tracker: &Arc<Mutex<AppTracker>>,
    source: &mut PidSnapshotSource,
    poke_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    disconnected: &Arc<AtomicBool>,
    runtimes: &mut RuntimeMap,
    pli_pipelines: &PliPipelineMap,
    input_tx: &std::sync::mpsc::Sender<InputWork>,
    meta_tx: &MetaSender,
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
                TrackerEvent::Opened { window } => {
                    let track_id = format!("win-{}", window.info.id);
                    match open_track_window(
                        peer,
                        &rt,
                        scale,
                        &window,
                        &track_id,
                        runtimes,
                        pli_pipelines,
                        meta_tx,
                    )
                    .await
                    {
                        Ok(()) => tracks_changed = true,
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
                    let msg = HostMessage::WindowOpened {
                        window_id: window.info.id,
                        title: window.info.title.clone(),
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
                        pli_pipelines.lock().unwrap().remove(&rt_entry.track_id);
                        if let Err(e) = peer.remove_track(&rt_entry.track_id).await {
                            warn!("remove_track: {e}");
                        }
                        tracks_changed = true;
                        teardown_runtime(rt_entry);
                    }
                    let _ = peer.send(&HostMessage::WindowClosed { window_id }).await;
                }
                TrackerEvent::Minimized { window_id } => {
                    if let Some(rt_entry) = runtimes.get_mut(&window_id) {
                        // Pause: drop the capture, keep the pipeline + track alive.
                        if let Some(mut c) = rt_entry.capture.take() {
                            c.stop();
                        }
                    }
                    let _ = peer.send(&HostMessage::WindowMinimized { window_id }).await;
                }
                TrackerEvent::Restored { window_id } => {
                    restart_capture(window_id, scale, tracker, runtimes, meta_tx);
                    let _ = peer.send(&HostMessage::WindowRestored { window_id }).await;
                }
                TrackerEvent::Resized {
                    window_id,
                    width,
                    height,
                } => {
                    if let Some(WindowRuntime {
                        capture: Some(c), ..
                    }) = runtimes.get_mut(&window_id)
                    {
                        let (pw, ph) = (even_px(width, scale), even_px(height, scale));
                        if let Err(e) = c.reconfigure(pw, ph) {
                            warn!("reconfigure {window_id}: {e}");
                        }
                    }
                    let _ = peer
                        .send(&HostMessage::WindowResized {
                            window_id,
                            width,
                            height,
                        })
                        .await;
                }
                TrackerEvent::TitleChanged { window_id, title } => {
                    let _ = peer
                        .send(&HostMessage::WindowTitleChanged { window_id, title })
                        .await;
                }
            }
        }

        if tracks_changed {
            if let Err(e) = peer.renegotiate().await {
                warn!("renegotiation failed: {e:#}");
            }
        }
        // Refresh the input sink's window→pid map after every non-empty
        // diff, so newly opened/closed windows are immediately targetable
        // (AxInput's AX lookup and PidInput's pid-targeted post both key off
        // this map).
        let _ = input_tx.send(InputWork::UpdatePids(tracker.lock().unwrap().pid_map()));
    }

    error!("peer disconnected; tearing down session");
    Ok(())
}
