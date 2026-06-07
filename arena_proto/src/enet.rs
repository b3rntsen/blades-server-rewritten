//! ENet wire walk + plaintext reconstruction + opcode/marker gates.
//!
//! Direct ports of `scripts/arena-decrypt.py`:
//!   - `reconstruct_plaintext`   == `_try_stream`
//!   - `first_opcode_in_plaintext` == `_first_opcode_in_plaintext`
//!   - `first_marker_in_plaintext` == `_first_marker_in_plaintext`
//!   - `walk_fragments`          == `_walk_enet_for_fragments`
//!
//! These MUST stay byte-for-byte faithful: the replay harness asserts the
//! reconstructed packet equals the `plaintext` BLOB the Python worker stored.
//!
//! Wire facts (`docs/arena-udp-protocol.md`): a 2-byte ENet protocol header
//! (peerID), extended to 4 bytes (peerID + sentTime) when bit `0x4000` of peerID
//! is set — Blades' build uses `0x4000`, not vanilla ENet's `0x8000`. Only the
//! user-data of the SEND_* command family is ChaCha20-encrypted; headers,
//! length prefixes, and control commands are plaintext on the wire.

use crate::crypto::chacha20_legacy;
use crate::opcodes::is_game_message_id;

pub const ENET_HDR_FLAG_SENT_TIME: u16 = 0x4000;

pub const ENET_CMD_ACK: u8 = 1;
pub const ENET_CMD_CONNECT: u8 = 2;
pub const ENET_CMD_VERIFY_CONNECT: u8 = 3;
pub const ENET_CMD_DISCONNECT: u8 = 4;
pub const ENET_CMD_PING: u8 = 5;
pub const ENET_CMD_SEND_RELIABLE: u8 = 6;
pub const ENET_CMD_SEND_UNRELIABLE: u8 = 7;
pub const ENET_CMD_SEND_FRAGMENT: u8 = 8;
pub const ENET_CMD_SEND_UNSEQUENCED: u8 = 9;
pub const ENET_CMD_BANDWIDTH_LIMIT: u8 = 10;
pub const ENET_CMD_THROTTLE_CONFIGURE: u8 = 11;
pub const ENET_CMD_SEND_UNRELIABLE_FRAGMENT: u8 = 12;

/// Per-message marker byte (`user_data[0]`): 0xAC game-state (dominant), 0x84
/// c2s/handshake, 0xBE s2c (`NetTransportMessage.MAGIC_HEADER`). A correct
/// decrypt always lands on one of these.
pub const STREAM_PLAINTEXT_LEADS: [u8; 3] = [0xAC, 0x84, 0xBE];

#[inline]
fn be_u16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
#[inline]
fn be_u32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Header length in bytes (2, or 4 when the sentTime flag is set). `None` if the
/// payload is too short to hold even the peerID.
pub fn header_len(payload: &[u8]) -> Option<usize> {
    if payload.len() < 2 {
        return None;
    }
    Some(if be_u16(payload, 0) & ENET_HDR_FLAG_SENT_TIME != 0 {
        4
    } else {
        2
    })
}

/// Resolver for fragment user-data: maps `(channel, start_seq, frag_offset,
/// data_length)` to the plaintext slice of a reassembled message, or `None` if
/// the group isn't resolved. (Fragments are encrypted pre-fragmentation as one
/// stream, so a fragment slice can't be decrypted in isolation — see
/// `reassembly`.)
pub type FragmentResolver<'a> = dyn Fn(u8, u16, u32, usize) -> Option<Vec<u8>> + 'a;

