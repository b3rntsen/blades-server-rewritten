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
//! (via [`crate::enet::walk_fragments`]), `_uuid_string_count`,
//! `_group_decode_is_real`, `_reassemble_session_fragments`.
//!
//! # Key selection: why byte 0 is NOT enough
//!
//! The obvious validator for this unauthenticated cipher — "the decrypted byte 0
//! is one of the three [`STREAM_PLAINTEXT_LEADS`] markers" — tests exactly ONE
//! keystream byte. With a few hundred candidate keys a *wrong* key clears it by
//! chance roughly 1-in-85, so first-hit-wins picks garbage long before it
//! reaches the real key. That is the documented `false-positive bug` in
//! `arena-decrypt.py` (it made session 385 look "decoded" when its big-message
//! key was never captured), and it is why the Python reference **abandoned**
//! `_try_assembled` (still present there, but dead code) in favour of the
//! endpoint-scoped, UUID-confirmed selector ported here as
//! [`select_endpoint_key`]:
//!
//!   1. Bucket every *complete* group by its GameLift endpoint.
//!   2. Try the **cross product** of the candidate keys × the candidate nonces
//!      (the Frida gadget mis-pairs key↔nonce, so the right pair can be
//!      `key[i]` with `nonce[j]`).
//!   3. Score each pair by the number of 36-char UUID *strings* it decodes
//!      across all of that endpoint's assembled buffers — a real arena message
//!      is name+UUID state and yields dozens-to-hundreds; a wrong key yields 0.
//!   4. Require [`REASSEMBLY_UUID_CONFIRM_MIN`]; below that the endpoint's key
//!      was simply never captured (honest no-key, leave the frames undecrypted).
//!   5. Accept a group's decode under the confirmed pair only if
//!      [`group_decode_is_real`] holds — the same endpoint can carry a group
//!      encrypted under a *different*, uncaptured key.

use std::collections::{HashMap, HashSet};

use crate::crypto::{chacha20_legacy, chacha20_legacy_xor};
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
///
/// **Known reference limitation (kept for byte parity).** This key does NOT
/// include the *client* side of the flow. Two concurrent connections from
/// different client ports to the same GameLift endpoint can therefore collide
/// on `(channel, startSeq)`; the second message's fragments are dropped by the
/// `total_length` guard in [`reassemble_session`], and its frames then resolve
/// against the *first* message's plaintext. `scripts/arena-decrypt.py` has the
/// same behaviour and its stored plaintext already carries the contamination —
/// see the module-level note in the parity report. Changing the key here would
/// break byte parity with the captured corpus, so it is deliberately unchanged.
pub type GroupKey = (String, i64, String, u8, u16);

/// Largest buffer we'll allocate for a single reassembly group. Real arena
/// messages top out around 40 KiB; this guards against a corrupt `totalLength`
/// causing a giant allocation. Oversized groups still *occupy* their group slot
/// (so later, differently-sized fragments for the same key are dropped exactly
/// as the Python reference drops them) but can never complete.
const MAX_GROUP_BYTES: usize = 8 << 20;

/// Minimum UUID strings a `(key, nonce)` pair must decode across an endpoint's
/// assembled groups before we believe it. Real decodes land far above; false
/// positives sit at 0. Port of `REASSEMBLY_UUID_CONFIRM_MIN`.
pub const REASSEMBLY_UUID_CONFIRM_MIN: usize = 4;

/// One reassembly group: ciphertext placed at fragment offsets, with
/// exactly-once coverage tracking. Port of `_Group`.
struct Group {
    total_length: usize,
    /// `None` when `total_length` exceeds [`MAX_GROUP_BYTES`] (or is 0): the
    /// group exists and blocks its key, but can never be filled.
    buffer: Option<Vec<u8>>,
    covered: Vec<(usize, usize)>, // (offset, len)
}

impl Group {
    fn new(total_length: usize) -> Self {
        let buffer = if total_length == 0 || total_length > MAX_GROUP_BYTES {
            None
        } else {
            Some(vec![0u8; total_length])
        };
        Group {
            total_length,
            buffer,
            covered: Vec::new(),
        }
    }

