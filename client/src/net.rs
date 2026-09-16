use anyhow::{Context, Result};
use srw_core::pixels::BgraFrame;
use srw_core::protocol::{ClientMessage, HostMessage};
use srw_transport::codec::H264Decoder;
use srw_transport::peer::ClientPeer;
use srw_transport::signalling::post_offer;
use std::sync::mpsc::Sender;
use std::sync::Arc;
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
    pub peer: Arc<ClientPeer>,
    pub rt: tokio::runtime::Runtime,
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

    let peer = rt.block_on(async { ClientPeer::new(2).await })?;
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
        peer.on_track(move |track_id, track| {
            info!("track arrived: {track_id}");
            let _ = ui.send(UiEvent::TrackOpened { track_id: track_id.clone() });
            wake();
            let ui = ui.clone();
            let wake = wake.clone();
            rt_handle.spawn(read_track(track_id, track, ui, wake));
        });
    }

    let offer = rt.block_on(peer.offer())?;
    let url = signal_url.to_string();
    // `post_offer` is a blocking (ureq) call. `block_in_place` requires being
    // inside an active runtime worker context; we're on the plain calling
    // thread between two separate `block_on` calls, so it would panic here.
    // Run it on a blocking pool thread within one `block_on` instead.
    let answer = rt
        .block_on(async move { tokio::task::spawn_blocking(move || post_offer(&url, &offer)).await })
        .context("signalling task panicked")?
        .context("signalling failed — is the host running?")?;
    rt.block_on(peer.accept_answer(answer))?;
    info!("connected");

    Ok(Net { peer, rt })
}

async fn read_track(
    track_id: String,
    track: Arc<TrackRemote>,
    ui: Sender<UiEvent>,
    wake: impl Fn() + Send + Sync + 'static,
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
    loop {
        let (pkt, _) = match track.read_rtp().await {
            Ok(p) => p,
            Err(e) => {
                warn!("{track_id}: track ended: {e}");
                return;
            }
        };
        builder.push(pkt);
        while let Some(sample) = builder.pop() {
            match decoder.decode(&sample.data) {
                Ok(Some(frame)) => {
                    let _ = ui.send(UiEvent::TrackFrame { track_id: track_id.clone(), frame });
                    wake();
                }
                Ok(None) => {}
                Err(e) => warn!("{track_id}: decode error (dropped): {e}"),
            }
        }
    }
}

/// Fire-and-forget send from the UI thread.
pub fn send_client_msg(net: &Net, msg: ClientMessage) {
    let peer = net.peer.clone();
    net.rt.spawn(async move {
        if let Err(e) = peer.send(&msg).await {
            warn!("send failed: {e}");
        }
    });
}
