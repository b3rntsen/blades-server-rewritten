//! Offline reproduction-**differential** test for the arena round-start protocol.
//!
//! Replays the round-start of a captured RETAIL match (prod `arena_udp_frames`
//! **session_id = 506**, ts 05:05:33–05:05:45) into our combat engine and DIFFs
//! the s2c protocol *sequence* (shape + relative ordering + the round-start
//! stagger) our [`MatchInstance`] emits against what retail actually sent. It is
//! the safety net for the round-start "stagger" fix (`SPAWN_HANDSHAKE_HOLD`) AND
//! the MatchState progression past BackendMatchCreation(5) to InRound(13)
//! (`MATCH_STATE_ROUND0_PROGRESSION`): if our emission order, the MatchState walk,
//! or the timing drifts from s506, this fails with a message naming the divergence.
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
//!  t+2  op50 spawn  (self avatar, 60 B — role 3 Autonomous, obj 124, self UUID),
//!                    sent AFTER the Match net-object reaches InitialPlayerSetup(4)
//!  t+2  op54 PROFILE (opponent, ~1400 B, fragmented JSON)   ← opponent-only
//!  t+2  op50 spawn  (OPPONENT avatar — role 2 Simulated, obj 125, opponent UUID;
//!                    s506 DOES spawn a Simulated opponent Avatar net-object — its
//!                    discovery binds `HasOpponentPlayer` via GetPvpPlayer. [2026-06-19
//!                    correction: the earlier "no opponent-avatar op50" belief was WRONG;
//!                    re-decoded s506 obj 125 + injection-proved the bind on-device.])
//!  t+4  op54 stat word ×2 · op53 · FLOW BackendMatchCreated ×2 · op53
//!  t+6  FLOW StateTimeout ×3   (the op79 FLOW-controller heartbeat begins — a
//!                               SEPARATE state machine from the Match net-object)
//!  t+9  FLOW StateTimeout …
//! ```
//! → **spawns (t+0) → BackendMatchCreated (t+4) ≈ 4 s** == `SPAWN_HANDSHAKE_HOLD`.
//!
//! ## The Match net-object MatchState walk (obj 123 prop5) — the LAST gate
//! Distinct from the op79 FLOW stateName above, the **type-54 Match net-object**'s
//! replicated `MatchState` (prop5) is what the client reads to leave "Setting up…"
//! and enter the combat scene. s506 obj 123, ROUND 0 (op55 carrier-0x35 updates,
//! capture-proven 2026-06-19 — wall-clock and the `CurrentMatchStateTimeout` propId6):
//! ```text
//!  3 WaitingForPlayers     05:05:36  (20s)   ← in the op50 SPAWN
//!  4 InitialPlayerSetup    05:05:37  (30s)
//!  5 BackendMatchCreation  05:05:40  (10s)
//!  6 OpponentFoundFeedback 05:05:40  (1.5s)  (same tick as 5)
//!  7 PreMatch              05:05:42  (3s)
//! 11 OpponentShowcase      05:05:45  (12s)   (round 0 SKIPS 8/9/10 — between-rounds only)
//! 12 PreRound              05:05:57  (4s)
//! 13 InRound               05:06:02  (120s)  ← THE FIGHT (client enters the combat scene)
//! ```
//! Every transition is server-timer-driven (each gap ≈ the prior state's timeout);
//! none waits on a client message (the client uploads its loadout EARLY, c2s op54
//! gmid20/36 during 3→5, and emits periodic op80 acks). The engine reproduces this
//! via `MATCH_STATE_ROUND0_PROGRESSION`; section (5) of the differential asserts the
//! emitted MatchState sequence == `[3,4,5,6,7,11,12,13]` and that InRound is reached.
//!
//! ## The post-InRound walk — round end + match end (the "error 3" fix)
//! s506 obj 123 continues PAST InRound(13) when a round ends. **Round 0** (a fighter
//! reached 0 HP at 05:06:11) ends and LOOPS BACK for round 1 (best-of-3):
//! ```text
//! 13 InRound          05:06:02  (120s)   ← the fight
//!    op79 "RoundEnd"  05:06:13           (Control flow; client op80-echoes)
//! 14 PostRound        05:06:13  (3.0)    +11s (the killing blow / round timer)
//!  8 ChooseLoadout    05:06:16  (20)     +3s  round→1  ← between-rounds loadout re-choice
//!  9 AwaitingClientBackendSynchronization 05:06:36 (10)  +20s
//! 10 SynchronizingLoadout 05:06:37  (15) +1s
//! 11 OpponentShowcase 05:06:40  (5.0)    +3s
//! 12 PreRound         05:06:45  (4.0)    +5s
//! 13 InRound          05:06:50  (120s)   +5s  ← round 1 fight
//! ```
//! **The FINAL round** (round 1; obj 124=Flappety died at 05:07:01) walks to the
//! terminal state — this is what the solo-vs-ghost match hits (the player's first kill
//! IS the match-ending blow):
//! ```text
//!    op29 PlayerDead  05:07:01           (carrier 0x36, GMID 29, dead obj 124)
//!    op79 "RoundEnd"  05:07:01
//! 14 PostRound        05:07:01  (3.0)
//!    op48 MatchPostRoundInfoMsg 05:07:01 (the per-ROUND result: winner/loser UUIDs + matchId)
//!    op79 "StateTimeout" 05:07:04        (a flow heartbeat, +3s)
//!    op49 MatchEndMatchMsg 05:07:06      ← THE RESULTS/REWARDS message (the victory CARD).
//!                                         CORRECTION: op49 IS sent at match-end — carrier
//!                                         0x36, GMID 49 at NetData propId 3, ResultsJSON at
//!                                         propId 13 (the earlier "retail NEVER sends op49 /
//!                                         it rides 0xc2/0xc6" was WRONG: 0xc2/0xc6 was a
//!                                         misread of the ENet fragment-frame header; op49 is
//!                                         fragmented so it only round-trips after reassembly).
//!                                         [docs/arena-match-end-spec.md; 6 sessions.]
//! 17 BackendMatchEnd  05:07:05  (20)     +4s  (Victory(15) is SKIPPED; 17 precedes 16)
//! 16 PostMatch        05:07:11  (5.0)    +6s
//! 19 DisconnectingPlayersAfterMatch 05:07:16 (~0) +5s  ← terminal; client returns to lobby
//! ```
//! The engine reproduces the FINAL-round path: `resolve::on_round_ending_death` emits op29
//! + op79 RoundEnd + op48 + MatchState→PostRound(14) on the killing blow, then
//! `MATCH_STATE_MATCHEND_PROGRESSION` walks 17→16→19 AND emits one **op49** per player
//! (the victory card) at the matchend_step==0 tick, and finishes. Covered by
//! `engine::tests::{post_match_state_walk_reaches_terminal_then_finishes,
//! match_end_emits_op49_per_player_on_final_round}` +
//! `messages::tests::{player_dead,match_post_round_info,match_end_match}_matches_s506`.
//! [decoded from prod arena_udp_frames s506, 2026-06-19/06-20.]
//!
//! BLOCK MODEL NOTE (the cross-spec correction, `docs/arena-combat-reproduction-spec.md`
//! §4.4): a connected OPTIMAL block NEGATES physical (×0) but only HALVES elemental
//! (×0.5) — `wasOptimalBlocking` is a defender-STATE bit, not "hit absorbed". The
//! ÷1.6/÷1.23 divisors are the LATE/imperfect tier, NOT optimal (the status-resistance
//! spec's "÷1.6/÷1.23-for-optimal" was a flag-averaging artifact). See `damage::block_outcome`.
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
    /// op53/op55 carrier-0x35 net-object property UPDATE of the **Match** net-object
    /// (NetData prop1 == 54): the replicated `MatchState` (prop5) the client reads to
    /// advance the match. The payload is the MatchState enum value. This is the gate
    /// the round-start drives 3→4→5→…→13 (s506 obj 123). Distinguished from a generic
    /// channeling update so the differential can assert the MatchState sequence.
    MatchState(u8),
    /// op53 — PlayerChannelingStateChange or any other carrier-0x35 update that is
    /// NOT the Match net-object.
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
        0x3a => Kind::Clock, // op58
        0x32 => {
            // op50 SPAWN. The Match net-object (prop1 == 54) is spawned carrying its
            // INITIAL MatchState (prop5 == WaitingForPlayers=3, s506 obj 123) — surface
            // it as MatchState(3) so the progression check sees the 3 that arrives in
            // the spawn (subsequent states arrive via carrier-0x35 updates). All other
            // spawns (Player/Avatar) stay generic Spawn landmarks.
            let nd = parse_netdata(body);
            if nd.int(1) == Some(54) {
                match nd.int(5) {
                    Some(state) => Kind::MatchState(state as u8),
                    None => Kind::Spawn,
                }
            } else {
                Kind::Spawn
            }
        }
        0x35 => {
            // carrier-0x35 net-object UPDATE. If it carries the Match net-object
            // (prop1 == 54 == NetObjectType::Match), surface its replicated MatchState
            // (prop5) — that's the gate the round-start drives 3→4→5→…→13 (s506 obj
            // 123). Otherwise it's a generic channeling/player update.
            let nd = parse_netdata(body);
            if nd.int(1) == Some(54) {
                match nd.int(5) {
                    Some(state) => Kind::MatchState(state as u8),
                    None => Kind::Channeling,
                }
            } else {
                Kind::Channeling
            }
        }
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

