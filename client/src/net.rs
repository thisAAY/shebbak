use anyhow::{Context, Result};
use srw_core::protocol::{ClientMessage, HostMessage};
use srw_transport::peer::ClientPeer;
use srw_transport::signalling::post_offer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::track::track_remote::TrackRemote;

/// Events pushed to the coordinator thread. Video never comes this way —
/// access units go straight from read_track to the VideoRouter.
pub enum UiEvent {
    Host(HostMessage),
    Disconnected,
}

pub struct Net {
    // Neither field is read directly anymore now that outbound sends go
    // through `input_tx` — both are kept for RAII: `rt` must outlive every
    // task spawned on it (dropping the runtime tears those down), and `peer`
    // documents that the connection is scoped to `Net`'s lifetime.
    #[allow(dead_code)]
    pub peer: Arc<ClientPeer>,
    #[allow(dead_code)]
    pub rt: tokio::runtime::Runtime,
    /// Ordered outbound queue — a single task drains this sequentially so
    /// two back-to-back sends (e.g. a mouse Down then Up, or a KeyEvent then
    /// FocusChange) can never race each other onto the data channel out of
    /// order.
    input_tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
}

impl Net {
    /// Cloneable ordered outbound sender (the helper-relay path).
    pub fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<ClientMessage> {
        self.input_tx.clone()
    }
}

/// Build the connection. `on_event` delivers UiEvents; the coordinator's
/// wake callback is expected to already be folded into it by the caller.
pub fn connect(
    signal_url: &str,
    on_event: Arc<dyn Fn(UiEvent) + Send + Sync>,
    router: Arc<crate::router::VideoRouter>,
) -> Result<Net> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let peer = rt.block_on(async { ClientPeer::new().await })?;
    let peer = Arc::new(peer);

    {
        let on_event = on_event.clone();
        peer.on_host_message(move |m| {
            on_event(UiEvent::Host(m));
        });
    }
    {
        let on_event = on_event.clone();
        peer.on_disconnect(move || {
            on_event(UiEvent::Disconnected);
        });
    }
    {
        let rt_handle = rt.handle().clone();
        let peer_for_tracks = peer.clone();
        let router = router.clone();
        peer.on_track(move |track_id, track| {
            info!("track arrived: {track_id}");
            let router = router.clone();
            let peer = peer_for_tracks.clone();
            rt_handle.spawn(read_track(track_id, track, router, peer));
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
    // matches the order sends were made in.
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
    router: Arc<crate::router::VideoRouter>,
    peer: Arc<ClientPeer>,
) {
    let mut builder = SampleBuilder::new(512, H264Packet::default(), 90000)
        .with_max_time_delay(std::time::Duration::from_millis(500));

    // Keyframe requester (see M2): PLI on join, then once/sec while the
    // stream is stalled. Stall state now comes from the owning helper's
    // DecodeProgress reports via the router — a helper that just spawned
    // (or respawned) has no progress, so it gets an IDR immediately.
    let alive = Arc::new(AtomicBool::new(true));
    {
        let peer = peer.clone();
        let ssrc = track.ssrc();
        let alive = alive.clone();
        let router = router.clone();
        let track_id = track_id.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await; // first tick fires immediately -> PLI on join
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                if router.stalled(&track_id) && peer.write_pli(ssrc).await.is_err() {
                    return;
                }
            }
        });
    }

    loop {
        let (pkt, _) = match track.read_rtp().await {
            Ok(p) => p,
            Err(e) => {
                warn!("{track_id}: track ended: {e}");
                alive.store(false, Ordering::Relaxed);
                return;
            }
        };
        builder.push(pkt);
        while let Some(sample) = builder.pop() {
            // Unrouted (window not announced yet / helper gone): dropped;
            // PLI recovery re-syncs the fresh decoder once routed.
            let _ = router.forward(&track_id, sample.data.to_vec());
        }
    }
}