/// Reconstruct one frame's plaintext: ENet headers + command headers + length
/// prefixes preserved verbatim, with each SEND_* command's user-data replaced
/// by its decrypted bytes. Returns `None` if parsing fails, the (optional)
/// strict byte-0 gate rejects a candidate, or a fragment can't be resolved.
/// Port of `_try_stream`.
pub fn reconstruct_plaintext(
    payload: &[u8],
    key: &[u8; 32],
    nonce: &[u8; 8],
    resolver: Option<&FragmentResolver<'_>>,
    strict_gate: bool,
) -> Option<Vec<u8>> {
    let n = payload.len();
    if n < 2 {
        return None;
    }
    let hdr_len = header_len(payload)?;
    if n < hdr_len + 4 {
        return None;
    }
    let mut out: Vec<u8> = payload[..hdr_len].to_vec();
    let mut off = hdr_len;
    let mut parsed_any = false;

    while off + 4 <= n {
        let cmd_lo = payload[off] & 0x0F;
        match cmd_lo {
            ENET_CMD_SEND_RELIABLE => {
                // cmd_hdr(4) + dataLength(2) + user_data
                if off + 6 > n {
                    return None;
                }
                let dlen = be_u16(payload, off + 4) as usize;
                let ud_start = off + 6;
                let ud_end = ud_start + dlen;
                if ud_end > n {
                    return None;
                }
                let pt = if dlen > 0 {
                    chacha20_legacy(&payload[ud_start..ud_end], key, nonce)
                } else {
                    Vec::new()
                };
                if strict_gate && !pt.is_empty() && !STREAM_PLAINTEXT_LEADS.contains(&pt[0]) {
                    return None;
                }
                out.extend_from_slice(&payload[off..ud_start]);
                out.extend_from_slice(&pt);
                off = ud_end;
                parsed_any = true;
            }
            ENET_CMD_SEND_UNRELIABLE => {
                // cmd_hdr(4) + unreliableSeq(2) + dataLength(2) + user_data
                if off + 8 > n {
                    return None;
                }
                let dlen = be_u16(payload, off + 6) as usize;
                let ud_start = off + 8;
                let ud_end = ud_start + dlen;
                if ud_end > n {
                    return None;
                }
                let pt = if dlen > 0 {
                    chacha20_legacy(&payload[ud_start..ud_end], key, nonce)
                } else {
                    Vec::new()
                };
                if strict_gate && !pt.is_empty() && !STREAM_PLAINTEXT_LEADS.contains(&pt[0]) {
                    return None;
                }
                out.extend_from_slice(&payload[off..ud_start]);
                out.extend_from_slice(&pt);
                off = ud_end;
                parsed_any = true;
            }
            ENET_CMD_SEND_FRAGMENT | ENET_CMD_SEND_UNRELIABLE_FRAGMENT => {
                // cmd_hdr(4) + startSeq(2) + dataLength(2) + fragCount(4)
                //   + fragNum(4) + totalLen(4) + fragOffset(4) = 24-byte header.
                if off + 24 > n {
                    return None;
                }
                let channel = payload[off + 1];
                let start_seq = be_u16(payload, off + 4);
                let dlen = be_u16(payload, off + 6) as usize;
                let frag_offset = be_u32(payload, off + 20);
                let ud_start = off + 24;
                let ud_end = ud_start + dlen;
                if ud_end > n {
                    return None;
                }
                let resolver = resolver?; // no resolver ⇒ can't decrypt this frame
                let pt = resolver(channel, start_seq, frag_offset, dlen)?; // group unresolved
                out.extend_from_slice(&payload[off..ud_start]);
                out.extend_from_slice(&pt);
                off = ud_end;
                parsed_any = true;
            }
            ENET_CMD_SEND_UNSEQUENCED => {
                // cmd_hdr(4) + unsequencedGroup(2) + dataLength(2) + user_data
                if off + 8 > n {
                    return None;
                }
                let dlen = be_u16(payload, off + 6) as usize;
                let ud_start = off + 8;
                let ud_end = ud_start + dlen;
                if ud_end > n {
                    return None;
                }
                let pt = if dlen > 0 {
                    chacha20_legacy(&payload[ud_start..ud_end], key, nonce)
                } else {
                    Vec::new()
                };
                if strict_gate && !pt.is_empty() && !STREAM_PLAINTEXT_LEADS.contains(&pt[0]) {
                    return None;
                }
                out.extend_from_slice(&payload[off..ud_start]);
                out.extend_from_slice(&pt);
                off = ud_end;
                parsed_any = true;
            }
            // Non-encrypted commands — copy through at their fixed sizes.
            ENET_CMD_ACK => {
                if off + 8 > n {
                    return None;
                }
                out.extend_from_slice(&payload[off..off + 8]);
                off += 8;
                parsed_any = true;
            }
            ENET_CMD_PING => {
                out.extend_from_slice(&payload[off..off + 4]);
                off += 4;
                parsed_any = true;
            }
            ENET_CMD_DISCONNECT => {
                if off + 8 > n {
                    return None;
                }
                out.extend_from_slice(&payload[off..off + 8]);
                off += 8;
                parsed_any = true;
            }
            ENET_CMD_CONNECT => {
                if off + 48 > n {
                    return None;
                }
                out.extend_from_slice(&payload[off..off + 48]);
                off += 48;
                parsed_any = true;
            }
            ENET_CMD_VERIFY_CONNECT => {
                if off + 44 > n {
                    return None;
                }
                out.extend_from_slice(&payload[off..off + 44]);
                off += 44;
                parsed_any = true;
            }
            ENET_CMD_BANDWIDTH_LIMIT => {
                if off + 12 > n {
                    return None;
                }
                out.extend_from_slice(&payload[off..off + 12]);
                off += 12;
                parsed_any = true;
            }
            ENET_CMD_THROTTLE_CONFIGURE => {
                if off + 16 > n {
                    return None;
                }
                out.extend_from_slice(&payload[off..off + 16]);
                off += 16;
                parsed_any = true;
            }
            0 => break, // NONE / end of commands
            _ => {
                // cmd_lo 13..15 unspecified. Strict (Pass 1) bails; Pass 2
                // releases whatever decoded so far.
                if strict_gate {
                    return None;
                }
                break;
            }
        }
    }

    if !parsed_any {
        return None;
    }
    Some(out)
}

