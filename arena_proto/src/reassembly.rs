//! Fragment reassembly-before-decrypt.
//!
//! ENet fragments are NOT independently encrypted: the whole pre-fragmentation
//! message is ChaCha20'd as one counter-0 stream, then ENet chops the
//! *ciphertext* into fragments. So a fragment slice can't be decrypted in
//! isolation (only fragment 0 would land right). We must concatenate every
//! fragment's ciphertext by `fragmentOffset` into a `totalLength` buffer and
//! decrypt the assembled buffer once.
//!
//! Ports `scripts/arena-decrypt.py`: `_Group`, `_walk_enet_for_fragments`
//! (via [`crate::enet::walk_fragments`]), `_try_assembled`,
//! `_reassemble_session_fragments`.

use std::collections::HashMap;

use crate::crypto::chacha20_legacy;
use crate::enet::{walk_fragments, STREAM_PLAINTEXT_LEADS};

/// A stream key candidate (decoded from `arena_session_keys`).
#[derive(Debug, Clone)]
pub struct KeyCandidate {
    pub id: i64,
    pub key: [u8; 32],
    pub nonce: [u8; 8],
}

/// One captured UDP frame row (subset of `arena_udp_frames` the pipeline needs).
#[derive(Debug, Clone)]
pub struct Frame {
    pub id: i64,
    pub direction: String, // "c2s" | "s2c"
    pub src_ip: Option<String>,
    pub src_port: Option<i64>,
    pub dst_ip: Option<String>,
    pub dst_port: Option<i64>,
    pub ciphertext: Vec<u8>,
    pub plaintext: Option<Vec<u8>>,
    pub decrypt_status: String,
    pub opcode: Option<i64>,
    pub decryption_key_id: Option<i64>,
}

impl Frame {
    /// The GameLift endpoint a frame's keys are indexed by: destination for
    /// c2s, source for s2c (mirrors `_reassemble_session_fragments`).
    pub fn gamelift(&self) -> (String, i64) {
        if self.direction == "c2s" {
            (
                self.dst_ip.clone().unwrap_or_default(),
                self.dst_port.unwrap_or(0),
            )
        } else {
            (
                self.src_ip.clone().unwrap_or_default(),
                self.src_port.unwrap_or(0),
            )
        }
    }
}

/// Reassembly group identity: `(gl_ip, gl_port, direction, channel, start_seq)`.
pub type GroupKey = (String, i64, String, u8, u16);

/// Largest buffer we'll allocate for a single reassembly group. Real arena
/// messages top out around 40 KiB; this guards against a corrupt `totalLength`
/// causing a giant allocation. Pathological frames that exceed it are dropped
/// (they would never be `decrypt_status='ok'` anyway, so parity is unaffected).
const MAX_GROUP_BYTES: usize = 8 << 20;

/// One reassembly group: ciphertext placed at fragment offsets, with
/// exactly-once coverage tracking. Port of `_Group`.
struct Group {
    total_length: usize,
    buffer: Vec<u8>,
    covered: Vec<(usize, usize)>, // (offset, len)
}

impl Group {
    fn new(total_length: usize) -> Self {
        Group {
            total_length,
            buffer: vec![0u8; total_length],
            covered: Vec::new(),
        }
    }

    /// Place a fragment's ciphertext at its offset. Returns false on an
    /// inconsistent placement (past end, or conflicting overlap). Exact
    /// duplicates (same bytes) are tolerated.
    fn add(&mut self, frag_offset: usize, data_length: usize, ct_slice: &[u8]) -> bool {
        if frag_offset + data_length > self.total_length {
            return false;
        }
        let existing = &self.buffer[frag_offset..frag_offset + data_length];
        if !self.covered.is_empty() {
            for &(o, l) in &self.covered {
                if o == frag_offset && l == data_length {
                    return existing == ct_slice; // duplicate: ok iff identical
                }
                // Overlap with a different range ⇒ reject.
                let disjoint = frag_offset + data_length <= o || o + l <= frag_offset;
                if !disjoint {
                    return false;
                }
            }
        }
        self.buffer[frag_offset..frag_offset + data_length].copy_from_slice(ct_slice);
        self.covered.push((frag_offset, data_length));
        true
    }

    /// True when `[0, total_length)` is covered exactly once (contiguous).
    fn is_complete(&self) -> bool {
        if self.covered.is_empty() {
            return false;
        }
        let mut ranges = self.covered.clone();
        ranges.sort_unstable();
        let mut cursor = 0usize;
        for (off, len) in ranges {
            if off != cursor {
                return false;
            }
            cursor = off + len;
        }
        cursor == self.total_length
    }
}

/// Try every candidate key on an assembled ciphertext. Winner = the key whose
/// decrypted byte 0 ∈ {0xAC, 0x84, 0xBE} (the de-facto auth of this unauthed
/// cipher). Port of `_try_assembled`.
pub fn try_assembled(assembled: &[u8], keys: &[KeyCandidate]) -> Option<(Vec<u8>, i64)> {
    if assembled.is_empty() {
        return None;
    }
    for kc in keys {
        let pt = chacha20_legacy(assembled, &kc.key, &kc.nonce);
        if let Some(&b0) = pt.first() {
            if STREAM_PLAINTEXT_LEADS.contains(&b0) {
                return Some((pt, kc.id));
            }
        }
    }
    None
}

