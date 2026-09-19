//! Regression test for sharing many windows at once: each shared window adds
//! a video m-section to the renegotiation offer (~3 KB of JSON-wrapped SDP),
//! and webrtc-rs SCTP rejects any single data-channel message over 64 KiB.
//! Before chunked control messages this failed around the 21st track with
//! "outbound packet larger than maximum message size" (reported as black
//! screens when sharing ~5 multi-window apps).

use srw_core::protocol::HostMessage;
use srw_transport::peer::{ClientPeer, HostPeer};
use std::sync::Arc;
use std::time::Duration;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

const TRACKS: u32 = 30;

fn spawn_black_frame_writer(track: Arc<TrackLocalStaticSample>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let black = srw_core::pixels::BgraFrame {
            width: 64,
            height: 64,
            data: vec![0; 64 * 64 * 4],
        };
        let mut enc = srw_transport::codec::H264Encoder::new().unwrap();
        loop {
            if let Ok(Some(au)) = enc.encode_bgra(&black) {
                let _ = track
                    .write_sample(&webrtc::media::Sample {
                        data: au.into(),
                        duration: Duration::from_millis(33),
                        ..Default::default()
                    })
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn renegotiation_survives_many_tracks() {
    let host = HostPeer::new().await.unwrap();
    let client = ClientPeer::new().await.unwrap();

    let (track_tx, mut track_rx) = tokio::sync::mpsc::unbounded_channel();
    client.on_track(move |id, t| {
        let _ = track_tx.send((id, t));
    });

    let offer = client.offer().await.unwrap();
    let answer = host.answer(offer).await.unwrap();
    client.accept_answer(answer).await.unwrap();

    // Await data channel open: retry a host send until it succeeds.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host
                .send(&HostMessage::WindowClosed { window_id: 0 })
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("data channel did not open in time");

    // Mirror the host session's per-window flow: add a track, renegotiate,
    // repeat. By the ~21st track the offer SDP exceeds the 64 KiB SCTP cap,
    // so the tail of this loop only passes with chunked control messages.
    let mut writers = Vec::new();
    for i in 1..=TRACKS {
        let track = host.add_track(&format!("win-{i}")).await.unwrap();
        host.renegotiate()
            .await
            .unwrap_or_else(|e| panic!("renegotiation failed at track {i}: {e:#}"));
        // Feed RTP into the first and last track so on_track (which fires on
        // the first packet) proves media flows after oversized renegotiations.
        if i == 1 || i == TRACKS {
            writers.push(spawn_black_frame_writer(track));
        }
    }

    let mut seen = std::collections::HashSet::new();
    while !(seen.contains("win-1") && seen.contains(&format!("win-{TRACKS}"))) {
        let (id, _t) = tokio::time::timeout(Duration::from_secs(10), track_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for tracks; got {seen:?}"))
            .expect("on_track channel closed");
        seen.insert(id);
    }

    for w in &writers {
        w.abort();
    }
    host.close().await;
    client.close().await;
}