    /// Place a fragment's ciphertext at its offset. Returns false on an
    /// inconsistent placement (past end, or conflicting overlap). Exact
    /// duplicates (same bytes) are tolerated — the capture contains re-ingested
    /// duplicate frames, and arrival order is not assumed anywhere: placement
    /// is purely by `fragmentOffset`.
    fn add(&mut self, frag_offset: usize, data_length: usize, ct_slice: &[u8]) -> bool {
        if frag_offset + data_length > self.total_length {
            return false;
        }
        let Some(buffer) = self.buffer.as_mut() else {
            return false;
        };
        let existing = &buffer[frag_offset..frag_offset + data_length];
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
        buffer[frag_offset..frag_offset + data_length].copy_from_slice(ct_slice);
        self.covered.push((frag_offset, data_length));
        true
    }

    /// True when `[0, total_length)` is covered exactly once (contiguous).
    fn is_complete(&self) -> bool {
        if self.covered.is_empty() || self.buffer.is_none() {
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

#[inline]
fn is_hex_ascii(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'A'..=b'F')
}

/// Count non-overlapping 36-char UUID *strings* (`8-4-4-4-12` hex, ASCII).
///
/// Port of `_uuid_string_count`, which runs the regex over `pt.lower()` — hence
/// upper-case hex counts too. Leftmost-first, non-overlapping, exactly like
/// `re.findall`. A valid UUID string is effectively impossible to hit by chance,
/// which is what makes this a far stronger key validator than the 1-byte marker.
pub fn uuid_string_count(pt: &[u8]) -> usize {
    const UUID_LEN: usize = 36;
    if pt.len() < UUID_LEN {
        return 0;
    }
    let last = pt.len() - UUID_LEN;
    let mut count = 0usize;
    let mut i = 0usize;
    while i <= last {
        // Cheapest discriminator first: the four dashes at fixed slots.
        if pt[i + 8] == b'-'
            && pt[i + 13] == b'-'
            && pt[i + 18] == b'-'
            && pt[i + 23] == b'-'
            && pt[i..i + 8].iter().all(|&b| is_hex_ascii(b))
            && pt[i + 9..i + 13].iter().all(|&b| is_hex_ascii(b))
            && pt[i + 14..i + 18].iter().all(|&b| is_hex_ascii(b))
            && pt[i + 19..i + 23].iter().all(|&b| is_hex_ascii(b))
            && pt[i + 24..i + 36].iter().all(|&b| is_hex_ascii(b))
        {
            count += 1;
            i += UUID_LEN;
            continue;
        }
        i += 1;
    }
    count
}

/// Accept a fragment group's decode under the endpoint's UUID-confirmed key.
///
/// Real decodes are either name+UUID state (>= 1 UUID string) OR a valid
/// NetTransport marker on a zero-DOMINATED buffer (a small control message or a
/// zero-padded channel-0 transfer that carries no UUID). A wrong key yields
/// high-entropy noise: it clears the marker byte only ~1/256 AND is never
/// zero-dominated, so requiring BOTH stays false-positive-safe. Port of
/// `_group_decode_is_real`.
pub fn group_decode_is_real(pt: &[u8]) -> bool {
    if uuid_string_count(pt) > 0 {
        return true;
    }
    match pt.first() {
        Some(&b0) if STREAM_PLAINTEXT_LEADS.contains(&b0) => {
            let zeros = pt.iter().filter(|&&b| b == 0).count();
            // `zeros / len >= 0.5`, exactly, in integer arithmetic.
            zeros * 2 >= pt.len()
        }
        _ => false,
    }
}

/// The `(key, nonce)` cross-product pair the endpoint selector scores.
#[derive(Clone, Copy)]
struct Pair {
    kid: i64,
    key: [u8; 32],
    nonce: [u8; 8],
}

/// Build the deduplicated `key × nonce` cross product in the reference's order:
/// outer loop over candidates for the key, inner loop for the nonce, first
/// occurrence of a `(key, nonce)` signature wins (and carries the *outer*
/// candidate's id). Mirrors the `pairs`/`seen` construction in
/// `_reassemble_session_fragments`.
fn cross_product_pairs(keys: &[KeyCandidate]) -> Vec<Pair> {
    let mut seen: HashSet<([u8; 32], [u8; 8])> = HashSet::with_capacity(keys.len() * keys.len());
    let mut pairs = Vec::with_capacity(keys.len() * keys.len());
    for kc in keys {
        for other in keys {
            if !seen.insert((kc.key, other.nonce)) {
                continue;
            }
            pairs.push(Pair {
                kid: kc.id,
                key: kc.key,
                nonce: other.nonce,
            });
        }
    }
    pairs
}

/// Total UUID-string count a pair decodes across `buffers`.
///
/// The keystream only depends on `(key, nonce)`, and every buffer restarts at
/// counter 0, so we generate it **once** up to the longest buffer and XOR each
/// buffer against a prefix — exactly equivalent to decrypting each buffer
/// separately, but it collapses the ChaCha20 cost from `sum(len)` to `max(len)`.
fn score_pair(pair: &Pair, buffers: &[&[u8]], ks: &mut Vec<u8>, scratch: &mut Vec<u8>) -> usize {
    let max_len = buffers.iter().map(|b| b.len()).max().unwrap_or(0);
    ks.clear();
    ks.resize(max_len, 0);
    chacha20_legacy_xor(ks, &pair.key, &pair.nonce);
    let mut total = 0usize;
    for buf in buffers {
        scratch.clear();
        scratch.extend(buf.iter().zip(ks.iter()).map(|(a, b)| a ^ b));
        total += uuid_string_count(scratch);
    }
    total
}

/// Resolve every complete group at ONE GameLift endpoint.
///
/// Walks the `key × nonce` cross product in reference order and, for each
/// group, accepts the **first** pair whose decode satisfies
/// [`group_decode_is_real`]. Returns `(buffer index, plaintext, winning key id)`
/// for every group that resolved; groups whose key was never captured are
/// simply absent (honest no-key).
///
/// # Why per-group and not the reference's single per-endpoint argmax
///
/// `_reassemble_session_fragments` picks ONE `(key, nonce)` for the whole
/// endpoint (the pair with the highest total UUID count) and then filters each
/// group through `_group_decode_is_real`. That is a strictly weaker rule: an
/// endpoint routinely carries groups encrypted under *different* captured keys
/// — the arena rotates keys per match, and one GameLift ip:port is reused by
/// several matches. In the captured corpus, endpoint `35.182.254.124:5076`
/// alone has `ch4/startSeq1` decoded under key 35 while `ch4/startSeq26` and
/// `ch4/startSeq51` decode under key 47; a single-key-per-endpoint pass can
/// only ever resolve one of those sets. Applying the reference's own acceptance
/// predicate per group resolves both, and cannot be looser, because
/// `_group_decode_is_real` is exactly the false-positive-safe gate the
/// reference introduced to replace the 1-byte marker check: a wrong key must
/// produce either a real 36-char UUID string (impossible by chance) or a
/// marker byte on a >=50%-zero buffer (impossible for high-entropy noise).
///
/// The per-endpoint UUID score is retained as [`select_endpoint_key`] for
/// callers that want the reference's summary answer.
fn resolve_endpoint_groups(buffers: &[&[u8]], keys: &[KeyCandidate]) -> Vec<(usize, Vec<u8>, i64)> {
    if buffers.is_empty() || keys.is_empty() {
        return Vec::new();
    }
    let pairs = cross_product_pairs(keys);
    if pairs.is_empty() {
        return Vec::new();
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, pairs.len());
    let chunk = pairs.len().div_ceil(threads);

    // Per group: the lowest-indexed pair that produced a real decode.
    let per_chunk: Vec<Vec<Option<(usize, Vec<u8>, i64)>>> = if threads <= 1 {
        vec![scan_chunk(&pairs, 0, buffers)]
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = pairs
                .chunks(chunk)
                .enumerate()
                .map(|(ci, slice)| s.spawn(move || scan_chunk(slice, ci * chunk, buffers)))
                .collect();
            handles.into_iter().map(|h| h.join().expect("scan")).collect()
        })
    };