/// `user_data[1]` (the GameMessageId byte) of the first SEND_* command, gated on
/// membership in the opcode set; `None` for control-only frames. Fragment opcode
/// only on `fragOffset == 0`. Port of `_first_opcode_in_plaintext`.
pub fn first_opcode_in_plaintext(pt: &[u8]) -> Option<u8> {
    let n = pt.len();
    if n < 2 {
        return None;
    }
    let hdr_len = header_len(pt)?;
    let mut off = hdr_len;
    while off + 4 <= n {
        let cmd_lo = pt[off] & 0x0F;
        match cmd_lo {
            ENET_CMD_SEND_RELIABLE => {
                if off + 6 > n {
                    return None;
                }
                let dlen = be_u16(pt, off + 4) as usize;
                let ud = off + 6;
                if dlen >= 2 && ud + 2 <= n {
                    let b = pt[ud + 1];
                    return is_game_message_id(b).then_some(b);
                }
                return None;
            }
            ENET_CMD_SEND_UNRELIABLE | ENET_CMD_SEND_UNSEQUENCED => {
                if off + 8 > n {
                    return None;
                }
                let dlen = be_u16(pt, off + 6) as usize;
                let ud = off + 8;
                if dlen >= 2 && ud + 2 <= n {
                    let b = pt[ud + 1];
                    return is_game_message_id(b).then_some(b);
                }
                return None;
            }
            ENET_CMD_SEND_FRAGMENT | ENET_CMD_SEND_UNRELIABLE_FRAGMENT => {
                if off + 24 > n {
                    return None;
                }
                let dlen = be_u16(pt, off + 6) as usize;
                let frag_offset = be_u32(pt, off + 20);
                let ud = off + 24;
                if frag_offset == 0 && dlen >= 2 && ud + 2 <= n {
                    let b = pt[ud + 1];
                    return is_game_message_id(b).then_some(b);
                }
                return None;
            }
            ENET_CMD_ACK => off += 8,
            ENET_CMD_PING => off += 4,
            ENET_CMD_DISCONNECT => off += 8,
            ENET_CMD_CONNECT => off += 48,
            ENET_CMD_VERIFY_CONNECT => off += 44,
            ENET_CMD_BANDWIDTH_LIMIT => off += 12,
            ENET_CMD_THROTTLE_CONFIGURE => off += 16,
            _ => return None,
        }
    }
    None
}