// --- Two DISTINCT fighter identities (Phase 0.2) ---------------------------
//
// The fixture used to hand BOTH fighters the same `character_uuid` and a
// customization-less `{"name":"X"}` profile, which made the whole file blind to
// identity mix-ups: the avatar propId4 the client binds appearance off (and the
// profile it renders the opponent from) were indistinguishable between slots.
// Slot 0 and slot 1 now carry different UUIDs AND different customization, so a
// swapped identity is observable. (Shapes follow the real data: propId4 is a
// 36-char hyphenated UUID; the profile carries `customization.CharacterUID` =
// the `Visual_Player_{Gender}{Race}Visual` label the client resolves for looks.)

/// Slot 0's character UUID (`propId4` on its op50 Player/Avatar spawns).
const UUID_SLOT0: &str = "11111111-1111-4111-8111-111111111111";
/// Slot 1's character UUID — deliberately DIFFERENT from [`UUID_SLOT0`].
const UUID_SLOT1: &str = "22222222-2222-4222-8222-222222222222";
/// Slot 0's appearance block (the bit that visibly differs on screen).
const CUSTOMIZATION_SLOT0: &str =
    r#"{"CharacterUID":"Visual_Player_MaleNordVisual","hairIndex":3,"skinIndex":1}"#;
/// Slot 1's appearance block — deliberately DIFFERENT from [`CUSTOMIZATION_SLOT0`].
const CUSTOMIZATION_SLOT1: &str =
    r#"{"CharacterUID":"Visual_Player_FemaleRedguardVisual","hairIndex":7,"skinIndex":5}"#;