/// Pass 0 for a session: discover every fragment group across all frames,
/// decrypt the complete ones, and return the assembled *plaintext* keyed by
/// group. Port of `_reassemble_session_fragments`.
pub fn reassemble_session(frames: &[Frame], keys: &[KeyCandidate]) -> HashMap<GroupKey, Vec<u8>> {
    let mut groups: HashMap<GroupKey, Group> = HashMap::new();

    for f in frames {
        let (gl_ip, gl_port) = f.gamelift();
        for frag in walk_fragments(&f.ciphertext) {
            let total = frag.total_length as usize;
            if total == 0 || total > MAX_GROUP_BYTES {
                continue;
            }
            let gkey: GroupKey = (
                gl_ip.clone(),
                gl_port,
                f.direction.clone(),
                frag.channel,
                frag.start_seq,
            );
            let grp = groups.entry(gkey).or_insert_with(|| Group::new(total));
            if grp.total_length != total {
                // Inconsistent group (startSeq wrap / corruption) — drop frag.
                continue;
            }
            let end = frag.ud_start + frag.data_length;
            if end > f.ciphertext.len() {
                continue;
            }
            let slice = &f.ciphertext[frag.ud_start..end];
            grp.add(frag.fragment_offset as usize, frag.data_length, slice);
        }
    }

    let mut results: HashMap<GroupKey, Vec<u8>> = HashMap::new();
    for (gkey, grp) in &groups {
        if !grp.is_complete() {
            continue;
        }
        if let Some((pt, _kid)) = try_assembled(&grp.buffer, keys) {
            results.insert(gkey.clone(), pt);
        }
    }
    results
}

/// Look up a fragment's plaintext slice from a reassembly map (the resolver
/// body). Port of the closure built by `_build_resolver`.
pub fn resolve_fragment(
    reassembly: &HashMap<GroupKey, Vec<u8>>,
    direction: &str,
    gl_ip: &str,
    gl_port: i64,
    channel: u8,
    start_seq: u16,
    frag_offset: u32,
    data_length: usize,
) -> Option<Vec<u8>> {
    let key: GroupKey = (
        gl_ip.to_string(),
        gl_port,
        direction.to_string(),
        channel,
        start_seq,
    );
    let assembled = reassembly.get(&key)?;
    let start = frag_offset as usize;
    let end = start + data_length;
    if end > assembled.len() {
        return None;
    }
    Some(assembled[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::chacha20_legacy;
    use crate::enet::{reconstruct_plaintext, ENET_CMD_SEND_FRAGMENT};

    const KEY: [u8; 32] = [9u8; 32];
    const NONCE: [u8; 8] = [8, 7, 6, 5, 4, 3, 2, 1];

    /// Build a single SEND_FRAGMENT command frame (peerID 0x3000, 2-byte hdr).
    fn frag_frame(start_seq: u16, frag_num: u32, total: u32, frag_offset: u32, ct: &[u8]) -> Vec<u8> {
        let mut f = vec![0x30, 0x00, ENET_CMD_SEND_FRAGMENT, 0];
        f.extend_from_slice(&0u16.to_be_bytes()); // reliableSeq (completes the 4-byte cmd header)
        f.extend_from_slice(&start_seq.to_be_bytes());
        f.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        f.extend_from_slice(&2u32.to_be_bytes()); // fragCount
        f.extend_from_slice(&frag_num.to_be_bytes());
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&frag_offset.to_be_bytes());
        f.extend_from_slice(ct);
        f
    }

    #[test]
    fn reassemble_two_fragments_then_decrypt() {
        // Whole message encrypted once, then split into two 6-byte halves.
        let message = [0xBEu8, 50, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a];
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        let (ct0, ct1) = full_ct.split_at(6);

        let frames = vec![
            Frame {
                id: 1,
                direction: "s2c".into(),
                src_ip: Some("3.78.254.65".into()),
                src_port: Some(5075),
                dst_ip: None,
                dst_port: None,
                ciphertext: frag_frame(5, 0, 12, 0, ct0),
                plaintext: None,
                decrypt_status: "pending".into(),
                opcode: None,
                decryption_key_id: None,
            },
            Frame {
                id: 2,
                direction: "s2c".into(),
                src_ip: Some("3.78.254.65".into()),
                src_port: Some(5075),
                dst_ip: None,
                dst_port: None,
                ciphertext: frag_frame(5, 1, 12, 6, ct1),
                plaintext: None,
                decrypt_status: "pending".into(),
                opcode: None,
                decryption_key_id: None,
            },
        ];
        let keys = vec![KeyCandidate { id: 42, key: KEY, nonce: NONCE }];

        let reassembly = reassemble_session(&frames, &keys);
        assert_eq!(reassembly.len(), 1, "one complete group");

        // Reconstructing frame 1 via the resolver yields the first 6 plaintext
        // bytes spliced into the ENet wrapper.
        let (gl_ip, gl_port) = frames[0].gamelift();
        let resolver = |ch: u8, ss: u16, fo: u32, dl: usize| {
            resolve_fragment(&reassembly, "s2c", &gl_ip, gl_port, ch, ss, fo, dl)
        };
        let out = reconstruct_plaintext(&frames[0].ciphertext, &KEY, &NONCE, Some(&resolver), false)
            .expect("decode frag0");
        assert_eq!(&out[out.len() - 6..], &message[..6]);
    }

    #[test]
    fn incomplete_group_not_resolved() {
        let frames = vec![Frame {
            id: 1,
            direction: "s2c".into(),
            src_ip: Some("1.2.3.4".into()),
            src_port: Some(5075),
            dst_ip: None,
            dst_port: None,
            ciphertext: frag_frame(5, 0, 12, 0, &[0u8; 6]), // only half of a 12-byte msg
            plaintext: None,
            decrypt_status: "pending".into(),
            opcode: None,
            decryption_key_id: None,
        }];
        let keys = vec![KeyCandidate { id: 1, key: KEY, nonce: NONCE }];
        assert!(reassemble_session(&frames, &keys).is_empty());
    }
}