    let mut best: Vec<Option<(usize, Vec<u8>, i64)>> = vec![None; buffers.len()];
    for found in per_chunk {
        for (gi, hit) in found.into_iter().enumerate() {
            let Some(hit) = hit else { continue };
            match &best[gi] {
                Some(prev) if prev.0 <= hit.0 => {}
                _ => best[gi] = Some(hit),
            }
        }
    }
    best.into_iter()
        .enumerate()
        .filter_map(|(gi, hit)| hit.map(|(_pi, pt, kid)| (gi, pt, kid)))
        .collect()
}

/// Scan one contiguous slice of the pair list. For each buffer, records the
/// first (lowest global index) pair whose decode passes [`group_decode_is_real`]
/// and stops early once every buffer has a hit.
fn scan_chunk(
    pairs: &[Pair],
    base: usize,
    buffers: &[&[u8]],
) -> Vec<Option<(usize, Vec<u8>, i64)>> {
    let mut found: Vec<Option<(usize, Vec<u8>, i64)>> = vec![None; buffers.len()];
    let mut remaining = buffers.len();
    let max_len = buffers.iter().map(|b| b.len()).max().unwrap_or(0);
    let mut ks: Vec<u8> = Vec::with_capacity(max_len);
    let mut scratch: Vec<u8> = Vec::with_capacity(max_len);
    for (i, p) in pairs.iter().enumerate() {
        // One keystream per pair, reused across every buffer: each buffer
        // restarts at counter 0, so a prefix of the same keystream decrypts it.
        ks.clear();
        ks.resize(max_len, 0);
        chacha20_legacy_xor(&mut ks, &p.key, &p.nonce);
        for (gi, buf) in buffers.iter().enumerate() {
            if found[gi].is_some() {
                continue;
            }
            scratch.clear();
            scratch.extend(buf.iter().zip(ks.iter()).map(|(a, b)| a ^ b));
            if group_decode_is_real(&scratch) {
                found[gi] = Some((base + i, scratch.clone(), p.kid));
                remaining -= 1;
            }
        }
        if remaining == 0 {
            break;
        }
    }
    found
}

