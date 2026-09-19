//! Mirror helper: one process per shared host app. Spawned by the
//! coordinator from a per-app stub bundle with the IPC socketpair on fd 3.
//! Owns the app's mirror windows, decodes its tracks in-process, relays
//! input upstream. See docs/superpowers/specs/2026-09-19-per-app-dock-identity-design.md.

use srw_client::ipc::{self, DownFrame, UpMessage};
use srw_client::mirror_app::{exit_code, HelperApp, HelperEvent, ProgressThrottle};
use srw_core::protocol::{ClientMessage, HostMessage};
use srw_transport::codec::H264Decoder;
use std::collections::HashMap;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Instant;
use tracing::warn;
use winit::event_loop::EventLoop;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Fd 3 is dup2()'d in by the coordinator before exec.
    let stream = unsafe { UnixStream::from_raw_fd(ipc::HELPER_FD) };

    if std::env::var_os("SRW_MIRROR_PROBE").is_some() {
        return probe(stream);
    }

    let event_loop: EventLoop<()> = match EventLoop::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    let proxy = event_loop.create_proxy();
    let wake = move || {
        let _ = proxy.send_event(());
    };

    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<HelperEvent>();
    let (up_tx, up_rx) = std::sync::mpsc::channel::<UpMessage>();

    // Single ordered upstream writer (mirrors the M2 input_tx design: a
    // mouse Down/Up pair must never reorder on the socket).
    {
        let mut w = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("clone socket: {e}");
                return ExitCode::FAILURE;
            }
        };
        std::thread::Builder::new()
            .name("up-writer".into())
            .spawn(move || {
                for m in up_rx {
                    if ipc::write_up(&mut w, &m).is_err() {
                        return; // coordinator gone; reader thread handles exit
                    }
                }
            })
            .expect("spawn up-writer");
    }

    // IPC reader + decoder thread: control frames go to the UI thread,
    // video AUs are decoded here (decode-where-you-render: only the
    // decoded frame handoff to our own UI thread remains).
    {
        let ev_tx = ev_tx.clone();
        let up_tx = up_tx.clone();
        let wake = wake.clone();
        let mut r = stream;
        std::thread::Builder::new()
            .name("ipc-reader".into())
            .spawn(move || {
                let mut decoders: HashMap<String, H264Decoder> = HashMap::new();
                let mut throttle = ProgressThrottle::new();
                loop {
                    match ipc::read_down(&mut r) {
                        Ok(Some(DownFrame::Control(msg))) => {
                            let _ = ev_tx.send(HelperEvent::Host(msg));
                            wake();
                        }
                        Ok(Some(DownFrame::Icon(png))) => {
                            let _ = ev_tx.send(HelperEvent::Icon(png));
                            wake();
                        }
                        Ok(Some(DownFrame::Video { track_id, au })) => {
                            // A decoder that fails to construct panics the
                            // helper; the coordinator treats that as a crash
                            // and respawns once.
                            let dec = decoders
                                .entry(track_id.clone())
                                .or_insert_with(|| H264Decoder::new().expect("openh264 decoder"));
                            match dec.decode(&au) {
                                Ok(Some(frame)) => {
                                    if throttle.allow(&track_id, Instant::now()) {
                                        let _ = up_tx.send(UpMessage::DecodeProgress {
                                            track_id: track_id.clone(),
                                        });
                                    }
                                    let _ = ev_tx.send(HelperEvent::Frame { track_id, frame });
                                    wake();
                                }
                                Ok(None) => {}
                                Err(e) => warn!("{track_id}: decode error (dropped): {e}"),
                            }
                        }
                        Ok(None) => {
                            let _ = ev_tx.send(HelperEvent::Eof);
                            wake();
                            return;
                        }
                        Err(e) => {
                            warn!("ipc read: {e:#}");
                            let _ = ev_tx.send(HelperEvent::Eof);
                            wake();
                            return;
                        }
                    }
                }
            })
            .expect("spawn ipc-reader");
    }

    let mut app = HelperApp::new(ev_rx, up_tx);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("event loop: {e}");
        return ExitCode::FAILURE;
    }
    match app.exit_reason() {
        Some(reason) => ExitCode::from(exit_code(reason) as u8),
        None => ExitCode::SUCCESS,
    }
}

/// Test-only loopback (SRW_MIRROR_PROBE=1): no windows, no decode. Acks
/// each WindowOpened with a FocusChange relay and exits 0 on EOF, so the
/// integration test can exercise spawn + fd inheritance + framing.
fn probe(stream: UnixStream) -> ExitCode {
    let mut r = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return ExitCode::FAILURE,
    };
    let mut w = stream;
    while let Ok(Some(frame)) = ipc::read_down(&mut r) {
        if let DownFrame::Control(HostMessage::WindowOpened { window_id, .. }) = frame {
            let ok = ipc::write_up(
                &mut w,
                &UpMessage::Relay {
                    msg: ClientMessage::FocusChange { window_id },
                },
            )
            .is_ok();
            if !ok {
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
