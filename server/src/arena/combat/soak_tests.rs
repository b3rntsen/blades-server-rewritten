//! CRE-SOAK — whole-match soak of the arena engine against prod-shaped characters.
//!
//! A green unit suite has hidden live defects before (four at once, found only by
//! driving a real character). This drives FULL matches, start to
//! `DisconnectingPlayersAfterMatch`, through the same two entry points the match
//! registry uses (`MatchInstance::on_tick` / `on_c2s`), on a simulated clock, with
//! loadouts built by the production builder (`loadout::from_character`) from:
//!
//!   * `testdata/soak_prod_characters.json` — 30 real `characters` rows that carry
//!     learned perks (read-only prod SELECT, 2026-09-25; names, character ids and
//!     user ids never selected, item instance ids replaced). Only the fields the
//!     loadout builder reads are kept.
//!   * `testdata/soak_edge_characters.json` — hand-built edge builds: every perk at
//!     max rank, a Mettle/Reckless Fury maneuver kit, Healing Surge at 0 stamina, a
//!     Maximum Power caster (plain and ravaged), no learned map, and a junk learned
//!     map. They borrow real prod gear by predicate (`gear`).
//!
//! It guards the prod-bug classes that matter for a match: a hang, a match that
//! never terminates, a panic in a round, and a broken round / match-end progression.
//! Per match it asserts: termination inside [`match_bound`]; no panic; at most three
//! rounds; a valid MatchState progression (internal and on the wire, and they
//! agree); no NaN/inf in any fighter state or any float on the wire; every s2c frame
//! survives the registry's real send path (channel pick + ChaCha20 seal/open) and is
//! a well-formed NetData stream that re-encodes byte-identically through
//! `NetDataWriter`; health/stamina/magicka within `[0, max]`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use arena_proto::{NetDataValue, NetDataWriter};
use blades_lib::user_data::{
    Backpack, CompleteCharacter, CompleteInventory, EquippedItems, Loadout as InvLoadout, Treasury,
};
use serde_json::Value;

use super::*;
use crate::arena::combat::loadout;
use crate::arena::combat::state::{DamageType, MatchState};
use crate::arena::combat::tables::Weight;

const PROD_FIXTURES: &str = include_str!("testdata/soak_prod_characters.json");
const EDGE_FIXTURES: &str = include_str!("testdata/soak_edge_characters.json");

