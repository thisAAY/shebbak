//! Spawns the real srw-mirror-helper binary in probe mode over a
//! socketpair and drives a scripted window lifecycle through the IPC
//! framing — the process-boundary half of the coordinator/helper split,
//! without needing a WindowServer.

use srw_client::ipc::{self, DownFrame, UpMessage, HELPER_FD};
use srw_core::protocol::{ClientMessage, HostMessage};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn opened(window_id: u32) -> DownFrame {
    DownFrame::Control(HostMessage::WindowOpened {
        window_id,
        title: format!("win {window_id}"),
        width: 320.0,
        height: 240.0,
        track_id: format!("win-{window_id}"),
        app_id: 4242,
    })
}

#[test]
fn helper_probe_acks_window_lifecycle_over_socketpair() {
    let (mut coord, helper_end) = UnixStream::pair().unwrap();
    let fd = helper_end.as_raw_fd();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_srw-mirror-helper"));
    cmd.env("SRW_MIRROR_PROBE", "1");
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(fd, HELPER_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn helper");
    drop(helper_end);

    // Scripted lifecycle: icon and video frames must be skipped without
    // desyncing the stream; each WindowOpened must be acked upstream.
    ipc::write_down(&mut coord, &DownFrame::Icon(vec![0x89, b'P', b'N', b'G'])).unwrap();
    ipc::write_down(&mut coord, &opened(7)).unwrap();
    ipc::write_down(
        &mut coord,
        &DownFrame::Video {
            track_id: "win-7".into(),
            au: vec![0, 0, 0, 1, 0x67],
        },
    )
    .unwrap();
    ipc::write_down(
        &mut coord,
        &DownFrame::Control(HostMessage::WindowResized {
            window_id: 7,
            width: 100.0,
            height: 100.0,
        }),
    )
    .unwrap();
    ipc::write_down(&mut coord, &opened(9)).unwrap();

    let up = ipc::read_up(&mut coord).unwrap().expect("first ack");
    assert_eq!(
        up,
        UpMessage::Relay {
            msg: ClientMessage::FocusChange { window_id: 7 }
        }
    );
    let up = ipc::read_up(&mut coord).unwrap().expect("second ack");
    assert_eq!(
        up,
        UpMessage::Relay {
            msg: ClientMessage::FocusChange { window_id: 9 }
        }
    );

    // Closing our end is the shutdown signal; the helper must exit 0.
    drop(coord);
    let status = child.wait().expect("wait");
    assert!(status.success(), "helper exit status: {status:?}");
}