/// `(marker_byte, validatable)` for the first message-starting SEND_* command's
/// `user_data[0]`. `validatable` is false when the frame holds only fragment
/// continuations (fragOffset > 0). Port of `_first_marker_in_plaintext`.
pub fn first_marker_in_plaintext(pt: &[u8]) -> (Option<u8>, bool) {
    let n = pt.len();
    if n < 2 {
        return (None, false);
    }
    let Some(hdr_len) = header_len(pt) else {
        return (None, false);
    };
    let mut off = hdr_len;
    while off + 4 <= n {
        let cmd_lo = pt[off] & 0x0F;
        match cmd_lo {
            ENET_CMD_SEND_RELIABLE => {
                let ud = off + 6;
                return (if ud < n { Some(pt[ud]) } else { None }, true);
            }
            ENET_CMD_SEND_UNRELIABLE | ENET_CMD_SEND_UNSEQUENCED => {
                let ud = off + 8;
                return (if ud < n { Some(pt[ud]) } else { None }, true);
            }
            ENET_CMD_SEND_FRAGMENT | ENET_CMD_SEND_UNRELIABLE_FRAGMENT => {
                if off + 24 > n {
                    return (None, false);
                }
                let frag_offset = be_u32(pt, off + 20);
                let ud = off + 24;
                if frag_offset == 0 {
                    return (if ud < n { Some(pt[ud]) } else { None }, true);
                }
                return (None, false);
            }
            ENET_CMD_ACK => off += 8,
            ENET_CMD_PING => off += 4,
            ENET_CMD_DISCONNECT => off += 8,
            ENET_CMD_CONNECT => off += 48,
            ENET_CMD_VERIFY_CONNECT => off += 44,
            ENET_CMD_BANDWIDTH_LIMIT => off += 12,
            ENET_CMD_THROTTLE_CONFIGURE => off += 16,
            _ => return (None, false),
        }
    }
    (None, false)
}

/// One SEND_FRAGMENT / SEND_UNRELIABLE_FRAGMENT command's wire fields.
#[derive(Debug, Clone)]
pub struct RawFragment {
    pub channel: u8,
    pub start_seq: u16,
    pub fragment_count: u32,
    pub fragment_number: u32,
    pub total_length: u32,
    pub fragment_offset: u32,
    pub data_length: usize,
    /// Absolute offset of this fragment's user-data within the frame payload.
    pub ud_start: usize,
}

