use anyhow::{anyhow, Context, Result};
use srw_core::protocol::{ClientMessage, HostMessage};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::warn;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;

/// Shared slot for an optional message-handler callback, set once via
/// `on_message`/`on_pli` and invoked from the data-channel/RTCP callbacks.
type HandlerSlot<M> = Arc<Mutex<Option<Arc<dyn Fn(M) + Send + Sync>>>>;

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
    let config = RTCConfiguration { ice_servers: vec![], ..Default::default() };
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

/// The ordered (media type, mid) pairs across an SDP's media sections, used to
/// tell whether an answer structurally corresponds to a particular offer.
fn media_section_mids(desc: &RTCSessionDescription) -> Result<Vec<(String, Option<String>)>> {
    let parsed = desc.unmarshal().context("unmarshal SDP for renegotiation validation")?;
    Ok(parsed
        .media_descriptions
        .iter()
        .map(|m| {
            let mid = m.attribute(webrtc::sdp::description::session::ATTR_KEY_MID).flatten().map(str::to_owned);
            (m.media_name.media.clone(), mid)
        })
        .collect())
}

/// Guards `renegotiate_inner` against applying an answer that doesn't
/// correspond to the offer it just sent (see `PendingAnswerQueue`'s doc for
/// why that can happen even with the queue's cleanup). Compares the ordered
/// (media type, mid) set of each side's media sections: a mismatch means the
/// answer was negotiated against different tracks than this offer describes,
/// so it cannot be the right answer. A structurally identical answer is
/// benign to apply even if it happens to be a stale one, since it carries the
/// same track/media layout this call itself just offered.
fn validate_answer_matches_offer(
    offer: &RTCSessionDescription,
    answer: &RTCSessionDescription,
) -> Result<()> {
    let offer_mids = media_section_mids(offer)?;
    let answer_mids = media_section_mids(answer)?;
    if offer_mids != answer_mids {
        return Err(anyhow!(
            "SdpAnswer media sections {answer_mids:?} do not match the current offer {offer_mids:?} \
             (stale answer for an abandoned renegotiation?)"
        ));
    }
    Ok(())
}

/// Holds at most one renegotiation's worth of "waiting for the client's
/// SdpAnswer" state at a time (`renegotiation_lock` in `HostPeer` guarantees
/// only one `renegotiate()` call runs at once). It's a queue rather than a
/// bare `Option` purely so a timed-out/failed call can be told apart from a
/// fresh one when cleaning up: `push` hands back a generation id, and
/// `discard` removes exactly that entry if it's still sitting there unclaimed.
///
/// This does NOT, by itself, guarantee an incoming answer belongs to the
/// renegotiation currently awaiting it — the generation id never goes on the
/// wire, so a late answer for an abandoned renegotiation can still be popped
/// into a newer one's receiver if `discard` didn't run before it arrived. The
/// actual safety net against that mispairing is content validation in
/// `HostPeer::renegotiate_inner` (comparing the answer's m-section/mid set
/// against the offer this call just sent) before ever calling
/// `set_remote_description`; `discard`-on-failure here just keeps the queue
/// from accumulating dead entries that would otherwise block a later, correct
/// answer from ever being popped.
#[derive(Default)]
struct PendingAnswerQueue {
    next_generation: AtomicU64,
    queue: Mutex<VecDeque<(u64, tokio::sync::oneshot::Sender<RTCSessionDescription>)>>,
}

impl PendingAnswerQueue {
    fn new() -> Self {
        Self::default()
    }

    /// Registers a fresh receiver for the next renegotiation answer, returning
    /// its generation id (passed to `discard` if that renegotiation fails).
    fn push(&self, tx: tokio::sync::oneshot::Sender<RTCSessionDescription>) -> u64 {
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.queue.lock().unwrap().push_back((generation, tx));
        generation
    }

    /// Removes this generation's entry if it's still queued (no answer
    /// arrived for it yet).
    fn discard(&self, generation: u64) {
        self.queue.lock().unwrap().retain(|(g, _)| *g != generation);
    }

    /// Pops the oldest still-pending entry. Used by both the production
    /// SdpAnswer dispatch arm and by tests, so the exact code path that ships
    /// is what gets exercised.
    fn pop(&self) -> Option<(u64, tokio::sync::oneshot::Sender<RTCSessionDescription>)> {
        self.queue.lock().unwrap().pop_front()
    }
}