// ---------------------------------------------------------------------------
// Deterministic RNG (SplitMix64) — no dependency, reproducible from the seed.
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, p_per_mille: u64) -> bool {
        self.below(1000) < p_per_mille
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Fixture {
    name: String,
    loadout: Loadout,
    /// Hook: zero this fighter's stamina whenever a round goes live.
    zero_stamina_at_round_start: bool,
    /// Hook: ravage this much of the magicka pool whenever a round goes live.
    ravage_magicka_at_round_start: f32,
}

fn inventory_from(equipped: &Value) -> CompleteInventory {
    let equipped_items: EquippedItems = if equipped.is_null() {
        EquippedItems::default()
    } else {
        serde_json::from_value(equipped.clone()).expect("fixture equippedItems deserialises")
    };
    CompleteInventory {
        backpack: Backpack::default(),
        loadout: InvLoadout { equipped_items, ..Default::default() },
        treasury: Treasury::default(),
        overflow_treasury: Treasury::default(),
        backpack_version: 1,
        treasury_version: 0,
    }
}

fn character_from(row: &Value, name: &str) -> CompleteCharacter {
    let u = |k: &str| row.get(k).and_then(Value::as_u64).unwrap_or(0);
    CompleteCharacter {
        name: name.to_string(),
        level: u("level") as u16,
        stamina_attribute_points: u("staminaAttributePoints") as u32,
        magicka_attribute_points: u("magickaAttributePoints") as u32,
        equipped_abilities: row.get("equippedAbilities").cloned().unwrap_or(Value::Null),
        abilities: row.get("abilities").cloned().unwrap_or(Value::Null),
        ..Default::default()
    }
}

/// The production builder, plus the two identity fields the matchmaker's
/// `loadout_from_row` adds that the engine reads (distinct character uuids keep the
/// two avatars distinct, exactly as the paired-match guard requires).
fn build_loadout(row: &Value, name: &str, equipped: &Value, idx: usize) -> Loadout {
    let mut lo = loadout::from_character(&character_from(row, name), &inventory_from(equipped));
    lo.character_uuid = format!("50ac0000-0000-4000-8000-{:012x}", idx + 1);
    lo.display_name = name.to_string();
    lo
}

fn load_fixtures() -> Vec<Fixture> {
    let prod: Vec<Value> = serde_json::from_str(PROD_FIXTURES).expect("prod fixture JSON");
    let edge: Vec<Value> = serde_json::from_str(EDGE_FIXTURES).expect("edge fixture JSON");
    assert_eq!(prod.len(), 30, "the prod sample is 30 characters");

    let mut out = Vec::new();
    for (i, row) in prod.iter().enumerate() {
        let name = row["fixture"].as_str().unwrap().to_string();
        out.push(Fixture {
            loadout: build_loadout(row, &name, &row["equippedItems"], i),
            name,
            zero_stamina_at_round_start: false,
            ravage_magicka_at_round_start: 0.0,
        });
    }

    // Gear donors for the edge builds, strongest first.
    let mut donors: Vec<(usize, &Value)> = prod.iter().enumerate().collect();
    donors.sort_by_key(|(i, r)| (std::cmp::Reverse(r["level"].as_u64().unwrap_or(0)), *i));
    let pick = |pred: &dyn Fn(&Loadout) -> bool| -> Value {
        donors
            .iter()
            .find(|(i, _)| pred(&out[*i].loadout))
            .map(|(_, r)| r["equippedItems"].clone())
            .expect("a prod fixture matches the gear predicate")
    };
    let mut edges = Vec::new();
    for (j, row) in edge.iter().enumerate() {
        let name = row["fixture"].as_str().unwrap().to_string();
        let gear = match row["gear"].as_str().unwrap_or("none") {
            "shield" => pick(&|l: &Loadout| l.has_shield),
            "two-handed" => pick(&|l: &Loadout| !l.has_shield && l.weapon.weight == Some(Weight::Heavy)),
            "strongest" => pick(&|_: &Loadout| true),
            _ => Value::Null,
        };
        edges.push(Fixture {
            loadout: build_loadout(row, &name, &gear, 100 + j),
            zero_stamina_at_round_start: row["zeroStaminaAtRoundStart"].as_bool().unwrap_or(false),
            ravage_magicka_at_round_start: row["ravageMagickaAtRoundStart"].as_f64().unwrap_or(0.0)
                as f32,
            name,
        });
    }
    out.extend(edges);
    out
}

/// Every soak fixture as `(name, loadout)`, for the real-transport soak in
/// `enet_host` (which drives the same loadouts over rusty_enet + ChaCha20).
pub(crate) fn fixture_loadouts() -> Vec<(String, Loadout)> {
    load_fixtures().into_iter().map(|f| (f.name, f.loadout)).collect()
}

/// [`match_bound`] at the soak's maximum tick, for the real-transport soak.
pub(crate) fn soak_match_bound(max_tick: Duration) -> Duration {
    match_bound(max_tick)
}

// ---------------------------------------------------------------------------
// The bound
// ---------------------------------------------------------------------------

fn walk(table: &[(MatchState, Duration, f32)]) -> Duration {
    table.iter().map(|(_, hold, _)| *hold).sum()
}

/// The longest a match may legitimately take, derived from the engine's own timers:
/// the round-start handshake + round-0 walk, three rounds each capped by the
/// authoritative `ROUND_TIMEOUT`, two between-rounds walks, and the match-end walk.
/// Plus a slack of one maximum tick per state transition (a hold is only noticed on
/// the first tick at/after it expires).
fn match_bound(max_tick: Duration) -> Duration {
    let timers = SPAWN_HANDSHAKE_HOLD
        + walk(MATCH_STATE_ROUND0_PROGRESSION)
        + ROUND_TIMEOUT * 3
        + walk(MATCH_STATE_INTERROUND_PROGRESSION) * 2
        + walk(MATCH_STATE_MATCHEND_PROGRESSION);
    let transitions = 2
        + MATCH_STATE_ROUND0_PROGRESSION.len()
        + 1
        + 3
        + 2 * MATCH_STATE_INTERROUND_PROGRESSION.len()
        + MATCH_STATE_MATCHEND_PROGRESSION.len()
        + 1;
    timers + max_tick * transitions as u32
}

// ---------------------------------------------------------------------------
// Match driver
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Both slots are driven by the engine's own bot AI (swings, guards, abilities).
    BotVsBot,
    /// Slot 0 is a "human" peer sending random real c2s input frames (swing, guard,
    /// ability casts through `request_execute_ability`, Ready presses); slot 1 is the
    /// engine bot. The prod solo-fallback shape.
    ScriptedHumanVsBot,
    /// Both slots are scripted human peers (the PvP shape, expected_peers 2).
    ScriptedPvp,
    /// PvP where nobody ever sends a byte: every round must end on ROUND_TIMEOUT.
    SilentPvp,
    /// Human vs bot where the human never acts.
    SilentHumanVsBot,
    /// Bot vs bot, one side leaves (ENet departure) at a random moment.
    DepartureMidMatch,
    /// Scripted human vs bot, the human presses the exit button at a random moment.
    ConcedeMidMatch,
}

#[derive(Default, Debug)]
struct Outcome {
    sim: Duration,
    rounds: usize,
    winner: Option<usize>,
    rounds_won: [u8; 2],
    frames: usize,
    casts: [u32; 2],
    damage_frames: usize,
    victory_cards: usize,
    ended_by_timeout: usize,
    conceded: bool,
    deaths: usize,
    round_results_sent: usize,
    double_kos: usize,
    seen_round_ends: usize,
    seen_score: u8,
    states: Vec<u8>,
}

const KEY: [u8; 32] = [0x5a; 32];
const NONCE: [u8; 8] = [0xa5; 8];

fn finite_f32(v: f32) -> bool {
    v.is_finite()
}

fn check_value(v: &NetDataValue) -> Result<(), String> {
    match v {
        NetDataValue::Float(f) if !finite_f32(*f) => Err(format!("non-finite Float {f}")),
        NetDataValue::Double(f) if !f.is_finite() => Err(format!("non-finite Double {f}")),
        NetDataValue::Vector2(b) => vec_finite(b),
        NetDataValue::Vector3(b) => vec_finite(b),
        _ => Ok(()),
    }
}