/// A fighter that carries a (non-empty) profile, so `broadcast_profiles` emits the
/// op54 PROFILE — required to reproduce s506's t+2 opponent profile.
///
/// `character_uuid` is what the op50 Player/Avatar spawns put at NetData propId4
/// (the client's `GetPvpPlayer` lookup key → appearance binding), and
/// `customization` is embedded in `profile_character_json` so the two fighters'
/// profiles are byte-distinct. Callers MUST pass distinct values per slot — the
/// s506 differential only checks protocol shape, but
/// `identity_burst_is_per_viewer_and_distinct` checks WHICH identity each viewer
/// receives, and that needs the two fighters to be tellable apart.
fn profiled(name: &str, character_uuid: &str, customization: &str) -> Loadout {
    let mut l = super::loadout::starter();
    l.display_name = name.to_string();
    l.character_uuid = character_uuid.to_string();
    l.abilities.push(super::state::EquippedAbility {
        instance_uuid: "5b764e61-8851-4703-8fea-3d8e589ed24f".to_string(),
        level: 1,
        tag: super::state::AbilityTag::Generic,
    });
    l.profile_equipped_json = r#"{"equippedItems":{}}"#.to_string();
    l.profile_character_json = format!(
        r#"{{"id":"{character_uuid}","name":"{name}","customization":{customization}}}"#
    );
    l
}

/// The two fixture fighters, slot 0 then slot 1 — distinct UUIDs, distinct names,
/// distinct customization.
fn s506_fighters() -> Vec<Loadout> {
    vec![
        profiled("Flappety", UUID_SLOT0, CUSTOMIZATION_SLOT0),
        profiled("Opponent", UUID_SLOT1, CUSTOMIZATION_SLOT1),
    ]
}

