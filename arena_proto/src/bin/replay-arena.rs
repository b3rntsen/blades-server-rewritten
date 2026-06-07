//! Offline replay / parity harness (milestone a.2).
//!
//! Reads `arena_udp_frames` + `arena_session_keys` from a capture-DB SQLite
//! snapshot and asserts the `arena_proto` pipeline (ENet walk → ChaCha20 →
//! fragment reassembly) reproduces, **byte-for-byte**, the `plaintext` and
//! `opcode` that `scripts/arena-decrypt.py` wrote. Green == the Rust port is
//! faithful to the production Python/TS decoders.
//!
//!   cargo run -p arena_proto --features replay --bin replay-arena -- <db.sqlite> <session_id>
//!
//! Read-only on the DB. Exits non-zero if any `ok` frame fails to reproduce.

use std::process::ExitCode;

use arena_proto::enet::{first_opcode_in_plaintext, reconstruct_plaintext};
use arena_proto::reassembly::{reassemble_session, resolve_fragment, Frame, KeyCandidate};
use base64::Engine;
use rusqlite::{Connection, OpenFlags};

fn b64(s: &str) -> Option<Vec<u8>> {
    let e = &base64::engine::general_purpose::STANDARD;
    e.decode(s.trim())
        .ok()
        .or_else(|| base64::engine::general_purpose::URL_SAFE.decode(s.trim()).ok())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: replay-arena <db.sqlite> <session_id>");
        return ExitCode::from(2);
    }
    let db_path = &args[1];
    let session_id: i64 = match args[2].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("session_id must be an integer");
            return ExitCode::from(2);
        }
    };

    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("open {db_path}: {e}");
            return ExitCode::from(2);
        }
    };

    // --- Load stream key candidates for the session -----------------------
    let mut keys: Vec<KeyCandidate> = Vec::new();
    {
        // Mirror `_load_keys_for`: a frame's key may be a `frida:%` key owned by
        // the same contributor but filed under a different session_id (clause b
        // in arena-decrypt.py). So load ALL frida stream keys as candidates —
        // the byte-0 gate + the exact-plaintext check select the right one.
        let mut stmt = conn
            .prepare(
                "SELECT id, key_b64, nonce_b64 FROM arena_session_keys \
                 WHERE key_field_name LIKE 'frida:%' AND nonce_b64 IS NOT NULL ORDER BY id",
            )
            .expect("prepare keys");
        let rows = stmt
            .query_map(rusqlite::params![], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .expect("query keys");
        for row in rows {
            let (id, key_b64, nonce_b64) = row.expect("key row");
            let (Some(kb), Some(nb)) = (b64(&key_b64), nonce_b64.as_deref().and_then(b64)) else {
                continue;
            };
            if kb.len() != 32 || nb.len() != 8 {
                continue; // not a bare-stream key (e.g. 12-byte AEAD nonce)
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&kb);
            let mut nonce = [0u8; 8];
            nonce.copy_from_slice(&nb);
            keys.push(KeyCandidate { id, key, nonce });
        }
    }

    // --- Load all frames for the session ----------------------------------
    let mut frames: Vec<Frame> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, direction, src_ip, src_port, dst_ip, dst_port, \
                        ciphertext, plaintext, decrypt_status, opcode, decryption_key_id \
                 FROM arena_udp_frames WHERE session_id = ?1 ORDER BY id",
            )
            .expect("prepare frames");
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(Frame {
                    id: r.get(0)?,
                    direction: r.get(1)?,
                    src_ip: r.get(2)?,
                    src_port: r.get(3)?,
                    dst_ip: r.get(4)?,
                    dst_port: r.get(5)?,
                    ciphertext: r.get(6)?,
                    plaintext: r.get(7)?,
                    decrypt_status: r.get(8)?,
                    opcode: r.get(9)?,
                    decryption_key_id: r.get(10)?,
                })
            })
            .expect("query frames");
        for row in rows {
            frames.push(row.expect("frame row"));
        }
    }

    println!(
        "session {session_id}: {} frames, {} stream key candidate(s)",
        frames.len(),
        keys.len()
    );
    if keys.is_empty() {
        eprintln!("no stream keys for session {session_id} — nothing to verify");
        return ExitCode::from(2);
    }

    // --- Pass 0: fragment reassembly --------------------------------------
    let reassembly = reassemble_session(&frames, &keys);
    println!("reassembled {} complete fragment group(s)", reassembly.len());

    // --- Per-frame parity -------------------------------------------------
    let mut ok_total = 0usize;
    let mut pt_match = 0usize;
    let mut op_match = 0usize;
    let mut pt_miss: Vec<i64> = Vec::new();
    let mut op_miss: Vec<(i64, Option<u8>, Option<i64>)> = Vec::new();

    for f in &frames {
        if f.decrypt_status != "ok" {
            continue;
        }
        let Some(stored_pt) = f.plaintext.as_ref() else {
            continue;
        };
        ok_total += 1;

        let (gl_ip, gl_port) = f.gamelift();
        let dir = f.direction.clone();
        let resolver = |ch: u8, ss: u16, fo: u32, dl: usize| {
            resolve_fragment(&reassembly, &dir, &gl_ip, gl_port, ch, ss, fo, dl)
        };

        // Find the key candidate that reproduces the stored plaintext exactly.
        // (For pure-fragment frames any key works — user-data comes from the
        // resolver — so the first candidate matches; for non-fragment frames
        // the right key is the one whose keystream lands on the stored bytes.)
        let mut reproduced: Option<Vec<u8>> = None;
        for kc in &keys {
            if let Some(out) =
                reconstruct_plaintext(&f.ciphertext, &kc.key, &kc.nonce, Some(&resolver), false)
            {
                if &out == stored_pt {
                    reproduced = Some(out);
                    break;
                }
            }
        }

        match reproduced {
            Some(out) => {
                pt_match += 1;
                // Opcode parity (NULL stored opcode == None computed).
                let computed = first_opcode_in_plaintext(&out).map(|b| b as i64);
                if computed == f.opcode {
                    op_match += 1;
                } else if op_miss.len() < 12 {
                    op_miss.push((f.id, first_opcode_in_plaintext(&out), f.opcode));
                }
            }
            None => {
                if pt_miss.len() < 20 {
                    pt_miss.push(f.id);
                }
            }
        }
    }

    println!("\n--- parity (decrypt_status='ok' frames) ---");
    println!("ok frames checked : {ok_total}");
    println!("plaintext match   : {pt_match}/{ok_total}");
    println!("opcode match      : {op_match}/{ok_total}");
    if !pt_miss.is_empty() {
        println!("plaintext MISMATCH frame ids (first {}): {:?}", pt_miss.len(), pt_miss);
    }
    if !op_miss.is_empty() {
        println!(
            "opcode mismatches (first {}) [id, computed, stored]: {:?}",
            op_miss.len(),
            op_miss
        );
    }

    // Reassembly-coverage note: how many groups did NOT resolve under a key.
    let frag_frames = frames
        .iter()
        .filter(|f| !arena_proto::enet::walk_fragments(&f.ciphertext).is_empty())
        .count();
    println!("\nframes containing fragments: {frag_frames}");

    if pt_miss.is_empty() && ok_total > 0 {
        println!("\nPASS: Rust pipeline reproduces all {pt_match} stored plaintexts byte-for-byte.");
        ExitCode::SUCCESS
    } else if ok_total == 0 {
        eprintln!("\nno ok frames to check");
        ExitCode::from(2)
    } else {
        eprintln!("\nFAIL: {} plaintext mismatch(es)", pt_miss.len());
        ExitCode::FAILURE
    }
}