fn vec_finite(b: &[u8]) -> Result<(), String> {
    for c in b.chunks(4) {
        let f = f32::from_le_bytes(c.try_into().unwrap());
        if !f.is_finite() {
            return Err(format!("non-finite vector component {f}"));
        }
    }
    Ok(())
}

/// Every s2c frame, through what the registry does to it before ENet (the retail
/// channel pick, then ChaCha20 sealing under the peer key), and then decoded and
/// re-encoded through the real NetData codec. Returns the frame's gmid if it is a
/// carrier-0x36 user message.
fn check_frame(frame: &[u8], stats: &mut BTreeMap<u8, usize>) -> Result<Option<i64>, String> {
    if frame.len() < 3 {
        return Err(format!("frame too short: {frame:02x?}"));
    }
    if frame[0] != 0xBE {
        return Err(format!("s2c frame does not start with the 0xBE marker: {:02x?}", &frame[..3]));
    }
    *stats.entry(frame[1]).or_insert(0) += 1;
    let channel = messages::retail_channel(frame);
    if channel > 7 {
        return Err(format!("channel {channel} out of range"));
    }
    let mut sealed = frame.to_vec();
    arena_proto::chacha20_legacy_xor(&mut sealed, &KEY, &NONCE);
    arena_proto::chacha20_legacy_xor(&mut sealed, &KEY, &NONCE);
    if sealed != frame {
        return Err("seal/open round trip changed the frame".into());
    }
    let body = &frame[2..];
    let nd = arena_proto::parse_netdata(body);
    if !nd.ok {
        return Err(format!("carrier 0x{:02x}: NetData ran out of bytes at {}", frame[1], nd.consumed));
    }
    if nd.consumed != body.len() {
        return Err(format!(
            "carrier 0x{:02x}: {} trailing byte(s) after the NetData stream",
            frame[1],
            body.len() - nd.consumed
        ));
    }
    for (pid, v) in &nd.props {
        check_value(v).map_err(|e| format!("carrier 0x{:02x} prop {pid}: {e}", frame[1]))?;
    }
    let mut w = NetDataWriter::new();
    for (pid, v) in &nd.props {
        w.put(*pid, v.clone());
    }
    let re = w
        .finish_checked()
        .map_err(|o| format!("carrier 0x{:02x}: re-encode overflow {o:?}", frame[1]))?;
    if re != body {
        return Err(format!("carrier 0x{:02x}: NetData does not re-encode byte-identically", frame[1]));
    }
    Ok(if frame[1] == 0x36 { nd.int(3) } else { None })
}

/// No NaN / inf anywhere in the fighters' state (pools, loadout, perks, effects,
/// negation pools, damage history, pending impacts). `Debug` walks every field, so a
/// newly added float cannot escape the check.
fn check_combat_floats(m: &MatchInstance) -> Result<(), String> {
    let dump = format!("{:?}", m.combat);
    for bad in ["NaN", ": inf", ": -inf", "(inf", "(-inf", " inf,", " -inf,", " inf)", " -inf)"] {
        if let Some(at) = dump.find(bad) {
            let lo = at.saturating_sub(160);
            return Err(format!("non-finite float in combat state (`{bad}`): …{}…", &dump[lo..(at + 40).min(dump.len())]));
        }
    }
    Ok(())
}

fn check_pools(m: &MatchInstance) -> Result<(), String> {
    for f in &m.combat.fighters {
        if f.health > f.max_health || f.stamina > f.max_stamina || f.magicka > f.max_magicka {
            return Err(format!(
                "slot {} pools out of range: hp {}/{} st {}/{} mg {}/{}",
                f.slot, f.health, f.max_health, f.stamina, f.max_stamina, f.magicka, f.max_magicka
            ));
        }
    }
    Ok(())
}

/// Legal MatchState successions. `concede` adds the edges a concession or departure
/// may take out of any pre-terminal walk into PostRound.
fn legal(prev: u8, next: u8) -> bool {
    use MatchState as S;
    let (p, n) = (prev, next);
    let e = |a: S, b: S| p == a as u8 && n == b as u8;
    e(S::Idle, S::WaitingForPlayers)
        || e(S::WaitingForPlayers, S::InitialPlayerSetup)
        || e(S::InitialPlayerSetup, S::BackendMatchCreation)
        || e(S::BackendMatchCreation, S::OpponentFoundFeedback)
        || e(S::OpponentFoundFeedback, S::PreMatch)
        || e(S::PreMatch, S::OpponentShowcase)
        || e(S::OpponentShowcase, S::PreRound)
        || e(S::PreRound, S::InRound)
        || e(S::InRound, S::PostRound)
        || e(S::PostRound, S::ChooseLoadout)
        || e(S::ChooseLoadout, S::AwaitingClientBackendSynchronization)
        || e(S::AwaitingClientBackendSynchronization, S::SynchronizingLoadout)
        || e(S::SynchronizingLoadout, S::OpponentShowcase)
        || e(S::PostRound, S::BackendMatchEnd)
        || e(S::BackendMatchEnd, S::Victory)
        || e(S::Victory, S::PostMatch)
        || e(S::PostMatch, S::DisconnectingPlayersAfterMatch)
}

/// A concession out of a walk that is not already the match-end walk.
fn legal_concede(prev: u8, next: u8) -> bool {
    use MatchState as S;
    next == S::PostRound as u8
        && ![S::PostRound as u8, S::BackendMatchEnd as u8, S::Victory as u8, S::PostMatch as u8,
            S::DisconnectingPlayersAfterMatch as u8, S::Idle as u8]
            .contains(&prev)
}