pub struct HostPeer {
    pc: Arc<RTCPeerConnection>,
    dc: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    on_msg: HandlerSlot<ClientMessage>,
    senders: Arc<Mutex<HashMap<String, Arc<RTCRtpSender>>>>,
    pending_answers: Arc<PendingAnswerQueue>,
    renegotiation_lock: Arc<tokio::sync::Mutex<()>>,
    on_pli: HandlerSlot<String>,
}

impl HostPeer {
    pub async fn new() -> Result<Self> {
        let pc = new_pc().await?;
        let dc: Arc<Mutex<Option<Arc<RTCDataChannel>>>> = Arc::new(Mutex::new(None));
        let on_msg: HandlerSlot<ClientMessage> = Arc::new(Mutex::new(None));
        let pending_answers = Arc::new(PendingAnswerQueue::new());

        let dc_slot = dc.clone();
        let on_msg_slot = on_msg.clone();
        let pending_answers_slot = pending_answers.clone();
        pc.on_data_channel(Box::new(move |ch: Arc<RTCDataChannel>| {
            let dc_slot = dc_slot.clone();
            let on_msg_slot = on_msg_slot.clone();
            let pending_answers_slot = pending_answers_slot.clone();
            Box::pin(async move {
                let handler_slot = on_msg_slot.clone();
                let pending_answers_slot = pending_answers_slot.clone();
                ch.on_message(Box::new(move |m: DataChannelMessage| {
                    let handler = handler_slot.lock().unwrap().clone();
                    match serde_json::from_slice::<ClientMessage>(&m.data) {
                        Ok(ClientMessage::SdpAnswer { sdp }) => {
                            match serde_json::from_str::<RTCSessionDescription>(&sdp) {
                                Ok(answer) => {
                                    if let Some((_generation, tx)) = pending_answers_slot.pop() {
                                        let _ = tx.send(answer);
                                    } else {
                                        warn!("SdpAnswer received with no pending renegotiation");
                                    }
                                }
                                Err(e) => warn!("bad SdpAnswer payload: {e}"),
                            }
                        }
                        Ok(msg) => {
                            if let Some(h) = handler {
                                h(msg);
                            } else {
                                warn!("client message dropped: no handler registered yet");
                            }
                        }
                        Err(e) => warn!("malformed client message: {e}"),
                    }
                    Box::pin(async {})
                }));
                *dc_slot.lock().unwrap() = Some(ch);
            })
        }));

        Ok(Self {
            pc,
            dc,
            on_msg,
            senders: Arc::new(Mutex::new(HashMap::new())),
            pending_answers,
            renegotiation_lock: Arc::new(tokio::sync::Mutex::new(())),
            on_pli: Arc::new(Mutex::new(None)),
        })
    }

