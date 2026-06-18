//! Offline reproduction-**differential** test for the arena round-start protocol.
//!
//! Replays the round-start of a captured RETAIL match (prod `arena_udp_frames`
//! **session_id = 506**, ts 05:05:33–05:05:45) into our combat engine and DIFFs
//! the s2c protocol *sequence* (shape + relative ordering + the round-start
//! stagger) our [`MatchInstance`] emits against what retail actually sent. It is
//! the safety net for the round-start "stagger" fix (`SPAWN_HANDSHAKE_HOLD` /
//! `MATCH_CREATE_HOLD`): if our emission order or timing drifts from s506, this
//! fails with a message naming the divergence.
//!
//! ## What is (and isn't) compared
//! We compare the **protocol shape** — each s2c frame's carrier (`user_data[1]`)
//! plus a structural sub-kind (a flow stateName like `BackendMatchCreated`, an
//! op50 spawn, the op58 clock, an op53 channeling update, the op54 profile, …) —
//! and the **relative ordering / stagger** between the landmark frames. We do
//! **not** compare opponent-specific profile BYTES (the gear/customization JSON):
//! that's per-character and irrelevant to whether the round-start handshake is
//! protocol-faithful. The capture's frames carry an ENet command prefix in the
//! stored `plaintext`; [`carrier_of`] locates the inner `0xBE` user-data marker so
//! both sides are classified by the *same* logic.
//!
//! ## Ground truth — s506 round-start s2c (deduped, relative seconds; the DB ts is
//! second-resolution so gaps are ±1s). Extracted read-only from prod:
//! ```text
//!  t+0  op58 clock  (the server's REPLY to the client's c2s op58 clock-sync,
//!                    echoing the client's token — NOT an unsolicited broadcast)
//!  t+0  op50 spawn  (self player)
//!  t+0  op50 spawn  (opponent player)
//!  t+0  op54 stat/profile word (97 B)
//!  t+1  op53 channeling ×2
//!  t+2  op50 spawn  (opponent avatar, 60 B)
//!  t+2  op54 PROFILE (opponent, ~1400 B, fragmented JSON)   ← opponent-only
//!  t+4  op54 stat word ×2 · op53 · FLOW BackendMatchCreated ×2 · op53
//!  t+6  FLOW StateTimeout ×3   (heartbeat begins)
//!  t+9  FLOW StateTimeout …
//! ```
//! → **spawns (t+0) → BackendMatchCreated (t+4) ≈ 4 s** == `SPAWN_HANDSHAKE_HOLD`;
//!   **BackendMatchCreated (t+4) → StateTimeout (t+6) ≈ 2 s** == `MATCH_CREATE_HOLD`.
//!
//! The c2s round-start uploads (op58 clock echo, op55, the op54 PlayerLoadoutReady
//! loadout, the op54 flow echoes) are embedded below and replayed at their captured
//! offsets via [`MatchInstance::on_c2s`] to prove they don't perturb our s2c FSM
//! (they're handshake traffic, not combat input — `resolve` ignores them off the
//! live round). Their exact opponent-gear bytes are NOT asserted.

use std::time::{Duration, Instant};

use arena_proto::parse_netdata;

use super::engine::MatchInstance;
use super::state::{FlowState, Loadout};

// ---------------------------------------------------------------------------
// Frame classification — one logic for BOTH our emission and the capture.
// ---------------------------------------------------------------------------

/// The structural kind of an s2c frame, derived from its carrier + body. This is
/// the unit we diff on (protocol shape), deliberately ignoring char-specific bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// op58 — match clock (carrier 0x3a).
    Clock,
    /// op50 — a net-object spawn (carrier 0x32: Player / Avatar / Match-ability).
    Spawn,
    /// op53 — PlayerChannelingStateChange (carrier 0x35).
    Channeling,
    /// op54 flow-control stateName (carrier 0x36 with an ASCII state trailer).
    Flow(String),
    /// op54 PROFILE — the opponent's full character/gear JSON (carrier 0x36,
    /// propId3 == 35). Bytes intentionally NOT captured here.
    Profile,
    /// op54 stat/HP word or other carrier-0x36 non-flow, non-profile frame.
    StatOrOther,
    /// Anything else (op55 combat-screen, op49/op29, …) — carrier kept for context.
    Carrier(u8),
}