fn c2s_user_message(net_object_id: i32, gmid: u8) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, net_object_id).byte(1, 56).byte(2, 3).byte(3, gmid);
    let mut f = messages::frame_for_test(w.finish());
    f[0] = 0x84;
    f
}

fn c2s_zone(held: bool, block_zone: bool) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, 565).byte(1, 56).byte(2, 3).byte(3, 46).bool(4, held).float(5, 0.0).bool(6, block_zone);
    let mut f = messages::frame_for_test(w.finish());
    f[0] = 0x84;
    f
}

/// One scripted human input for `slot`, or none this tick.
fn human_input(m: &MatchInstance, slot: usize, rng: &mut Rng) -> Option<Vec<u8>> {
    let f = &m.combat.fighters[slot];
    match m.combat.phase {
        FlowState::StateTimeout => {
            if !rng.chance(180) {
                return None;
            }
            Some(match rng.below(10) {
                0..=2 => vec![0x84, 0x36], // swing (the engine tests' swing frame)
                // An attack PRESS: the next release (5) commits it after a random hold,
                // so the soak also drives the charge clock (a tap below MinDamageTime
                // fails, a hold on the plateau crits, combat-spec 02 X1/X2).
                3 => c2s_zone(true, false),
                4 => c2s_zone(true, true),  // raise guard
                5 => c2s_zone(false, false), // release
                _ => {
                    let casts: Vec<&str> = f
                        .loadout
                        .abilities
                        .iter()
                        .filter(|a| a.tag != crate::arena::combat::state::AbilityTag::Perk)
                        .map(|a| a.instance_uuid.as_str())
                        .collect();
                    if casts.is_empty() {
                        vec![0x84, 0x36]
                    } else {
                        let uuid = casts[rng.below(casts.len() as u64) as usize];
                        messages::request_execute_ability(f.net_object_id, uuid)
                    }
                }
            })
        }
        FlowState::BackendMatchCreated | FlowState::NextState | FlowState::RoundEnd => {
            // Occasionally press Ready (op57), which must only ever shorten a hold.
            rng.chance(8).then(|| c2s_user_message(f.player_net_object_id, 57))
        }
        _ => None,
    }
}

