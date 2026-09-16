use srw_core::protocol::{ClientMessage, HostMessage};
use srw_transport::peer::{ClientPeer, HostPeer};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

#[tokio::test(flavor = "multi_thread")]
async fn data_channel_roundtrip_over_local_peers() {
    let host = HostPeer::new().await.unwrap();
    let _track = host.add_track("win-1").await.unwrap();
    let client = ClientPeer::new(1).await.unwrap();

    let (host_rx_tx, mut host_rx) = mpsc::unbounded_channel();
    host.on_client_message(move |m| {
        let _ = host_rx_tx.send(m);
    });
    let (client_rx_tx, mut client_rx) = mpsc::unbounded_channel();
    client.on_host_message(move |m| {
        let _ = client_rx_tx.send(m);
    });
    let (track_tx, mut track_rx) = mpsc::unbounded_channel();
    client.on_track(move |id, _t| {
        let _ = track_tx.send(id);
    });

    // Direct SDP exchange, no HTTP.
    let offer = client.offer().await.unwrap();
    let answer = host.answer(offer).await.unwrap();
    client.accept_answer(answer).await.unwrap();

    // Write a few dummy samples so the client's on_track callback fires
    // (webrtc-rs only fires on_track once media/RTP starts flowing).
    let t = _track.clone();
    tokio::spawn(async move {
        let black = srw_core::pixels::BgraFrame { width: 64, height: 64, data: vec![0; 64 * 64 * 4] };
        let mut enc = srw_transport::codec::H264Encoder::new().unwrap();
        loop {
            if let Ok(Some(au)) = enc.encode_bgra(&black) {
                let _ = t
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

    // Client → host over the control channel.
    let msg = ClientMessage::CloseWindow { window_id: 1 };
    tokio::time::timeout(Duration::from_secs(15), async {
        // Retry until the channel opens.
        loop {
            if client.send(&msg).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(15), host_rx.recv()).await.unwrap().unwrap();
    assert_eq!(got, msg);

    // Host → client.
    let hmsg = HostMessage::WindowClosed { window_id: 1 };
    host.send(&hmsg).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(15), client_rx.recv()).await.unwrap().unwrap();
    assert_eq!(got, hmsg);

    // The host's track appears client-side with the binding key "win-1".
    let tid = tokio::time::timeout(Duration::from_secs(20), track_rx.recv()).await;
    match tid {
        Ok(Some(id)) => assert_eq!(id, "win-1"),
        _ => panic!("no remote track arrived"),
    }

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