    /// Adds a sendonly H.264 track, keeps its RTCRtpSender, spawns an RTCP
    /// drain task for it. Does NOT renegotiate — call renegotiate() after.
    pub async fn add_track(&self, track_id: &str) -> Result<Arc<TrackLocalStaticSample>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability { mime_type: MIME_TYPE_H264.to_owned(), ..Default::default() },
            track_id.to_owned(),
            track_id.to_owned(), // stream_id == track_id: client binds on either
        ));
        let sender = self.pc.add_track(track.clone()).await.context("add_track")?;
        self.senders.lock().unwrap().insert(track_id.to_owned(), sender.clone());
        // Drain RTCP so NACK/PLI interceptors run; surface PLI to the pipeline.
        let tid = track_id.to_owned();
        let on_pli = self.on_pli.clone();
        tokio::spawn(async move {
            while let Ok((packets, _)) = sender.read_rtcp().await {
                for p in packets {
                    if p.as_any().downcast_ref::<PictureLossIndication>().is_some() {
                        if let Some(f) = on_pli.lock().unwrap().clone() {
                            f(tid.clone());
                        }
                    }
                }
            }
        });
        Ok(track)
    }

    /// Removes the track's sender from the peer connection. Does NOT renegotiate.
    pub async fn remove_track(&self, track_id: &str) -> Result<()> {
        let sender = self
            .senders
            .lock()
            .unwrap()
            .remove(track_id)
            .ok_or_else(|| anyhow!("no sender for track {track_id}"))?;
        self.pc.remove_track(&sender).await.context("remove_track")?;
        Ok(())
    }

    /// Serialized: create offer → send SdpOffer over the data channel → await
    /// the client's SdpAnswer (10 s timeout) → set remote description.
    pub async fn renegotiate(&self) -> Result<()> {
        let _guard = self.renegotiation_lock.lock().await; // one renegotiation at a time
        let (tx, rx) = tokio::sync::oneshot::channel();
        let generation = self.pending_answers.push(tx);
        let result = self.renegotiate_inner(rx).await;
        // Every exit path funnels through `result` above: on success, our
        // entry was already popped by the SdpAnswer dispatch (discard is then
        // a harmless no-op); on any failure/timeout — including a rejected,
        // mismatched answer — it may still be queued and MUST be removed here
        // so a dead entry never blocks a later, correct answer from popping.
        if result.is_err() {
            self.pending_answers.discard(generation);
        }
        result
    }

    async fn renegotiate_inner(
        &self,
        rx: tokio::sync::oneshot::Receiver<RTCSessionDescription>,
    ) -> Result<()> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer).await?;
        let local = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after offer"))?;
        self.send(&HostMessage::SdpOffer { sdp: serde_json::to_string(&local)? }).await?;
        let answer = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .context("renegotiation answer timeout")?
            .map_err(|_| anyhow!("renegotiation answer channel dropped"))?;
        // The wire format carries no correlation id (see `PendingAnswerQueue`
        // doc), so a late answer for an abandoned renegotiation could still
        // reach here. Reject it unless it structurally matches the offer this
        // call just sent: a mismatch means it can't be the right answer.
        validate_answer_matches_offer(&local, &answer)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Called with the track_id whose remote receiver sent a PLI.
    pub fn on_pli(&self, f: impl Fn(String) + Send + Sync + 'static) {
        *self.on_pli.lock().unwrap() = Some(Arc::new(f));
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
    on_msg: HandlerSlot<HostMessage>,
}