fn run_match(
    seed: u64,
    a: &Fixture,
    b: &Fixture,
    mode: Mode,
    bound: Duration,
) -> Result<Outcome, String> {
    let mut rng = Rng(seed);
    let t0 = Instant::now();
    let (expected_peers, connected) = match mode {
        Mode::BotVsBot | Mode::DepartureMidMatch => (1, 1),
        Mode::ScriptedHumanVsBot | Mode::SilentHumanVsBot | Mode::ConcedeMidMatch => (1, 1),
        Mode::ScriptedPvp | Mode::SilentPvp => (2, 2),
    };
    let fixtures = [a, b];
    let mut m = MatchInstance::new(2, expected_peers, vec![a.loadout.clone(), b.loadout.clone()], t0);
    m.set_game_session_id(format!("soak-{seed:016x}"));
    let humans: Vec<usize> = match mode {
        Mode::ScriptedHumanVsBot | Mode::ConcedeMidMatch => vec![0],
        Mode::ScriptedPvp => vec![0, 1],
        _ => vec![],
    };
    // Leave / concede somewhere inside the first two live rounds' worth of time.
    let quit_at = matches!(mode, Mode::DepartureMidMatch | Mode::ConcedeMidMatch)
        .then(|| t0 + Duration::from_millis(27_000 + rng.below(150_000)));
    let quitter = rng.below(2) as usize;

    let mut o = Outcome::default();
    let mut carriers = BTreeMap::new();
    let mut internal: Vec<u8> = vec![m.combat.match_state as u8];
    let mut wire: Vec<u8> = Vec::new();
    let mut now = t0;
    let mut ticks: u64 = 0;
    let mut quit_done = false;
    let mut last_phase = m.combat.phase;
    let trace = std::env::var("SOAK_TRACE").ok().as_deref() == Some(&format!("{seed:#018x}"));
    let mut traced_winners = 0usize;
    if trace {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Info).try_init();
    }

    let mut absorb = |out: Vec<(usize, Vec<u8>)>,
                      m: &MatchInstance,
                      o: &mut Outcome,
                      wire: &mut Vec<u8>,
                      internal: &mut Vec<u8>|
     -> Result<(), String> {
        for (viewer, frame) in &out {
            if *viewer > 1 {
                return Err(format!("frame addressed to slot {viewer}"));
            }
            let gmid = check_frame(frame, &mut carriers)?;
            o.frames += 1;
            match gmid {
                Some(38) if *viewer == 0 => {
                    // PerformExecuteAbility, counted once (viewer 0) per cast.
                    let nd = arena_proto::parse_netdata(&frame[2..]);
                    let obj = nd.int(0).unwrap_or(-1) as i32;
                    for s in 0..2 {
                        if m.combat.fighters[s].net_object_id == obj {
                            o.casts[s] += 1;
                        }
                    }
                }
                Some(50) => o.damage_frames += 1,
                Some(29) if *viewer == 0 => o.deaths += 1,
                Some(48) if *viewer == 0 => {
                    o.round_results_sent += 1;
                    // The client tallies the score from op48's per-round array
                    // (`GetNumberOfRoundsWonBy@0x1cea200` counts entries naming the
                    // winner; a tied round has empty ids). So the decided entries must
                    // equal the server's score after every round end, replays included.
                    let nd = arena_proto::parse_netdata(&frame[2..]);
                    let decided = [5u8, 7, 9]
                        .iter()
                        .filter(|&&p| nd.string(p).is_some_and(|w| !w.is_empty()))
                        .count();
                    let score: usize = m.combat.rounds_won.iter().map(|&w| w as usize).sum();
                    if decided != score {
                        return Err(format!(
                            "op48 carries {decided} decided rounds but the score is {:?}",
                            m.combat.rounds_won
                        ));
                    }
                }
                Some(49) => {
                    if m.combat.match_state != MatchState::Victory {
                        return Err(format!(
                            "op49 victory card sent while MatchState is {:?}, not Victory",
                            m.combat.match_state
                        ));
                    }
                    o.victory_cards += 1;
                }
                _ => {}
            }
            if *viewer == 0 && frame[1] == 0x35 {
                if let Some(s) = arena_proto::parse_netdata(&frame[2..]).int(5) {
                    if wire.last() != Some(&(s as u8)) {
                        wire.push(s as u8);
                    }
                }
            }
        }
        // The MatchState after EVERY engine call (a c2s and the tick after it can
        // each move it), checked against the legal succession.
        let st = m.combat.match_state as u8;
        if internal.last() != Some(&st) {
            let prev = *internal.last().unwrap();
            if !(legal(prev, st) || (o.conceded && legal_concede(prev, st))) {
                return Err(format!("illegal MatchState transition {prev} -> {st} (so far {internal:?})"));
            }
            internal.push(st);
        }
        // A round recorded with no score change is a DOUBLE KO: the authored rule
        // replays it, so it legitimately adds a round beyond three.
        let score: u8 = m.combat.rounds_won.iter().sum();
        if m.combat.round_winners.len() > o.seen_round_ends {
            if score == o.seen_score {
                o.double_kos += m.combat.round_winners.len() - o.seen_round_ends;
            }
            o.seen_round_ends = m.combat.round_winners.len();
            o.seen_score = score;
        }
        if m.combat.round as usize > 3 + o.double_kos || m.combat.round_winners.len() > 3 + o.double_kos {
            return Err(format!(
                "more than 3 rounds: round {} winners {:?} score {:?} ({} double KO)",
                m.combat.round, m.combat.round_winners, m.combat.rounds_won, o.double_kos
            ));
        }
        Ok(())
    };

    loop {
        // Jittered tick: 30-90 ms, like a real service loop under load.
        now += Duration::from_millis(30 + rng.below(61));
        ticks += 1;
        if now.duration_since(t0) > bound {
            return Err(format!(
                "did not terminate within the {bound:?} bound (phase {}, state {:?}, round {}, score {:?})",
                m.combat.phase_name(),
                m.combat.match_state,
                m.combat.round,
                m.combat.rounds_won
            ));
        }

        // Human inputs first (they arrive between ticks in the registry).
        for &h in &humans {
            if let Some(frame) = human_input(&m, h, &mut rng) {
                let out = m.on_c2s(h, &frame, now);
                absorb(out, &m, &mut o, &mut wire, &mut internal)?;
            }
        }
        if let (Some(at), false) = (quit_at, quit_done) {
            if now >= at && !m.is_finished() {
                quit_done = true;
                let before = m.combat.phase;
                let out = match mode {
                    Mode::ConcedeMidMatch => m.on_c2s(0, &[0xBE, GameMessageId::ConcedeMatch as u8], now),
                    _ => m.concede_by_departure(quitter, now),
                };
                if !matches!(before, FlowState::RoundEnd | FlowState::Finished) {
                    o.conceded = true;
                }
                absorb(out, &m, &mut o, &mut wire, &mut internal)?;
            }
        }

        let out = m.on_tick(connected, now);
        absorb(out, &m, &mut o, &mut wire, &mut internal)?;

        // Bot-vs-bot: once past Connecting, hand slot 0 to the bot AI too.
        if matches!(mode, Mode::BotVsBot | Mode::DepartureMidMatch)
            && m.combat.expected_peers != 0
            && !m.is_connecting()
        {
            m.combat.expected_peers = 0;
        }

        // Round-start hooks (edge fixtures): fire on the tick a round goes live.
        if m.combat.phase == FlowState::StateTimeout && last_phase != FlowState::StateTimeout {
            for s in 0..2 {
                let fx = fixtures[s];
                let f = &mut m.combat.fighters[s];
                if fx.zero_stamina_at_round_start {
                    f.stamina = 0;
                }
                if fx.ravage_magicka_at_round_start > 0.0 {
                    f.apply_ravage(&[(DamageType::Magicka, fx.ravage_magicka_at_round_start)], 1.0);
                }
            }
        }
        // A round that closed with both fighters alive was closed by ROUND_TIMEOUT
        // (a concession is counted separately).
        if last_phase == FlowState::StateTimeout
            && matches!(m.combat.phase, FlowState::NextState | FlowState::RoundEnd)
            && !o.conceded
            && m.combat.fighters.iter().all(|f| !f.is_dead())
        {
            o.ended_by_timeout += 1;
        }
        last_phase = m.combat.phase;

        if trace && m.combat.round_winners.len() != traced_winners {
            traced_winners = m.combat.round_winners.len();
            eprintln!(
                "SOAK-TRACE t={:?} phase={} state={:?} round={} winners={:?} won={:?} hp={}/{} {}/{}",
                now.duration_since(t0),
                m.combat.phase_name(),
                m.combat.match_state,
                m.combat.round,
                m.combat.round_winners,
                m.combat.rounds_won,
                m.combat.fighters[0].health,
                m.combat.fighters[0].max_health,
                m.combat.fighters[1].health,
                m.combat.fighters[1].max_health,
            );
        }

        // Invariants, every tick.
        check_pools(&m)?;
        if ticks % 16 == 0 || m.is_finished() {
            check_combat_floats(&m)?;
        }
        if m.is_finished() {
            break;
        }
    }

    o.sim = now.duration_since(t0);
    o.rounds = m.combat.round_winners.len();
    o.winner = m.combat.winner;
    o.rounds_won = m.combat.rounds_won;
    o.states = internal.clone();

    // --- terminal-shape assertions --------------------------------------------
    if internal.last() != Some(&(MatchState::DisconnectingPlayersAfterMatch as u8)) {
        return Err(format!("finished without reaching Disconnecting(19): {internal:?}"));
    }
    // The wire must carry the same progression the engine walked (the spawn frame
    // carries WaitingForPlayers, so the op55 stream starts at InitialPlayerSetup).
    let expected_wire: Vec<u8> = internal
        .iter()
        .copied()
        .filter(|s| *s != MatchState::Idle as u8 && *s != MatchState::WaitingForPlayers as u8)
        .collect();
    if wire != expected_wire {
        return Err(format!("op55 wire progression {wire:?} != engine progression {expected_wire:?}"));
    }
    let inrounds = internal.iter().filter(|s| **s == MatchState::InRound as u8).count();
    let postrounds = internal.iter().filter(|s| **s == MatchState::PostRound as u8).count();
    // A concession while the Match object already sits at PostRound (the hold before
    // ChooseLoadout) records its round without a second PostRound broadcast.
    let postround_ok = postrounds == o.rounds || (o.conceded && postrounds + 1 == o.rounds);
    if !postround_ok || inrounds < o.rounds.saturating_sub(1) || inrounds > o.rounds {
        return Err(format!(
            "round bookkeeping: {inrounds} InRound, {postrounds} PostRound, {} recorded rounds ({internal:?})",
            o.rounds
        ));
    }
    let Some(w) = o.winner else {
        return Err("match finished with no winner".into());
    };
    if !o.conceded && o.rounds_won[w] != 2 {
        return Err(format!("un-conceded match ended without a 2-round winner: {:?}", o.rounds_won));
    }
    if (o.rounds_won[0] + o.rounds_won[1]) as usize > o.rounds {
        return Err(format!("more round wins {:?} than rounds {}", o.rounds_won, o.rounds));
    }
    if o.deaths > o.rounds {
        return Err(format!("{} op29 death frames for {} rounds", o.deaths, o.rounds));
    }
    if o.round_results_sent != o.rounds {
        return Err(format!("{} op48 round results for {} rounds", o.round_results_sent, o.rounds));
    }
    if o.victory_cards == 0 {
        return Err("no op49 victory card was sent".into());
    }
    if o.rounds == 0 {
        return Err("no round was played".into());
    }
    let _ = carriers;
    Ok(o)
}

