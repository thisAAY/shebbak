use srw_core::blit::{chunk_blit, BlitAssembler};
use srw_core::protocol::{ClientMessage, HostMessage};
use srw_transport::peer::{ClientPeer, HostPeer};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

#[tokio::test(flavor = "multi_thread")]
async fn peer_v2_runtime_tracks_renegotiation_and_pli() {
    let host = HostPeer::new().await.unwrap();
    let client = ClientPeer::new().await.unwrap();

    host.on_client_message(|_m: ClientMessage| {
        // No client->host data-channel messages are expected in this flow
        // (SdpAnswer is intercepted internally); registering avoids the
        // "dropped: no handler" warning path.
    });
    let (client_msg_tx, mut client_msg_rx) = mpsc::unbounded_channel();
    client.on_host_message(move |m| {
        let _ = client_msg_tx.send(m);
    });
    let (track_tx, mut track_rx) = mpsc::unbounded_channel();
    client.on_track(move |id, t| {
        let _ = track_tx.send((id, t));
    });

    // 1. Connect with ZERO tracks. Direct SDP exchange, no HTTP.
    let offer = client.offer().await.unwrap();
    let answer = host.answer(offer).await.unwrap();
    client.accept_answer(answer).await.unwrap();

    // 2. Await data channel open: retry a host send until it succeeds.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.send(&HostMessage::WindowClosed { window_id: 0 }).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("data channel did not open in time");

    // 3. Runtime track add, then renegotiate over the control channel.
    let track = host.add_track("win-1").await.unwrap();
    host.renegotiate().await.unwrap();

    // 4. Sample writer, held so a panic surfaces at the end of the test.
    let writer_track = track.clone();
    let writer = tokio::spawn(async move {
        let black = srw_core::pixels::BgraFrame { width: 64, height: 64, data: vec![0; 64 * 64 * 4] };
        let mut enc = srw_transport::codec::H264Encoder::new().unwrap();
        loop {
            if let Ok(Some(au)) = enc.encode_bgra(&black) {
                let _ = writer_track
                    .write_sample(&webrtc::media::Sample {
                        data: au.into(),
                        duration: Duration::from_millis(33),
                        ..Default::default()
                    })
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });

    // 5. The client's on_track fires with the new track, and RTP flows.
    let (track_id, remote_track) = tokio::time::timeout(Duration::from_secs(5), track_rx.recv())
        .await
        .expect("timed out waiting for on_track")
        .expect("on_track channel closed");
    assert_eq!(track_id, "win-1");
    tokio::time::timeout(Duration::from_secs(5), remote_track.read_rtp())
        .await
        .expect("timed out waiting for RTP")
        .expect("read_rtp failed");

    // 6. PLI path: client sends a PLI RTCP packet; host's on_pli fires.
    let (pli_tx, mut pli_rx) = mpsc::unbounded_channel();
    host.on_pli(move |tid| {
        let _ = pli_tx.send(tid);
    });
    client.write_pli(remote_track.ssrc()).await.unwrap();
    let pli_track_id = tokio::time::timeout(Duration::from_secs(5), pli_rx.recv())
        .await
        .expect("timed out waiting for PLI")
        .expect("pli channel closed");
    assert_eq!(pli_track_id, "win-1");

    // 7. Blit roundtrip over the control channel, reassembled client-side.
    let payload = vec![0x42u8; 40_000];
    for msg in chunk_blit(7, 1, &payload) {
        host.send(&msg).await.unwrap();
    }
    let mut assembler = BlitAssembler::new();
    let reassembled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = client_msg_rx.recv().await.expect("client message channel closed");
            if let Some(result) = assembler.push(&msg) {
                return result;
            }
        }
    })
    .await
    .expect("timed out waiting for blit reassembly");
    assert_eq!(reassembled, (7, payload));

    // 8. Runtime track remove, then renegotiate again; must not error.
    host.remove_track("win-1").await.unwrap();
    host.renegotiate().await.unwrap();

    // Sample writer must still be running (no panic) at this point.
    assert!(!writer.is_finished());
    writer.abort();

    // 9. Close both peers.
    client.close().await;
    host.close().await;
}

#[test]
fn signalling_roundtrip_over_http() {
    use srw_transport::signalling::{post_offer, serve_one_offer};
    let offer = RTCSessionDescription::offer("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_string()).unwrap();
    let answer_sdp = RTCSessionDescription::answer("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_string()).unwrap();

    let server = std::thread::spawn(move || {
        let (got_offer, responder) = serve_one_offer(19009).unwrap();
        let ans = answer_sdp;
        responder.respond(&ans).unwrap();
        got_offer
    });
    std::thread::sleep(Duration::from_millis(300)); // let the server bind
    let got_answer = post_offer("http://127.0.0.1:19009/offer", &offer).unwrap();
    let got_offer = server.join().unwrap();
    assert_eq!(got_offer.sdp, offer.sdp);
    assert!(got_answer.sdp.contains("o=- 1 1"));
}
