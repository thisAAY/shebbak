//! Coordinator ↔ mirror-helper IPC over an inherited socketpair.
//!
//! Frame layout: `u32` LE length (of tag + payload), one tag byte, payload.
//! Control/up messages are JSON (reusing the wire enums); video access
//! units and the icon PNG are raw bytes — never JSON-escaped.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use srw_core::protocol::{ClientMessage, HostMessage};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::RawFd;

/// The helper's end of the socketpair is dup2()'d onto this fd at spawn.
pub const HELPER_FD: RawFd = 3;

const TAG_CONTROL: u8 = 0;
const TAG_VIDEO: u8 = 1;
const TAG_ICON: u8 = 2;
/// Sanity cap: an access unit is a few hundred KB at worst; 32 MiB means
/// a desynced stream fails fast instead of allocating gigabytes.
const MAX_FRAME: u32 = 32 * 1024 * 1024;

/// Coordinator → helper.
#[derive(Debug, Clone, PartialEq)]
pub enum DownFrame {
    /// Window lifecycle / input mapping: the HostMessage forwarded verbatim.
    Control(HostMessage),
    /// One depacketized H.264 Annex-B access unit for a track.
    Video { track_id: String, au: Vec<u8> },
    /// The shared app's Dock icon, raw PNG bytes.
    Icon(Vec<u8>),
}

/// Helper → coordinator (always JSON under TAG_CONTROL).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UpMessage {
    /// Input/window events to relay onto the control data channel.
    Relay { msg: ClientMessage },
    /// A frame decoded OK on this track (throttled); feeds the
    /// coordinator's PLI stall detector.
    DecodeProgress { track_id: String },
}

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> Result<()> {
    let len = 1 + payload.len();
    ensure!(len <= MAX_FRAME as usize, "frame too large: {len} bytes");
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&[tag])?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

fn read_frame(r: &mut impl Read) -> Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    let mut filled = 0;

    loop {
        match r.read(&mut len_buf[filled..]) {
            Ok(0) => {
                // EOF reached. Clean shutdown only if zero bytes were read.
                if filled == 0 {
                    return Ok(None);
                } else {
                    return Err(anyhow::anyhow!(
                        "EOF inside length header (read {}/4 bytes)",
                        filled
                    ));
                }
            }
            Ok(n) => {
                filled += n;
                if filled == 4 {
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                continue;
            }
            Err(e) => {
                return Err(e).context("read frame length");
            }
        }
    }

    let len = u32::from_le_bytes(len_buf);
    ensure!((1..=MAX_FRAME).contains(&len), "bad frame length {len}");
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).context("read frame body")?;
    let tag = buf[0];
    let payload = buf.split_off(1);
    Ok(Some((tag, payload)))
}

pub fn write_down(w: &mut impl Write, f: &DownFrame) -> Result<()> {
    match f {
        DownFrame::Control(msg) => {
            write_frame(w, TAG_CONTROL, serde_json::to_string(msg)?.as_bytes())
        }
        DownFrame::Icon(png) => write_frame(w, TAG_ICON, png),
        DownFrame::Video { track_id, au } => {
            ensure!(track_id.len() <= u16::MAX as usize, "track id too long");
            let mut p = Vec::with_capacity(2 + track_id.len() + au.len());
            p.extend_from_slice(&(track_id.len() as u16).to_le_bytes());
            p.extend_from_slice(track_id.as_bytes());
            p.extend_from_slice(au);
            write_frame(w, TAG_VIDEO, &p)
        }
    }
}

pub fn read_down(r: &mut impl Read) -> Result<Option<DownFrame>> {
    let Some((tag, payload)) = read_frame(r)? else {
        return Ok(None);
    };
    match tag {
        TAG_CONTROL => Ok(Some(DownFrame::Control(
            serde_json::from_slice(&payload).context("parse control frame")?,
        ))),
        TAG_ICON => Ok(Some(DownFrame::Icon(payload))),
        TAG_VIDEO => {
            ensure!(payload.len() >= 2, "video frame too short");
            let tid_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
            ensure!(
                payload.len() >= 2 + tid_len,
                "video frame track id truncated"
            );
            let track_id = std::str::from_utf8(&payload[2..2 + tid_len])
                .context("track id not utf8")?
                .to_string();
            Ok(Some(DownFrame::Video {
                track_id,
                au: payload[2 + tid_len..].to_vec(),
            }))
        }
        other => bail!("unknown down-frame tag {other}"),
    }
}

