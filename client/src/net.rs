use anyhow::{Context, Result};
use srw_core::pixels::BgraFrame;
use srw_core::protocol::{ClientMessage, HostMessage};
use srw_transport::codec::H264Decoder;
use srw_transport::peer::ClientPeer;
use srw_transport::signalling::post_offer;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::track::track_remote::TrackRemote;

/// Events pushed to the UI thread. The winit side drains these on wake.
pub enum UiEvent {
    Host(HostMessage),
    TrackFrame { track_id: String, frame: BgraFrame },
    TrackOpened { track_id: String },
    Disconnected,
}

pub struct Net {
    // Neither field is read directly anymore now that `send_client_msg` goes
    // through `input_tx` — both are kept for RAII: `rt` must outlive every
    // task spawned on it (dropping the runtime tears those down), and `peer`
    // documents that the connection is scoped to `Net`'s lifetime.
    #[allow(dead_code)]
    pub peer: Arc<ClientPeer>,
    #[allow(dead_code)]
    pub rt: tokio::runtime::Runtime,
    /// Ordered outbound queue for `send_client_msg` — a single task drains
    /// this sequentially so two back-to-back sends (e.g. a mouse Down then
    /// Up, or a KeyEvent then FocusChange) can never race each other onto
    /// the data channel out of order.
    input_tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
}

/// Build the connection. `notify` delivers UiEvents plus a wake callback
/// (winit EventLoopProxy) so the UI thread drains the queue.
pub fn connect(
    signal_url: &str,
    ui_tx: Sender<UiEvent>,
    wake: impl Fn() + Send + Sync + Clone + 'static,
) -> Result<Net> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let peer = rt.block_on(async { ClientPeer::new().await })?;
    let peer = Arc::new(peer);

    {
        let ui = ui_tx.clone();
        let wake = wake.clone();
        peer.on_host_message(move |m| {
            let _ = ui.send(UiEvent::Host(m));
            wake();
        });
    }
    {
        let ui = ui_tx.clone();
        let wake = wake.clone();
        peer.on_disconnect(move || {
            let _ = ui.send(UiEvent::Disconnected);
            wake();
        });
    }
    {
        let ui = ui_tx.clone();
        let wake = wake.clone();
        let rt_handle = rt.handle().clone();
        let peer_for_tracks = peer.clone();
        peer.on_track(move |track_id, track| {
            info!("track arrived: {track_id}");
            let _ = ui.send(UiEvent::TrackOpened {
                track_id: track_id.clone(),
            });
            wake();
            let ui = ui.clone();
            let wake = wake.clone();
            let peer = peer_for_tracks.clone();
            rt_handle.spawn(read_track(track_id, track, ui, wake, peer));
        });
    }

    let offer = rt.block_on(peer.offer())?;
    let url = signal_url.to_string();
    // `post_offer` is a blocking (ureq) call. `block_in_place` requires being
    // inside an active runtime worker context; we're on the plain calling
    // thread between two separate `block_on` calls, so it would panic here.
    // Run it on a blocking pool thread within one `block_on` instead.
    let answer = rt
        .block_on(
            async move { tokio::task::spawn_blocking(move || post_offer(&url, &offer)).await },
        )
        .context("signalling task panicked")?
        .context("signalling failed — is the host running?")?;
    rt.block_on(peer.accept_answer(answer))?;
    info!("connected");

    // Single ordered sender task: all client input goes through this one
    // channel and is awaited sequentially, so message order on the wire
    // matches the order `send_client_msg` was called in.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
    {
        let peer = peer.clone();
        rt.spawn(async move {
            while let Some(msg) = input_rx.recv().await {
                if let Err(e) = peer.send(&msg).await {
                    warn!("send failed: {e}");
                }
            }
        });
    }

    Ok(Net { peer, rt, input_tx })
}

async fn read_track(
    track_id: String,
    track: Arc<TrackRemote>,
    ui: Sender<UiEvent>,
    wake: impl Fn() + Send + Sync + 'static,
    peer: Arc<ClientPeer>,
) {
    let mut builder = SampleBuilder::new(512, H264Packet::default(), 90000)
        .with_max_time_delay(std::time::Duration::from_millis(500));
    let mut decoder = match H264Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("{track_id}: decoder init failed: {e}");
            return;
        }
    };

    // Keyframe requester. H.264 decoding cannot start until an IDR arrives,
    // and the host only forces one on a PLI (see host on_pli -> request_idr).
    // A track we start reading after the host's initial IDR would otherwise
    // never decode — staying blank until an unrelated renegotiation or
    // capture reconfigure happened to emit a fresh keyframe. So we send a PLI
    // on join and keep asking (once/sec) whenever the stream is stalled — no
    // successful decode yet, or none in the last ~1.2s (packet loss). This is
    // also the loss-recovery path the acceptance soak relies on.
    let start = Instant::now();
    let last_decode_ms = Arc::new(AtomicU64::new(0)); // 0 = nothing decoded yet
    let alive = Arc::new(AtomicBool::new(true));
    {
        let peer = peer.clone();
        let ssrc = track.ssrc();
        let last_decode_ms = last_decode_ms.clone();
        let alive = alive.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await; // first tick fires immediately -> PLI on join
                if !alive.load(Ordering::Relaxed) {
                    return; // track ended; stop asking for a dead ssrc
                }
                let now_ms = start.elapsed().as_millis() as u64;
                let last = last_decode_ms.load(Ordering::Relaxed);
                let stalled = last == 0 || now_ms.saturating_sub(last) > 1200;
                if stalled && peer.write_pli(ssrc).await.is_err() {
                    return; // peer gone
                }
            }
        });
    }

    // Rate-limit decode-error spam (a bad GOP can produce one per frame): log
    // at most once per second per track, folding the rest into a count.
    let mut suppressed: u32 = 0;
    let mut last_warn = std::time::Instant::now() - std::time::Duration::from_secs(1);
    loop {
        let (pkt, _) = match track.read_rtp().await {
            Ok(p) => p,
            Err(e) => {
                warn!("{track_id}: track ended: {e}");
                alive.store(false, Ordering::Relaxed); // stop the PLI requester
                return;
            }
        };
        builder.push(pkt);
        while let Some(sample) = builder.pop() {
            match decoder.decode(&sample.data) {
                Ok(Some(frame)) => {
                    // Mark progress so the keyframe requester stops asking
                    // (and treats a later gap as a stall worth a fresh PLI).
                    last_decode_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                    let _ = ui.send(UiEvent::TrackFrame {
                        track_id: track_id.clone(),
                        frame,
                    });
                    wake();
                }
                Ok(None) => {}
                Err(e) => {
                    let now = std::time::Instant::now();
                    if now.duration_since(last_warn) >= std::time::Duration::from_secs(1) {
                        if suppressed > 0 {
                            warn!("{track_id}: decode error (dropped, {suppressed} more suppressed in the last second): {e}");
                        } else {
                            warn!("{track_id}: decode error (dropped): {e}");
                        }
                        last_warn = now;
                        suppressed = 0;
                    } else {
                        suppressed += 1;
                    }
                }
            }
        }
    }
}

/// Fire-and-forget send from the UI thread. Ordered relative to every other
/// call to this function — see `Net::input_tx`.
pub fn send_client_msg(net: &Net, msg: ClientMessage) {
    let _ = net.input_tx.send(msg);
}