impl ClientPeer {
    pub async fn new() -> Result<Self> {
        let pc = new_pc().await?;
        let dc = pc.create_data_channel("control", None).await?;
        let on_msg: HandlerSlot<HostMessage> = Arc::new(Mutex::new(None));

        let pc_handler = pc.clone();
        let dc_handler = dc.clone();
        let on_msg_slot = on_msg.clone();
        dc.on_message(Box::new(move |m: DataChannelMessage| {
            let pc = pc_handler.clone();
            let dc = dc_handler.clone();
            let on_msg_slot = on_msg_slot.clone();
            Box::pin(async move {
                match serde_json::from_slice::<HostMessage>(&m.data) {
                    Ok(HostMessage::SdpOffer { sdp }) => {
                        let offer: RTCSessionDescription = match serde_json::from_str(&sdp) {
                            Ok(o) => o,
                            Err(e) => {
                                warn!("bad SdpOffer payload: {e}");
                                return;
                            }
                        };
                        if let Err(e) = pc.set_remote_description(offer).await {
                            warn!("renegotiation set_remote: {e}");
                            return;
                        }
                        let answer = match pc.create_answer(None).await {
                            Ok(a) => a,
                            Err(e) => {
                                warn!("create_answer: {e}");
                                return;
                            }
                        };
                        if let Err(e) = pc.set_local_description(answer).await {
                            warn!("set_local: {e}");
                            return;
                        }
                        // ICE transport already established (BUNDLE); no gathering wait needed on renegotiation.
                        let local = pc.local_description().await.expect("local description set above");
                        let msg = ClientMessage::SdpAnswer { sdp: serde_json::to_string(&local).unwrap() };
                        if let Err(e) = dc.send_text(serde_json::to_string(&msg).unwrap()).await {
                            warn!("send SdpAnswer: {e}");
                        }
                    }
                    Ok(msg) => {
                        if let Some(f) = on_msg_slot.lock().unwrap().clone() {
                            f(msg);
                        }
                    }
                    Err(e) => warn!("malformed host message: {e}"),
                }
            })
        }));

        Ok(Self { pc, dc, on_msg })
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

    /// Must be called BEFORE `offer()`: webrtc-rs stores a single handler read at dispatch time,
    /// and a track arriving before registration is silently lost.
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
        *self.on_msg.lock().unwrap() = Some(Arc::new(f));
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

    /// Test-only: sends a PLI RTCP packet for the given media SSRC, to exercise
    /// the host's RTCP drain / `on_pli` callback path in the loopback test.
    pub async fn write_pli(&self, ssrc: u32) -> Result<()> {
        self.pc
            .write_rtcp(&[Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc })])
            .await
            .context("write_rtcp PLI")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_answer() -> RTCSessionDescription {
        RTCSessionDescription::answer("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_string())
            .unwrap()
    }

    // --- PendingAnswerQueue: plain, runtime-free tests against the exact
    // production push/discard/pop path (no HostPeer or RTCPeerConnection
    // needed — `oneshot::Sender::send` and `channel()` don't require a
    // runtime, only `.await`ing the receiver would).

    #[test]
    fn discarded_generation_is_not_delivered_to_next_renegotiation() {
        let queue = PendingAnswerQueue::new();

        // Renegotiation 1: pushed, then times out. renegotiate() would call
        // discard(gen1) on that path; simulate it directly.
        let (tx1, rx1) = tokio::sync::oneshot::channel::<RTCSessionDescription>();
        let gen1 = queue.push(tx1);
        queue.discard(gen1);
        drop(rx1); // the timed-out caller's receiver goes out of scope

        // Renegotiation 2 starts fresh (as it would after the lock is
        // released and reacquired).
        let (tx2, _rx2) = tokio::sync::oneshot::channel::<RTCSessionDescription>();
        let gen2 = queue.push(tx2);
        assert_ne!(gen1, gen2);

        // A late SdpAnswer "arrives" now. The queue must contain only
        // generation 2's entry — generation 1's was already discarded.
        let (next_gen, next_tx) = queue.pop().expect("generation 2 must still be queued");
        assert_eq!(next_gen, gen2, "a discarded generation must not resurface");
        assert!(queue.pop().is_none(), "queue must be empty after popping the only entry");

        // Routing succeeded (receiver not dropped): send returns Ok.
        assert!(next_tx.send(dummy_answer()).is_ok());
    }

    /// When two renegotiations are genuinely both still in flight (neither
    /// discarded), answers must be routed oldest-generation-first, matching
    /// the ordered data channel's delivery order.
    #[test]
    fn pending_answers_are_routed_fifo_by_generation() {
        let queue = PendingAnswerQueue::new();

        let (tx1, _rx1) = tokio::sync::oneshot::channel::<RTCSessionDescription>();
        let gen1 = queue.push(tx1);
        let (tx2, _rx2) = tokio::sync::oneshot::channel::<RTCSessionDescription>();
        let gen2 = queue.push(tx2);

        let (g, tx) = queue.pop().unwrap();
        assert_eq!(g, gen1);
        assert!(tx.send(dummy_answer()).is_ok());

        let (g, tx) = queue.pop().unwrap();
        assert_eq!(g, gen2);
        assert!(tx.send(dummy_answer()).is_ok());
    }

    /// A stray SdpAnswer with nothing queued for it must not panic; the real
    /// dispatch arm just warns and drops it.
    #[test]
    fn popping_an_empty_queue_returns_none() {
        let queue = PendingAnswerQueue::new();
        assert!(queue.pop().is_none());
    }

    // --- Answer/offer content validation ---

    fn sdp_with_mids(kind: &str, mids: &[&str]) -> RTCSessionDescription {
        let mut sdp = String::from("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n");
        for mid in mids {
            sdp.push_str(&format!("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:{mid}\r\n"));
        }
        match kind {
            "offer" => RTCSessionDescription::offer(sdp).unwrap(),
            "answer" => RTCSessionDescription::answer(sdp).unwrap(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn matching_answer_passes_validation() {
        let offer = sdp_with_mids("offer", &["0", "1"]);
        let answer = sdp_with_mids("answer", &["0", "1"]);
        assert!(validate_answer_matches_offer(&offer, &answer).is_ok());
    }

    /// The scenario this guards against: a stale answer for an earlier,
    /// differently-tracked offer arrives after a newer renegotiation's offer
    /// (e.g. one fewer track) has already been sent.
    #[test]
    fn structurally_mismatched_answer_is_rejected() {
        let offer = sdp_with_mids("offer", &["0"]);
        let stale_answer_for_a_bigger_offer = sdp_with_mids("answer", &["0", "1"]);
        assert!(validate_answer_matches_offer(&offer, &stale_answer_for_a_bigger_offer).is_err());
    }

    #[test]
    fn empty_offer_and_answer_are_considered_matching() {
        let offer = sdp_with_mids("offer", &[]);
        let answer = sdp_with_mids("answer", &[]);
        assert!(validate_answer_matches_offer(&offer, &answer).is_ok());
    }
}