fn run_guarded(
    seed: u64,
    a: &Fixture,
    b: &Fixture,
    mode: Mode,
    bound: Duration,
) -> Result<Outcome, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_match(seed, a, b, mode, bound)))
        .unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".into());
            Err(format!("PANIC: {msg}"))
        })
}

const MAX_TICK: Duration = Duration::from_millis(90);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every fixture builds, and the perk path under test actually has something to
/// resolve: most prod characters get a non-empty perk set (the #364 change), the
/// edge builds get what they were built for, and every loadout is finite.
#[test]
fn soak_fixtures_build_and_carry_perks() {
    let fx = load_fixtures();
    assert_eq!(fx.len(), 38);
    let prod_with_perks = fx[..30].iter().filter(|f| !f.loadout.perks.is_empty()).count();
    assert!(prod_with_perks >= 25, "only {prod_with_perks}/30 prod fixtures resolved any perk");
    for f in &fx {
        let dump = format!("{:?}", f.loadout);
        assert!(!dump.contains("NaN") && !dump.contains(": inf"), "{}: non-finite loadout", f.name);
        assert!(!f.loadout.weapon.base_by_type.is_empty(), "{}: no weapon", f.name);
    }
    let by = |n: &str| fx.iter().find(|f| f.name == n).unwrap();
    let all = &by("edge-all-perks-max").loadout.perks;
    assert!(all.mettle > 0.0 && all.max_power > 0.0 && all.healing_surge > 0.0 && all.combat_focus > 0.0);
    assert!(by("edge-mettle-reckless-fury").loadout.perks.mettle > 0.0);
    assert!(!by("edge-mettle-reckless-fury").loadout.has_shield, "Mettle build must be two-handed");
    assert!(by("edge-healing-surge-zero-stamina").loadout.perks.healing_surge > 0.0);
    assert!(by("edge-maximum-power-caster").loadout.perks.max_power > 0.0);
    assert!(by("edge-no-learned-map").loadout.perks.is_empty());
    assert!(by("edge-no-learned-map").loadout.abilities.is_empty());
    // Junk map: only the well-formed entries survive (MatchingSet clamped, CombatFocus
    // clamped); rank 0, "5", -1, 2.5, unknown and enemy-only are dropped.
    let junk = &by("edge-junk-learned-map").loadout.perks;
    assert!(junk.combat_focus > 0.0, "CombatFocus 65535 clamps to max, not dropped");
    assert_eq!(junk.healing_surge, 0.0, "a string rank is not a learned perk");
    assert_eq!(junk.mettle, 0.0, "a float rank is not a learned perk");
    let names: Vec<String> = fx.iter().map(|f| f.name.clone()).collect();
    eprintln!("CRE-SOAK fixtures: {prod_with_perks}/30 prod with perks; {names:?}");
}