pub fn write_up(w: &mut impl Write, m: &UpMessage) -> Result<()> {
    write_frame(w, TAG_CONTROL, serde_json::to_string(m)?.as_bytes())
}

pub fn read_up(r: &mut impl Read) -> Result<Option<UpMessage>> {
    let Some((tag, payload)) = read_frame(r)? else {
        return Ok(None);
    };
    ensure!(tag == TAG_CONTROL, "unexpected up-frame tag {tag}");
    Ok(Some(
        serde_json::from_slice(&payload).context("parse up message")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use srw_core::protocol::{ClientMessage, HostMessage};
    use std::io::Cursor;

    fn roundtrip_down(f: DownFrame) -> DownFrame {
        let mut buf = Vec::new();
        write_down(&mut buf, &f).unwrap();
        read_down(&mut Cursor::new(buf)).unwrap().unwrap()
    }

    #[test]
    fn control_frame_roundtrips() {
        let f = DownFrame::Control(HostMessage::WindowMinimized { window_id: 9 });
        assert_eq!(roundtrip_down(f.clone()), f);
    }

    #[test]
    fn video_frame_roundtrips_binary_payload() {
        let f = DownFrame::Video {
            track_id: "win-42".into(),
            au: vec![0, 0, 0, 1, 0x65, 0xFF, 0x00, 0x88],
        };
        assert_eq!(roundtrip_down(f.clone()), f);
    }

    #[test]
    fn icon_frame_roundtrips() {
        let f = DownFrame::Icon(vec![0x89, b'P', b'N', b'G']);
        assert_eq!(roundtrip_down(f.clone()), f);
    }

    #[test]
    fn up_messages_roundtrip() {
        for m in [
            UpMessage::Relay {
                msg: ClientMessage::FocusChange { window_id: 7 },
            },
            UpMessage::DecodeProgress {
                track_id: "win-7".into(),
            },
        ] {
            let mut buf = Vec::new();
            write_up(&mut buf, &m).unwrap();
            assert_eq!(read_up(&mut Cursor::new(buf)).unwrap().unwrap(), m);
        }
    }

    #[test]
    fn several_frames_stream_in_order() {
        let mut buf = Vec::new();
        write_down(&mut buf, &DownFrame::Icon(vec![1, 2, 3])).unwrap();
        write_down(
            &mut buf,
            &DownFrame::Video {
                track_id: "t".into(),
                au: vec![9],
            },
        )
        .unwrap();
        write_down(
            &mut buf,
            &DownFrame::Control(HostMessage::WindowRestored { window_id: 1 }),
        )
        .unwrap();
        let mut r = Cursor::new(buf);
        assert!(matches!(
            read_down(&mut r).unwrap().unwrap(),
            DownFrame::Icon(_)
        ));
        assert!(matches!(
            read_down(&mut r).unwrap().unwrap(),
            DownFrame::Video { .. }
        ));
        assert!(matches!(
            read_down(&mut r).unwrap().unwrap(),
            DownFrame::Control(_)
        ));
        assert!(read_down(&mut r).unwrap().is_none(), "clean EOF -> None");
    }

    #[test]
    fn truncated_frame_is_an_error_not_eof() {
        let mut buf = Vec::new();
        write_down(&mut buf, &DownFrame::Icon(vec![1, 2, 3, 4])).unwrap();
        buf.truncate(buf.len() - 2);
        assert!(read_down(&mut Cursor::new(buf)).is_err());
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.push(0);
        assert!(read_down(&mut Cursor::new(buf)).is_err());
    }

    #[test]
    fn eof_inside_length_header_is_an_error() {
        let mut buf = Vec::new();
        write_down(&mut buf, &DownFrame::Icon(vec![1, 2, 3])).unwrap();
        // Append 2 stray bytes (simulating helper crash mid-write of next frame header).
        buf.push(0xAB);
        buf.push(0xCD);

        let mut r = Cursor::new(buf);
        // First read succeeds (valid frame).
        assert!(matches!(
            read_down(&mut r).unwrap().unwrap(),
            DownFrame::Icon(_)
        ));
        // Second read hits EOF in the middle of the 4-byte length header.
        assert!(
            read_down(&mut r).is_err(),
            "partial length header should be error"
        );
    }
}
