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

pub struct BlitAssembler {
    partials: HashMap<WindowId, Partial>,
    last_completed_seq: HashMap<WindowId, u32>,
}

impl BlitAssembler {
    pub fn new() -> Self {
        Self {
            partials: HashMap::new(),
            last_completed_seq: HashMap::new(),
        }
    }

    pub fn push(&mut self, msg: &HostMessage) -> Option<(WindowId, Vec<u8>)> {
        let HostMessage::TransientBlit { window_id, seq, chunk_index, chunk_count, data_b64 } = msg else {
            return None;
        };
        // Reject stale seq that has already completed
        if let Some(&completed_seq) = self.last_completed_seq.get(window_id) {
            if *seq <= completed_seq {
                return None;
            }
        }
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
            self.last_completed_seq.insert(*window_id, *seq);
            self.partials.remove(window_id);
            Some((*window_id, out))
        } else { None }
    }

    /// Drop all state for `window_id` — call when the host reports the
    /// window closed. Without this, `last_completed_seq` only ever grows,
    /// so a recycled CGWindowID's fresh seq=1 blits are silently rejected
    /// forever (stuck behind the dead window's high-water mark), and the
    /// maps grow unbounded across menu churn.
    pub fn forget(&mut self, window_id: WindowId) {
        self.partials.remove(&window_id);
        self.last_completed_seq.remove(&window_id);
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

    #[test]
    fn forget_lets_a_recycled_window_id_start_fresh() {
        // Complete seq 5 at window 9 (simulates a long-lived window with many blits).
        let payload = vec![9u8; 100];
        let msgs = chunk_blit(9, 5, &payload);
        let mut asm = BlitAssembler::new();
        assert_eq!(asm.push(&msgs[0]), Some((9, payload)));

        // Window 9's CGWindowID gets recycled for a brand-new window; without
        // forget(), its fresh seq=1 blit would be rejected forever as stale.
        asm.forget(9);
        let fresh_payload = vec![1u8; 50];
        let fresh = chunk_blit(9, 1, &fresh_payload);
        assert_eq!(asm.push(&fresh[0]), Some((9, fresh_payload)), "seq 1 must assemble after forget");
    }

    #[test]
    fn forget_does_not_affect_independent_windows() {
        let payload9 = vec![9u8; 100];
        let msgs9 = chunk_blit(9, 5, &payload9);
        let mut asm = BlitAssembler::new();
        assert_eq!(asm.push(&msgs9[0]), Some((9, payload9)));

        // window 3 has an in-flight partial (2 of 3 chunks) when window 9 is forgotten.
        let payload3: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let msgs3 = chunk_blit(3, 1, &payload3);
        assert_eq!(msgs3.len(), 3);
        assert_eq!(asm.push(&msgs3[0]), None);
        assert_eq!(asm.push(&msgs3[1]), None);

        asm.forget(9);

        // window 3's in-flight partial and high-water mark are untouched.
        assert_eq!(asm.push(&msgs3[2]), Some((3, payload3)));
        // and window 9's own stale seq-5 duplicate is still rejected before forget... but
        // after forget it's gone, so a stale seq-5 duplicate would now be treated as fresh.
        // That's the intended tradeoff of forget(): the window identity is gone entirely.
    }

    #[test]
    fn completed_seq_never_reassembles_from_stale_duplicate() {
        // Complete seq 1 at window 9
        let payload1 = vec![1u8; 100];
        let msgs1 = chunk_blit(9, 1, &payload1);
        let mut asm = BlitAssembler::new();
        let result1 = asm.push(&msgs1[0]);
        assert_eq!(result1, Some((9, payload1.clone())));

        // Stale duplicate of seq 1 arrives after seq 1 was evicted
        let stale_dup = &msgs1[0];
        let result_stale = asm.push(stale_dup);
        assert_eq!(result_stale, None, "stale duplicate must not reassemble");

        // Newer seq 2 should still work
        let payload2 = vec![2u8; 100];
        let msgs2 = chunk_blit(9, 2, &payload2);
        let result2 = asm.push(&msgs2[0]);
        assert_eq!(result2, Some((9, payload2)), "newer seq must still work");
    }
}