/// The reference's per-endpoint answer: the `(key_id, key, nonce, uuid_score)`
/// whose cross-product pair decodes the endpoint's assembled groups to the most
/// 36-char UUID strings, or `None` when nothing clears
/// [`REASSEMBLY_UUID_CONFIRM_MIN`] (honest no-key). Direct port of the selection
/// block in `_reassemble_session_fragments`; ties go to the first pair in
/// cross-product order (`u > best` in the reference) so the answer is
/// deterministic regardless of thread scheduling.
///
/// [`reassemble_session`] does NOT use this — see [`resolve_endpoint_groups`]
/// for why one key per endpoint is too coarse for the captured corpus.
pub fn select_endpoint_key(
    buffers: &[&[u8]],
    keys: &[KeyCandidate],
) -> Option<(i64, [u8; 32], [u8; 8], usize)> {
    if buffers.is_empty() || keys.is_empty() {
        return None;
    }
    let pairs = cross_product_pairs(keys);
    let mut ks: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut best: Option<(usize, usize)> = None; // (score, pair index)
    for (i, p) in pairs.iter().enumerate() {
        let u = score_pair(p, buffers, &mut ks, &mut scratch);
        if u > best.map_or(0, |b| b.0) {
            best = Some((u, i));
        }
    }
    let (score, idx) = best?;
    if score < REASSEMBLY_UUID_CONFIRM_MIN {
        return None;
    }
    let p = pairs[idx];
    Some((p.kid, p.key, p.nonce, score))
}