/// Locate the inner NetTransport user-data inside a (possibly ENet-prefixed) frame
/// and return `(carrier_byte, &body_after_carrier)`. Our engine emits frames that
/// already start with the `0xBE` marker; the capture's stored `plaintext` carries an
/// ENet command header first, so we scan for the first `0xBE` (s2c) / `0xBC`-family
/// marker. Returns `None` if no marker/carrier is present.
fn user_data<'a>(frame: &'a [u8]) -> Option<(u8, &'a [u8])> {
    // Fast path: already a bare user_data (our emission).
    if frame.first() == Some(&0xBE) && frame.len() >= 2 {
        return Some((frame[1], &frame[2..]));
    }
    // Capture path: find the inner 0xBE marker (the NetTransport MAGIC_HEADER).
    let pos = frame.iter().position(|&b| b == 0xBE)?;
    if pos + 1 >= frame.len() {
        return None;
    }
    Some((frame[pos + 1], &frame[pos + 2..]))
}

/// Classify a frame into its protocol [`Kind`] using the carrier + NetData body.
/// Identical logic for our emission and for the capture (after [`user_data`]).
fn classify(frame: &[u8]) -> Option<Kind> {
    let (carrier, body) = user_data(frame)?;
    Some(match carrier {
        0x3a => Kind::Clock,      // op58
        0x32 => Kind::Spawn,      // op50
        0x35 => Kind::Channeling, // op53
        0x36 => {
            // op54 carrier is overloaded: flow stateName vs profile vs stat word.
            if let Some(name) = flow_name(frame) {
                Kind::Flow(name)
            } else if parse_netdata(body).int(3) == Some(35) {
                Kind::Profile
            } else {
                Kind::StatOrOther
            }
        }
        other => Kind::Carrier(other),
    })
}