/// THE SOAK: 240 full matches, fixtures paired by a seeded RNG, every mode, each to
/// `DisconnectingPlayersAfterMatch` on a simulated clock.
#[test]
fn soak_240_full_matches_terminate_cleanly() {
    let fx = load_fixtures();
    let bound = match_bound(MAX_TICK);
    let mut rng = Rng(0xC0FFEE_5EED);
    let modes = [
        (Mode::BotVsBot, 120),
        (Mode::ScriptedHumanVsBot, 50),
        (Mode::ScriptedPvp, 30),
        (Mode::DepartureMidMatch, 16),
        (Mode::ConcedeMidMatch, 16),
        (Mode::SilentHumanVsBot, 4),
        (Mode::SilentPvp, 4),
    ];
    // `SOAK_SCALE=N` multiplies every count, for a longer explicit run.
    let scale: usize = std::env::var("SOAK_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut failures = Vec::new();
    let mut n = 0usize;
    let mut slowest = (Duration::ZERO, String::new());
    let mut per_mode: BTreeMap<String, (usize, Duration, Duration)> = BTreeMap::new();
    let (mut rounds_hist, mut casts, mut dmg, mut frames) = ([0usize; 4], [0u64; 2], 0usize, 0usize);
    let mut timeouts = 0usize;
    let mut double_ko_matches: Vec<String> = Vec::new();
    // Every edge fixture fights at least twice in bot-vs-bot, whatever the RNG says.
    let edge_first: Vec<usize> = (30..fx.len()).flat_map(|i| [i, i]).collect();
    for (mode, count) in modes {
        for k in 0..count * scale {
            let seed = rng.next();
            let ia = if mode == Mode::BotVsBot && k < edge_first.len() {
                edge_first[k]
            } else {
                rng.below(fx.len() as u64) as usize
            };
            let mut ib = rng.below(fx.len() as u64) as usize;
            if ib == ia {
                ib = (ib + 1) % fx.len();
            }
            let label = format!("{mode:?} seed={seed:#018x} {} vs {}", fx[ia].name, fx[ib].name);
            n += 1;
            match run_guarded(seed, &fx[ia], &fx[ib], mode, bound) {
                Ok(o) => {
                    if o.sim > slowest.0 {
                        slowest = (o.sim, label.clone());
                    }
                    let e = per_mode.entry(format!("{mode:?}")).or_insert((0, Duration::MAX, Duration::ZERO));
                    e.0 += 1;
                    e.1 = e.1.min(o.sim);
                    e.2 = e.2.max(o.sim);
                    rounds_hist[o.rounds.min(3)] += 1;
                    casts[0] += o.casts[0] as u64;
                    casts[1] += o.casts[1] as u64;
                    dmg += o.damage_frames;
                    frames += o.frames;
                    timeouts += o.ended_by_timeout;
                    if o.double_kos > 0 {
                        double_ko_matches.push(format!("{label} ({} double KO, {} rounds)", o.double_kos, o.rounds));
                    }
                }
                Err(e) => failures.push(format!("{label}: {e}")),
            }
        }
    }
    eprintln!(
        "CRE-SOAK: {n} matches, {} failed; bound {bound:?}; slowest {:?} ({}); rounds 1/2/3 = {}/{}/{}; \
         casts slot0/slot1 = {}/{}; damage frames {dmg}; s2c frames {frames}; timeouts {timeouts}",
        failures.len(),
        slowest.0,
        slowest.1,
        rounds_hist[1],
        rounds_hist[2],
        rounds_hist[3],
        casts[0],
        casts[1],
    );
    eprintln!("CRE-SOAK   double-KO replays (authored rule, extra round): {}", double_ko_matches.len());
    for d in double_ko_matches.iter().take(20) {
        eprintln!("CRE-SOAK     {d}");
    }
    for (mode, (c, lo, hi)) in &per_mode {
        eprintln!("CRE-SOAK   {mode}: {c} ok, sim {lo:?} .. {hi:?}");
    }
    assert!(failures.is_empty(), "{} of {n} matches failed:\n{}", failures.len(), failures.join("\n"));
    assert!(n >= 200 * scale);
    assert!(casts[0] > 0 && casts[1] > 0, "both sides must cast abilities: {casts:?}");
}

/// Nobody acts: every round must be closed by the authoritative 120 s ROUND_TIMEOUT
/// and the match must still walk to Disconnecting.
#[test]
fn soak_a_silent_pvp_match_ends_on_round_timeouts() {
    let fx = load_fixtures();
    let bound = match_bound(MAX_TICK);
    let o = run_guarded(7, &fx[3], &fx[20], Mode::SilentPvp, bound).expect("silent PvP terminates");
    assert_eq!(o.rounds, 2, "two timed-out rounds decide it: {o:?}");
    assert!(o.sim >= ROUND_TIMEOUT * 2, "rounds ended before their timeout: {:?}", o.sim);
    assert_eq!(o.damage_frames, 0, "nobody acted, nothing was damaged");
}

/// One side (the human) never acts; the bot must finish it.
#[test]
fn soak_a_passive_human_against_a_bot_still_terminates() {
    let fx = load_fixtures();
    let bound = match_bound(MAX_TICK);
    for (i, j) in [(0, 29), (29, 0), (31, 33)] {
        let o = run_guarded(11 + i as u64, &fx[i], &fx[j], Mode::SilentHumanVsBot, bound)
            .unwrap_or_else(|e| panic!("{} vs {}: {e}", fx[i].name, fx[j].name));
        assert_eq!(o.winner, Some(1), "{} (idle) vs {}: the bot wins", fx[i].name, fx[j].name);
        assert!(o.casts[1] > 0 || fx[j].loadout.abilities.is_empty(), "the bot used its abilities");
    }
}

/// A departure and an explicit concede, mid-round, still reach Disconnecting — for
/// every one of a spread of quit instants.
#[test]
fn soak_departure_and_concede_mid_round_terminate() {
    let fx = load_fixtures();
    let bound = match_bound(MAX_TICK);
    for seed in 0..12u64 {
        for mode in [Mode::DepartureMidMatch, Mode::ConcedeMidMatch] {
            let (a, b) = ((seed as usize * 7) % fx.len(), (seed as usize * 11 + 3) % fx.len());
            let o = run_guarded(1000 + seed, &fx[a], &fx[b], mode, bound)
                .unwrap_or_else(|e| panic!("{mode:?} seed {seed}: {e}"));
            assert!(o.rounds <= 3);
        }
    }
}

/// The derived bound, printed so the report can quote it, and pinned to the
/// engine timers it was derived from.
#[test]
fn soak_bound_is_derived_from_the_engine_timers() {
    let b = match_bound(MAX_TICK);
    // 4 + 22 + 3*120 + 2*36 + 16 = 474 s of timers, plus per-transition tick slack.
    assert_eq!(
        SPAWN_HANDSHAKE_HOLD + walk(MATCH_STATE_ROUND0_PROGRESSION),
        Duration::from_secs(26)
    );
    assert_eq!(walk(MATCH_STATE_INTERROUND_PROGRESSION), Duration::from_secs(36));
    assert_eq!(walk(MATCH_STATE_MATCHEND_PROGRESSION), Duration::from_secs(16));
    assert!(b >= Duration::from_secs(474) && b < Duration::from_secs(480), "bound {b:?}");
    eprintln!("CRE-SOAK bound: {b:?}");
}

/// Regression: this BotVsBot pairing used to double-KO every round after the first
/// (the killing swing, then the victim's Frost Revenge on 2 HP) and the uncapped
/// replay rule looped it past the termination bound. Corrected damage/DoT cadence
/// leaves it with no replayed double-KO rounds, but it still pins termination and a
/// two-round winner for the old loop seed.
#[test]
fn the_double_ko_loop_seed_terminates() {
    let fx = load_fixtures();
    let find = |n: &str| fx.iter().find(|f| f.name == n).unwrap_or_else(|| panic!("no fixture {n}"));
    let o = run_guarded(
        0x4d4e3b4e79f4ca03,
        find("prod-32"),
        find("edge-healing-surge-zero-stamina"),
        Mode::BotVsBot,
        match_bound(MAX_TICK),
    )
    .expect("the match terminates within the bound");
    assert_eq!(o.double_kos, 0, "corrected damage avoids the old replayed double KO");
    assert!(o.rounds <= super::super::state::MATCH_ROUND_HARD_CAP, "{} rounds", o.rounds);
    let w = o.winner.expect("a match winner");
    assert_eq!(o.rounds_won[w], 2, "{:?}", o.rounds_won);
}

/// Replay ONE soak match, for diagnosis: `SOAK_ONE=<seed>,<Mode>,<fixture a>,<fixture b>`
/// (the four fields a soak failure line prints), plus `SOAK_TRACE=<seed>` for engine
/// logs. A no-op when `SOAK_ONE` is unset.
#[test]
fn soak_replay_one() {
    let Ok(spec) = std::env::var("SOAK_ONE") else { return };
    let parts: Vec<&str> = spec.split(',').collect();
    let seed = u64::from_str_radix(parts[0].trim_start_matches("0x"), 16).expect("hex seed");
    let mode = match parts[1] {
        "BotVsBot" => Mode::BotVsBot,
        "ScriptedHumanVsBot" => Mode::ScriptedHumanVsBot,
        "ScriptedPvp" => Mode::ScriptedPvp,
        "SilentPvp" => Mode::SilentPvp,
        "SilentHumanVsBot" => Mode::SilentHumanVsBot,
        "DepartureMidMatch" => Mode::DepartureMidMatch,
        "ConcedeMidMatch" => Mode::ConcedeMidMatch,
        m => panic!("unknown mode {m}"),
    };
    let fx = load_fixtures();
    let find = |n: &str| fx.iter().find(|f| f.name == n).unwrap_or_else(|| panic!("no fixture {n}"));
    let r = run_guarded(seed, find(parts[2]), find(parts[3]), mode, match_bound(MAX_TICK));
    eprintln!("SOAK_ONE {spec}: {r:?}");
    r.unwrap();
}