/// Try every candidate key on an assembled ciphertext, accepting the first whose
/// decrypted byte 0 ∈ [`STREAM_PLAINTEXT_LEADS`].
///
/// **Do not use this to select a reassembly key.** It is the direct port of
/// `_try_assembled`, which is *dead code* in `arena-decrypt.py` precisely
/// because a 1-byte gate false-positives across a large candidate set (see the
/// module docs). It survives only as a cheap "could this buffer plausibly be a
/// message at all" probe. [`reassemble_session`] uses [`select_endpoint_key`].
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
/// decrypt the complete ones under their endpoint's UUID-confirmed key, and
/// return the assembled *plaintext* keyed by group. Port of
/// `_reassemble_session_fragments`.
pub fn reassemble_session(frames: &[Frame], keys: &[KeyCandidate]) -> HashMap<GroupKey, Vec<u8>> {
    let mut groups: HashMap<GroupKey, Group> = HashMap::new();

    for f in frames {
        let (gl_ip, gl_port) = f.gamelift();
        for frag in walk_fragments(&f.ciphertext) {
            let total = frag.total_length as usize;
            let gkey: GroupKey = (
                gl_ip.clone(),
                gl_port,
                f.direction.clone(),
                frag.channel,
                frag.start_seq,
            );
            let grp = groups.entry(gkey).or_insert_with(|| Group::new(total));
            if grp.total_length != total {
                // Inconsistent group (startSeq collision between two concurrent
                // client flows / wrap / corruption) — drop this fragment. The
                // FIRST totalLength seen owns the group, exactly as in Python.
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

    // Bucket the COMPLETE groups by GameLift endpoint: the key is confirmed
    // per endpoint (over all of its groups at once), not per group.
    let mut by_gl: HashMap<(String, i64), Vec<(GroupKey, &[u8])>> = HashMap::new();
    for (gkey, grp) in &groups {
        if !grp.is_complete() {
            continue;
        }
        let Some(buf) = grp.buffer.as_deref() else {
            continue;
        };
        by_gl
            .entry((gkey.0.clone(), gkey.1))
            .or_default()
            .push((gkey.clone(), buf));
    }

    let mut results: HashMap<GroupKey, Vec<u8>> = HashMap::new();
    for (_gl, mut grouplist) in by_gl {
        // Deterministic scoring order (HashMap iteration is not stable). The
        // score is a plain sum so order cannot change the winner, but a stable
        // order keeps the harness reproducible run-to-run.
        grouplist.sort_by(|a, b| a.0.cmp(&b.0));
        let buffers: Vec<&[u8]> = grouplist.iter().map(|(_, b)| *b).collect();
        for (gi, pt, _kid) in resolve_endpoint_groups(&buffers, keys) {
            results.insert(grouplist[gi].0.clone(), pt);
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
    // A second, WRONG key that still happens to be a candidate.
    const OTHER_KEY: [u8; 32] = [3u8; 32];
    const OTHER_NONCE: [u8; 8] = [1, 1, 1, 1, 1, 1, 1, 1];

    /// A realistic-shaped arena message: 0xBE marker, an opcode, then several
    /// name+UUID pairs — enough UUID strings to clear
    /// `REASSEMBLY_UUID_CONFIRM_MIN`, which is what actually confirms a key.
    fn uuid_message(n_uuids: usize) -> Vec<u8> {
        let mut m = vec![0xBEu8, 54];
        for i in 0..n_uuids {
            m.extend_from_slice(b"Player");
            m.push(b'0' + (i % 10) as u8);
            m.extend_from_slice(
                format!("0badf00d-dead-beef-cafe-{:012x}", i).as_bytes(),
            );
        }
        m
    }

    fn keys() -> Vec<KeyCandidate> {
        vec![
            // Wrong key FIRST: a 1-byte byte-0 gate would be free to pick it.
            KeyCandidate { id: 1, key: OTHER_KEY, nonce: OTHER_NONCE },
            KeyCandidate { id: 42, key: KEY, nonce: NONCE },
        ]
    }

    /// Build a single SEND_FRAGMENT command frame (peerID 0x3000, 2-byte hdr).
    fn frag_frame(
        start_seq: u16,
        frag_num: u32,
        frag_count: u32,
        total: u32,
        frag_offset: u32,
        ct: &[u8],
    ) -> Vec<u8> {
        let mut f = vec![0x30, 0x00, ENET_CMD_SEND_FRAGMENT, 0];
        f.extend_from_slice(&0u16.to_be_bytes()); // reliableSeq (completes the 4-byte cmd header)
        f.extend_from_slice(&start_seq.to_be_bytes());
        f.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        f.extend_from_slice(&frag_count.to_be_bytes());
        f.extend_from_slice(&frag_num.to_be_bytes());
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&frag_offset.to_be_bytes());
        f.extend_from_slice(ct);
        f
    }

    fn s2c_frame(id: i64, port: i64, ciphertext: Vec<u8>) -> Frame {
        Frame {
            id,
            direction: "s2c".into(),
            src_ip: Some("3.78.254.65".into()),
            src_port: Some(port),
            dst_ip: Some("10.99.0.10".into()),
            dst_port: Some(40000 + id),
            ciphertext,
            plaintext: None,
            decrypt_status: "pending".into(),
            opcode: None,
            decryption_key_id: None,
        }
    }

    /// Split `ct` into `n` roughly equal fragments and return the frames, in the
    /// order given by `order` (frame ids follow the order they are emitted).
    fn fragment_frames(ct: &[u8], start_seq: u16, chunk: usize, order: &[usize]) -> Vec<Frame> {
        let total = ct.len();
        let n = total.div_ceil(chunk);
        order
            .iter()
            .enumerate()
            .map(|(emit_i, &fi)| {
                assert!(fi < n, "test bug: fragment {fi} of only {n}");
                let off = fi * chunk;
                let end = (off + chunk).min(total);
                s2c_frame(
                    emit_i as i64 + 1,
                    5074,
                    frag_frame(
                        start_seq,
                        fi as u32,
                        n as u32,
                        total as u32,
                        off as u32,
                        &ct[off..end],
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn uuid_string_count_matches_reference_regex() {
        assert_eq!(uuid_string_count(b"nope"), 0);
        assert_eq!(
            uuid_string_count(b"x0badf00d-dead-beef-cafe-000000000001y"),
            1
        );
        // Upper case counts (the reference lowercases before matching).
        assert_eq!(
            uuid_string_count(b"0BADF00D-DEAD-BEEF-CAFE-000000000001"),
            1
        );
        // Non-overlapping, leftmost-first: two back-to-back UUIDs = 2.
        let mut two = Vec::new();
        two.extend_from_slice(b"0badf00d-dead-beef-cafe-000000000001");
        two.extend_from_slice(b"0badf00d-dead-beef-cafe-000000000002");
        assert_eq!(uuid_string_count(&two), 2);
        // A dash in the wrong slot does not match.
        assert_eq!(
            uuid_string_count(b"0badf00d-dead-beef-caf-e000000000001"),
            0
        );
    }

    #[test]
    fn group_decode_is_real_gates() {
        // UUID present ⇒ real.
        assert!(group_decode_is_real(&uuid_message(1)));
        // Marker + zero-dominated ⇒ real (small control / zero-padded transfer).
        let mut padded = vec![0xBEu8, 0x37, 1, 2, 3];
        padded.extend_from_slice(&[0u8; 64]);
        assert!(group_decode_is_real(&padded));
        // Marker but high-entropy, no UUID ⇒ NOT real (wrong-key garbage).
        let noise: Vec<u8> = std::iter::once(0xBEu8).chain(1u8..=200).collect();
        assert!(!group_decode_is_real(&noise));
        // No marker at all ⇒ not real.
        assert!(!group_decode_is_real(&[0x11u8; 128]));
    }

    #[test]
    fn reassemble_two_fragments_then_decrypt() {
        let message = uuid_message(6);
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        let chunk = message.len().div_ceil(2);
        let frames = fragment_frames(&full_ct, 5, chunk, &[0, 1]);

        let reassembly = reassemble_session(&frames, &keys());
        assert_eq!(reassembly.len(), 1, "one complete group");
        let got = reassembly.values().next().unwrap();
        assert_eq!(got, &message, "assembled plaintext must be the whole message");

        // Reconstructing frame 0 via the resolver yields its plaintext slice
        // spliced into the ENet wrapper.
        let (gl_ip, gl_port) = frames[0].gamelift();
        let resolver = |ch: u8, ss: u16, fo: u32, dl: usize| {
            resolve_fragment(&reassembly, "s2c", &gl_ip, gl_port, ch, ss, fo, dl)
        };
        let out = reconstruct_plaintext(&frames[0].ciphertext, &KEY, &NONCE, Some(&resolver), false)
            .expect("decode frag0");
        assert_eq!(&out[out.len() - chunk..], &message[..chunk]);
    }

    /// The defect this module was fixed for: with several candidate keys, a
    /// *wrong* one can clear the 1-byte marker gate on the assembled buffer.
    /// The endpoint selector must ignore it and pick the UUID-confirmed key.
    #[test]
    fn byte0_false_positive_key_is_not_selected() {
        let message = uuid_message(8);
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        let frames = fragment_frames(&full_ct, 5, 96, &[0, 1, 2, 3]);

        // Find a bogus key whose decrypt of the assembled buffer starts with a
        // legal marker byte — i.e. exactly the false positive `try_assembled`
        // would accept — and put it FIRST in the candidate list.
        let mut decoy: Option<KeyCandidate> = None;
        for seed in 0u8..=255 {
            let k = [seed; 32];
            let pt = chacha20_legacy(&full_ct, &k, &OTHER_NONCE);
            if STREAM_PLAINTEXT_LEADS.contains(&pt[0]) {
                decoy = Some(KeyCandidate { id: 900 + seed as i64, key: k, nonce: OTHER_NONCE });
                break;
            }
        }
        let decoy = decoy.expect("a byte-0 false positive exists among 256 seeds");
        let keys = vec![decoy.clone(), KeyCandidate { id: 42, key: KEY, nonce: NONCE }];

        // try_assembled (the dead-code 1-byte gate) really is fooled...
        let (bad_pt, bad_id) = try_assembled(&full_ct, &keys).expect("byte-0 gate accepts");
        assert_eq!(bad_id, decoy.id);
        assert_ne!(bad_pt, message);

        // ...but reassemble_session is not.
        let reassembly = reassemble_session(&frames, &keys);
        assert_eq!(reassembly.len(), 1);
        assert_eq!(reassembly.values().next().unwrap(), &message);
    }

    /// Fragments are placed by `fragmentOffset`, never by arrival order, and
    /// exact duplicates (the capture holds re-ingested frames) are tolerated.
    #[test]
    fn out_of_order_and_duplicate_fragments() {
        let message = uuid_message(10);
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        // Shuffled arrival (7 fragments), with fragments 1 and 3 delivered twice.
        let mut frames = fragment_frames(&full_ct, 7, 64, &[3, 0, 6, 4, 1, 5, 2, 1, 3]);
        // Interleave an unrelated endpoint so grouping really is keyed, not positional.
        frames.push(s2c_frame(99, 5099, vec![0x30, 0x00, 5, 0]));

        let reassembly = reassemble_session(&frames, &keys());
        assert_eq!(reassembly.len(), 1, "duplicates must not create a second group");
        assert_eq!(reassembly.values().next().unwrap(), &message);
    }

    /// A group that legitimately never completes must stay unresolved — never
    /// emit a partially-filled (zero-padded) buffer as if it were plaintext.
    #[test]
    fn incomplete_group_not_resolved() {
        let message = uuid_message(10);
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        // Everything except fragment 2 ⇒ a hole in the coverage.
        let frames = fragment_frames(&full_ct, 7, 64, &[0, 1, 3, 4, 5, 6]);
        assert!(reassemble_session(&frames, &keys()).is_empty());
    }

    /// Two concurrent client flows to the same GameLift endpoint can collide on
    /// `(channel, startSeq)`. The FIRST `totalLength` seen owns the group and
    /// the other flow's fragments are dropped — this is the Python reference's
    /// behaviour and the captured corpus depends on it byte-for-byte.
    #[test]
    fn colliding_start_seq_first_total_length_wins() {
        let msg_a = uuid_message(10);
        let msg_b = uuid_message(4); // different length ⇒ different totalLength
        assert_ne!(msg_a.len(), msg_b.len());
        let ct_a = chacha20_legacy(&msg_a, &KEY, &NONCE);
        let ct_b = chacha20_legacy(&msg_b, &KEY, &NONCE);

        let mut frames = fragment_frames(&ct_a, 10, 64, &[0, 1, 2, 3, 4, 5, 6]);
        let n_a = frames.len();
        // Same start_seq 10, same endpoint, different client port + totalLength.
        for (i, f) in fragment_frames(&ct_b, 10, 64, &[0, 1, 2]).into_iter().enumerate() {
            let mut f = f;
            f.id = (n_a + i) as i64 + 100;
            f.dst_port = Some(47209); // a DIFFERENT client flow
            frames.push(f);
        }

        let reassembly = reassemble_session(&frames, &keys());
        assert_eq!(reassembly.len(), 1, "collided flows share one group slot");
        assert_eq!(
            reassembly.values().next().unwrap(),
            &msg_a,
            "the first totalLength seen owns the group"
        );
    }

    /// No candidate key decodes the endpoint ⇒ honest no-key, nothing emitted.
    #[test]
    fn uncaptured_key_yields_no_group() {
        let message = uuid_message(6);
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        let frames = fragment_frames(&full_ct, 5, 96, &[0, 1, 2]);
        let wrong = vec![KeyCandidate { id: 1, key: OTHER_KEY, nonce: OTHER_NONCE }];
        assert!(reassemble_session(&frames, &wrong).is_empty());
    }

    /// The cross product must recover a MIS-PAIRED key↔nonce: the Frida gadget
    /// files `key[i]` next to the wrong nonce, so the correct combination only
    /// exists as `key` from one row × `nonce` from another.
    #[test]
    fn cross_product_recovers_mispaired_key_and_nonce() {
        let message = uuid_message(8);
        let full_ct = chacha20_legacy(&message, &KEY, &NONCE);
        let frames = fragment_frames(&full_ct, 5, 96, &[0, 1, 2, 3]);
        // Neither row is the right (key, nonce) on its own.
        let mispaired = vec![
            KeyCandidate { id: 111, key: KEY, nonce: OTHER_NONCE },
            KeyCandidate { id: 113, key: OTHER_KEY, nonce: NONCE },
        ];
        let reassembly = reassemble_session(&frames, &mispaired);
        assert_eq!(reassembly.len(), 1, "cross product must find key111 × nonce113");
        assert_eq!(reassembly.values().next().unwrap(), &message);
    }

    /// The regression this module exists for: ONE GameLift endpoint carrying two
    /// groups encrypted under two DIFFERENT captured keys. The reference's
    /// single-key-per-endpoint argmax can only ever resolve one of them (it is
    /// what leaves `35.182.254.124:5076 ch4/startSeq1` unresolved in the
    /// corpus); per-group acceptance resolves both.
    #[test]
    fn endpoint_with_two_different_keys_resolves_both_groups() {
        let msg_a = uuid_message(8);
        let msg_b = uuid_message(12);
        let ct_a = chacha20_legacy(&msg_a, &KEY, &NONCE);
        let ct_b = chacha20_legacy(&msg_b, &OTHER_KEY, &OTHER_NONCE);

        let mut frames = fragment_frames(&ct_a, 1, 96, &[0, 1, 2, 3]);
        for (i, mut f) in fragment_frames(&ct_b, 26, 96, &[0, 1, 2, 3, 4, 5])
            .into_iter()
            .enumerate()
        {
            f.id = 300 + i as i64;
            frames.push(f);
        }
        let ks = vec![
            KeyCandidate { id: 35, key: KEY, nonce: NONCE },
            KeyCandidate { id: 47, key: OTHER_KEY, nonce: OTHER_NONCE },
        ];
        let reassembly = reassemble_session(&frames, &ks);
        assert_eq!(reassembly.len(), 2, "both keys must be used at one endpoint");
        let mut got: Vec<&Vec<u8>> = reassembly.values().collect();
        got.sort();
        let mut want = vec![&msg_a, &msg_b];
        want.sort();
        assert_eq!(got, want);

        // The reference's per-endpoint summary answer still works, and — being
        // one key for the whole endpoint — can only account for one of them.
        let buffers: Vec<&[u8]> = vec![&ct_a, &ct_b];
        let (kid, _k, _n, score) =
            select_endpoint_key(&buffers, &ks).expect("one endpoint key is confirmable");
        assert!(score >= REASSEMBLY_UUID_CONFIRM_MIN);
        assert_eq!(kid, 47, "msg_b has more UUIDs, so its key wins the argmax");
    }

    /// `select_endpoint_key` reports honest no-key when nothing clears the gate.
    #[test]
    fn select_endpoint_key_reports_no_key() {
        let msg = uuid_message(8);
        let ct = chacha20_legacy(&msg, &KEY, &NONCE);
        let buffers: Vec<&[u8]> = vec![&ct];
        let wrong = vec![KeyCandidate { id: 1, key: OTHER_KEY, nonce: OTHER_NONCE }];
        assert!(select_endpoint_key(&buffers, &wrong).is_none());
        assert!(select_endpoint_key(&[], &wrong).is_none());
    }

    /// One group at an endpoint may be encrypted under a key we never captured.
    /// The endpoint key is still confirmed by the OTHER group, and the
    /// undecodable one must be dropped rather than emitted as garbage.
    #[test]
    fn endpoint_confirmed_but_one_group_stays_garbage() {
        let good = uuid_message(8);
        let good_ct = chacha20_legacy(&good, &KEY, &NONCE);
        let other = uuid_message(9);
        let other_ct = chacha20_legacy(&other, &OTHER_KEY, &OTHER_NONCE); // uncaptured key

        let mut frames = fragment_frames(&good_ct, 5, 96, &[0, 1, 2, 3]);
        for (i, mut f) in fragment_frames(&other_ct, 60, 96, &[0, 1, 2, 3, 4])
            .into_iter()
            .enumerate()
        {
            f.id = 200 + i as i64;
            frames.push(f);
        }
        // Only the good key is a candidate.
        let ks = vec![KeyCandidate { id: 42, key: KEY, nonce: NONCE }];
        let reassembly = reassemble_session(&frames, &ks);
        assert_eq!(reassembly.len(), 1, "only the decodable group survives");
        assert_eq!(reassembly.values().next().unwrap(), &good);
    }
}
