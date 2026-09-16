use anyhow::{anyhow, Context, Result};
use srw_core::protocol::{ClientMessage, HostMessage};
use std::sync::{Arc, Mutex};
use tracing::warn;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;

async fn new_pc() -> Result<Arc<RTCPeerConnection>> {
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media)?;
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    // Loopback: no STUN/TURN needed, host candidates suffice.
    let config = RTCConfiguration { ice_servers: vec![RTCIceServer::default()], ..Default::default() };
    Ok(Arc::new(api.new_peer_connection(config).await?))
}

fn hook_disconnect(pc: &Arc<RTCPeerConnection>, f: impl Fn() + Send + Sync + 'static) {
    let f = Arc::new(f);
    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let f = f.clone();
        if matches!(
            s,
            RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
        ) {
            f();
        }
        Box::pin(async {})
    }));
}

async fn send_json<T: serde::Serialize>(
    dc: &Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    msg: &T,
) -> Result<()> {
    let ch = dc.lock().unwrap().clone().ok_or_else(|| anyhow!("data channel not open"))?;
    let json = serde_json::to_string(msg)?;
    ch.send_text(json).await.context("send on data channel")?;
    Ok(())
}

pub struct HostPeer {
    pc: Arc<RTCPeerConnection>,
    dc: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    on_msg: Arc<Mutex<Option<Arc<dyn Fn(ClientMessage) + Send + Sync>>>>,
}

impl HostPeer {
    pub async fn new() -> Result<Self> {
        let pc = new_pc().await?;
        let dc: Arc<Mutex<Option<Arc<RTCDataChannel>>>> = Arc::new(Mutex::new(None));
        let on_msg: Arc<Mutex<Option<Arc<dyn Fn(ClientMessage) + Send + Sync>>>> =
            Arc::new(Mutex::new(None));

        let dc_slot = dc.clone();
        let on_msg_slot = on_msg.clone();
        pc.on_data_channel(Box::new(move |ch: Arc<RTCDataChannel>| {
            let dc_slot = dc_slot.clone();
            let on_msg_slot = on_msg_slot.clone();
            Box::pin(async move {
                let handler_slot = on_msg_slot.clone();
                ch.on_message(Box::new(move |m: DataChannelMessage| {
                    let handler = handler_slot.lock().unwrap().clone();
                    match serde_json::from_slice::<ClientMessage>(&m.data) {
                        Ok(msg) => {
                            if let Some(h) = handler {
                                h(msg);
                            }
                        }
                        Err(e) => warn!("malformed client message: {e}"),
                    }
                    Box::pin(async {})
                }));
                *dc_slot.lock().unwrap() = Some(ch);
            })
        }));

        Ok(Self { pc, dc, on_msg })
    }

    pub async fn add_track(&self, track_id: &str) -> Result<Arc<TrackLocalStaticSample>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability { mime_type: MIME_TYPE_H264.to_owned(), ..Default::default() },
            track_id.to_owned(),
            track_id.to_owned(), // stream_id == track_id: client binds on either
        ));
        self.pc.add_track(track.clone()).await.context("add_track")?;
        Ok(track)
    }

    pub async fn answer(&self, offer: RTCSessionDescription) -> Result<RTCSessionDescription> {
        self.pc.set_remote_description(offer).await?;
        let answer = self.pc.create_answer(None).await?;
        let mut gathered = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(answer).await?;
        let _ = gathered.recv().await;
        self.pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after gathering"))
    }

    pub fn on_client_message(&self, f: impl Fn(ClientMessage) + Send + Sync + 'static) {
        *self.on_msg.lock().unwrap() = Some(Arc::new(f));
    }

    pub async fn send(&self, msg: &HostMessage) -> Result<()> {
        send_json(&self.dc, msg).await
    }

    pub fn on_disconnect(&self, f: impl Fn() + Send + Sync + 'static) {
        hook_disconnect(&self.pc, f);
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

pub struct ClientPeer {
    pc: Arc<RTCPeerConnection>,
    dc: Arc<RTCDataChannel>,
}

impl ClientPeer {
    pub async fn new(num_windows: usize) -> Result<Self> {
        let pc = new_pc().await?;
        for _ in 0..num_windows {
            pc.add_transceiver_from_kind(
                RTPCodecType::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await?;
        }
        let dc = pc.create_data_channel("control", None).await?;
        Ok(Self { pc, dc })
    }

    pub async fn offer(&self) -> Result<RTCSessionDescription> {
        let offer = self.pc.create_offer(None).await?;
        let mut gathered = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(offer).await?;
        let _ = gathered.recv().await;
        self.pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after gathering"))
    }

    pub async fn accept_answer(&self, answer: RTCSessionDescription) -> Result<()> {
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    pub fn on_track(&self, f: impl Fn(String, Arc<TrackRemote>) + Send + Sync + 'static) {
        let f = Arc::new(f);
        self.pc.on_track(Box::new(move |track: Arc<TrackRemote>, _receiver, _transceiver| {
            let f = f.clone();
            let key = if track.id().is_empty() { track.stream_id() } else { track.id() };
            f(key, track);
            Box::pin(async {})
        }));
    }

    pub fn on_host_message(&self, f: impl Fn(HostMessage) + Send + Sync + 'static) {
        let f = Arc::new(f);
        self.dc.on_message(Box::new(move |m: DataChannelMessage| {
            match serde_json::from_slice::<HostMessage>(&m.data) {
                Ok(msg) => f(msg),
                Err(e) => warn!("malformed host message: {e}"),
            }
            Box::pin(async {})
        }));
    }

    pub async fn send(&self, msg: &ClientMessage) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        self.dc.send_text(json).await.context("send on data channel")?;
        Ok(())
    }

    pub fn on_disconnect(&self, f: impl Fn() + Send + Sync + 'static) {
        hook_disconnect(&self.pc, f);
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}