/// Enumerate every fragment command in a raw (encrypted) frame. Best-effort:
/// bails silently on structural inconsistency. Port of
/// `_walk_enet_for_fragments`.
pub fn walk_fragments(payload: &[u8]) -> Vec<RawFragment> {
    let mut out = Vec::new();
    let n = payload.len();
    let Some(hdr_len) = header_len(payload) else {
        return out;
    };
    if n < hdr_len + 4 {
        return out;
    }
    let mut off = hdr_len;
    while off + 4 <= n {
        let cmd_lo = payload[off] & 0x0F;
        match cmd_lo {
            ENET_CMD_SEND_FRAGMENT | ENET_CMD_SEND_UNRELIABLE_FRAGMENT => {
                if off + 24 > n {
                    return out;
                }
                let dlen = be_u16(payload, off + 6) as usize;
                let ud_start = off + 24;
                let ud_end = ud_start + dlen;
                if ud_end > n {
                    return out;
                }
                out.push(RawFragment {
                    channel: payload[off + 1],
                    start_seq: be_u16(payload, off + 4),
                    fragment_count: be_u32(payload, off + 8),
                    fragment_number: be_u32(payload, off + 12),
                    total_length: be_u32(payload, off + 16),
                    fragment_offset: be_u32(payload, off + 20),
                    data_length: dlen,
                    ud_start,
                });
                off = ud_end;
            }
            ENET_CMD_SEND_RELIABLE => {
                if off + 6 > n {
                    return out;
                }
                off += 6 + be_u16(payload, off + 4) as usize;
            }
            ENET_CMD_SEND_UNRELIABLE | ENET_CMD_SEND_UNSEQUENCED => {
                if off + 8 > n {
                    return out;
                }
                off += 8 + be_u16(payload, off + 6) as usize;
            }
            0 => return out,
            _ => {
                let sz = match cmd_lo {
                    ENET_CMD_ACK => 8,
                    ENET_CMD_PING => 4,
                    ENET_CMD_DISCONNECT => 8,
                    ENET_CMD_CONNECT => 48,
                    ENET_CMD_VERIFY_CONNECT => 44,
                    ENET_CMD_BANDWIDTH_LIMIT => 12,
                    ENET_CMD_THROTTLE_CONFIGURE => 16,
                    _ => 0,
                };
                if sz == 0 || off + sz > n {
                    return out;
                }
                off += sz;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::chacha20_legacy;

    const KEY: [u8; 32] = [7u8; 32];
    const NONCE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    /// Build a 2-byte-header frame with one SEND_RELIABLE carrying `plain` as
    /// (encrypted) user-data. peerID 0x3000 has the 0x4000 flag clear → 2-byte
    /// header.
    fn reliable_frame(channel: u8, seq: u16, plain: &[u8]) -> Vec<u8> {
        let ct = chacha20_legacy(plain, &KEY, &NONCE);
        let mut f = vec![0x30, 0x00]; // peerID, flag clear
        f.push(ENET_CMD_SEND_RELIABLE);
        f.push(channel);
        f.extend_from_slice(&seq.to_be_bytes());
        f.extend_from_slice(&(plain.len() as u16).to_be_bytes());
        f.extend_from_slice(&ct);
        f
    }

    #[test]
    fn reconstruct_round_trip_reliable() {
        // marker 0xBE, opcode 50 (ReceiveDamage), then body.
        let plain = [0xBE, 50, 0xde, 0xad, 0xbe, 0xef];
        let frame = reliable_frame(2, 7, &plain);
        let out = reconstruct_plaintext(&frame, &KEY, &NONCE, None, true).expect("decode");
        // Header + cmd header + length prefix preserved, user-data decrypted.
        let mut expected = vec![0x30, 0x00, ENET_CMD_SEND_RELIABLE, 2];
        expected.extend_from_slice(&7u16.to_be_bytes());
        expected.extend_from_slice(&(plain.len() as u16).to_be_bytes());
        expected.extend_from_slice(&plain);
        assert_eq!(out, expected);
        assert_eq!(first_opcode_in_plaintext(&out), Some(50));
        assert_eq!(first_marker_in_plaintext(&out), (Some(0xBE), true));
    }

    #[test]
    fn strict_gate_rejects_bad_marker() {
        // Plaintext byte0 = 0x11 is not a legal lead → strict gate rejects.
        let plain = [0x11, 50, 0x00];
        let frame = reliable_frame(0, 1, &plain);
        assert!(reconstruct_plaintext(&frame, &KEY, &NONCE, None, true).is_none());
        // ...but the lenient (Pass-2) path still reconstructs it.
        assert!(reconstruct_plaintext(&frame, &KEY, &NONCE, None, false).is_some());
    }

    #[test]
    fn fragment_needs_resolver() {
        // peerID 0x3000, one SEND_FRAGMENT: 24-byte header + 4 bytes ud.
        let mut f = vec![0x30, 0x00, ENET_CMD_SEND_FRAGMENT, 0];
        f.extend_from_slice(&0u16.to_be_bytes()); // reliableSeq (completes the 4-byte cmd header)
        f.extend_from_slice(&5u16.to_be_bytes()); // startSeq
        f.extend_from_slice(&4u16.to_be_bytes()); // dataLength
        f.extend_from_slice(&2u32.to_be_bytes()); // fragCount
        f.extend_from_slice(&0u32.to_be_bytes()); // fragNum
        f.extend_from_slice(&8u32.to_be_bytes()); // totalLen
        f.extend_from_slice(&0u32.to_be_bytes()); // fragOffset
        f.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]); // ud (ciphertext, ignored)

        // No resolver ⇒ None.
        assert!(reconstruct_plaintext(&f, &KEY, &NONCE, None, false).is_none());

        // walk_fragments finds it.
        let frags = walk_fragments(&f);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].total_length, 8);
        assert_eq!(frags[0].data_length, 4);

        // A resolver that yields the assembled slice ⇒ user-data replaced.
        let resolver = |_ch: u8, _ss: u16, _fo: u32, dl: usize| Some(vec![0xBEu8; dl]);
        let out = reconstruct_plaintext(&f, &KEY, &NONCE, Some(&resolver), false).expect("decode");
        assert_eq!(&out[out.len() - 4..], &[0xBE, 0xBE, 0xBE, 0xBE]);
    }
}
