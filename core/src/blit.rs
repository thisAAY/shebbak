use crate::protocol::{HostMessage, WindowId};
use base64::Engine;
use std::collections::HashMap;

pub const BLIT_CHUNK_BYTES: usize = 16 * 1024;

pub fn chunk_blit(window_id: WindowId, seq: u32, png: &[u8]) -> Vec<HostMessage> {
    let chunks: Vec<&[u8]> = if png.is_empty() { vec![&[]] } else { png.chunks(BLIT_CHUNK_BYTES).collect() };
    let count = chunks.len() as u16;
    chunks.into_iter().enumerate().map(|(i, c)| HostMessage::TransientBlit {
        window_id, seq, chunk_index: i as u16, chunk_count: count,
        data_b64: base64::engine::general_purpose::STANDARD.encode(c),
    }).collect()
}

struct Partial { seq: u32, chunk_count: u16, received: HashMap<u16, Vec<u8>> }

pub struct BlitAssembler { partials: HashMap<WindowId, Partial> }

impl BlitAssembler {
    pub fn new() -> Self { Self { partials: HashMap::new() } }

    pub fn push(&mut self, msg: &HostMessage) -> Option<(WindowId, Vec<u8>)> {
        let HostMessage::TransientBlit { window_id, seq, chunk_index, chunk_count, data_b64 } = msg else {
            return None;
        };
        let data = base64::engine::general_purpose::STANDARD.decode(data_b64).ok()?;
        let entry = self.partials.entry(*window_id).or_insert_with(|| Partial {
            seq: *seq, chunk_count: *chunk_count, received: HashMap::new(),
        });
        if *seq < entry.seq { return None; }          // stale
        if *seq > entry.seq {                          // newer image supersedes partial
            *entry = Partial { seq: *seq, chunk_count: *chunk_count, received: HashMap::new() };
        }
        entry.received.insert(*chunk_index, data);
        if entry.received.len() as u16 == entry.chunk_count {
            let count = entry.chunk_count;
            let mut out = Vec::new();
            for i in 0..count { out.extend_from_slice(&entry.received[&i]); }
            self.partials.remove(window_id);
            Some((*window_id, out))
        } else { None }
    }
}

impl Default for BlitAssembler { fn default() -> Self { Self::new() } }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HostMessage;

    #[test]
    fn small_payload_is_one_chunk() {
        let msgs = chunk_blit(9, 1, b"hello");
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            HostMessage::TransientBlit { window_id: 9, seq: 1, chunk_index: 0, chunk_count: 1, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn chunk_and_reassemble_roundtrip() {
        let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect(); // 3 chunks
        let msgs = chunk_blit(9, 2, &payload);
        assert_eq!(msgs.len(), 3);
        let mut asm = BlitAssembler::new();
        let mut out = None;
        for m in &msgs { out = asm.push(m).or(out); }
        assert_eq!(out, Some((9, payload)));
    }

    #[test]
    fn newer_seq_discards_stale_partial() {
        let payload: Vec<u8> = vec![7u8; 40_000];
        let old = chunk_blit(9, 1, &payload);
        let new = chunk_blit(9, 2, &payload);
        let mut asm = BlitAssembler::new();
        asm.push(&old[0]); // partial seq 1
        for m in &new { asm.push(m); } // seq 2 completes
        // stale seq-1 chunks arriving late must not complete or corrupt
        assert_eq!(asm.push(&old[1]), None);
        assert_eq!(asm.push(&old[2]), None);
    }

    #[test]
    fn non_blit_messages_are_ignored() {
        let mut asm = BlitAssembler::new();
        assert_eq!(asm.push(&HostMessage::WindowClosed { window_id: 1 }), None);
    }
}