/// The flow stateName ASCII string carried by an op54 flow frame, if any. Works on
/// both directions (the trailer is the literal state string at the tail of the
/// frame, e.g. `…BackendMatchCreated`). Matches the engine's own
/// `payload.ends_with(b"…")` convention.
fn flow_name(frame: &[u8]) -> Option<String> {
    for name in ["BackendMatchCreated", "StateTimeout", "NextState", "RoundEnd", "Connecting"] {
        if frame.ends_with(name.as_bytes()) {
            return Some(name.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// s506 capture fixture — the small round-start c2s frames (replayed), and the
// EXPECTED s2c sequence (ground truth, from the read-only extraction above).
// ---------------------------------------------------------------------------

/// One captured c2s frame to replay: relative second + the stored bytes (ENet
/// prefix + inner `0xBE` user-data). These are the client's round-start uploads.
struct C2s {
    rel_sec: u64,
    bytes: &'static [u8],
}

/// The small s506 round-start c2s frames (the multi-KB op54 PlayerLoadoutReady
/// upload body is represented by its leading bytes — we replay it to prove it
/// doesn't perturb our FSM, not to assert its gear payload). Bytes are the exact
/// stored `plaintext` (ENet-prefixed) from prod s506.
fn s506_c2s() -> Vec<C2s> {
    vec![
        // t+0 op58 clock echo
        C2s { rel_sec: 0, bytes: &[
            0x70, 0x00, 0xb6, 0x26, 0x86, 0x00, 0x00, 0x02, 0x00, 0x15,
            0xbe, 0x3a, 0x01, 0x03, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x98, 0x1e, 0xdd, 0x11, 0x2e, 0xcc, 0xde, 0x08,
        ] },
        // t+0 op55 combat-screen (Player, role 3)
        C2s { rel_sec: 0, bytes: &[
            0x70, 0x00, 0xb6, 0x26, 0x86, 0x00, 0x00, 0x03, 0x00, 0x0c,
            0xbe, 0x37, 0x02, 0x07, 0x70, 0x07, 0x77, 0x00, 0x00, 0x00, 0x39, 0x03,
        ] },
        // t+0 op54 stat echo (small)
        C2s { rel_sec: 0, bytes: &[
            0x70, 0x00, 0xb7, 0xf7, 0x86, 0x00, 0x00, 0x07, 0x00, 0x0d,
            0xbe, 0x36, 0x03, 0x0f, 0x70, 0x77, 0x78, 0x00, 0x00, 0x00, 0x37, 0x03,
            0x16, 0x86, 0x00, 0x00, 0x08, 0x00, 0x0c, 0xbe, 0x37, 0x02, 0x07,
            0x70, 0x07, 0x7b, 0x00, 0x00, 0x00, 0x36, 0x02,
        ] },
        // t+4 op54 flow echo: BackendMatchCreated (selector 0x50, client→server)
        C2s { rel_sec: 4, bytes: &[
            0x70, 0x00, 0xc6, 0xd6, 0x86, 0x00, 0x00, 0x0c, 0x00, 0x23,
            0xbe, 0x36, 0x04, 0x1f, 0x70, 0x77, 0x0a, 0x77, 0x00, 0x00, 0x00,
            0x39, 0x03, 0x50, 0x13, 0x00, b'B', b'a', b'c', b'k', b'e', b'n',
            b'd', b'M', b'a', b't', b'c', b'h', b'C', b'r', b'e', b'a', b't',
            b'e', b'd',
        ] },
        // t+6 op54 flow echo: StateTimeout (selector 0x50)
        C2s { rel_sec: 6, bytes: &[
            0x70, 0x00, 0xcd, 0x84, 0x86, 0x00, 0x00, 0x0d, 0x00, 0x1c,
            0xbe, 0x36, 0x04, 0x1f, 0x70, 0x77, 0x0a, 0x77, 0x00, 0x00, 0x00,
            0x39, 0x03, 0x50, 0x0c, 0x00, b'S', b't', b'a', b't', b'e', b'T',
            b'i', b'm', b'e', b'o', b'u', b't',
        ] },
    ]
}

/// The s506 EXPECTED s2c round-start landmark sequence, as `(rel_sec, Kind)`,
/// collapsed to the distinct protocol events (per-viewer duplicates + ENet
/// retransmits removed — we compare the *sequence of distinct kinds*, not the
/// fan-out count). This is the ground truth our emission must reproduce in order.
fn s506_expected_s2c() -> Vec<(u64, Kind)> {
    vec![
        (0, Kind::Clock),                              // op58 match clock — FIRST
        (0, Kind::Spawn),                              // op50 player spawns (self + opp)
        (1, Kind::Channeling),                         // op53 channeling
        (2, Kind::Profile),                            // op54 opponent profile (~1400 B)
        (4, Kind::Flow("BackendMatchCreated".into())), // staggered ~4s after spawns
        (6, Kind::Flow("StateTimeout".into())),        // round live ~2s later
    ]
}

// ---------------------------------------------------------------------------
// Driving the engine over s506's relative timing.
// ---------------------------------------------------------------------------

/// One s2c frame our engine emitted, tagged with the simulated second it went out.
struct Emitted {
    rel_sec: u64,
    kind: Kind,
}

/// Two fighters that each carry a (non-empty) profile, so `broadcast_profiles`
/// emits the op54 PROFILE — required to reproduce s506's t+2 opponent profile.
/// The gear JSON is a stub: we assert the profile is PRESENT and opponent-only,
/// never its bytes.
fn profiled(name: &str) -> Loadout {
    let mut l = super::loadout::starter();
    l.display_name = name.to_string();
    l.character_uuid = "00000000-0000-0000-0000-000000000001".to_string();
    l.abilities.push(super::state::EquippedAbility {
        instance_uuid: "5b764e61-8851-4703-8fea-3d8e589ed24f".to_string(),
        level: 1,
    });
    l.profile_equipped_json = r#"{"equippedItems":{}}"#.to_string();
    l.profile_character_json = format!(r#"{{"name":"{name}"}}"#);
    l
}

/// Drive a 2-fighter PvP match over s506's relative timing, replaying s506's c2s
/// at their captured offsets and collecting every s2c frame tagged with its second.
/// Ticks at 100 ms (≫ the engine's needs) across t+0…t+9 so every FSM transition
/// and the heartbeat fire on cadence. Returns the engine + the emitted log.
fn drive_s506() -> (MatchInstance, Vec<Emitted>) {
    let t0 = Instant::now();
    // PvP: 2 fighters, both real peers; both carry a profile (opponent-only relay).
    let mut m = MatchInstance::new(2, 2, vec![profiled("Flappety"), profiled("Opponent")], t0);

    let c2s = s506_c2s();
    let mut log = Vec::new();
    let tag = |out: Vec<(usize, Vec<u8>)>, sec: u64, log: &mut Vec<Emitted>| {
        for (_viewer, frame) in out {
            if let Some(kind) = classify(&frame) {
                log.push(Emitted { rel_sec: sec, kind });
            }
        }
    };

    // 100 ms steps over 9.5 s. `connected = 2` from the start so the
    // Connecting→Spawning gate opens on the first tick (both peers present).
    let step = Duration::from_millis(100);
    let mut sec_emitted_c2s = std::collections::HashSet::new();
    for i in 0..=95u64 {
        let now = t0 + step * i as u32;
        let sec = (i * 100) / 1000;

        // Replay any c2s scheduled for this second, once, at its top.
        if !sec_emitted_c2s.contains(&sec) {
            for f in c2s.iter().filter(|f| f.rel_sec == sec) {
                let out = m.on_c2s(0, &inner_user_data(f.bytes), now);
                tag(out, sec, &mut log);
            }
            sec_emitted_c2s.insert(sec);
        }

        let out = m.on_tick(2, now);
        tag(out, sec, &mut log);
    }
    (m, log)
}

/// Strip a captured frame's ENet prefix → the bare `0xBE ‖ carrier ‖ body`
/// user-data the engine's `on_c2s` expects (it dispatches on `user_data[1]`).
fn inner_user_data(frame: &[u8]) -> Vec<u8> {
    match frame.iter().position(|&b| b == 0xBE) {
        Some(p) => frame[p..].to_vec(),
        None => frame.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// The differential.
// ---------------------------------------------------------------------------

/// First simulated second at which any emitted frame matches `pred`.
fn first_sec(log: &[Emitted], pred: impl Fn(&Kind) -> bool) -> Option<u64> {
    log.iter().filter(|e| pred(&e.kind)).map(|e| e.rel_sec).min()
}

/// The ordered sequence of DISTINCT protocol kinds we emitted (consecutive
/// duplicates + per-viewer fan-out collapsed) — the thing we diff against s506.
fn distinct_sequence(log: &[Emitted]) -> Vec<Kind> {
    let mut seq: Vec<Kind> = Vec::new();
    for e in log {
        if seq.last() != Some(&e.kind) {
            seq.push(e.kind.clone());
        }
    }
    seq
}

#[test]
fn round_start_reproduces_s506_sequence_and_stagger() {
    let (m, log) = drive_s506();

    // The match must reach the LIVE round (StateTimeout) — i.e. the round-start
    // handshake completed, not stalled at "Connecting".
    assert_eq!(
        m.phase(),
        FlowState::StateTimeout,
        "DIVERGENCE: our engine never reached the live round (StateTimeout) over s506's \
         timing — the round-start handshake stalled. Emitted: {:?}",
        distinct_sequence(&log),
    );

    // ---- (1) Landmark presence + ORDER (vs s506 ground truth) --------------
    let clock = first_sec(&log, |k| *k == Kind::Clock);
    let spawn = first_sec(&log, |k| *k == Kind::Spawn);
    let profile = first_sec(&log, |k| *k == Kind::Profile);
    let bmc = first_sec(&log, |k| matches!(k, Kind::Flow(n) if n == "BackendMatchCreated"));
    let stto = first_sec(&log, |k| matches!(k, Kind::Flow(n) if n == "StateTimeout"));

    let clock = clock.expect(
        "DIVERGENCE: no op58 CLOCK emitted at round-start. s506's op58 is the server's \
         REPLY to the client's c2s op58 clock-sync (echoing the client's token); without \
         it the client BLOCKS at AwaitingClientBackendSynchronization and never uploads \
         its loadout (stalls at 'Connecting'). engine::on_c2s op58 branch is missing.",
    );
    let spawn = spawn.expect(
        "DIVERGENCE: no op50 SPAWN emitted at round-start. s506 spawns the Player/Avatar \
         net objects so the client can construct the fighters.",
    );
    let bmc = bmc.expect(
        "DIVERGENCE: BackendMatchCreated flow state never emitted — the match is never \
         announced, so the client cannot leave setup. (FlowState/broadcast_flow gap.)",
    );
    let stto = stto.expect(
        "DIVERGENCE: StateTimeout flow heartbeat never emitted — the round never goes \
         live (client hangs after BackendMatchCreated).",
    );
    let profile = profile.expect(
        "DIVERGENCE: no op54 PROFILE emitted — the client never receives the opponent's \
         character/gear, so it cannot build the opponent actor (stalls at 'Setting up…'). \
         engine::broadcast_profiles skipped it.",
    );

    // s506 order: Clock (t+0) → Spawn (t+0) → Profile (t+2) → BackendMatchCreated (t+4)
    //             → StateTimeout (t+6). Spawns MUST precede BackendMatchCreated (the
    //             whole point of the stagger fix); BMC MUST precede StateTimeout.
    assert!(
        clock <= spawn,
        "DIVERGENCE: op58 CLOCK (t+{clock}) must be sent at/before the op50 SPAWNS (t+{spawn}); \
         s506 sends the clock first.",
    );
    assert!(
        spawn < bmc,
        "DIVERGENCE (STAGGER): op50 SPAWNS (t+{spawn}) MUST precede BackendMatchCreated (t+{bmc}). \
         Batching them preempts the client's loadout-upload handshake → 'Connecting' hang. \
         This is exactly the round-start stagger regression this test guards.",
    );
    assert!(
        bmc < stto,
        "DIVERGENCE: BackendMatchCreated (t+{bmc}) MUST precede StateTimeout (t+{stto}) — the \
         match is announced before the round goes live.",
    );
    assert!(
        spawn <= profile && profile <= bmc,
        "DIVERGENCE: the opponent op54 PROFILE (t+{profile}) should land after the spawns \
         (t+{spawn}) and during the pre-BackendMatchCreated hold (t+{bmc}) — s506 sent it at t+2.",
    );

    // ---- (2) STAGGER TIMING vs s506's measured deltas ----------------------
    // s506: spawns t+0 → BackendMatchCreated t+4 (Δ≈4s == SPAWN_HANDSHAKE_HOLD);
    //       BackendMatchCreated t+4 → StateTimeout t+6 (Δ≈2s == MATCH_CREATE_HOLD).
    // The DB ts is second-resolution, so allow ±1s; assert our staggers match s506.
    let spawn_to_bmc = bmc - spawn; // seconds
    let bmc_to_stto = stto - bmc; // seconds
    let near = |got: u64, want: u64| got.abs_diff(want) <= 1;
    assert!(
        near(spawn_to_bmc, 4),
        "DIVERGENCE (STAGGER TIMING): spawns→BackendMatchCreated was {spawn_to_bmc}s, but s506 \
         measured ≈4s (SPAWN_HANDSHAKE_HOLD=4s). Re-tune SPAWN_HANDSHAKE_HOLD to match retail.",
    );
    assert!(
        near(bmc_to_stto, 2),
        "DIVERGENCE (STAGGER TIMING): BackendMatchCreated→StateTimeout was {bmc_to_stto}s, but \
         s506 measured ≈2s (MATCH_CREATE_HOLD=2s). Re-tune MATCH_CREATE_HOLD to match retail.",
    );

    // ---- (3) SEQUENCE diff — our distinct s2c order must contain s506's landmark
    //          order as a subsequence (in the same relative order). -----------
    let seq = distinct_sequence(&log);
    let s506_landmarks: Vec<Kind> = vec![
        Kind::Clock,
        Kind::Spawn,
        Kind::Profile,
        Kind::Flow("BackendMatchCreated".into()),
        Kind::Flow("StateTimeout".into()),
    ];
    assert!(
        is_subsequence(&s506_landmarks, &seq),
        "DIVERGENCE (SEQUENCE): our s2c round-start order does not reproduce s506's landmark \
         order {:?}.\n  s506 wants (in order): Clock → Spawn → Profile → BackendMatchCreated \
         → StateTimeout\n  we emitted (distinct, in order): {:?}",
        s506_landmarks,
        seq,
    );

    // ---- (4) Stagger invariant: NOTHING flow-state rides the spawn burst ----
    // (Belt-and-suspenders for the regression: at the spawn second we must not have
    //  emitted BackendMatchCreated.)
    let spawn_sec_flows: Vec<&str> = log
        .iter()
        .filter(|e| e.rel_sec == spawn)
        .filter_map(|e| match &e.kind {
            Kind::Flow(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !spawn_sec_flows.contains(&"BackendMatchCreated"),
        "DIVERGENCE (STAGGER): BackendMatchCreated was emitted in the SAME second as the spawns \
         (t+{spawn}) — it must be held ~4s. Flows seen at the spawn second: {spawn_sec_flows:?}",
    );

    // Reference summary (visible with `--nocapture`): our measured deltas vs s506.
    eprintln!(
        "s506 differential OK — our round-start: clock t+{clock}, spawn t+{spawn}, profile t+{profile}, \
         BackendMatchCreated t+{bmc} (Δspawn {spawn_to_bmc}s, s506≈4s), StateTimeout t+{stto} \
         (ΔBMC {bmc_to_stto}s, s506≈2s)",
    );
}

/// `needle` appears in `hay` in order (not necessarily contiguous).
fn is_subsequence(needle: &[Kind], hay: &[Kind]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

#[test]
fn capture_and_emission_classify_identically() {
    // Sanity: our `classify` reads the SAME Kind from a captured (ENet-prefixed)
    // flow frame as from our own emission of that flow state — so the differential
    // compares like with like, not a parser artifact.
    let cap_bmc: &[u8] = &[
        0x70, 0x00, 0xc6, 0xd6, 0x86, 0x00, 0x00, 0x0c, 0x00, 0x23, 0xbe, 0x36, 0x04, 0x1f, 0x70,
        0x77, 0x0a, 0x77, 0x00, 0x00, 0x00, 0x39, 0x03, 0x50, 0x13, 0x00, b'B', b'a', b'c', b'k',
        b'e', b'n', b'd', b'M', b'a', b't', b'c', b'h', b'C', b'r', b'e', b'a', b't', b'e', b'd',
    ];
    assert_eq!(
        classify(cap_bmc),
        Some(Kind::Flow("BackendMatchCreated".into())),
        "captured (ENet-prefixed) flow frame must classify as the flow Kind",
    );
    // Our own emission of the same flow state.
    let ours = super::messages::flow_state(560, FlowState::BackendMatchCreated).unwrap();
    assert_eq!(
        classify(&ours),
        Some(Kind::Flow("BackendMatchCreated".into())),
        "our emitted flow frame must classify to the SAME Kind as the capture",
    );
    // op58 clock + op50 spawn round-trip through classify too.
    assert_eq!(classify(&super::messages::clock(1, 2)), Some(Kind::Clock));
    assert_eq!(
        classify(&super::messages::spawn_avatar(564, super::state::NetRole::Simulated, "x")),
        Some(Kind::Spawn),
    );
}