/// Drive a 2-fighter PvP match over s506's relative timing, replaying s506's c2s
/// at their captured offsets and collecting every s2c frame tagged with its second.
/// Ticks at 100 ms (≫ the engine's needs) across t+0…t+9 so every FSM transition
/// and the heartbeat fire on cadence. Returns the engine + the emitted log.
fn drive_s506() -> (MatchInstance, Vec<Emitted>) {
    let t0 = Instant::now();
    // PvP: 2 fighters, both real peers; both carry a profile (opponent-only relay).
    let mut m = MatchInstance::new(2, 2, s506_fighters(), t0);

    let c2s = s506_c2s();
    let mut log = Vec::new();
    let tag = |out: Vec<(usize, Vec<u8>)>, sec: u64, log: &mut Vec<Emitted>| {
        for (_viewer, frame) in out {
            if let Some(kind) = classify(&frame) {
                log.push(Emitted { rel_sec: sec, kind });
            }
        }
    };

    // 100 ms steps over 32 s. `connected = 2` from the start so the
    // Connecting→Spawning gate opens on the first tick (both peers present). The
    // window covers the FULL round-0 MatchState progression: spawns (t≈0) →
    // BackendMatchCreation(5) @t≈4 (SPAWN_HANDSHAKE_HOLD) → the 6→7→11→12→13 walk
    // (s506 deltas 0/2/3/12/5 s ≈ 22 s) → InRound(13) @t≈26 → StateTimeout (live round).
    let step = Duration::from_millis(100);
    let mut sec_emitted_c2s = std::collections::HashSet::new();
    for i in 0..=320u64 {
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
    // s506: spawns t+0 → BackendMatchCreated t+4 (Δ≈4s == SPAWN_HANDSHAKE_HOLD). The
    // round then walks the MatchState progression (5→6→7→11→12→13) into the live
    // round (StateTimeout) — that ~22s walk is validated in section (5), not here.
    // The DB ts is second-resolution, so allow ±1s.
    let spawn_to_bmc = bmc - spawn; // seconds
    let near = |got: u64, want: u64| got.abs_diff(want) <= 1;
    assert!(
        near(spawn_to_bmc, 4),
        "DIVERGENCE (STAGGER TIMING): spawns→BackendMatchCreated was {spawn_to_bmc}s, but s506 \
         measured ≈4s (SPAWN_HANDSHAKE_HOLD=4s). Re-tune SPAWN_HANDSHAKE_HOLD to match retail.",
    );
    assert!(
        bmc < stto,
        "DIVERGENCE: BackendMatchCreated (t+{bmc}) must precede the live round StateTimeout (t+{stto}).",
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

    // ---- (5) MatchState PROGRESSION — the round-0 walk past BackendMatchCreation(5)
    //          to InRound(13), the LAST gate to the fight. The client parks at
    //          "Setting up…" until the Match net-object's MatchState (obj 123 prop5)
    //          moves past 5; it enters the combat scene at InRound(13). s506 obj 123
    //          round 0: 3→4→5→6→7→11→12→13 (8/9/10 are the between-rounds re-choice,
    //          round 1 only). Our distinct MatchState emissions must reproduce this.
    // Filter to MatchState emissions in order, then collapse consecutive duplicates
    // (each state fans out to BOTH viewers → 2 identical copies; and the spawn's
    // state-3 to viewer 0/1 is split by the interleaved Player spawns, so we dedup the
    // STATE stream itself rather than the mixed-kind `distinct_sequence`).
    let states: Vec<u8> = {
        let raw: Vec<u8> = log
            .iter()
            .filter_map(|e| match e.kind {
                Kind::MatchState(s) => Some(s),
                _ => None,
            })
            .collect();
        let mut deduped = Vec::new();
        for s in raw {
            if deduped.last() != Some(&s) {
                deduped.push(s);
            }
        }
        deduped
    };
    // The exact round-0 sequence (deduped — consecutive repeats collapsed by
    // distinct_sequence). 3 and 4 are emitted in the Spawning phase; 5→…→13 in the
    // BackendMatchCreated phase via the progression table.
    let want_states: Vec<u8> = vec![3, 4, 5, 6, 7, 11, 12, 13];
    assert_eq!(
        states, want_states,
        "DIVERGENCE (MATCHSTATE): the Match net-object's MatchState progression must reproduce \
         s506 obj-123 round 0 (3→4→5→6→7→11→12→13 — InRound is the LAST gate to the fight).\n  \
         want: {want_states:?}\n  got:  {states:?}",
    );
    // InRound(13) MUST be reached and MUST precede the live round (StateTimeout): the
    // client only enters the combat scene once MatchState hits 13.
    let inround = first_sec(&log, |k| matches!(k, Kind::MatchState(13)));
    let inround = inround.expect(
        "DIVERGENCE (MATCHSTATE): MatchState never reached InRound(13) — the client stays parked \
         at 'Setting up…' (BackendMatchCreation=5). This is the gate this task drives past.",
    );
    assert!(
        inround <= stto,
        "DIVERGENCE: InRound(13) (t+{inround}) must be reached at/before the live round \
         StateTimeout (t+{stto}) — combat resolution begins only after InRound.",
    );

    // Reference summary (visible with `--nocapture`): our measured deltas vs s506.
    eprintln!(
        "s506 differential OK — round-start: clock t+{clock}, spawn t+{spawn}, profile t+{profile}, \
         BackendMatchCreated t+{bmc} (Δspawn {spawn_to_bmc}s, s506≈4s); MatchState walk {states:?} \
         → InRound t+{inround}; StateTimeout (live round) t+{stto}",
    );
}

/// `needle` appears in `hay` in order (not necessarily contiguous).
fn is_subsequence(needle: &[Kind], hay: &[Kind]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

// ---------------------------------------------------------------------------
// Phase 0.3 — per-viewer IDENTITY of the round-start burst.
//
// The s506 differential above only checks protocol SHAPE (which kinds, in which
// order, with what stagger). It is structurally blind to *whose* identity each
// frame carries — which is exactly the failure seen in the wild: both clients
// rendered the opponent with their OWN character's appearance while the NAMES
// (bound off a different path) looked right.
//
// Retail ground truth (s506, per viewer):
//   · self Player   (type 55, NetRole 3 Autonomous)   propId4 = self UUID
//   · opponent Player (type 55, NetRole 2 Simulated)  propId4 = opponent UUID
//   · self Avatar   (type 56, NetRole 3 Autonomous)   propId4 = self UUID
//   · opponent Avatar (type 56, NetRole 2 Simulated)  propId4 = opponent UUID
//     ← the Simulated Avatar's propId4 is the char-UUID the client looks up in
//       `_pvpPlayers` (`GetPvpPlayer`) to bind the OPPONENT's appearance.
//   · exactly ONE op54 PROFILE per viewer = the OPPONENT's (never the viewer's own).
// ---------------------------------------------------------------------------

/// One decoded op50 Avatar spawn: `(NetRole, character UUID at propId4)`.
fn avatar_spawns_for(burst: &[(usize, Vec<u8>)], viewer: usize) -> Vec<(i64, String)> {
    burst
        .iter()
        .filter(|(v, _)| *v == viewer)
        .filter_map(|(_, b)| {
            if b.len() < 3 || b[0] != 0xBE || b[1] != 0x32 {
                return None; // not an op50 spawn
            }
            let nd = parse_netdata(&b[2..]);
            if nd.int(1) != Some(56) {
                return None; // not an Avatar (55 = Player, 54 = Match)
            }
            Some((nd.int(2)?, nd.string(4)?.to_string()))
        })
        .collect()
}

/// The op54 PROFILE character-JSON blobs (propId5) delivered to `viewer`.
fn profiles_for(burst: &[(usize, Vec<u8>)], viewer: usize) -> Vec<String> {
    burst
        .iter()
        .filter(|(v, _)| *v == viewer)
        .filter_map(|(_, b)| {
            if b.len() < 3 || b[0] != 0xBE || b[1] != 0x36 {
                return None;
            }
            let nd = parse_netdata(&b[2..]);
            if nd.int(3) != Some(35) {
                return None; // op54 carrier is shared: 35 == the PROFILE GameMessageId
            }
            Some(nd.string(5)?.to_string())
        })
        .collect()
}

/// Every s2c frame of the round-start identity burst (spawns → avatars → profiles
/// → BackendMatchCreated), tagged with the viewer slot the engine addressed it to.
/// Ticks 100 ms over 0…6 s, which covers the whole burst (`MATCH_SETUP_STAGGER` 1 s
/// for the avatars+profiles, `SPAWN_HANDSHAKE_HOLD` 4 s for BackendMatchCreated).
fn drive_identity_burst() -> Vec<(usize, Vec<u8>)> {
    let t0 = Instant::now();
    let mut m = MatchInstance::new(2, 2, s506_fighters(), t0);
    let step = Duration::from_millis(100);
    let mut burst = Vec::new();
    for i in 0..=60u32 {
        burst.extend(m.on_tick(2, t0 + step * i));
    }
    burst
}

/// **Phase 0.3.** Each viewer's round-start identity burst must describe the RIGHT
/// two characters: its own as Autonomous(3), the OTHER one as Simulated(2), and the
/// single op54 profile it receives must be the opponent's. A viewer that is handed
/// its own UUID on the Simulated avatar renders the opponent wearing its own
/// character's appearance — the observed live bug.
///
/// **Status at `af2602d`: PASSES.** The engine's per-viewer construction is
/// correct in isolation — `broadcast_spawns` / `broadcast_avatars` /
/// `broadcast_profiles` all key off `actor.slot == viewer`. The live identity
/// inversion is therefore NOT in the burst's construction but in the fighter-slot
/// → peer resolution one layer up (`MatchRegistry::{tick_matches,
/// handle_live_user_data}` fall back to the FIFO `m.players[target]`), which
/// `match_registry::tests::admission_order_reversed_still_binds_correctly` pins.
/// Keep this test: it is what stops a Phase-2 routing fix from being "corrected"
/// by breaking the engine side instead.
#[test]
fn identity_burst_is_per_viewer_and_distinct() {
    let fighters = s506_fighters();
    let uuid: [String; 2] = [fighters[0].character_uuid.clone(), fighters[1].character_uuid.clone()];
    let name: [String; 2] = [fighters[0].display_name.clone(), fighters[1].display_name.clone()];
    assert_ne!(uuid[0], uuid[1], "fixture precondition: the two fighters must be tellable apart");

    let burst = drive_identity_burst();
    assert!(!burst.is_empty(), "the round-start burst emitted nothing at all");

    let mut delivered_profiles: Vec<String> = Vec::new();

    for viewer in 0..2usize {
        let opp = 1 - viewer;

        // ---- (a) op50 Avatar spawns: one Autonomous (self), one Simulated (opp) ----
        let avatars = avatar_spawns_for(&burst, viewer);
        assert_eq!(
            avatars.len(),
            2,
            "viewer {viewer}: retail s506 sends TWO Avatar (type 56) spawns per viewer — its own \
             (obj 124, role 3 Autonomous) and the OPPONENT's (obj 125, role 2 Simulated); the \
             Simulated one is what flips HasOpponentPlayer. Got: {avatars:?}",
        );

        let simulated: Vec<&(i64, String)> = avatars.iter().filter(|(r, _)| *r == 2).collect();
        assert_eq!(
            simulated.len(),
            1,
            "viewer {viewer}: exactly ONE Simulated(2) Avatar (the opponent's body). Got: {avatars:?}",
        );
        let sim_uuid = &simulated[0].1;
        assert_ne!(
            sim_uuid, &uuid[viewer],
            "IDENTITY BUG — viewer {viewer}: the Simulated(2) Avatar's propId4 is the viewer's OWN \
             character UUID ({}). The client binds the opponent's appearance off this UUID \
             (GetPvpPlayer → SpawnOpponent), so it renders the opponent as a copy of the local \
             character. It must be the OPPONENT's UUID ({}).",
            uuid[viewer], uuid[opp],
        );
        assert_eq!(
            sim_uuid, &uuid[opp],
            "viewer {viewer}: the Simulated(2) Avatar must carry the OPPONENT's (slot {opp}) \
             character UUID at propId4",
        );

        let autonomous: Vec<&(i64, String)> = avatars.iter().filter(|(r, _)| *r == 3).collect();
        assert_eq!(
            autonomous.len(),
            1,
            "viewer {viewer}: exactly ONE Autonomous(3) Avatar (the viewer's own body). Got: {avatars:?}",
        );
        assert_eq!(
            &autonomous[0].1, &uuid[viewer],
            "IDENTITY BUG — viewer {viewer}: the Autonomous(3) Avatar must carry the VIEWER's own \
             character UUID (IsLocalPlayer == NetRole::Autonomous)",
        );

        // ---- (b) exactly ONE op54 PROFILE, and it is the OPPONENT's ----------
        let profiles = profiles_for(&burst, viewer);
        assert_eq!(
            profiles.len(),
            1,
            "viewer {viewer}: retail sends EXACTLY ONE op54 profile per match per viewer — the \
             opponent's. Got {} ({:?}).",
            profiles.len(),
            profiles.iter().map(|p| p.len()).collect::<Vec<_>>(),
        );
        let profile = &profiles[0];
        assert!(!profile.is_empty(), "viewer {viewer}: the delivered profile JSON is empty");
        assert!(
            profile.contains(uuid[opp].as_str()) && profile.contains(name[opp].as_str()),
            "IDENTITY BUG — viewer {viewer}: the op54 profile it receives is not the OPPONENT's \
             (slot {opp}, {} / {}). The client builds the opponent actor — appearance, gear — \
             straight from this JSON. Got: {profile}",
            name[opp], uuid[opp],
        );
        assert!(
            !profile.contains(uuid[viewer].as_str()),
            "IDENTITY BUG — viewer {viewer}: it was handed a profile carrying its OWN character \
             UUID ({}). Retail never echoes a client its own profile. Got: {profile}",
            uuid[viewer],
        );
        delivered_profiles.push(profile.clone());
    }

    // ---- (c) the two viewers must not receive the SAME profile ---------------
    assert_ne!(
        delivered_profiles[0], delivered_profiles[1],
        "IDENTITY BUG: both viewers received the identical opponent profile — with a correct \
         per-viewer burst they are mirror images (viewer 0 gets slot 1's, viewer 1 gets slot 0's).",
    );
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

// ---------------------------------------------------------------------------
// The gmid DISTRIBUTION differential — did we fix the animation class?
// ---------------------------------------------------------------------------

/// The retail s2c `game_message_id` histogram for a live round, from prod session
/// **503** (the reference fight). Extracted read-only, session-scoped — never scan
/// `arena_udp_frames` unscoped, that took the box down for an hour once:
///
/// ```sql
/// SELECT game_message_id, COUNT(*) FROM arena_udp_frames
///  WHERE session_id=503 AND direction='s2c' AND game_message_id IS NOT NULL
///  GROUP BY game_message_id ORDER BY 2 DESC;
/// ```
///
/// ```text
///  50 → 894   51 → 832   45 → 491   39 → 361   75 → 338   52 → 330   43 → 325
///  44 → 291   38 → 207   65 → 182   41 → 175   79 → 147   53 → 116   42 →  50
///  58 →  45   72 →  41   59 →  32   35 →  19   64 →  13   29 →  12   48 →  10
/// ```
///
/// The rows this test exists for are the ones we used to send **zero** of. Absolute
/// counts are not comparable — s503 is a real human fight of a different length — so
/// the assertion is on the *shape*: the messages retail sent hundreds of, we must send
/// at least one of, in the right order, to both viewers.
const S503_ANIMATION_GMIDS: &[(i64, &str, u32)] = &[
    (52, "PlayerAutoAttackStateChange", 330),
    (43, "PlayerFollowThroughStateChange", 325),
    (44, "PlayerRecoveryStateChange", 291),
    (39, "PlayerStateChange", 361),
];

/// The `GameMessageId` at propId 3 of a carrier-0x36 frame, or `None`.
fn gmid_of(frame: &[u8]) -> Option<i64> {
    let (carrier, body) = user_data(frame)?;
    if carrier != 0x36 {
        return None;
    }
    parse_netdata(body).int(3)
}

/// A prod-shaped `PlayerCombatInputActivate` (gmid 46) press or release, as c2s.
/// Mirrors `resolve::tests::make_act_frame`; kept local because that module is private.
fn act_frame(held: bool) -> Vec<u8> {
    let mut w = arena_proto::NetDataWriter::new();
    w.int(0, 565)
        .byte(1, 56)
        .byte(2, 3)
        .byte(3, 46)
        .bool(4, held)
        .float(5, 0.0)
        .bool(6, false);
    let mut f = super::messages::frame_for_test(w.finish());
    f[0] = 0x84; // c2s marker
    f
}

/// Drive a live round with real swings and collect every s2c frame emitted, tagged
/// with the emitting tick so ordering can be checked.
fn drive_live_fight_gmids() -> Vec<(u64, i64)> {
    let (mut m, _t0, live) = super::engine::tests::live_inst_at(2);
    let mut log: Vec<(u64, i64)> = Vec::new();
    let step = Duration::from_millis(10);
    // ~3 s of round time: long enough for several swings at the starter weapon's
    // cadence, and for each swing's scheduled beats to come due on the tick.
    for i in 0..300u64 {
        let now = live + step * i as u32;
        // Slot 0 presses and releases every 400 ms (past the swing cooldown), so the
        // full AutoAttack → FollowThrough → Recovery → Idle walk runs repeatedly.
        if i % 40 == 0 {
            for (_, f) in m.on_c2s(0, &act_frame(true), now) {
                if let Some(g) = gmid_of(&f) {
                    log.push((i, g));
                }
            }
        }
        if i % 40 == 5 {
            for (_, f) in m.on_c2s(0, &act_frame(false), now) {
                if let Some(g) = gmid_of(&f) {
                    log.push((i, g));
                }
            }
        }
        for (_, f) in m.on_tick(2, now) {
            if let Some(g) = gmid_of(&f) {
                log.push((i, g));
            }
        }
    }
    log
}

/// **The objective pass/fail for the actor-state fix.** Every gmid retail sent
/// hundreds of during a fight must stop being zero for us.
///
/// This is the test that would have caught the original bug: before the fix our
/// emitted histogram contained op50 `ReceiveDamage` and essentially nothing else,
/// which is exactly why damage was right and nothing animated.
#[test]
fn live_fight_emits_the_retail_animation_gmids() {
    let log = drive_live_fight_gmids();
    assert!(!log.is_empty(), "the harness must emit something");
    for (gmid, name, retail) in S503_ANIMATION_GMIDS {
        let n = log.iter().filter(|(_, g)| g == gmid).count();
        assert!(
            n > 0,
            "gmid {gmid} {name}: emitted 0, retail s503 sent {retail}. \
             This is the animation class regressing — the client is being told the \
             result of combat and not the actor's state, so nothing animates."
        );
    }
    // op50 must still be there: this fix adds the state stream, it does not replace
    // the damage stream.
    assert!(
        log.iter().any(|(_, g)| *g == 50),
        "gmid 50 ReceiveDamage must still be emitted"
    );
}

/// The three beats of a swing must arrive in retail's order — 52, then 43, then 44 —
/// and be separated in TIME, not all crammed into one tick. Sending them together
/// would make the client flash through the animation rather than play it.
#[test]
fn swing_beats_are_ordered_and_staggered() {
    let log = drive_live_fight_gmids();
    let first = |gmid: i64| log.iter().find(|(_, g)| *g == gmid).map(|(tick, _)| *tick);
    let auto = first(52).expect("gmid 52 PlayerAutoAttackStateChange");
    let follow = first(43).expect("gmid 43 PlayerFollowThroughStateChange");
    let recover = first(44).expect("gmid 44 PlayerRecoveryStateChange");
    assert!(
        auto < follow && follow < recover,
        "retail order is 52 → 43 → 44; got ticks {auto} → {follow} → {recover}"
    );
    // 10 ms per tick: the capture-measured gaps are ~50 ms (52→43) and ~17 ms (43→44),
    // so each beat must land on a strictly later tick than the one before it.
    assert!(
        follow - auto >= 4,
        "52 → 43 must be ~50 ms apart (capture-measured 49-65 ms), got {} ms",
        (follow - auto) * 10
    );
    assert!(
        recover > follow,
        "43 → 44 must be on a later tick (capture-measured 16-21 ms)"
    );
}

/// Every actor-state frame goes to BOTH viewers. A viewer that only ever hears about
/// its own state cannot animate its opponent — that is the missing-enemy-swing bug.
#[test]
fn animation_frames_reach_both_viewers() {
    let (mut m, _t0, live) = super::engine::tests::live_inst_at(2);
    let step = Duration::from_millis(10);
    let mut per_viewer = [0usize; 2];
    for i in 0..300u64 {
        let now = live + step * i as u32;
        let mut burst = Vec::new();
        if i % 40 == 0 {
            burst.extend(m.on_c2s(0, &act_frame(true), now));
        }
        if i % 40 == 5 {
            burst.extend(m.on_c2s(0, &act_frame(false), now));
        }
        burst.extend(m.on_tick(2, now));
        for (viewer, f) in burst {
            if matches!(gmid_of(&f), Some(52) | Some(43) | Some(44) | Some(39) | Some(41)) {
                per_viewer[viewer] += 1;
            }
        }
    }
    assert!(per_viewer[0] > 0, "the acting player must see its own state changes");
    assert!(
        per_viewer[1] > 0,
        "the OPPONENT must see them too — this is the enemy-swing animation"
    );
    assert_eq!(
        per_viewer[0], per_viewer[1],
        "both viewers get the same stream; got {per_viewer:?}"
    );
}

// ---------------------------------------------------------------------------
// The BLOCK trigger — tracker report #5
// ---------------------------------------------------------------------------

/// A `PlayerCombatInputActivate` (gmid 46) press/release with an explicit block-zone
/// flag, at the pointer X retail actually observed for that class of press.
fn act_frame_zone(held: bool, block_zone: bool) -> Vec<u8> {
    let mut w = arena_proto::NetDataWriter::new();
    w.int(0, 565)
        .byte(1, 56)
        .byte(2, 3)
        .byte(3, 46)
        .bool(4, held)
        .float(5, 0.0)
        .bool(6, block_zone);
    let mut f = super::messages::frame_for_test(w.finish());
    f[0] = 0x84;
    f
}

/// A `PlayerCombatInputPosition` (gmid 47) sample, so the swing path has geometry to
/// classify from. Retail's block presses sit at X ≈ 0.077, attack presses at X ≈ 0.769.
fn pos_frame(x: f32) -> Vec<u8> {
    let mut w = arena_proto::NetDataWriter::new();
    w.int(0, 565)
        .byte(1, 56)
        .byte(2, 3)
        .byte(3, 47)
        .float(4, x)
        .float(5, 0.35)
        .float(6, 0.033_334)
        .float(7, 0.0)
        .int(8, 410);
    let mut f = super::messages::frame_for_test(w.finish());
    f[0] = 0x84;
    f
}

/// **Tracker report #5.** Pressing block must raise the shield on both screens.
///
/// The trigger is capture-proven: working backwards from every s2c gmid 41 across prod
/// sessions 503/506/486/615/616, the nearest preceding c2s 46 carried `blockZone=true`
/// in 432 of 433 cases, and never once `held=true, blockZone=false`.
///
/// Before this, the ONLY thing that could set the `Blocking` state was a handler gated
/// on an inbound gmid 41 — a frame retail's client sends **zero** of. So no shield, and
/// `damage::block_outcome` never saw a blocking defender either.
#[test]
fn block_zone_press_raises_the_shield_for_both_viewers() {
    let (mut m, _t0, live) = super::engine::tests::live_inst_at(2);
    let t = live + Duration::from_millis(100);

    // The client streams pointer geometry, then presses inside the block zone.
    m.on_c2s(0, &pos_frame(0.077), t);
    let out = m.on_c2s(0, &act_frame_zone(true, true), t + Duration::from_millis(10));

    let blocking: Vec<usize> = out
        .iter()
        .filter(|(_, f)| gmid_of(f) == Some(41))
        .map(|(v, _)| *v)
        .collect();
    assert_eq!(
        blocking.len(),
        2,
        "gmid 41 must reach BOTH viewers — the opponent has to see the guard go up too; \
         got {:?}",
        out.iter().map(|(v, f)| (*v, gmid_of(f))).collect::<Vec<_>>()
    );
    assert!(blocking.contains(&0) && blocking.contains(&1));

    // A block press is NOT a swing. Retail's block presses sit at X ≈ 0.077, which
    // `classify_side_from_x` would otherwise read as a Left swing on every guard.
    assert!(
        !out.iter().any(|(_, f)| gmid_of(f) == Some(50)),
        "a block press must deal no damage"
    );
    assert!(
        !out.iter().any(|(_, f)| gmid_of(f) == Some(52)),
        "a block press must not enter an attack state"
    );
}

/// A guard comes DOWN with a gmid 39 carrying stateId 0 (Idle) — never a second gmid
/// 41. All 578 decoded retail gmid-41 frames carry stateId Blocking; 199 of 225
/// own-avatar block exits are a `39/Idle` immediately after the release.
#[test]
fn releasing_the_block_zone_lowers_the_shield_with_gmid_39_idle() {
    let (mut m, _t0, live) = super::engine::tests::live_inst_at(2);
    let t = live + Duration::from_millis(100);
    m.on_c2s(0, &pos_frame(0.077), t);
    m.on_c2s(0, &act_frame_zone(true, true), t + Duration::from_millis(10));

    let out = m.on_c2s(0, &act_frame_zone(false, true), t + Duration::from_millis(700));
    let idle: Vec<&(usize, Vec<u8>)> = out
        .iter()
        .filter(|(_, f)| {
            gmid_of(f) == Some(39)
                && parse_netdata(user_data(f).unwrap().1).int(6) == Some(0)
        })
        .collect();
    assert_eq!(
        idle.len(),
        2,
        "the guard drops via gmid 39 stateId 0, to both viewers; got {:?}",
        out.iter().map(|(v, f)| (*v, gmid_of(f))).collect::<Vec<_>>()
    );
    assert!(
        !out.iter().any(|(_, f)| gmid_of(f) == Some(41)),
        "there is no shield-down variant of gmid 41 — retail never sends one"
    );
}

/// An ATTACK press cancels a standing guard: 94 of 225 real block exits end that way
/// rather than by a release.
#[test]
fn attack_press_cancels_a_standing_guard() {
    let (mut m, _t0, live) = super::engine::tests::live_inst_at(2);
    let t = live + Duration::from_millis(100);
    m.on_c2s(0, &pos_frame(0.077), t);
    m.on_c2s(0, &act_frame_zone(true, true), t + Duration::from_millis(10));

    // Pointer moves to the attack side, then presses outside the block zone.
    m.on_c2s(0, &pos_frame(0.769), t + Duration::from_millis(300));
    let out = m.on_c2s(0, &act_frame_zone(true, false), t + Duration::from_millis(310));
    assert!(
        out.iter().any(|(_, f)| {
            gmid_of(f) == Some(39) && parse_netdata(user_data(f).unwrap().1).int(6) == Some(0)
        }),
        "an attack press must lower the guard: got {:?}",
        out.iter().map(|(_, f)| gmid_of(f)).collect::<Vec<_>>()
    );
}

/// The pre-fix trigger is still dead, and must stay dead in the histogram sense: a
/// press OUTSIDE the block zone must never raise a shield.
#[test]
fn non_block_zone_press_never_raises_the_shield() {
    let (mut m, _t0, live) = super::engine::tests::live_inst_at(2);
    let t = live + Duration::from_millis(100);
    m.on_c2s(0, &pos_frame(0.769), t);
    let out = m.on_c2s(0, &act_frame_zone(true, false), t + Duration::from_millis(10));
    assert!(
        !out.iter().any(|(_, f)| gmid_of(f) == Some(41)),
        "retail never once followed a `held=true, blockZone=false` press with a gmid 41"
    );
}
