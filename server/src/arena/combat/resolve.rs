//! Combat resolution: turn inbound c2s inputs (swipe / ability / block) and tick
//! events into authoritative s2c messages.
//!
//! A carrier-54 c2s input is either a **weapon swing** (auto-attack, throttled) or
//! a **`RequestExecuteAbility`** (spell/ability cast, on cooldown). Both resolve via
//! the RE-derived [`RetailDamageModel`] from the attacker's loadout → `ReceiveDamage`
//! to both players (+ a `PerformExecuteAbility` echo for casts); a fighter reaching
//! 0 HP ends the match (`PlayerDeadStateChange` + the op48/op49 result burst).
//!
//! Combat fidelity now wired (`docs/arena-{combat-reproduction,status-resistance}-spec.md`):
//! a per-fighter **COMBO ramp** (auto-swings alternate Left/Right → `combo_factor`), the
//! **block** (a flat per-category budget from the blocking item's rating, `damage::BlockOutcome`),
//! **resistance** (flat per-type, elem-piercing) + `most_resisted`, **negation pools**
//! (Ward/Absorb → op66 `DamageNegated` + Absorb heal), and **status conditioning** (a
//! sliding `damage_history` window → op51 `ChangeCombatStatusEffect`, incl. poison→
//! `Paralyzed` with the victim's inputs locked).
//!
//! **Phase 4.1 (done):** the swing side is now CLASSIFIED FROM REAL CLIENT INPUT.
//! There is no `activeSide` enum on the c2s wire — the client streams raw pointer
//! geometry (`PlayerCombatInputPosition`, gmid 47, ~30 Hz) and commits with
//! `PlayerCombatInputActivate` (gmid 46). The server classifies Left/Right from the
//! normalised screen X of the freshest pointer sample; see the "swing-side
//! classification" block below for the prod ground-truth calibration. The synthetic
//! Left/Right alternation survives only as a fallback for bots / silent clients.
//!
//! The per-element status ticks and real ability UUID routing that older plans list as
//! TODOs are now wired. The remaining input-side calibration is the exact
//! `swingFactor` magnitude carried by the c2s body.

use std::time::{Duration, Instant};

use log::{debug, info};

use super::damage::{DamageModel, ResolvedDamage, RetailDamageModel};
use super::input;
use super::messages;
use super::messages_state::{self, StateFrame};
use super::state::{
    ActiveSide, ActorStateType, DamageSource, FlowState, MatchCombat, MatchState, NetObjectType,
    ManualAttackGesture, PendingHit,
};
use super::tables;

/// Carrier MessageType (`user_data[1]`) of the combat-input family — `0x36` (54).
const CARRIER_USERMESSAGE: u8 = 0x36;
/// Carrier for `PlayerCombatInputActivate` (op46) — `0x2e` (46). Op46 uses its own
/// carrier byte (`GameMessageId` value), NOT the generic `0x36` UserMessage carrier.
const CARRIER_OP46: u8 = 0x2e;

/// The minimum spacing between committed swings for `fighter` — **Phase 3.12**: the
/// equipped weapon's own `attackDelay + recoveryToComboTime`, floored at
/// `PlayerCombatParameters.globalMinimumAttackDelay` (0.1 s).
///
/// This replaces the guessed per-weight-class table (`Weight::swing_interval`, a flat
/// 400/650/900 ms). A Dragonbone Dagger commits every 0.333 s: 0.233 s attack delay
/// plus its 0.100 s combo-recovery gate. The old use of full `recoveryTime` imposed
/// 0.783 s and rejected retail-paced releases observed at 0.372–0.668 s.
fn combat_speed_multiplier(fighter: &super::state::Fighter, now: Instant) -> f32 {
    if fighter.is_frozen(now) {
        super::gamedata::combat_params::SLOW_STATUS_MULTIPLIER
    } else {
        1.0
    }
}

fn swing_cooldown_for(fighter: &super::state::Fighter, now: Instant) -> Duration {
    Duration::from_secs_f32(
        fighter.loadout.swing_interval().as_secs_f32() / combat_speed_multiplier(fighter, now),
    )
}

/// Server-measured hold duration threshold for a FULL charge (Critical state).
///
/// **APPROXIMATE — VIDEO-CALIBRATED** (≈1.2 s): from s293 video ground-truth
/// (`/tmp/arena-video-groundtruth.md` §3) the charge circle fills in ~1–1.5 s
/// (e.g. t=46→47 partial→full, t=54→55 partial→full). The exact game-data value
/// is `WeaponTemplate.MinDamageTime` (the CDN-hosted `PlayerCombatAbilitySettings`
/// ScriptableObjects, not yet captured). Refine when CDN WeaponTemplate data is
/// available; the threshold is also the `AttackChargeState.PreCritical → Critical`
/// state transition in `dump.cs TypeDefIndex 13116`.
///
/// **CALIBRATION FLAG RESOLVED.** The flag asked for `MinDamageTime`/`MaxDamageTime`
/// from the CDN WeaponTemplate assets. They are in hand, and 1.2 s was far too long:
/// across 246 real swings the hold-at-release distribution is median **0.317 s**,
/// p90 0.47-0.60 s, **maximum 1.73 s**. At a 1.2 s threshold a critical hit was very
/// nearly unreachable, which is why nobody has ever reported landing one.
///
/// The real full-charge point is `WeaponTemplate._backswingTime`, the input to
/// `AttackChargeState::DetermineState(chargeTime)` (`dump.cs` TypeDefIndex 13115):
/// damage ramps from `minDamageFactor` (0) to `maxDamageFactor` over the backswing,
/// holds for `_maxDamageTime` (0.035 s), then decays. 363 of 368 shipped weapon
/// entries collapse onto three signatures — Light 0.1167 s, Versatile 0.2 s, Heavy
/// 0.25 s. It also explains the 0.32 s clustering: players hold just past the sweet
/// spot, exactly as the ramp rewards.
///
/// The extractor now retains the fields, so both the threshold and the multiplier
/// are read from the exact equipped template. The weight values survive only inside
/// `Loadout` as a fallback for old/unresolved items.
fn critical_hold_secs(fighter: &super::state::Fighter) -> f32 {
    fighter.loadout.critical_hold_secs()
}

fn critical_hold_secs_for(fighter: &super::state::Fighter, now: Instant) -> f32 {
    critical_hold_secs(fighter) / combat_speed_multiplier(fighter, now)
}

/// Fallback ability cooldown for abilities without authoritative game-data.
const ABILITY_COOLDOWN: Duration = Duration::from_millis(3000);

/// Per-ability, **per-rank** cooldown from the shipped `<Name>Rank<N>` asset
/// (`_cooldown`). Unknown UUIDs fall back to [`ABILITY_COOLDOWN`].
///
/// **Phase 3.11:** replaces the hand-transcribed rank-independent table. Several of
/// that table's UUIDs never matched a shipped ability (fabricated tails), so those
/// abilities silently used the 3 s fallback.
fn ability_cooldown(ability_uuid: &str, rank: u8) -> Duration {
    match tables::ability_cooldown_secs(ability_uuid, rank) {
        Some(s) if s > 0.0 => Duration::from_secs_f32(s),
        _ => ABILITY_COOLDOWN,
    }
}

/// How long a `PlayerBlockingStateChange` (41) holds the guard up before it
/// auto-expires (a fresh op41 refreshes it). The dump's `PvpDefaultSettings`
/// `BLOCK_OPTIMAL_TIME` is 2.0s (docs/blades-combat-formulae.md §2); we use it as the
/// block window since the on/off flag isn't byte-pinned from a two-sided capture.
const BLOCK_WINDOW: Duration = Duration::from_secs(2);

/// Safety cap on a guard held with no matching release — a LEAK GUARD, not a game rule.
///
/// Retail has **no** auto-expiry: a block ends when the player lets go, or when an
/// attack press cancels it. Measured from `propId8` of the closing state change over
/// 539 decoded blocks, the durations are a continuous distribution with no cliff —
/// median 0.750 s, p90 1.800 s, p99 2.717 s, max **4.767 s**, and 42 of 539 ran past
/// two seconds. The old two-second `BLOCK_WINDOW` would have silently truncated ~8 % of
/// real guards, so it is not used as the block's lifetime any more. This cap exists
/// only so a dropped release packet cannot leave a fighter guarding forever.
const BLOCK_LEAK_GUARD: Duration = Duration::from_secs(8);

/// True iff `user_data` is a `PlayerCombatInputActivate` (op46) frame.
/// These have carrier `0x2e` (46) — NOT the generic `0x36` UserMessage carrier.
fn is_op46(user_data: &[u8]) -> bool {
    user_data.get(1) == Some(&CARRIER_OP46)
}

/// `GameMessageId` values whose body carries the client's swipe geometry.
const GMID_PLAYER_COMBAT_INPUT_ACTIVATE: u8 = 46;
const GMID_PLAYER_COMBAT_INPUT_POSITION: u8 = 47;

// ---------------------------------------------------------------------------
// Phase 4.1 — swing-side classification from the client's raw input geometry
// ---------------------------------------------------------------------------
//
// ## What the client actually sends (decoded from prod `arena_udp_frames`)
//
// There is **no `activeSide` enum on the c2s wire**. `CombatSwipeInfo` (gmid 54)
// does not exist in the corpus (10 frames total, none of them a swipe input). The
// client instead streams raw pointer geometry and the server classifies it:
//
// * **gmid 47 `PlayerCombatInputPosition`** — a ~30 Hz pointer stream, 68 913 c2s
//   frames on prod. NetData `{0:Int netObjectId · 1:Byte 56 Avatar · 2:Byte 3
//   Autonomous · 3:Byte 47 · 4:Float x · 5:Float y · 6:Float frameDelta ·
//   7:Float chargeSeconds · 8:Int seq}`.
// * **gmid 46 `PlayerCombatInputActivate`** — the discrete press/release, 17 817
//   c2s frames. NetData `{… 3:Byte 46 · 4:Bool held · 5:Float chargeSeconds ·
//   6:Bool isWithinBlockZone}`.
//
// **Both ride the generic `0x36` UserMessage carrier**, not their own GameMessageId
// byte: of 1 500 sampled prod gmid-46 frames, 1 497 are `(marker 0xBE, carrier
// 0x36)` and only 3 are `(0x84, 0x2e)`; gmid-47 is 1 500/1 500 on `0x36`. The
// pre-existing `CARRIER_OP46` (`0x2e`) path is therefore near-dead on real traffic
// and is kept only as a compatibility branch (see [`is_op46`]).
//
// ## Which feature classifies the side — measured, not assumed
//
// `ReceiveDamage` (gmid 50) propId 10 **is** the retail server's own `ActiveSide`
// decision (same field this server writes in `messages::receive_damage`), so prod
// captures provide ground-truth labels. Joining 3 277 attack hits against the c2s
// pointer stream that preceded them:
//
// | feature                              | accuracy @ 0.5 |
// |--------------------------------------|----------------|
// | absolute X at release (`p4`, last)   | **93.7 %**     |
// | absolute X at press (`p4`, first)    | 92.7 %         |
// | travel delta ΔX across the gesture   | ~chance        |
//
// **Absolute position wins; travel-delta carries no signal at all.** Despite the
// name "swipe", the gesture is a *hold at a point*, not a directional sweep: within
// a press→release burst X moves by ~0.0005/frame (finger jitter) and the sign split
// is symmetric for both classes (Left: 686 positive / 421 negative; Right: 828 /
// 778). Classifying on ΔX would be a coin flip.
//
// The two classes are cleanly bimodal on X with a wide empty valley — Left q1/med/q3
// = 0.160 / 0.213 / 0.232, Right = 0.785 / 0.814 / 0.838, and only 167 of 4 539
// samples (3.7 %) land anywhere in [0.30, 0.70]. Y (`p5`) is *not* discriminative
// (Left median 0.529 vs Right 0.497).
//
// Two further structural facts from the same ground truth:
// * `DamageSource::Attack` is the **only** source that ever produces Left/Right, and
//   it *always* does (2 706 Left / 3 889 Right; never None, never Middle). Every
//   other source (Spell, WeaponManeuver, StatusEffect, …) is None or Middle.
// * At high combo the recorded side strictly alternates (combo 9→Left, 10→Right,
//   11→Left, 12→Right, …), independently confirming that the combo ramp advances
//   only on alternating sides — which is why a synthetic alternator let every player
//   max the ramp for free.

/// **[Class 3 calibration]** Normalised-screen-X cut-point separating a Left swing
/// from a Right swing.
///
/// Retail is gone; this cannot be validated against the shipped client's own
/// constant. It is calibrated from 3 277 prod attack hits labelled by the retail
/// server's own `ReceiveDamage.activeSide` (see the module note above). A sweep over
/// candidate thresholds peaks **exactly at 0.50** (93.7 %) on a very flat plateau —
/// 0.30 → 0.924, 0.45 → 0.932, **0.50 → 0.937**, 0.60 → 0.934, 0.70 → 0.934 — so the
/// screen midpoint is both the empirical optimum and the obvious design value. The
/// residual ~6 % is dominated by capture-side pairing slop (dropped 30 Hz samples,
/// attributing a hit to the wrong burst), not by the feature.
const SIDE_CLASSIFY_X_MIDPOINT: f32 = 0.5;

/// **[Class 3 calibration]** How long a pointer sample stays usable for classifying
/// a swing.
///
/// The client streams gmid 47 **only while a finger is down** — in prod traces the
/// stream stops at release and resumes at the next press (gaps of seconds between
/// gestures). The last sample before a release is typically ~33 ms old (one 30 Hz
/// frame), so this window only has to survive a short burst of packet loss. 500 ms
/// ≈ 15 dropped frames, while still being far shorter than the inter-gesture gap, so
/// a stale position from a *previous* gesture can never be reused.
const SIDE_CLASSIFY_SAMPLE_TTL: Duration = Duration::from_millis(500);

/// **[Class 3 calibration]** Tolerance for the server-vs-client charge cross-check.
/// A divergence beyond this is logged (possible cheat / clock skew / packet loss);
/// it never changes the damage, which always uses the server measurement.
const CHARGE_CROSS_CHECK_TOLERANCE_SECS: f32 = 0.35;

// NetData propIds — gmid 47 `PlayerCombatInputPosition`.
const PROP_POS_X: u8 = 4;
const PROP_POS_Y: u8 = 5;
const PROP_POS_CHARGE: u8 = 7;
const PROP_POS_FLAGS: u8 = 8;
/// `PlayerCombatInputPositionMessage.START_TRIGGER_FLAG` (dump.cs:589451).
/// The client sets this after its measured screen-swipe speed and attack angle cross
/// the shipped `PlayerCombatParameters` gates (5.0 and 75 degrees on device).
const POS_START_ATTACK_TRIGGER_FLAG: i32 = 512;
// NetData propIds — gmid 46 `PlayerCombatInputActivate`.
const PROP_ACT_HELD: u8 = 4;
const PROP_ACT_CHARGE: u8 = 5;
const PROP_ACT_BLOCK_ZONE: u8 = 6;

/// Read a `Float` NetData prop. Deliberately strict: unlike `NetDataParse::int`
/// (which coerces `Bool` → 0/1 and would happily read a *bool* prop as a number),
/// this matches the `Float` variant only.
fn netdata_f32(nd: &arena_proto::NetDataParse, prop: u8) -> Option<f32> {
    match nd.get(prop) {
        Some(arena_proto::NetDataValue::Float(v)) => Some(*v),
        _ => None,
    }
}

/// Read a `Bool` NetData prop (strict — see [`netdata_f32`]).
fn netdata_bool(nd: &arena_proto::NetDataParse, prop: u8) -> Option<bool> {
    match nd.get(prop) {
        Some(arena_proto::NetDataValue::Bool(v)) => Some(*v),
        _ => None,
    }
}

/// Read an exact `Int` NetData prop. Prop 8 of gmid 47 is a packed ushort-shaped
/// integer; accepting a Bool/Byte coercion here would manufacture trigger flags.
fn netdata_i32(nd: &arena_proto::NetDataParse, prop: u8) -> Option<i32> {
    match nd.get(prop) {
        Some(arena_proto::NetDataValue::Int(v)) => Some(*v),
        _ => None,
    }
}

/// One decoded `PlayerCombatInputPosition` (gmid 47) pointer sample.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PointerSample {
    /// Normalised screen X (propId 4).
    x: f32,
    /// Normalised screen Y (propId 5).
    y: f32,
    /// Client-reported charge seconds latched at propId 7 (telemetry only).
    client_charge: Option<f32>,
    /// The client-side input path crossed the authored swipe-speed and angle gates.
    /// This selects only the visual state (manual gmid 40 vs fallback gmid 52); it
    /// never changes damage, charge, target, or hit acceptance.
    start_attack_trigger_ready: bool,
}

/// One decoded `PlayerCombatInputActivate` (gmid 46) press/release event.
#[derive(Debug, Clone, Copy, PartialEq)]
struct InputActivate {
    /// `true` = button DOWN (press), `false` = button UP (release/commit).
    held: bool,
    /// Client-reported hold duration in seconds (telemetry only — see
    /// [`state::Fighter::last_client_charge`]).
    client_charge: Option<f32>,
    /// The client's `_isWithinBlockZone` flag.
    block_zone: Option<bool>,
}

/// Decode a carrier-`0x36` `PlayerCombatInputPosition` (gmid 47) frame.
/// Returns `None` for any other frame.
fn parse_input_position(user_data: &[u8]) -> Option<PointerSample> {
    if messages::user_message_gmid(user_data)? != GMID_PLAYER_COMBAT_INPUT_POSITION {
        return None;
    }
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    Some(PointerSample {
        x: netdata_f32(&nd, PROP_POS_X)?,
        y: netdata_f32(&nd, PROP_POS_Y)?,
        client_charge: netdata_f32(&nd, PROP_POS_CHARGE),
        start_attack_trigger_ready: netdata_i32(&nd, PROP_POS_FLAGS)
            .is_some_and(|flags| flags & POS_START_ATTACK_TRIGGER_FLAG != 0),
    })
}

/// Derive the two vectors retail puts on gmid 40 from the segment that tripped the
/// client's swipe gate. `PlayerAttackState` expects the direction from CURRENT back
/// to PREVIOUS (capture-pinned in s616), and the previous point as the execution
/// point. The client flag remains presentational input: malformed geometry simply
/// falls back to gmid 52 and cannot affect damage.
fn manual_attack_gesture(
    previous: (f32, f32),
    current: (f32, f32),
) -> Option<ManualAttackGesture> {
    if ![previous.0, previous.1, current.0, current.1]
        .into_iter()
        .all(|v| v.is_finite() && (0.0..=1.0).contains(&v))
    {
        return None;
    }
    let dx = previous.0 - current.0;
    let dy = previous.1 - current.1;
    let length = dx.hypot(dy);
    if length <= f32::EPSILON {
        return None;
    }
    Some(ManualAttackGesture {
        direction: (dx / length, dy / length),
        execution_point: previous,
    })
}

/// Decode a carrier-`0x36` `PlayerCombatInputActivate` (gmid 46) frame.
/// Returns `None` for any other frame.
fn parse_input_activate(user_data: &[u8]) -> Option<InputActivate> {
    if messages::user_message_gmid(user_data)? != GMID_PLAYER_COMBAT_INPUT_ACTIVATE {
        return None;
    }
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    Some(InputActivate {
        held: netdata_bool(&nd, PROP_ACT_HELD)?,
        client_charge: netdata_f32(&nd, PROP_ACT_CHARGE),
        block_zone: netdata_bool(&nd, PROP_ACT_BLOCK_ZONE),
    })
}

/// Classify a swing side from a normalised-screen-X pointer position.
///
/// Left half of the screen → [`ActiveSide::Left`], right half → [`ActiveSide::Right`],
/// split at [`SIDE_CLASSIFY_X_MIDPOINT`]. Out-of-range values (a malformed or hostile
/// frame) yield `None` so the caller can fall back rather than trust them.
///
/// Never returns `Middle`: prod ground truth shows a weapon `Attack` is *always* Left
/// or Right (0 of 6 595 recorded attack hits carried Middle or None). `Middle` remains
/// reachable only through the maneuver/ability lane.
fn classify_side_from_x(x: f32) -> Option<ActiveSide> {
    if !x.is_finite() || !(0.0..=1.0).contains(&x) {
        return None;
    }
    Some(if x >= SIDE_CLASSIFY_X_MIDPOINT { ActiveSide::Right } else { ActiveSide::Left })
}

/// The side to use for `sender`'s next swing, from the freshest pointer sample the
/// client streamed. `None` when there is no usable sample — the caller then uses the
/// clearly-marked synthetic fallback in [`resolve_swing_with_side`].
fn classified_side_for(
    fighter: &super::state::Fighter,
    now: Instant,
) -> Option<ActiveSide> {
    let at = fighter.last_input_at?;
    // `Instant` arithmetic: guard against a sample stamped in the future (clock jitter
    // in tests) by using `checked_duration_since`.
    let age = now.checked_duration_since(at)?;
    if age > SIDE_CLASSIFY_SAMPLE_TTL {
        return None;
    }
    classify_side_from_x(fighter.last_input_x?)
}

/// Compare the server-measured hold against the client-reported one and log a
/// divergence. **Purely observational** — the returned value is never used for damage.
///
/// The client's number is a faithful wall-clock timer (in prod traces gmid 47 propId 7
/// ramps by exactly 1/30 s per frame and matches the gmid 46 release value to the last
/// decimal), but it is *client-authored*: trusting it would let a modified client claim
/// a full charge on every tap and crit for free. So the server keeps its own stopwatch
/// and only uses the client value to notice when the two disagree.
fn charge_cross_check(slot: usize, server_secs: f32, client_secs: Option<f32>) {
    let Some(client) = client_secs else { return };
    let delta = (server_secs - client).abs();
    if delta > CHARGE_CROSS_CHECK_TOLERANCE_SECS {
        info!(
            "combat: slot {slot} charge cross-check DIVERGED — server {server_secs:.3}s vs \
             client-reported {client:.3}s (Δ{delta:.3}s > {CHARGE_CROSS_CHECK_TOLERANCE_SECS}s). \
             Server measurement is authoritative; client value is telemetry only."
        );
    } else {
        debug!(
            "combat: slot {slot} charge cross-check ok — server {server_secs:.3}s vs client \
             {client:.3}s (Δ{delta:.3}s)"
        );
    }
}

/// **Superseded — the old `decode_active_side` lived here.**
///
/// It scanned NetData props above the header for any value in `0..=3` and read it as
/// an `ActiveSide`. That was wrong twice over: (a) there is no `activeSide` field on
/// the c2s wire at all (see the Phase 4.1 module note above), and (b)
/// `NetDataParse::int` coerces `Bool` → 0/1, so on a real carrier-`0x36` gmid-46
/// frame it hit propId 4 (`held`, a Bool) first and returned `Middle` for every press
/// and `None` for every release — `Middle` resets the combo chain. Replaced by
/// [`classified_side_for`] / [`classify_side_from_x`], which classify from the
/// pointer geometry the client actually streams.

/// Parse the `_held` flag (bit0 of `b[9]`, the float's MSB) from an op46 body.
///
/// Op46 wire layout (per `arena-charge-decode.md` §2):
/// ```text
/// user_data[0]   = C2S marker (0x84)
/// user_data[1]   = 0x2e (carrier = GameMessageId 46)
/// user_data[2:6] = netObjectId u32 LE
/// user_data[6]   = _isWithinBlockZone byte (not decoded here)
/// user_data[7]   = 0xcc structural separator
/// user_data[8:12]= _held(bit0 of [11]) + _clientChargeTime f32 LE (remaining 31 bits)
/// ```
/// Returns `Some(true)` on button-DOWN (attack press), `Some(false)` on button-UP
/// (attack release/commit), `None` when the frame is too short or not op46.
fn parse_op46_held(user_data: &[u8]) -> Option<bool> {
    if !is_op46(user_data) {
        return None;
    }
    // Need at least 12 bytes: marker(1) + carrier(1) + netObjId(4) + blockZone(1) +
    // separator(1) + chargeTime+held(4) = 12.
    if user_data.len() < 12 {
        return None;
    }
    // b[9] in the decode-doc's 0-indexed body is user_data[11] (marker+carrier = 2-byte prefix).
    // bit0 of the MSB of the f32 LE [user_data[8:12]] = bit0 of user_data[11].
    let held_bit = user_data[11] & 0x01;
    Some(held_bit == 1)
}

/// Determine the swing crit factor for a fighter based on how long they held the
/// attack button (server-measured). Returns the charge multiplier:
///   - the equipped template's `maxDamageFactor` when `hold_secs` reaches that
///     template's `backswingTime` (full charge / Critical or PostCriticalDecay — the
///     server-side equivalent of op45 reporting ≥3).
///   - `1.0` for a partial hold (uncharged swing, no crit).
fn charge_crit_factor(fighter: &super::state::Fighter, hold_secs: f32) -> f32 {
    if hold_secs < critical_hold_secs(fighter) {
        return 1.0;
    }
    fighter.loadout.critical_damage_factor()
}

fn charge_crit_factor_at(
    fighter: &super::state::Fighter,
    hold_secs: f32,
    now: Instant,
) -> f32 {
    if hold_secs < critical_hold_secs_for(fighter, now) {
        return 1.0;
    }
    fighter.loadout.critical_damage_factor()
}

/// Resolve one inbound, decrypted c2s combat input from `sender`.
pub fn on_c2s_input(
    combat: &mut MatchCombat,
    sender: usize,
    user_data: &[u8],
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    // `EquipAbilitiesAndConsumables` (56) is a LOADOUT DECLARATION, not combat input,
    // and it is the only frame that names the consumable item the client has equipped.
    // Latch it in EVERY phase: retail uploads it during round-start setup (before the
    // live round opens) and re-uploads it after each use with a decremented charge
    // count. Without this the server cannot answer a later op63 (which carries no item
    // id). Handled ahead of the live-round gate for exactly that reason; it emits
    // nothing and touches no combat state. [Phase 4.3 wire trigger]
    if let Some(eq) = input::parse_equip_consumables(user_data) {
        if sender < combat.fighters.len() {
            debug!(
                "combat: slot {sender} equipped consumable {} ({} charge(s) per the client)",
                eq.consumable_uuid, eq.charges,
            );
            combat.fighters[sender].equipped_consumable = Some(eq.consumable_uuid);
        }
        return Vec::new();
    }
    // Combat resolves ONLY in the live round (StateTimeout). During Connecting /
    // Spawning / BackendMatchCreated the inbound op54s are round-start handshake
    // traffic (the client's PlayerLoadoutReady upload, op55, op58) — resolving them as
    // swings would inject phantom damage before the fight.
    if !matches!(combat.phase, FlowState::StateTimeout) {
        return Vec::new();
    }
    // Op46 `PlayerCombatInputActivate` uses carrier `0x2e` (its own GameMessageId byte),
    // NOT the generic `0x36` UserMessage carrier. Handle it FIRST so the 0x36 gate
    // below doesn't drop it.
    //
    // The op46 frame signals a HOLD (button-DOWN, `_held=1`) or a COMMIT (button-UP,
    // `_held=0`). On DOWN we record the server timestamp; on UP we compute the
    // server-measured hold duration and apply the held-charge crit multiplier (bug 4):
    //   - hold ≥ the weapon's backswingTime → full charge → its maxDamageFactor
    //   - shorter hold → partial / uncharged → swing_factor = 1.0
    //
    // [arena-charge-decode.md §2-§5; decode-proven: _held bit0 of user_data[11]]
    if is_op46(user_data) {
        if !matches!(combat.phase, FlowState::StateTimeout) {
            return Vec::new();
        }
        if sender >= combat.fighters.len() {
            return Vec::new();
        }
        match parse_op46_held(user_data) {
            Some(true) => {
                // Button-DOWN: record the press timestamp for hold-duration measurement.
                combat.fighters[sender].charge_press_at = Some(now);
                debug!("combat: slot {sender} op46 DOWN — charge press recorded at {now:?}");
                // Enter the wind-up. The gmid 45 broadcast to BOTH viewers comes from
                // the actor-state drain, exactly as it does on the live 0x36 path —
                // one code path for the charge, so the two carriers cannot drift.
                //
                // NOTE ON THIS WHOLE BRANCH: carrier 0x2e appears **zero** times in
                // prod sessions 503/615/616. Real clients send op46 as a 0x36
                // UserMessage with propId 3 = 46 (579 of them in s503 alone), which is
                // the `parse_input_activate` path below. This branch is kept because it
                // is cheap and harmless, not because anything reaches it — do not read
                // its existence as evidence that it runs.
                let side = classified_side_for(&combat.fighters[sender], now)
                    .unwrap_or(ActiveSide::Right);
                combat.fighters[sender].charge_side = Some(side);
                combat.fighters[sender].set_actor_state(ActorStateType::Charging, now);
                return Vec::new();
            }
            Some(false) => {
                // Button-UP (commit): compute hold duration, apply crit.
                let hold_secs = combat.fighters[sender]
                    .charge_press_at
                    .map(|t| now.duration_since(t).as_secs_f32())
                    .unwrap_or(0.0);
                // Reset press timestamp — this charge is consumed.
                combat.fighters[sender].charge_press_at = None;
                let swing_factor =
                    charge_crit_factor_at(&combat.fighters[sender], hold_secs, now);
                let is_crit = swing_factor > 1.0;
                if is_crit {
                    info!(
                        "combat: slot {sender} op46 UP — hold {hold_secs:.3}s ≥ the weapon threshold threshold \
                         → CRIT ×{swing_factor:.3} (weapon {:?})",
                        combat.fighters[sender].loadout.weapon.weight,
                    );
                } else {
                    debug!(
                        "combat: slot {sender} op46 UP — hold {hold_secs:.3}s < the weapon threshold \
                         → normal swing ×1.0",
                    );
                }
                // Now run the usual pre-swing checks (paralysis, opponent, cooldown).
                for f in combat.fighters.iter_mut() {
                    f.reconcile_block(now);
                    f.reconcile_scheduled_states(now);
                    reconcile_paralysis(f, now);
                    f.prune_negation_pools(now);
                }
                if combat.fighters[sender].is_paralyzed() {
                    info!(
                        "combat attack_input: gsid={} input={} slot={sender} actor={} outcome=paralyzed hold_ms={:.1}",
                        combat.game_session_id,
                        if sender >= combat.expected_peers { "bot" } else { "player" },
                        combat.fighters[sender].loadout.display_name,
                        hold_secs * 1000.0,
                    );
                    return Vec::new();
                }
                let Some(target_slot) = combat.opponent_of(sender) else {
                    debug!("combat: slot {sender} op46 UP ignored — solo/bot match");
                    return Vec::new();
                };
                if combat.fighters[target_slot].is_dead() {
                    return Vec::new();
                }
                // Phase 4.1: classify from pointer geometry here too, so the legacy
                // `0x2e` shape behaves identically to the real `0x36` one. With no
                // geometry (the usual case on this path) this is `None` and the
                // synthetic fallback applies.
                let side = classified_side_for(&combat.fighters[sender], now);
                return resolve_swing_with_side(
                    combat,
                    sender,
                    target_slot,
                    swing_factor,
                    side,
                    now,
                );
            }
            None => {
                // Frame too short or not op46 — ignore.
                debug!("combat: slot {sender} op46 parse failed (frame too short?)");
                return Vec::new();
            }
        }
    }

    if user_data.get(1) != Some(&CARRIER_USERMESSAGE) {
        return Vec::new();
    }
    // Reconcile any lapsed block windows first (so a stale guard never keeps reducing
    // damage), expire lapsed paralysis / negation pools, using `now`. Cheap; both fighters.
    for f in combat.fighters.iter_mut() {
        f.reconcile_block(now);
        f.reconcile_scheduled_states(now);
        reconcile_paralysis(f, now);
        f.prune_negation_pools(now);
    }
    // op41 PlayerBlockingStateChange (c2s) — the client raised/refreshed its guard.
    // Apply a BLOCK state on the sender: incoming hits within the block window are
    // reduced/negated per `damage::block_outcome` (optimal on the matching side,
    // late/half otherwise). This is the block-as-input wiring (was a resolve.rs TODO).
    // Bounded by `BLOCK_WINDOW` (the dump's `BLOCK_OPTIMAL_TIME` 2.0s) and auto-expired
    // by `reconcile_block` — a fresh op41 simply refreshes the window. No damage.
    //
    // DOES emit s2c — but no longer from here. ENTERING the `Blocking` actor state is
    // what raises the shield, and [`drain_state_changes`] turns that transition into
    // the gmid 41 broadcast for both viewers. That is the point of routing every state
    // change through one seam: the notification is not this handler's job, so a block
    // raised by any OTHER path animates too, for free.
    //
    // ⚠️ THIS HANDLER IS UNREACHABLE IN PRODUCTION — a separate, still-open bug.
    // Retail's corpus holds **784 s2c gmid 41 frames and ZERO c2s**, verified across
    // sessions 503/506/486/615/616; the same holds for the whole family (39/42/43/44/
    // 52/59/75 — zero c2s, every gmid, every session). The client never sends gmid 41.
    // So nothing in production ever puts a fighter into `Blocking`, which means the
    // shield cannot rise AND `damage::block_outcome` never sees a blocking defender.
    // Finding the real c2s block signal is tracked separately; the leading candidate is
    // gmid 46 `PlayerCombatInputActivate`'s `_isWithinBlockZone` (17,817 c2s frames),
    // whose semantics are not yet pinned.
    //
    // The handler is kept because it is correct if a client ever does send op41, and it
    // is what the block tests drive. Do NOT read its existence as "blocking works".
    if messages::is_player_blocking_state_change(user_data) {
        if sender < combat.fighters.len() {
            let side = messages::blocking_active_side(user_data).unwrap_or(ActiveSide::Middle);
            // Reckless Fury cannot block — the trade its description makes for the
            // damage and the immunity. Drop the request rather than raising a guard.
            if combat.fighters[sender].has_reckless_fury(now) {
                debug!("combat: slot {sender} cannot block during Reckless Fury");
                return Vec::new();
            }
            let f = &mut combat.fighters[sender];
            // Record block-raise instant for OPTIMAL→LATE timeout logic.
            // If the fighter re-raises within the recovery window (`last_block_dropped_at`
            // + OPTIMAL_BLOCK_RECOVERY_SECS), the new block starts as LATE (not OPTIMAL).
            // `block_phase()` in damage::block_outcome handles this via `block_raised_at` +
            // `last_block_dropped_at`. [PvpDefaultSettings dump.cs 427014-427015]
            f.set_actor_state(super::state::ActorStateType::Blocking, now);
            f.blocking_side = side;
            f.blocking_until = Some(now + BLOCK_WINDOW);
            f.block_raised_at = Some(now);
            debug!("combat: slot {sender} raised guard ({side:?}) for {BLOCK_WINDOW:?}");
        }
        return Vec::new();
    }
    // Carrier 0x36 is shared by combat inputs AND round-transition handshake/flow
    // signals (op61 LoadoutClientBackendSynchronized, op36 PlayerLoadoutReady, op80
    // MatchStateChangeAck, op56 EquipAbilities, op20/22/57 …). Those arrive even in
    // the LIVE round (e.g. at a RoundEnd→NextState transition: s506 #3523229 op61,
    // #3523274 op36) — resolving them as a swing injects phantom damage. Only real
    // combat inputs (op37 ability, op46/47 swipe-input) and unstructured swipe bodies
    // fall through to resolution. [docs/arena-journey-log.md §7]
    // `RequestConsumeConsumable` (63) — the client drank its potion. This is the wire
    // TRIGGER for the (previously dormant) `use_consumable` budget: spend a charge and
    // echo `PerformConsumeConsumable` (64) to BOTH players so each renders the drink.
    // It MUST be handled before the swing fallback below: op63 is not in the
    // `is_noncombat_user_message` set and carries no gmid-46/47 structure, so it would
    // otherwise fall through to the "unstructured carrier-0x36 body" branch and be
    // resolved as a phantom weapon swing.
    if input::is_request_consume_consumable(user_data) {
        return on_consume_consumable(combat, sender, now);
    }
    if messages::is_noncombat_user_message(user_data) {
        debug!("combat: slot {sender} carrier-54 handshake/flow frame (not a swing) — ignored");
        return Vec::new();
    }

    // ---- Phase 4.1: the REAL combat-input family, on the generic 0x36 carrier ----
    //
    // `PlayerCombatInputPosition` (gmid 47) is a ~30 Hz POINTER STREAM, not an attack.
    // It must update the sender's geometry and emit nothing. (Before Phase 4.1 these
    // frames fell through to the swing path, so the server launched a swing on every
    // pointer sample — rate-limited only by the weapon cadence. Combined with the
    // synthetic side alternation that meant a player maxed the combo ramp merely by
    // holding a finger on the screen.)
    if sender < combat.fighters.len() {
        if let Some(sample) = parse_input_position(user_data) {
            let f = &mut combat.fighters[sender];
            let previous = f.last_input_x.zip(f.last_input_y);
            // A flagged sample BEFORE the attack press must not leak into the next
            // gesture. Retail does send such samples while merely repositioning the
            // sword; only a path inside the current Charging window is a committed
            // manual slash candidate.
            if sample.start_attack_trigger_ready && f.charge_press_at.is_some() {
                f.pending_manual_attack = previous.and_then(|p| {
                    manual_attack_gesture(p, (sample.x, sample.y))
                });
            }
            f.last_input_x = Some(sample.x);
            f.last_input_y = Some(sample.y);
            f.last_input_at = Some(now);
            if let Some(cc) = sample.client_charge {
                f.last_client_charge = Some(cc);
            }
            return Vec::new();
        }
    }

    let Some(target_slot) = combat.opponent_of(sender) else {
        debug!("combat: slot {sender} input ignored — solo/bot match, no opponent");
        return Vec::new();
    };
    if sender >= combat.fighters.len() || target_slot >= combat.fighters.len() {
        return Vec::new();
    }
    if combat.fighters[target_slot].is_dead() {
        if matches!(parse_input_activate(user_data), Some(act) if !act.held) {
            info!(
                "combat attack_input: gsid={} input=player slot={sender} actor={} outcome=target_dead",
                combat.game_session_id,
                combat.fighters[sender].loadout.display_name,
            );
        }
        debug!("combat: slot {sender} input ignored — target slot {target_slot} already dead");
        return Vec::new();
    }
    // A PARALYZED sender can't act — its inputs are locked for the paralyse duration
    // (`ActorParalyzedState`, §5.4). Handshake/block frames were already handled above;
    // this drops only the combat swing/ability of a paralysed attacker.
    if combat.fighters[sender].is_paralyzed() {
        if matches!(parse_input_activate(user_data), Some(act) if !act.held) {
            let hold_ms = combat.fighters[sender]
                .charge_press_at
                .map(|t| now.saturating_duration_since(t).as_secs_f32() * 1000.0)
                .unwrap_or(0.0);
            info!(
                "combat attack_input: gsid={} input=player slot={sender} actor={} outcome=paralyzed hold_ms={hold_ms:.1}",
                combat.game_session_id,
                combat.fighters[sender].loadout.display_name,
            );
        }
        debug!("combat: slot {sender} input ignored — paralysed (inputs locked)");
        return Vec::new();
    }
    // A STAGGERED sender can't act either, for `baseStaggerDuration` (1.5 s). [Phase 3.13]
    //
    // …with two exceptions — see `performable_while_staggered`. The first is
    // Recovery Strikes, and it is the whole point of the ability: `Ability.Maneuver.RecoveryStrikes.Description` reads
    //
    //     "These Quick Strikes can be performed AT ANY TIME, EXCEPT WHEN PARALYZED.
    //      They each deal {0} extra damage (no extra damage for two-handed weapons)."
    //
    // It is the only ability in the whole shipped description corpus that says this
    // — a survey of every `*.Description` string for "at any time" / "staggered"
    // returns Recovery Strikes and nothing else. A level-25, 5-point maneuver whose
    // sole selling point is acting through a stun did nothing through a stun here,
    // because this gate dropped every input from a staggered fighter.
    //
    // The paralysis gate above still applies, which is the one exception the text
    // itself names, so it deliberately stays in front of this.
    if combat.fighters[sender].is_staggered(now) {
        let acts_through = input::parse_execute_ability(user_data)
            .is_some_and(|ea| performable_while_staggered(&ea.ability_uuid));
        if !acts_through {
            if matches!(parse_input_activate(user_data), Some(act) if !act.held) {
                info!(
                    "combat attack_input: gsid={} input=player slot={sender} actor={} outcome=staggered",
                    combat.game_session_id,
                    combat.fighters[sender].loadout.display_name,
                );
            }
            debug!("combat: slot {sender} input ignored — staggered");
            return Vec::new();
        }
        info!("combat: slot {sender} acts THROUGH a stagger (Recovery Strikes / dodge)");
    }

    // `PlayerCombatInputActivate` (gmid 46) on the 0x36 carrier — the discrete
    // press/release. This is how a real client commits a swing (the `0x2e`-carrier
    // branch near the top of this function is the near-dead legacy shape: 3 of 1 500
    // sampled prod gmid-46 frames).
    if let Some(act) = parse_input_activate(user_data) {
        if sender >= combat.fighters.len() {
            return Vec::new();
        }
        {
            let f = &mut combat.fighters[sender];
            f.last_input_block_zone = act.block_zone;
            if let Some(cc) = act.client_charge {
                f.last_client_charge = Some(cc);
            }
        }
        // ---- THE BLOCK TRIGGER ----
        //
        // `_isWithinBlockZone` IS the guard signal. This corrects the note that used to
        // sit further down ("recorded but deliberately does NOT gate the swing… prod
        // attack hits occur after both block-zone and non-block-zone bursts"). That
        // reasoning did not separate the blocker's frames from the opponent's.
        //
        // CAPTURE-PROVEN over prod sessions 503/506/486/615/616. Working backwards from
        // every s2c gmid 41, the nearest preceding c2s 46 carried blockZone=true in
        // **432 of 433** cases (99.8 %), and NOT ONCE was it `held=true, blockZone=false`.
        // Forwards, a `held=true, blockZone=true` press is followed by a 41 within one
        // second 83.8 % of the time with a median gap of 2 messages, against a 27-41 %
        // background rate at a median gap of 13-25 messages for the three control
        // classes. Of the presses with no 41, every one inspected is explained: the
        // avatar was already Blocking, or was locked in another state.
        //
        // This must run BEFORE the swing path, and not only to avoid a phantom attack:
        // block presses land at pointer X ≈ 0.077 (a dedicated button on the far left
        // edge; 344 of 351 below 0.5), so `classify_side_from_x` would label every
        // single one `ActiveSide::Left` and register a left swing on every guard.
        if act.block_zone == Some(true) {
            let f = &mut combat.fighters[sender];
            f.pending_manual_attack = None;
            if act.held {
                // Guard UP. `set_actor_state` queues the transition; the drain turns it
                // into the gmid 41 that raises the shield on BOTH screens.
                f.set_actor_state(ActorStateType::Blocking, now);
                f.blocking_side = ActiveSide::Middle; // retail: propId 9 == 1 in 578/578
                f.blocking_until = Some(now + BLOCK_LEAK_GUARD);
                f.block_raised_at = Some(now);
                debug!("combat: slot {sender} op46 blockZone DOWN — guard UP");
            } else {
                // Guard DOWN on release. Retail ends a block with a gmid 39 carrying
                // stateId 0, not a second 41 — 199 of 225 own-avatar exits are exactly
                // this, immediately after the release. `reconcile_block` already maps
                // Blocking → Idle, so clearing the window is all that is needed and the
                // drain emits the 39.
                f.blocking_until = None;
                f.reconcile_block(now);
                debug!("combat: slot {sender} op46 blockZone UP — guard DOWN");
            }
            // A block press is not a charge and never a swing.
            f.charge_press_at = None;
            return Vec::new();
        }
        if act.held {
            // An ATTACK press also ends a guard — 94 of 225 real block exits are cut
            // short this way rather than by a release.
            let f = &mut combat.fighters[sender];
            if f.actor_state() == ActorStateType::Blocking {
                f.blocking_until = None;
                f.reconcile_block(now);
                debug!("combat: slot {sender} attack press cancels the guard");
            }
            // Button DOWN — start the server's charge stopwatch AND enter the wind-up.
            f.charge_press_at = Some(now);
            f.bot_swing_at = None;
            f.pending_manual_attack = None;
            debug!(
                "combat: slot {sender} op46 DOWN (carrier 0x36) — charge press recorded \
                 (blockZone={:?})",
                act.block_zone
            );
            // THE WIND-UP. Retail begins every swing with gmid 45 `Charging` 300-400 ms
            // before the 52 — 593 of 593 decoded swings, both avatars, no exceptions —
            // and it is the long, visible part: the 52 → 43 → 44 tail runs in 66 ms.
            // We were sending none of it, which is why the shield animated and the
            // swing did not.
            let side = classified_side_for(&combat.fighters[sender], now);
            combat.fighters[sender].charge_side = side;
            combat.fighters[sender]
                .set_actor_state(ActorStateType::Charging, now);
            return Vec::new();
        }
        // Button UP — commit the swing, but ONLY if we ever saw the press.
        //
        // A release with no recorded press is not a swing. The path that produces
        // one is the block: a guard press sets `charge_press_at = None` (a block is
        // "not a charge and never a swing", above), so if the matching RELEASE is
        // not classified as a block it falls through to here — and we committed a
        // full swing for a button the player never pressed, with `hold_secs` 0 and
        // no `Charging` wind-up, so no animation played either.
        //
        // That is exactly what report #113 describes, from a player who opens with
        // a held shield: "Every time I released that first block, it seemed like I
        // was quickly hitting my opponent, though I did not push a button for this
        // nor see the strike animation. All that I saw was my opponent taking the
        // hit."
        //
        // The block-vs-swing split is positional and admits misses by construction
        // — the comment above records 344 of 351 block presses below pointer X 0.5,
        // so 7 were not. Rather than chase that classification, require the thing a
        // real swing always has: a press. A genuine swing goes DOWN (which records
        // `charge_press_at` and enters `Charging`) and only then UP.
        if combat.fighters[sender].charge_press_at.is_none()
            && combat.fighters[sender].actor_state() != ActorStateType::Charging
        {
            debug!(
                "combat: slot {sender} op46 UP with no recorded press — ignoring                  (block release misread as a swing)"
            );
            return Vec::new();
        }
        let hold_secs = combat.fighters[sender]
            .charge_press_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0);
        combat.fighters[sender].charge_press_at = None;
        // Server measurement is authoritative; the client's number is only compared.
        charge_cross_check(sender, hold_secs, act.client_charge);
        let swing_factor = charge_crit_factor_at(&combat.fighters[sender], hold_secs, now);
        if combat.fighters[target_slot].is_dead() {
            return Vec::new();
        }
        // Prefer the side classified from live geometry (what the damage model is
        // calibrated on); fall back to the side the wind-up was announced with, so the
        // 45 and the 52 carry the same ActiveSide as they do in every captured pair.
        let side = classified_side_for(&combat.fighters[sender], now)
            .or(combat.fighters[sender].charge_side);
        debug!(
            "combat: slot {sender} op46 UP (carrier 0x36) — hold {hold_secs:.3}s ×{swing_factor:.3}, \
             side {side:?} from x={:?}",
            combat.fighters[sender].last_input_x
        );
        return resolve_swing_with_side(combat, sender, target_slot, swing_factor, side, now);
    }


    // A RequestExecuteAbility (spell/ability) vs a weapon swing.
    if let Some(ea) = input::parse_execute_ability(user_data) {
        resolve_ability_cast(combat, sender, target_slot, user_data, &ea, now)
    } else {
        // An *unstructured* carrier-0x36 body: no GameMessageId, so neither an
        // ability nor one of the gmid-46/47 input messages handled above. Real
        // clients commit swings through gmid 46; this branch keeps bots and
        // minimal/synthetic clients able to attack. No `_held` info ⇒ ×1.0 (no crit).
        //
        // Phase 4.1: still prefer the classified side if the sender happens to have
        // streamed fresh pointer geometry; otherwise `resolve_swing_with_side`
        // applies the synthetic-alternation fallback.
        let side = classified_side_for(&combat.fighters[sender], now);
        resolve_swing_with_side(combat, sender, target_slot, 1.0, side, now)
    }
}

/// `Recovery Strikes` — the one maneuver the shipped data says may be performed
/// while staggered. Pinned by uuid because that is what arrives on the wire; the
/// test `the_recovery_strikes_uuid_still_names_recovery_strikes` checks the uuid
/// against the shipped ability table, so a data change cannot silently unhook it.
const RECOVERY_STRIKES_UUID: &str = "e08f95de-85bb-4829-ba7e-cf45bc6fb422";

fn is_recovery_strikes(ability_uuid: &str) -> bool {
    ability_uuid.eq_ignore_ascii_case(RECOVERY_STRIKES_UUID)
}

/// May this ability be performed while STAGGERED?
///
/// Two families, and only two.
///
/// **Recovery Strikes** says so itself —
/// `Ability.Maneuver.RecoveryStrikes.Description`: *"These Quick Strikes can be
/// performed at any time, except when Paralyzed."* It is the only ability in the
/// shipped description corpus that carries that sentence.
///
/// **The dodges** say nothing, so this one is measured. Counting every ability
/// execution (gmid 37/38) that falls inside a `Staggered` (3) op51 apply→remove
/// window, over 11 retail sessions:
///
/// ```text
///   RecoveryStrikes    64 while staggered / 221 free   22.5%   (documented)
///   DodgingStrike      50 / 218                        18.7%
///   AdrenalineDodge    44 / 314                        12.3%
///   ---------------------------------------------------------
///   QuickStrikes        0 / 294                         0.0%
///   LightningBolt       0 / 219                         0.0%
///   HarryingBash        0 / 213                         0.0%
///   Ward                0 / 70                          0.0%
/// ```
///
/// The zeros are the point: comparably-sampled abilities are never once executed
/// inside a stun window, so this is a real distinction and not a measurement floor.
/// Note it does NOT follow the maneuver/spell split — QuickStrikes and HarryingBash
/// are maneuvers and score zero.
///
/// The obvious confound is a lagging `Staggered` remove making post-stun uses look
/// mid-stun. Measuring WHERE in the window each use falls rules that out: 30 % of
/// DodgingStrike's stunned uses sit in the FIRST HALF of the stun, earlier than
/// Recovery Strikes' own 14 %. The method self-checks too — StaggeringBash, the
/// ability that CAUSES a stagger, lands at median position 0.10, i.e. right at the
/// window's start, exactly where it must.
///
/// Keyed on the shipped dodge field rather than a uuid list, so all four dodge
/// maneuvers are covered and a data change cannot leave one behind.
///
/// Reported by Taheen (#113), who said dodges free you from a stun the way Recovery
/// Strikes does. He was right.
fn performable_while_staggered(ability_uuid: &str) -> bool {
    if is_recovery_strikes(ability_uuid) {
        return true;
    }
    super::gamedata::ability_rank_clamped(ability_uuid, 1)
        .and_then(|r| r.maximum_damage_dodged())
        .is_some_and(|cap| cap > 0.0)
}

/// A weapon auto-attack (committed swing), throttled per attacker.
///
/// `swing_factor` is the held-charge crit multiplier:
///   - `1.0` for a normal (partial / uncharged) swing via carrier-0x36 or bot swings.
///   - the weapon template's `maxDamageFactor` for a full-charge crit dispatched from
///     the op46 (0x2e) path once its server-measured `backswingTime` is reached.
fn resolve_swing_with_side(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    swing_factor: f32,
    decoded_side: Option<ActiveSide>,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    // Consume the gesture at commit even when the cadence gate rejects this swing;
    // otherwise a rejected release could make the NEXT tap look like a manual slash.
    let manual_attack = combat.fighters[sender].pending_manual_attack.take();
    let cooldown = swing_cooldown_for(&combat.fighters[sender], now);
    if let Some(last) = combat.fighters[sender].last_swing {
        let elapsed = now.saturating_duration_since(last);
        if elapsed < cooldown {
            info!(
                "combat attack_input: gsid={} input={} slot={sender} actor={} outcome=throttled elapsed_ms={:.1} required_ms={:.1} swing_factor={swing_factor:.3} side={decoded_side:?}",
                combat.game_session_id,
                if sender >= combat.expected_peers { "bot" } else { "player" },
                combat.fighters[sender].loadout.display_name,
                elapsed.as_secs_f32() * 1000.0,
                cooldown.as_secs_f32() * 1000.0,
            );
            debug!("combat: slot {sender} swing throttled (< {cooldown:?} since last, weapon cadence)");
            return Vec::new();
        }
    }
    let elapsed_ms = combat.fighters[sender]
        .last_swing
        .map(|last| now.saturating_duration_since(last).as_secs_f32() * 1000.0);
    info!(
        "combat attack_input: gsid={} input={} slot={sender} actor={} outcome=accepted elapsed_ms={elapsed_ms:?} required_ms={:.1} swing_factor={swing_factor:.3} side={decoded_side:?}",
        combat.game_session_id,
        if sender >= combat.expected_peers { "bot" } else { "player" },
        combat.fighters[sender].loadout.display_name,
        cooldown.as_secs_f32() * 1000.0,
    );
    combat.fighters[sender].last_swing = Some(now);

    // ---- Phase 4.1: swing side ----
    //
    // `decoded_side` is the side CLASSIFIED FROM THE CLIENT'S REAL POINTER GEOMETRY
    // (`classified_side_for` → `classify_side_from_x`). That is the normal path for a
    // real client and the whole point of Phase 4.1: the combo ramp (×1.00 → ×1.45 →
    // ×2.65 → ×4.12) only advances on *alternating* sides, so the side has to reflect
    // what the player actually did.
    //
    // ======================= SYNTHETIC FALLBACK (not real input) ==================
    // When `decoded_side` is `None` there is no usable geometry — a BOT, a client
    // that streams no `PlayerCombatInputPosition`, a stale (>`SIDE_CLASSIFY_SAMPLE_TTL`)
    // sample, or an out-of-range coordinate. We then ALTERNATE Left/Right so the
    // fight still progresses and nothing hangs. This is deliberately generous (it
    // maxes the combo ramp) but it is unreachable for a real, streaming client, and
    // the alternative — refusing the swing — would deadlock a bot match.
    // The first swing of a chain is Right (the s506 combo-0 reference).
    // ==============================================================================
    let next_side = decoded_side.filter(|s| *s != ActiveSide::None).unwrap_or_else(|| {
        match combat.fighters[sender].last_combo_side {
            ActiveSide::Right => ActiveSide::Left,
            _ => ActiveSide::Right, // None / Left / Middle → start (or restart) on Right
        }
    });
    // A `Middle` (maneuver/charged) swing is not part of a side chain — it resets it.
    let combo_count = if next_side == ActiveSide::Middle {
        combat.fighters[sender].reset_combo();
        0
    } else {
        combat.fighters[sender].register_combo_swing(next_side)
    };

    // The swing is committed: walk the attacker's actor state through
    // Attack/AutoAttack → FollowThrough → Recovery → Idle so BOTH clients animate it. Runs
    // after `register_combo_swing` so `last_combo_side` is this swing's side, and
    // before damage so the wind-up precedes the hit on the wire, as in retail.
    let impact_delay = begin_swing_animation(combat, sender, manual_attack, now);

    // The hit lands with the FollowThrough beat, not now (tracker #21).
    //
    // The animation already used retail's measured 50 ms; the damage did not, so the
    // wire said "hit at +50 ms" while the server applied it at 0. Scheduling it here
    // makes the two agree, and — the actual point — means the defender's guard is
    // read at IMPACT rather than at commit, so a block raised during the swing
    // counts. That is the window Taheen was reporting the absence of.
    //
    // Reuses FOLLOW_THROUGH_DELAY rather than adding a second constant: there is one
    // moment of impact and it should have one name.
    combat.pending_hits.push(PendingHit {
        sender,
        target: target_slot,
        side: next_side,
        swing_factor,
        combo_count,
        due: now + impact_delay,
    });
    debug!(
        "combat: slot {sender} swing COMMITTED — lands in {impact_delay:?}"
    );
    Vec::new()
}

/// Apply every committed swing whose FollowThrough beat has arrived.
///
/// The defender's guard is sampled HERE, by `resolve_attack` reading the target's
/// CURRENT state — which is what lets a guard raised during the windup block.
fn land_due_hits(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    if combat.pending_hits.is_empty() {
        return Vec::new();
    }
    let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut combat.pending_hits)
        .into_iter()
        .partition(|h| now >= h.due);
    combat.pending_hits = waiting;

    let mut out = Vec::new();
    for h in due {
        if h.sender >= combat.fighters.len() || h.target >= combat.fighters.len() {
            continue;
        }
        // A round can end while a swing is in the air; landing it afterwards would
        // deal damage into the next round.
        if !matches!(combat.phase, FlowState::StateTimeout) {
            continue;
        }
        if combat.fighters[h.target].is_dead() || combat.fighters[h.sender].is_dead() {
            continue;
        }
        let mut attacker_loadout = combat.fighters[h.sender].loadout.clone();
        // Reckless Fury adds its weapon-class bonus to every swing for its window
        // (`_bonusDamages`: Light 11.24 / Versatile 14.17 / Heavy 17.86 at rank 1).
        // Ride the maneuver channel — same "flat additive on the physical base"
        // treatment, on a clone so it expires with the buff rather than sticking.
        if combat.fighters[h.sender].has_reckless_fury(now) {
            attacker_loadout.maneuver_bonus_damage +=
                combat.fighters[h.sender].reckless_fury_bonus;
        }
        let resolved = RetailDamageModel.resolve_attack(
            &attacker_loadout,
            &combat.fighters[h.target],
            DamageSource::Attack,
            h.side,
            h.swing_factor,
            h.combo_count,
            now,
        );
        // A connected OPTIMAL block on the target RESETS the attacker's combo (§4.2: a
        // block breaks the chain — the next swing starts fresh at ×1.0) **and STUNS the
        // attacker** (tracker #31).
        let blocked_high = resolved.blocked
            && resolved.flags & super::damage::flags::WAS_OPTIMAL_BLOCKING != 0;
        if blocked_high {
            combat.fighters[h.sender].reset_combo();
        }
        out.extend(emit_damage(combat, h.sender, h.target, &resolved, now));
        // AFTER the damage frame: retail fires `_causedStagger` from inside
        // `CombatManager.ApplyDamage` (`dump.cs:546170`), so the stun follows the hit
        // it came from.
        if blocked_high {
            out.extend(stun_the_blocked_attacker(combat, h.sender, h.target, now));
        }
    }
    out
}

/// **The high-block stun** (tracker #31): a WEAPON attack that connects with an
/// OPTIMAL ("high") block stuns the ATTACKER.
///
/// Retail, from the shipped client text:
/// * `UI.Help.Blocking.Description` — *"At first, for a short time, you will block
///   high, then lower your shield to block low. **When a weapon attack is blocked high,
///   the attacker gets stunned.** … Weapons can also block high and stun your enemy …
///   Broken shields and weapons … cannot stun the attacker."*
/// * `UI.Help.Arena.Description` — *"High blocks can be held longer, refresh faster,
///   and **stun opponents for longer**."*
/// * `Challenge.StunEnemy.HighBlock` — *"Stun {0} Enemies with High Blocks"*, a shipped
///   challenge type.
/// * `Enchantment.Effect.PowerfulBlock` — *"Target stunned by a blocked attack takes
///   {0} extra damage while stunned."*
///
/// Client corroboration: `PowerfulBlockBonusInstance.CausedStagger(DamageSource, Actor
/// attacker, Actor owner)` (`dump.cs:621783`), registered into
/// `ActorBonusHandler._causedStagger` (`:618680`) and fired from
/// `CombatManager.ApplyDamage` (`:546170`) — the only stagger callback whose signature
/// carries BOTH the attacker and the block's owner.
///
/// **WEAPON ATTACKS ONLY.** `UI.Help.Skills.Description`: *"You do not get stunned when
/// your ability attack is blocked high."* That exclusion is structural here — this is
/// called only from [`land_due_hits`], which lands auto-attack swings. The maneuver /
/// spell lane in `resolve_ability_cast` never calls it, so a blocked Shield Bash or
/// Fireball leaves its caster free, exactly as the help text says.
///
/// **Duration is shipped data, not authored:**
/// `PvpDefaultSettings.BASE_STAGGER_DURATION = 2.5` (`dump.cs:427016`), already exposed
/// as [`state::BASE_STAGGER_DURATION_SECS`]. That is also the "for longer" in the arena
/// help text: `CombatParameters.baseStaggerDuration` (the PvE value) is 1.5 s. No
/// separate "blocked-attacker stun" constant exists anywhere in `PvpDefaultSettings`,
/// `CombatParameters` or `PlayerCombatParameters`, so the generic PvP stagger duration
/// is the shipped value that applies.
///
/// NOT MODELLED: retail's `PlayerBlockingState._consumedOptimalBlock`
/// (`dump.cs:597064`) makes one guard-raise yield one high block. It is unnecessary
/// here — the 2.5 s stun already outlasts the defender's 2.0 s
/// `BLOCK_OPTIMAL_TIME` window, so a second stun inside the same window is
/// unreachable. Also not modelled: broken shields/weapons cannot stun — nothing in this
/// engine breaks, so there is no broken state to check.
fn stun_the_blocked_attacker(
    combat: &mut MatchCombat,
    attacker_slot: usize,
    blocker_slot: usize,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{BASE_STAGGER_DURATION_SECS, StatusEffectType};
    let mut out = Vec::new();
    let viewers = combat.fighters.len();
    if attacker_slot >= viewers || combat.fighters[attacker_slot].is_dead() {
        return out;
    }
    let secs = BASE_STAGGER_DURATION_SECS;
    // Held Powerful Block weakness, read BEFORE the stagger refreshes anything.
    let weakness_held = combat.fighters[attacker_slot].has_staggered_weakness(now);
    // A refused stagger (Reckless Fury, paralysis) has no Staggered status, so
    // `CombatManager$$ApplyDamage@0x1bd2770` fires no `CausedStagger` hook: no op51(3),
    // and no Powerful Block `StaggeredWeakness` either.
    if !combat.fighters[attacker_slot].apply_stagger_for(now, secs) {
        debug!(
            "combat: slot {attacker_slot} high-blocked by slot {blocker_slot} but cannot \
             be staggered (Reckless Fury or paralysed) — no stun"
        );
        return out;
    }
    let obj = combat.fighters[attacker_slot].net_object_id;
    info!(
        "combat: slot {attacker_slot} STUNNED {secs:.2}s — its weapon attack was \
         blocked HIGH by slot {blocker_slot} (tracker #31)"
    );
    // Retail sends the actor-state frame BEFORE the status frame: 90 of 90 staggering
    // high blocks in s615/s616, no exceptions. `apply_stagger_for` above queued the
    // Staggered transition, so drain THIS actor now and the op39 goes out ahead of the
    // op51. Without this the end-of-tick drain appends it afterwards and every stun we
    // send is in the opposite order to every one retail sent.
    out.extend(drain_state_changes_for(combat, now, Some(attacker_slot)));

    // POWERFUL BLOCK. If the blocker's gear carries it, the attacker they just
    // stunned also takes `StaggeredWeakness` — "Target stunned by a blocked attack
    // takes {0} extra damage while stunned".
    //
    // The attacker HOLDS the status and it amplifies damage they TAKE (the client
    // registers it on `_weaknessSources`, read in the incoming-damage path; a matched
    // control over 36 samples shows exactly +0.0 when the holder is the one dealing).
    // It ships NO duration of its own — every captured op51 carries 0.0 — and is
    // removed with the Staggered that produced it.
    //
    // It never stacks and never refreshes: `PowerfulBlockBonusInstance$$CausedStagger
    // @0x1d51214` returns early when the attacker already has StaggeredWeakness (10),
    // so the first magnitude sticks and no second apply is sent.
    let weakness = combat.fighters[blocker_slot].loadout.powerful_block;
    if weakness > 0.0 && !weakness_held {
        combat.fighters[attacker_slot].weakness_rating = weakness;
        info!(
            "combat: slot {attacker_slot} takes STAGGERED WEAKNESS +{weakness:.2} from \
             slot {blocker_slot}'s Powerful Block"
        );
        let wframe = messages::change_combat_status_effect(
            obj,
            true,
            StatusEffectType::StaggeredWeakness,
            0.0,
        );
        for v in 0..viewers {
            out.push((v, wframe.clone()));
        }
    }

    let frame =
        messages::change_combat_status_effect(obj, true, StatusEffectType::Staggered, secs);
    for v in 0..viewers {
        out.push((v, frame.clone()));
    }
    out
}

/// Swing with the server-synthesised side (no decoded client geometry).
fn resolve_swing(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    swing_factor: f32,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    resolve_swing_with_side(combat, sender, target_slot, swing_factor, None, now)
}

/// A spell/ability cast: cooldown-gated, resource-gated (stamina for maneuvers /
/// magicka for spells), echoes `PerformExecuteAbility`, applies Spell-source damage,
/// deducts the resource cost, and emits `PlayerStatsUpdate`(65) to both players.
pub(super) fn resolve_ability_cast(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    user_data: &[u8],
    ea: &input::ExecuteAbility,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    // Cooldown gate (per ability instance).
    if let Some(&until) = combat.fighters[sender].cooldowns.get(&ea.ability_uuid) {
        if now < until {
            debug!("combat: slot {sender} ability {} on cooldown", ea.ability_uuid);
            return Vec::new();
        }
    }

    // Look up the ability tag and level from the equipped loadout (needed for cost +
    // tag routing below; default to level=1/Generic for unrecognised abilities).
    let (level, tag) = combat.fighters[sender]
        .loadout
        .abilities
        .iter()
        .find(|a| a.instance_uuid == ea.ability_uuid)
        .map(|a| (a.level, a.tag))
        .unwrap_or_else(|| {
            // A miss used to fall through silently to `Generic`, which routes to the
            // DAMAGE arm — so an unrecognised cast was treated as a DAMAGE SPELL and
            // fired at the opponent for whatever the model returned. Nothing said so in
            // the log, which is why the reported "0 damage effect on the opponent" was
            // undiagnosable from outside.
            //
            // Classify from gamedata instead. A cast carrying a TEMPLATE uuid now routes
            // correctly — a maneuver stays a maneuver rather than becoming a spell, which
            // matters because a maneuver's damage comes from the WEAPON and it ships no
            // ability damage field of its own. An instance uuid gamedata cannot resolve
            // still lands on `Generic`, and the `ships_damage` guard below then stops it
            // fabricating a hit.
            let tag = super::loadout::ability_tag_for_template(&ea.ability_uuid);
            debug!(
                "combat: slot {sender} cast ability {} — not in the equipped loadout \
                 ({} equipped); classified from gamedata as {tag:?} at level 1",
                ea.ability_uuid,
                combat.fighters[sender].loadout.abilities.len(),
            );
            (1, tag)
        });

    // Resource gate (spec §1, bug 2): reject the cast (no effect, no cooldown set,
    // no damage) if the caster lacks the required stamina (maneuvers) or magicka
    // (spells).  `ability_cost` returns APK-authoritative costs; unknown UUIDs
    // return (0,0) — no gate applies (backward-compatible: unrecognised spells still
    // fire). The rank (1-based, from the equipped level) drives the linear cost ramp.
    let (stam_cost, mag_cost) = tables::ability_cost(&ea.ability_uuid, level);
    let short_of_stamina = stam_cost > 0 && combat.fighters[sender].stamina < stam_cost;
    let short_of_magicka = mag_cost > 0 && combat.fighters[sender].magicka < mag_cost;
    // A SHIELD BASH needs a shield. There is no `requiresShield` field anywhere in
    // the shipped data — the precondition lives in the client's
    // `ShieldBashAbility.CanBeCast` — so it is keyed off the one thing that IS
    // authored: the four bashes are exactly the maneuvers that ship a `_blockDuration`
    // (0.50 s), because the bash IS a guard plus a strike. A player with no shield
    // could bash freely, getting the guard window and the bonus damage for nothing.
    //
    // Verified against the shipped table: a non-zero `_blockDuration` is carried by
    // EXACTLY the four player bashes (Harrying, Staggering, Reflecting, Shield Bash)
    // — Guardbreaker correctly does not, it is a weapon maneuver — plus the
    // enemy-only ShieldOfMania, which is excluded because an enemy's loadout does not
    // model a shield and gating it would silently disarm the AI.
    let needs_shield = super::gamedata::ability(&ea.ability_uuid)
        .is_some_and(|a| !a.enemy_only)
        && super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16)
            .is_some_and(|r| r.block_duration().is_some_and(|v| v > 0.0));
    let no_shield = needs_shield && !combat.fighters[sender].loadout.has_shield;
    if short_of_stamina || short_of_magicka || no_shield {
        if no_shield {
            debug!(
                "combat: slot {sender} ability {} REJECTED — a bash needs a shield",
                ea.ability_uuid,
            );
        } else if short_of_stamina {
            debug!(
                "combat: slot {sender} ability {} REJECTED — insufficient stamina ({} < {} required)",
                ea.ability_uuid, combat.fighters[sender].stamina, stam_cost,
            );
        } else {
            debug!(
                "combat: slot {sender} ability {} REJECTED — insufficient magicka ({} < {} required)",
                ea.ability_uuid, combat.fighters[sender].magicka, mag_cost,
            );
        }
        // Report #109: a rejection used to `return Vec::new()` — the server sent
        // NOTHING back. No op65, so the caster's HUD bars never moved, and no
        // cooldown, so the icon never greyed out. From the player's seat that reads
        // as "the cost is displayed but nothing is deducted and I can cast forever",
        // which is exactly how it was reported. The client is running its own
        // prediction; if the authority stays silent there is nothing to correct it.
        //
        // So a rejection now re-states the caster's CURRENT pools to both players.
        // It is the same frame the commit path sends, carrying the unchanged values:
        // the cast is still refused, still costs nothing and still sets no cooldown,
        // but the client is told what it actually has instead of being left with its
        // own guess. `stats_seq` is bumped so the client treats it as a fresh update
        // rather than a duplicate of the last one.
        combat.fighters[sender].stats_seq = combat.fighters[sender].stats_seq.wrapping_add(1);
        let packed = combat.fighters[sender].packed_stats();
        let obj_id = combat.fighters[sender].net_object_id;
        let other_packed = combat.fighters[target_slot].packed_stats();
        let frame = messages::player_stats_update(obj_id, packed, other_packed);
        return (0..combat.fighters.len()).map(|s| (s, frame.clone())).collect();
    }

    // MAXIMUM POWER is evaluated at CAST time — "Spells are {0}% more effective when
    // **cast while** Magicka is full" — and the client proves the order: in
    // `Actor.ExecuteAbility` the effectiveness fold runs BEFORE `PayAbilityCost`, and
    // the result is frozen into the AbilityExecution for every step and channel tick.
    //
    // We deducted the cost here and only read "is magicka full?" ~490 lines later, so
    // for ANY spell with a magicka cost the perk could never fire. It was dead on
    // arrival, and both existing tests hand-built `CasterPerks { magicka_full: true }`
    // so neither could see it.
    //
    // "Full" is against the pool's FULL maximum, not the ravage-lowered ceiling:
    // `BoundedPercent` reads the full `Maximum` (combat-spec ch. 11 §1.1), so
    // Maximum Power is void while ravaged. This used to compare against the lowered
    // `max_magicka` and override the ravage-aware `CasterPerks::of`, letting the perk
    // fire at a ravaged ceiling.
    let magicka_full_at_cast =
        super::perks::magicka_full_for_maximum_power(&combat.fighters[sender]);

    // Resource gate passed → commit: set cooldown and deduct the cost.
    combat
        .fighters[sender]
        .cooldowns
        .insert(ea.ability_uuid.clone(), now + ability_cooldown(&ea.ability_uuid, level));

    // Deduct stamina/magicka and emit op65 PlayerStatsUpdate to both players so the
    // HUD bars reflect the new pools immediately.  `stats_seq` is bumped inside
    // `packed_stats` as a monotonic counter (shared with `take_damage`).
    let stat_frames: Vec<(usize, Vec<u8>)> = if stam_cost > 0 || mag_cost > 0 {
        combat.fighters[sender].stamina =
            combat.fighters[sender].stamina.saturating_sub(stam_cost);
        combat.fighters[sender].magicka =
            combat.fighters[sender].magicka.saturating_sub(mag_cost);
        combat.fighters[sender].stats_seq =
            combat.fighters[sender].stats_seq.wrapping_add(1);
        info!(
            "combat: slot {sender} ability {} deducted stam={stam_cost} mag={mag_cost} → \
             stam={}/{} mag={}/{}",
            ea.ability_uuid,
            combat.fighters[sender].stamina,
            combat.fighters[sender].max_stamina,
            combat.fighters[sender].magicka,
            combat.fighters[sender].max_magicka,
        );
        let packed = combat.fighters[sender].packed_stats();
        let obj_id = combat.fighters[sender].net_object_id;
        // propId 5 is `_pvpOtherActorStats` — the OPPONENT of the avatar at propId 0.
        // It used to be a hardcoded `1`, which decodes to all-pools-zero.
        let other_packed = combat.fighters[target_slot].packed_stats();
        let frame = messages::player_stats_update(obj_id, packed, other_packed);
        (0..combat.fighters.len()).map(|s| (s, frame.clone())).collect()
    } else {
        Vec::new()
    };

    let mut out = Vec::new();
    // PerformExecuteAbility (38) echo to both — the cast confirmation/visual.
    let perform = messages::perform_execute_ability(user_data, ea.role_offset);
    out.push((sender, perform.clone()));
    out.push((target_slot, perform));

    // Emit the stat update (after the cast echo so the client sees the visual before
    // the bar drop — matches retail ordering).
    out.extend(stat_frames);

    // A shield bash is a compound action: raise the guard now, then land the strike
    // after the shipped `_blockDuration`. Queue and drain the Blocking transition
    // before op58 so the wire history remains chronological (Blocking → Maneuver).
    begin_ability_guard(combat, sender, &ea.ability_uuid, level, now);
    out.extend(drain_state_changes_for(combat, now, Some(sender)));

    // op53 `PlayerChannelingStateChange` — the CAST ANIMATION / channelling feedback.
    // Retail sends it immediately after the op38 echo (s127: c2s op37 #954963 → s2c
    // op38 #954965 → s2c op53 #954966), to both players, so each sees the caster wind
    // up. Without it spells fire with no channelling visual — this was the standing
    // "still to wire" gap in this module.
    //
    // The channel time comes from the CASTER'S OWN equipped ability at its own rank
    // (`ability_rank_clamped(uuid, level)._channelDuration`) — never a hard-coded UUID.
    // Abilities that ship no `_channelDuration` (e.g. `4be1d681…`) send 0.0. See the
    // note on `messages::player_channeling_state_change`: the float's exact retail
    // semantics are NOT pinned by the captures (the captured values are not the shipped
    // `_channelDuration`). propId 7 is the caster's state history with Channeling as
    // its newest entry; without it the client's `Deserialize` throws and the frame is
    // dropped (12-D1).
    //
    // op53 is for CHANNELLED casts only — a maneuver never gets one. Measured over
    // 60 decrypted sessions, scanning every coalesced `0xBE` rather than one message
    // per packet: the ten abilities that carry an op53 are all spells (Resist
    // Elements, Lightning Bolt, Fireball, Ice Spike, Frostbite, Paralyze, Poison
    // Cloud, Delayed Lightning Bolt, Blind, Consuming Inferno) and the five bashes
    // carry **zero** between them — across 788 bash op38 echoes (Guardbreaker 96,
    // Harrying Bash 245, Reflecting Bash 7, Shield Bash 61, Staggering Bash 379).
    //
    // We used to send one for every ability. A maneuver ships no `_channelDuration`,
    // so a bash went out as a `PlayerChannelingStateChange` of 0.0 s — a frame retail
    // never sends, carrying a value it almost never sends (12 of 2 860 captured op53
    // floats are 0.0). Putting the caster into a channelling state that ends the same
    // instant is the reported "shield bashes had no animation" (report #24): strikes
    // take no op53 and animate, spells take one with a real duration and animate,
    // bashes took one with 0.0 and did not.
    // A MANEUVER animates off op58 instead. The split is exact in the corpus:
    // 100% of the 788 bash op38 echoes are followed by an op58 and none by an op53,
    // and 100% of the 1,330 spell echoes by an op53 and none by an op58.
    let state_frame = if tag == AbilityTag::Maneuver {
        // `actor_animation_for_maneuver` returns None for a maneuver the corpus
        // never showed. Emitting `ActorAnimation::None` would tell the client to
        // play nothing — the very bug being fixed — so skip the frame instead and
        // leave the omission visible.
        super::loadout::actor_animation_for_maneuver(&ea.ability_uuid).map(|anim| {
            // Every one of 2,941 retail op58 frames carries propId 7, and its newest
            // history entry is Maneuver. The old sparse frame omitted it; on device,
            // Piercing Strikes then fell back to a spell-like generic cast.
            let state_blob = combat.fighters[sender]
                .record_presentational_state(ActorStateType::Maneuver);
            messages::player_maneuver_state_change(
                combat.fighters[sender].net_object_id,
                combat.fighters[sender].packed_stats(),
                combat.fighters[target_slot].packed_stats(),
                0.0, // timeInState at state entry
                &ea.ability_uuid,
                anim,
                &state_blob,
            )
        })
    } else {
        // An INSTANT buff is not a channelled cast either. The same rule that
        // excludes bashes excludes these: op53 announces a channel, and an ability
        // that ships no `_channelDuration` has none to announce.
        //
        // Ward, Absorb, Magicka Surge and Blizzard Armor all ship NO channelDuration,
        // so they were going out as a `PlayerChannelingStateChange` of 0.0 s — the
        // same malformed frame that made bashes animate wrongly (report #24), and a
        // value retail essentially never sends (12 of 2,860 captured op53 floats are
        // 0.0). The corpus is explicit that retail sends nothing here at all: across
        // 144 captured Ward cast echoes and 30 Absorb cast echoes there is not one
        // op53 or op58 between them.
        //
        // That is the "spell pose appears with no spell text" report: the pose was
        // spurious: a generic cast animation for an instant buff whose brief label
        // had already gone. The op38 echo above still identifies the cast, which is
        // where the client gets the spell's name from.
        super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16)
            .filter(|r| {
                // "Does this ability channel at all?" — NOT "does it ship
                // `_channelDuration`". Frostbite and Consuming Inferno carry their
                // channel on `_channelMaxLength` instead and ship no
                // `_channelDuration`, yet retail demonstrably sends op53 for both.
                //
                // Checked against the measured corpus: this predicate agrees with
                // observed behaviour on all 14 player abilities whose op53 status is
                // known — the ten carriers (Resist Elements, Lightning Bolt, Fireball,
                // Ice Spike, Frostbite, Paralyze, Poison Cloud, Delayed Lightning
                // Bolt, Blind, Consuming Inferno) and the four that send nothing
                // (Ward, Absorb, Magicka Surge, Blizzard Armor).
                r.channel_duration().is_some_and(|v| v > 0.0)
                    || r.get(super::gamedata::AbilityField::ChannelMaxLength)
                        .is_some_and(|v| v > 0.0)
            })
            .map(|r| {
                // The wire float stays `_channelDuration` (0.0 when absent), exactly
                // as before — only WHETHER the frame is sent has changed.
                let channel_secs = r.channel_duration().unwrap_or(0.0);
                // How long the client holds Channeling: an `AbilityChannel` step's
                // `_duration` is `_channelDuration` (`AbilityChannel$$Update@0x1e8f8a8`);
                // Frostbite and Consuming Inferno channel for `_channelMaxLength`
                // (combat-spec 06 §1.2, §3.1-3.2).
                let pose_secs = if channel_secs > 0.0 {
                    channel_secs
                } else {
                    r.get(super::gamedata::AbilityField::ChannelMaxLength).unwrap_or(0.0)
                };
                let state_blob = combat.fighters[sender]
                    .begin_channel_pose(now + Duration::from_secs_f32(pose_secs.max(0.0)));
                messages::player_channeling_state_change(
                    combat.fighters[sender].net_object_id,
                    combat.fighters[sender].packed_stats(),
                    combat.fighters[target_slot].packed_stats(),
                    channel_secs,
                    &ea.ability_uuid,
                    &state_blob,
                )
            })
    };
    if let Some(f) = state_frame {
        out.push((sender, f.clone()));
        out.push((target_slot, f));
    }

    info!(
        "combat ability: gsid={} caster_slot={sender} caster={} target_slot={target_slot} target={} ability={} tag={tag:?} effective_rank={level}",
        combat.game_session_id,
        combat.fighters[sender].loadout.display_name,
        combat.fighters[target_slot].loadout.display_name,
        ea.ability_uuid,
    );

    // Phase 3.11: route on the FULL shipped ability table. Ward/Absorb/ResistElements
    // are self-buffs (no direct damage); Paralyze/Damage/Maneuver deal the rank's own
    // `_damage`; Perks never activate.
    use super::state::AbilityTag;
    // Health damage this cast dealt, if any — the gate for threshold effects (Blind).
    // Zero for buffs and for a maneuver that missed, which is correct: a threshold
    // effect must not fire on a cast that did not land.
    // A spell with a wind-up does not land now. Queue it; `land_due_impacts`
    // delivers it when the cast completes. Zero-delay abilities (maneuvers,
    // Frostbite, anything shipping no `channelDuration`) run inline exactly as
    // before, so this changes nothing for them.
    // COMBAT FOCUS / WILLPOWER — resistance while the caster is committed to the
    // cast. Granted at cast START, not at impact.
    //
    // The two perks key off DIFFERENT actor states in the client:
    //   * Combat Focus (`CombatFocusPerk$$GetResistanceBonus@0x1A5AA60`): the
    //     `Maneuver` state, or the Reckless Fury status. NEVER a spell — spells run in
    //     `Channeling`. It used to be granted on every cast, spells included.
    //   * Willpower (`ConservationistPerk$$GetResistanceBonus@0x1A5D52C`): the
    //     `Channeling` state, i.e. spells only.
    // Combat Focus is read live off `maneuver_state_until` / Reckless Fury in
    // `Fighter::transient_resistance_against`; Willpower still rides the
    // transient list. The window is the same castingDelay + channel (>= 0.5 s)
    // approximation of the state's length for both.
    {
        let kind = super::gamedata::ability(&ea.ability_uuid).map(|a| a.kind);
        let window = super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16)
            .map(|r| {
                r.get(super::gamedata::AbilityField::CastingDelay).unwrap_or(0.0)
                    + r.channel_duration().unwrap_or(0.0)
            })
            .unwrap_or(0.0)
            .max(super::perks::ABILITY_USE_MIN_WINDOW_SECS);
        let expires = now + Duration::from_secs_f32(window);
        let f = &mut combat.fighters[sender];
        match kind {
            Some(super::gamedata::AbilityKind::Maneuver) => {
                f.maneuver_state_until = Some(f.maneuver_state_until.map_or(expires, |t| t.max(expires)));
            }
            Some(super::gamedata::AbilityKind::Spell) => {
                let willpower = f.loadout.perks.conservationist;
                if willpower > 0.0 {
                    f.transient_all_resistance.push((willpower, expires));
                }
            }
            _ => {}
        }
    }

    let delay = ability_impact_delay(&ea.ability_uuid, level);
    if delay.is_zero() {
        out.extend(apply_ability_impact(
            combat, sender, target_slot, &ea.ability_uuid, level, tag,
            magicka_full_at_cast, now,
        ));
    } else {
        debug!(
            "combat: slot {sender} cast {} — impact in {delay:?}",
            ea.ability_uuid
        );
        combat.pending_impacts.push(super::state::PendingImpact {
            sender,
            target: target_slot,
            ability_uuid: ea.ability_uuid.clone(),
            level,
            tag,
            magicka_full_at_cast,
            due: now + delay,
        });
    }
    out
}

/// The EFFECT half of a cast: damage, control, and every shipped side effect.
///
/// Split out of [`resolve_ability_cast`] so it can run either immediately or from
/// the deferred queue. A spell that ships a `channelDuration` does not land the
/// instant the button is pressed — Ice Spike ships **1.12 s** and Paralyze **1.5 s**
/// — and we were applying damage, stun and paralysis at the moment of the cast.
/// From the player's seat the target is stunned before the spike has left the hand.
///
/// Swings already work this way: [`resolve_swing`] queues a `PendingHit` and
/// [`land_due_hits`] delivers it after `FOLLOW_THROUGH_DELAY`. This is the same
/// pattern for casts.

/// Apply a **Reckless Fury** cast to `caster_slot`: start the 5 s window, pick the
/// weapon-class bonus, and announce `StatusEffectType::RecklessFury (11)`.
///
/// Fury had NO persistent implementation. It is tagged `Maneuver`, so it fell into
/// the generic maneuver arm and was resolved as one ordinary Middle weapon hit —
/// which is both a phantom attack it should never make (`parameters.bonusDamage` is
/// 0 with both grip multipliers 0: it swings nothing) and a complete absence of the
/// five behaviours it exists for. Production match fffe01ca has the AI casting Fury
/// at 18:38:48 and being stunned at 18:38:50, two seconds inside a window that is
/// supposed to make stunning impossible.
///
/// `RecklessFuryAbility` serializes only `_bonusDamages` and `_duration`, so the
/// damage numbers are read from the asset and the four boolean behaviours (no stun,
/// no death, no block, no skills) are modelled as state for the window. They are NOT
/// invented magnitudes — they have none to invent.
fn apply_reckless_fury(
    combat: &mut MatchCombat,
    caster_slot: usize,
    rank: u8,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::StatusEffectType;
    let mut out = Vec::new();
    if caster_slot >= combat.fighters.len() {
        return out;
    }
    let Some(r) = super::gamedata::ability_rank_clamped(uuid_reckless_fury(), u16::from(rank.max(1)))
    else {
        return out;
    };
    let secs = r.get(super::gamedata::AbilityField::Duration).unwrap_or(5.0);
    // Pick the bonus for the wielder's weapon class, falling back to the class-0
    // "None" entry the asset ships for exactly this purpose.
    let class_raw = combat.fighters[caster_slot]
        .loadout
        .weapon_template
        .map(|w| w.weapon_class as u8)
        .unwrap_or(0);
    let bonus = r
        .bonus_damages
        .iter()
        .find(|(c, _)| *c == class_raw)
        .or_else(|| r.bonus_damages.iter().find(|(c, _)| *c == 0))
        .map(|(_, v)| *v)
        .unwrap_or(0.0);

    let f = &mut combat.fighters[caster_slot];
    f.reckless_fury_until = Some(now + Duration::from_secs_f32(secs));
    f.reckless_fury_bonus = bonus;
    // Fury cannot block: drop any guard already up, and `can_block` keeps it down.
    f.blocking_until = None;
    f.block_raised_at = None;
    let obj = f.net_object_id;
    info!(
        "combat: slot {caster_slot} RECKLESS FURY r{rank} for {secs:.2}s \
         (+{bonus:.2} dmg, weapon class {class_raw}, no stun / no death / no block)"
    );
    let frame =
        messages::change_combat_status_effect(obj, true, StatusEffectType::RecklessFury, secs);
    for v in 0..combat.fighters.len() {
        out.push((v, frame.clone()));
    }
    out
}

/// Reckless Fury's uuid, resolved from the shipped table rather than hardcoded.
fn uuid_reckless_fury() -> &'static str {
    super::gamedata::ABILITIES
        .iter()
        .find(|a| a.editor_name == "RecklessFury")
        .map(|a| a.uuid)
        .unwrap_or("")
}

/// The flat bonus damage a MANEUVER rank contributes to its swing, grip applied.
///
/// Every maneuver rank ships `parameters.bonusDamage` together with
/// `oneHandedMultiplier` / `twoHandedMultiplier`, and until now **nothing read
/// them** — the generated tables carried the numbers and the resolver never looked.
/// That is why all 17 maneuvers resolved to the same plain Middle weapon hit:
/// Guardbreaker's 131.15 and Quick Strikes' 13.78 produced identical damage.
///
/// The three authored families:
///
/// | family        | 1H  | 2H  | members                                             |
/// |---------------|-----|-----|-----------------------------------------------------|
/// | power-attack  | 0.5 | 1.0 | Power Attack, Guardbreaker, Skullcrusher, Ind. Smash |
/// | quick-strikes | 1.0 | 0.0 | Quick / Piercing / Recovery / Venom Strikes          |
/// | symmetric     | 1.0 | 1.0 | all Bash and Dodge variants                          |
///
/// so a power-attack family member is authored at its TWO-handed figure and halved
/// one-handed, while a quick-strikes member gets **no** bonus two-handed at all.
/// Reckless Fury ships 0/0 — it is a buff that swings nothing.
///
/// Grip follows the same rule as the weapon's own base damage:
/// `two_handed = !has_shield` (`loadout::base_damage_in_hand`).
///
/// CALIBRATION NOTE: the recorded s506 Middle-maneuver values (201.37 / 274.51 /
/// 186.98) sit inside the band of a PLAIN swing (150.81..271.46 across the
/// swing-factor range), and `swing_factor` is not observable in the capture, so that
/// recording can neither confirm nor refute the magnitude of this bonus. What it
/// cannot excuse is every maneuver dealing identical damage. The mechanism and the
/// authored numbers are what is shipped here; the magnitude wants a capture with a
/// known maneuver and a known charge state.
fn maneuver_bonus_damage(rank: &super::gamedata::AbilityRank, two_handed: bool) -> f32 {
    let Some(params) = rank.parameters else {
        return 0.0;
    };
    let mult = if two_handed {
        params.two_handed_multiplier
    } else {
        params.one_handed_multiplier
    };
    (params.bonus_damage * mult).max(0.0)
}

fn apply_ability_impact(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    ability_uuid: &str,
    level: u8,
    tag: super::state::AbilityTag,
    // Was the caster's magicka full when the cast was ACCEPTED, i.e. BEFORE its own
    // cost was deducted? Maximum Power's condition, frozen at cast the way the client
    // freezes it (`Actor.ExecuteAbility` folds effectiveness before `PayAbilityCost`).
    magicka_full_at_cast: bool,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::AbilityTag;
    let mut out: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut last_hit_total = 0.0f32;
    // Did the TARGET's guard take this cast's hit? Guardbreaker and Staggering Bash
    // ship the SAME `_damageToCauseStagger` but opposite block conditions, and
    // Harrying Bash is gated on it too, so `apply_shipped_effects` needs to know.
    // False for arms that deal no damage — a cast that never touched the target was
    // not blocked. (Not read from the wire flags: a low block carries none.)
    let mut target_blocked = false;
    let target_absorbing =
        target_slot < combat.fighters.len() && combat.fighters[target_slot].has_absorb(now);
    match tag {
        AbilityTag::Ward => out.extend(apply_ward(combat, sender, level, now)),
        AbilityTag::Absorb => out.extend(apply_absorb(combat, sender, level, now)),
        AbilityTag::ResistElements => out.extend(apply_resist_elements(combat, sender, level, now)),
        // A perk is PASSIVE — it is never "cast", so having nothing to do at
        // activation is correct. What was wrong is that nothing read perks
        // ANYWHERE: all 20 shipped perks were parsed, carried a rank, and had no
        // effect on any fight. They now resolve into `Loadout::perks` at parse time
        // (see `combat::perks`) and are applied where each one belongs — the damage
        // model, the block outcome, the regen tick and the cast window.
        AbilityTag::Perk => {}
        // A MANEUVER is a weapon attack, not a spell. It deals the attacker's WEAPON
        // damage on the Middle side — which the damage model already implements and
        // `roundtrip_s506_damage::s506_middle_maneuver_lands_in_recorded_band` already
        // validates against recorded s506 values.
        //
        // It was routed to `resolve_ability` instead, which reads the ability's own
        // shipped `_damage` — and a maneuver rank does not have one. Measured on prod
        // 2026-08-03: QuickStrikes (150 stamina) and PiercingStrikes (180 stamina) ship
        // NO damage field at any rank, so `unwrap_or(0.0)` made both cost a third of
        // the stamina bar and do literally nothing. 87 of 160 casts that day dealt 0.0.
        // Reckless Fury is tagged Maneuver but is a pure SELF-BUFF: its
        // `parameters.bonusDamage` is 0 with both grip multipliers 0, so it swings
        // nothing. Resolving it as a weapon hit gave the caster a free phantom
        // attack on every cast.
        AbilityTag::Maneuver if ability_uuid == uuid_reckless_fury() => {
            out.extend(apply_reckless_fury(combat, sender, level, now));
        }
        AbilityTag::Maneuver => {
            let mut attacker_loadout = combat.fighters[sender].loadout.clone();
            // METTLE applies to MANEUVERS — and only to the maneuver's own flat bonus.
            // The client folds `1 + Σ EnhanceAbility` into
            // `ManeuverParameters._effectivenessMultiplier` at `ExecuteAbility`, and
            // the only damage consumers are `get_OneHandedMultiplier` /
            // `get_TwoHandedMultiplier`, which `ResolveManeuverDamage@0x1BD2D88` hands
            // to `DistributeBonusDamage@0x1A21844` as the grip factor on
            // `_bonusDamage` (combat-spec ch. 07 §7). The weapon's base damage, the
            // combo factor and mitigation are untouched — so it scales
            // `bonusDamage × grip` here, not the resolved hit.
            //
            // It used to multiply the WHOLE post-mitigation hit (weapon + bonus):
            // weapon 100, bonus 75, Mettle 0.45 gave 253.75 where the client gives
            // 100 + 75 × 1.45 = 208.75.
            let mettle = {
                let f = &combat.fighters[sender];
                f.loadout
                    .perks
                    .ability_multiplier(super::perks::fighter_health_is_critical(f))
            };
            // §5 PIERCING. Both of these ratings are ALREADY consumed by the damage
            // pipeline — `armor_piercing_rating` is subtracted from the defender's armor
            // (`damage.rs`, the armor stage) and `elem_resist_piercing_rating` feeds
            // `resistance_rating_against`. Nothing ever SET them from an ability, so
            // Skullcrusher's 225.00 armor pierce and PiercingStrikes' 20.88 elemental
            // pierce did nothing at all.
            //
            // Applied to a CLONE for this one cast: no persistent state on the fighter,
            // so a maneuver cannot leak its piercing into the next auto-attack.
            //
            // These are FLAT RATINGS despite `armor_piercing_percent`'s name — the
            // shipped values are 225.00 and 60.00, and a percentage reading would make
            // Skullcrusher pierce 22,500% of armor.
            if let Some(r) = super::gamedata::ability_rank_clamped(ability_uuid, level as u16) {
                if let Some(ap) = r.armor_piercing_percent() {
                    attacker_loadout.armor_piercing_rating += ap;
                }
                if let Some(erp) = r.elemental_resistance_piercing() {
                    attacker_loadout.elem_resist_piercing_rating += erp;
                }
                if let Some(bp) = r.block_piercing_percent() {
                    attacker_loadout.block_piercing_rating += bp;
                }
                if let Some(ebp) = r.elemental_block_piercing() {
                    attacker_loadout.elem_block_piercing_rating += ebp;
                }
                // §6 THE MANEUVER'S OWN BONUS DAMAGE. Every maneuver rank ships
                // `parameters.bonusDamage` with a one-/two-handed multiplier, and
                // nothing read them — which is why all 17 maneuvers resolved to an
                // identical plain weapon hit regardless of which one was cast.
                //
                // Grip follows the same rule as the weapon's own base damage:
                // `two_handed = !has_shield` (see `loadout::base_damage_in_hand`).
                //
                // The three authored families (see ability-spec):
                //   power-attack  1H 0.5 / 2H 1.0 — the bonus is the TWO-handed figure
                //   quick-strikes 1H 1.0 / 2H 0.0 — two-handed gets no bonus at all
                //   bashes/dodges 1.0 / 1.0
                // Reckless Fury ships 0/0 because it is a buff that swings nothing.
                attacker_loadout.maneuver_bonus_damage +=
                    maneuver_bonus_damage(&r, !attacker_loadout.has_shield) * mettle;
                if mettle != 1.0 {
                    debug!("combat: slot {sender} maneuver bonus scaled x{mettle:.2} by Mettle");
                }
                // Venom Strikes' `_poisonEffectIncrease` (0.08 → ×1.08 poison).
                if let Some(inc) = r.get(super::gamedata::AbilityField::PoisonEffectIncrease) {
                    if inc > 0.0 {
                        attacker_loadout.poison_effect_multiplier = 1.0 + inc;
                    }
                }
            }
            // Middle is not part of a Left/Right chain, so it resets the combo — the
            // same rule `resolve_swing_with_side` applies to a Middle swing.
            combat.fighters[sender].reset_combo();
            // Report #107: the SOURCE, not the damage. A maneuver deals weapon
            // damage, so this arm reasonably reached for `DamageSource::Attack` — but
            // that is a pair retail never puts on the wire.
            //
            // Measured over 168 decoded retail op50 `ReceiveDamage` messages in the
            // stored captures, `(propId 6 = source, propId 10 = activeSide)`:
            //
            //   source 1 Attack          side 2 Left (15) / 3 Right (21) — NEVER Middle
            //   source 3 WeaponManeuver  side 1 Middle (13) — only ever Middle
            //   source 2 Spell           side 1 Middle (13) / 0 None (2)
            //   source 4 StatusEffect    side 0 (73)
            //
            // We were sending `(1, Middle)`, which occurs **0 times in 168**. The
            // client picks its hit and death reaction off this byte, which is why the
            // reporter saw an opponent "simply collapse to the ground as if he
            // fainted" for some kills and ragdoll properly for others: a maneuver kill
            // was arriving labelled as a plain swing with a side a swing cannot have.
            //
            // Shield bashes route through this same arm (there is no shield-specific
            // `AbilityTag`), and `ShieldManeuver (11)` almost certainly belongs to
            // them — but 11 appears in **none** of the 168, so there is no measurement
            // behind it and it is deliberately not guessed here.
            let resolved = RetailDamageModel.resolve_attack(
                &attacker_loadout,
                &combat.fighters[target_slot],
                DamageSource::WeaponManeuver,
                ActiveSide::Middle,
                1.0,
                0,
                now,
            );
            info!(
                "combat: slot {sender} maneuver {} → weapon damage {:.1} (Middle)",
                ability_uuid, resolved.total,
            );
            last_hit_total = resolved.total;
            target_blocked = resolved.blocked;
            out.extend(emit_damage(combat, sender, target_slot, &resolved, now));
        }
        AbilityTag::Paralyze | AbilityTag::Damage | AbilityTag::Generic
            if !super::damage::ships_damage(ability_uuid, level) =>
        {
            // This ability ships neither `_damage` nor `_damagePerSecond`. Resolving it
            // as a hit yields exactly 0.0, and `emit_damage` has no zero guard — it puts
            // an op50 on the wire addressed to the TARGET, so the opponent wears a
            // floating `0` for a buff that was cast on the caster.
            //
            // Fall through to `apply_shipped_effects` below, which every arm reaches:
            // whatever defensive or control fields the rank DOES ship still apply. The
            // cast keeps its cost and cooldown. It simply stops pretending to be a hit.
            debug!(
                "combat: slot {sender} ability {ability_uuid} (tag {tag:?}) ships no \
                 damage number — no op50 emitted (buff or mis-tagged cast)",
            );
        }
        AbilityTag::Paralyze | AbilityTag::Damage | AbilityTag::Generic => {
            let caster = super::perks::CasterPerks {
                magicka_full: magicka_full_at_cast,
                ..super::perks::CasterPerks::of(&combat.fighters[sender])
            };
            let resolved = RetailDamageModel.resolve_ability(
                ability_uuid,
                level,
                &caster,
                &combat.fighters[target_slot],
                ActiveSide::Middle,
                now,
            );
            last_hit_total = resolved.total;
            target_blocked = resolved.blocked;
            out.extend(emit_damage(combat, sender, target_slot, &resolved, now));
            // THUNDERSTORM is not a DoT — it ships `_numberOfBolts` (3) over a
            // `_duration` (9 s) and a per-BOLT `_damage`, so `channel_ticks` (which
            // keys off `_damagePerSecond`) never saw it and it landed as one
            // immediate hit. Schedule the remaining bolts at duration/bolts.
            let bolts = super::gamedata::ability_rank_clamped(ability_uuid, level as u16)
                .and_then(|r| {
                    let n = r.get(super::gamedata::AbilityField::NumberOfBolts)?;
                    let span = r.duration()?;
                    (n >= 2.0 && span > 0.0).then(|| (n as u32, span / n))
                });
            if let Some((n_bolts, interval)) = bolts {
                combat.channels.push(super::state::ActiveChannel {
                    caster_slot: sender,
                    target_slot,
                    ability_uuid: ability_uuid.to_string(),
                    ability_level: level,
                    remaining_ticks: n_bolts - 1,
                    magicka_full_at_cast,
                    next_tick_at: now + Duration::from_secs_f32(interval),
                    interval_secs: interval,
                });
                info!(
                    "combat: slot {sender} THUNDERSTORM {n_bolts} bolts, one every \
                     {interval:.2}s ({ability_uuid})"
                );
            }
            // A CHANNELLED spell just emitted tick 1 of many. Schedule the rest;
            // `apply_channel_ticks` delivers them on the shipped PvP tick.
            if let Some(total_ticks) = super::damage::channel_ticks(ability_uuid, level) {
                if total_ticks > 1 {
                    combat.channels.push(super::state::ActiveChannel {
                        caster_slot: sender,
                        target_slot,
                        ability_uuid: ability_uuid.to_string(),
                        ability_level: level,
                        remaining_ticks: total_ticks - 1,
                        magicka_full_at_cast,
                        next_tick_at: now
                            + Duration::from_secs_f32(super::damage::CHANNEL_TICK_INTERVAL_SECS),
                        interval_secs: super::damage::CHANNEL_TICK_INTERVAL_SECS,
                    });
                }
            }
            // A landed Paralyze also carries its own paralyse threshold + duration
            // (`_damageToCauseParalyze` / `_duration`), applied by
            // `apply_status_conditioning` via the caster's `paralyze_rank`.
            if tag == AbilityTag::Paralyze {
                // Threshold is checked against the Poison THIS CAST actually landed
                // (post-negation — `resolved.components` is what survived Ward/Absorb),
                // not the sliding window. See `try_paralyze`.
                let cast_poison: f32 = resolved
                    .components
                    .iter()
                    .filter(|(t, _)| *t == super::state::DamageType::Poison)
                    .map(|(_, v)| *v)
                    .sum();
                out.extend(try_paralyze(
                    combat, sender, target_slot, level, cast_poison, target_absorbing, now,
                ));
            }
            // `_damageToCauseStagger` used to be handled HERE, inside this arm. It is
            // now in `apply_shipped_effects` below, which every arm reaches — the two
            // abilities that actually ship the field are maneuvers and could never
            // get here. See the note there.
        }
    }

    // Whatever DEFENSIVE or CONTROL fields this rank ships, applied from the data
    // rather than from the ability's name. Seven abilities used to spend a resource
    // and do nothing because these fields were read by no code.
    out.extend(apply_shipped_effects(
        combat, sender, target_slot, ability_uuid, level, last_hit_total, target_blocked,
        target_absorbing, now,
    ));
    out
}

/// The authored first phase before an ability's impact lands, from shipped rank data.
///
/// `channelDuration` is the wind-up the client animates: Ice Spike 1.12 s,
/// Paralyze 1.5 s, Poison Cloud 1.3 s, Fireball 0.9 s, Lightning Bolt 0.5 s.
/// Frostbite ships none and lands immediately, which is correct — it is a
/// channelled stream, not a projectile.
///
/// A shield bash instead ships `_blockDuration` (0.50 s): that is its first, guarding
/// phase. The weapon damage and Staggering Bash's conditional stun land only when the
/// second, striking phase begins. Applying both at button-down is what stunned the
/// opponent while the caster was visibly still in the initial block.
///
/// **Projectile travel is deliberately NOT added.** Ranks also ship a
/// `projectileSpeed` (Ice Spike 10, Fireball 15), but travel time needs a distance
/// between the two fighters and the arena has no positional model here. Inventing
/// a distance would be inventing a number; the cast time is the part the data
/// actually gives us.
fn ability_impact_delay(ability_uuid: &str, level: u8) -> Duration {
    let secs = super::gamedata::ability_rank_clamped(ability_uuid, level as u16)
        .map(|r| {
            // `_delayDuration` is the spell's OWN delay and is ADDITIVE to the channel
            // — they are separate fields, not alternatives. Delayed Lightning Bolt
            // ships channelDuration 1.3 + delayDuration 4.0, i.e. 5.3 s to impact;
            // ignoring the delay landed it after ~1.3 s and made it indistinguishable
            // from the ordinary Lightning Bolt it is supposed to trade time for.
            let channel = r.channel_duration().unwrap_or(0.0)
                + r.get(super::gamedata::AbilityField::DelayDuration).unwrap_or(0.0);
            channel.max(r.block_duration().unwrap_or(0.0))
        })
        .unwrap_or(0.0);
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f32(secs)
    } else {
        Duration::ZERO
    }
}

/// Begin the guarding half of a shield-bash maneuver at cast time.
///
/// This deliberately lives outside [`apply_shipped_effects`], which runs at impact.
/// A bash's `_blockDuration` and any `_damageReduction` protect the caster during the
/// wind-up; starting them after the delayed strike would reverse the two phases.
fn begin_ability_guard(
    combat: &mut MatchCombat,
    caster: usize,
    ability_uuid: &str,
    level: u8,
    now: Instant,
) {
    let Some(r) = super::gamedata::ability_rank_clamped(ability_uuid, level.max(1) as u16) else {
        return;
    };
    let Some(window) = r.block_duration() else {
        return;
    };
    if !window.is_finite()
        || window <= 0.0
        || caster >= combat.fighters.len()
        || combat.fighters[caster].is_dead()
    {
        return;
    }

    let until = now + Duration::from_secs_f32(window);
    let f = &mut combat.fighters[caster];
    f.set_actor_state(ActorStateType::Blocking, now);
    f.blocking_side = ActiveSide::Middle;
    f.blocking_until = Some(until);
    f.block_raised_at = Some(now);

    if let Some(reduction) = r.damage_reduction().filter(|v| v.is_finite() && *v > 0.0) {
        use super::state::DamageType;
        for ty in [
            DamageType::Slashing,
            DamageType::Cleaving,
            DamageType::Bashing,
            DamageType::Fire,
            DamageType::Frost,
            DamageType::Shock,
            DamageType::Poison,
        ] {
            f.transient_resistances.push((ty, reduction, until));
        }
        // …and REDIRECT it. `_damageReduction` caps damage "absorbed AND redirected";
        // only the absorb half existed, which made Reflecting Bash strictly worse than
        // a plain Shield Bash — the same guard, less damage, and no reflection.
        //
        // Verified against the shipped table: `_damageReduction` is carried by EXACTLY
        // two abilities — ReflectingBash (110.67) and the enemy-only ShieldOfMania
        // (50.11), which is its AI analogue. Shield Bash, Harrying Bash and
        // Staggering Bash ship none, so they guard without reflecting, which is the
        // distinction between them.
        f.reflect_until = Some(until);
        f.reflect_remaining = reduction;
        info!(
            "combat: slot {caster} bash reduction {reduction:.1} for {window:.2}s ({ability_uuid})"
        );
    }
    info!("combat: slot {caster} bash guard UP for {window:.2}s ({ability_uuid})");
}

/// Deliver **Echo Weapon** echoes whose `_weaponDelay` has elapsed.
///
/// An echo is a flat follow-up, not a re-swing: it carries the spell's per-weapon-class
/// `_bonusDamages` and nothing else — no combo, no charge, no block interaction. It is
/// therefore emitted directly rather than routed back through the swing resolver,
/// which would re-apply the whole multiplier chain to it.
fn land_due_echoes(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    if combat.pending_echoes.is_empty() {
        return Vec::new();
    }
    let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut combat.pending_echoes)
        .into_iter()
        .partition(|e| now >= e.due);
    combat.pending_echoes = waiting;

    let mut out = Vec::new();
    for e in due {
        if e.sender >= combat.fighters.len() || e.target >= combat.fighters.len() {
            continue;
        }
        // A round can end while an echo is in flight; landing it afterwards would
        // deal damage into the next round (the same rule `land_due_hits` applies).
        if !matches!(combat.phase, FlowState::StateTimeout) {
            continue;
        }
        if combat.fighters[e.target].is_dead() || combat.fighters[e.sender].is_dead() {
            continue;
        }
        combat.fighters[e.target].take_damage_at(e.damage.round().max(0.0) as u32, now);
        let msg = {
            let hit = &combat.fighters[e.target];
            let other = &combat.fighters[e.sender];
            messages::receive_damage(
                hit.net_object_id,
                NetObjectType::Avatar as u8,
                hit.packed_stats(),
                other.packed_stats(),
                super::state::DamageSource::Spell,
                super::damage::flags::SHOW_DAMAGE
                    | super::damage::flags::HAS_ATTACKER
                    | hit.optimal_block_flag(now),
                e.damage,
                0,
                ActiveSide::Middle,
                super::state::DamageType::None,
                &[(super::state::DamageType::Health, e.damage)],
            )
        };
        info!(
            "combat: slot {} ECHO landed {:.1} on slot {}",
            e.sender, e.damage, e.target
        );
        for v in 0..combat.fighters.len() {
            out.push((v, msg.clone()));
        }
        if combat.fighters[e.target].is_dead() {
            out.extend(on_round_ending_death(combat, e.sender, now));
        }
    }
    out
}

/// Deliver casts whose wind-up has elapsed. Mirrors [`land_due_hits`].
pub(super) fn land_due_impacts(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    if combat.pending_impacts.is_empty() {
        return Vec::new();
    }
    let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut combat.pending_impacts)
        .into_iter()
        .partition(|p| now >= p.due);
    combat.pending_impacts = waiting;

    let mut out = Vec::new();
    for p in due {
        if p.sender >= combat.fighters.len() || p.target >= combat.fighters.len() {
            continue;
        }
        // The same rule `land_due_hits` and `land_due_echoes` apply. `due` was taken
        // out of the queue before this loop, so `on_round_ended` clearing
        // `pending_impacts` cannot stop a second impact due on the SAME tick: without
        // this it landed from a dead caster into a finished round, killed the
        // survivor and ended the round again (CRE-SOAK, `round_ends_once_tests`).
        if !matches!(combat.phase, FlowState::StateTimeout) {
            continue;
        }
        if combat.fighters[p.target].is_dead() || combat.fighters[p.sender].is_dead() {
            continue;
        }
        out.extend(apply_ability_impact(
            combat, p.sender, p.target, &p.ability_uuid, p.level, p.tag,
            p.magicka_full_at_cast, now,
        ));
    }
    out
}

/// Apply the effect fields a rank ships that are not direct damage.
///
/// Driven off the DATA, not the ability's editor name: a rank that carries
/// `_shieldHealth` gets a shield whether it is called FirestormArmor or something
/// added later. Every value here is the shipped number — none is invented.
///
/// Which field goes where, and why:
///
/// * `_maximumAmountDodged` → a **Dodge** negation pool on the CASTER, plus op51
///   `Dodging` (12, already pinned). DodgingStrike / RenewingDodge / AdrenalineDodge /
///   FocusingDodge ship 86-283 absolute points, so it is a flat pool, not a fraction.
/// * `_shieldHealth` → an absorb pool on the CASTER. FirestormArmor / BlizzardArmor /
///   TempestArmor ship 116-158. **No op51 is emitted for these** — the elemental-armor
///   `StatusEffectType` value is not pinned by any capture we hold, and a guessed id is
///   dropped silently by the client, which would look like a working fix that does
///   nothing. The pool is server-authoritative and reduces real damage regardless, so
///   the mechanic works today and the visual follows when the id is known.
///   Their shipped `_damagePerSecond` is **0.00 at every rank**, so there is no
///   retaliation burn to model — these are pure shields. (An earlier plan revision
///   assumed an aura that burns attackers; the data says otherwise.)
/// * `_freezeDuration` / `_paralyzeDuration` → control on the TARGET. FlashFreeze ships
///   both, identical per rank (2.50 s @ R1 → 2.90 s @ R5), so it is one effect duration
///   expressed twice. Emits op51 `Frozen` (5) and `Paralyzed` (9), both pinned, and
///   locks the target's inputs through the existing paralysis path.
///
/// Neither the shield nor the dodge pool ships a `_duration`, so neither gets a timed
/// expiry: the pool lasts until it is consumed. `reset_fighters_for_next_round` clears
/// `negation_pools`, so it cannot outlive the round.
/// Statuses this ability rank CURES on the fighter that used it.
///
/// Driven by the shipped `statuses_to_remove` list, which until now was parsed into
/// `gamedata.rs` and read by nothing at all. Resist Elements carries `[4, 5, 6, 7]`
/// on every one of its 15 ranks (Burning, Frozen, Enervated, Poisoned), while
/// Indomitable Smash carries `[4, 5, 6, 7, 8]` and therefore cures Blind as well.
///
/// The cure applies to the USER. Both abilities that ship the list are self-directed
/// (a protective ward and a shake-it-off smash); nothing ships a list aimed at an
/// opponent, so a target-side cure would be inventing a mechanic.
///
/// Ids outside the modelled range are skipped rather than guessed at.
fn apply_status_cures(
    combat: &mut MatchCombat,
    slot: usize,
    ability_uuid: &str,
    level: u8,
    _now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::StatusEffectType;
    let mut out = Vec::new();
    if slot >= combat.fighters.len() {
        return out;
    }
    let Some(rank) = super::gamedata::ability_rank_clamped(ability_uuid, level.max(1) as u16)
    else {
        return out;
    };
    if rank.statuses_to_remove.is_empty() {
        return out;
    }

    let cured: Vec<StatusEffectType> = rank
        .statuses_to_remove
        .iter()
        .filter_map(|id| match id {
            4 => Some(StatusEffectType::Burning),
            5 => Some(StatusEffectType::Frozen),
            6 => Some(StatusEffectType::Enervated),
            7 => Some(StatusEffectType::Poisoned),
            8 => Some(StatusEffectType::Blind),
            _ => None,
        })
        .collect();

    // Only announce what the fighter actually HAD. Emitting a remove for a status
    // that was never applied would put traffic on the wire retail never sent.
    let held: Vec<StatusEffectType> = {
        let f = &combat.fighters[slot];
        cured
            .iter()
            .copied()
            .filter(|c| f.effects.iter().any(|e| e.effect == *c))
            .collect()
    };
    if held.is_empty() {
        return out;
    }

    combat.fighters[slot]
        .effects
        .retain(|e| !held.contains(&e.effect));
    for status in &held {
        combat.fighters[slot].acknowledge_status_removed(*status);
    }

    let obj = combat.fighters[slot].net_object_id;
    for status in &held {
        info!("combat: slot {slot} CURED {status:?} via {ability_uuid}");
        // Duration on a remove is meaningless; retail carries 0.
        let frame = messages::change_combat_status_effect(obj, false, *status, 0.0);
        for dest in 0..combat.fighters.len() {
            out.push((dest, frame.clone()));
        }
    }
    out
}

fn apply_shipped_effects(
    combat: &mut MatchCombat,
    caster: usize,
    target_slot: usize,
    ability_uuid: &str,
    level: u8,
    // Health damage this cast just dealt — the gate for threshold effects like Blind.
    last_hit_total: f32,
    // Did the target's guard take this cast's hit? Guardbreaker stuns only when it
    // DID; Staggering Bash and Harrying Bash act only when it did NOT.
    target_blocked: bool,
    // Did the target hold an Absorb shield when this cast's hit began? Read BEFORE
    // the hit, because the hit itself may drain the pool: the client's `Absorb`
    // status survives until the step's next `Update`, so the proc still sees it.
    target_absorbing: bool,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageNegationSource, NegationPool, StatusEffectType};
    let mut out = Vec::new();
    let Some(r) = super::gamedata::ability_rank_clamped(ability_uuid, level.max(1) as u16) else {
        return out;
    };
    let viewers = combat.fighters.len();
    // No shipped duration → until consumed. Round reset clears the pools.
    let until_consumed = now + Duration::from_secs(3600);

    // CURES — the shipped `statuses_to_remove` list, applied to the CASTER. This is
    // how Resist Elements puts out a fire that is already burning; see
    // [`apply_status_cures`]. Runs first so the cast's own new statuses, applied
    // below, cannot be cured by the same cast.
    out.extend(apply_status_cures(combat, caster, ability_uuid, level, now));

    // Harrying Bash's `_cooldownIncrease` applies to every active target skill,
    // whether magicka- or stamina-powered. It extends an existing deadline, or
    // starts a delay from now when the skill was ready. Perks are passive.
    //
    // Only on a bash that got through: `AbilityDoHarryingBash$$ApplyAdditionalEffects
    // @0x1e958b8` calls `target.ModifyCooldowns` only when the target is NOT Blocking,
    // NOT Absorbing (17), and took health damage > 0 (03 §5.4, 03-D13). It used to
    // fire on every bash, blocked or not.
    let harry_lands = !target_blocked && !target_absorbing && last_hit_total > 0.0;
    if let Some(secs) = r
        .get(super::gamedata::AbilityField::CooldownIncrease)
        .filter(|secs| secs.is_finite() && *secs > 0.0 && harry_lands)
    {
        if target_slot < viewers && !combat.fighters[target_slot].is_dead() {
            let delay = Duration::from_secs_f32(secs);
            let abilities: Vec<String> = combat.fighters[target_slot]
                .loadout
                .abilities
                .iter()
                .filter(|ability| ability.tag != super::state::AbilityTag::Perk)
                .map(|ability| ability.instance_uuid.clone())
                .collect();
            for target_ability in &abilities {
                let baseline = combat.fighters[target_slot]
                    .cooldowns
                    .get(target_ability)
                    .copied()
                    .unwrap_or(now)
                    .max(now);
                combat.fighters[target_slot]
                    .cooldowns
                    .insert(target_ability.clone(), baseline + delay);
            }
            info!(
                "combat: slot {target_slot} skill cooldowns +{secs:.2}s on {} active skill(s) via {ability_uuid}",
                abilities.len(),
            );
            // Tell the victim's client. `PvpPlayerActor$$ModifyCooldowns@0x1a32ad0` is
            // a no-op, so op83 → `Actor$$ForceModifyCooldowns@0x1c5a0dc` is the only way
            // its HUD learns of the delay; without it the icons stayed ready and every
            // tap hit the cooldown gate in silence (07-D1 / 13-D3, #227). Retail: all 63
            // captured op83 frames go to the harried player, name that player's own
            // avatar, and carry exactly the rank's `_cooldownIncrease`.
            out.push((
                target_slot,
                messages::modify_ability_cooldowns(combat.fighters[target_slot].net_object_id, secs),
            ));
        }
    }

    // **WALL OF FIRE** (editor `Firewall`) — a persistent wall, not a hit. It ships
    // `_damage` **per attack passing through**, `_duration`, and `_selfDamagePercent`.
    // The server resolved it as a single immediate hit and there was no wall.
    // `StatusEffectType::Firewall` (13) already exists, so the client can show it.
    if let Some(dmg) = r.damage() {
        if caster < viewers && super::gamedata::ability(ability_uuid)
            .is_some_and(|a| a.editor_name == "Firewall")
        {
            let secs = r.duration().unwrap_or(0.0);
            let self_pct = r
                .get(super::gamedata::AbilityField::SelfDamagePercent)
                .unwrap_or(0.0);
            let f = &mut combat.fighters[caster];
            f.firewall_until = Some(now + Duration::from_secs_f32(secs));
            f.firewall_damage = dmg;
            f.firewall_self_pct = self_pct;
            let obj = f.net_object_id;
            info!(
                "combat: slot {caster} WALL OF FIRE {dmg:.1}/attack for {secs:.1}s \
                 (self {:.0}%)",
                self_pct * 100.0
            );
            let frame = messages::change_combat_status_effect(
                obj, true, super::state::StatusEffectType::Firewall, secs,
            );
            for v in 0..viewers {
                out.push((v, frame.clone()));
            }
        }
    }

    // **ECHO WEAPON** — for `_duration`, each landed weapon hit is echoed
    // `_weaponDelay` later for a flat per-weapon-class bonus. Nothing was
    // implemented: the spell produced no echoes at all.
    if let Some(delay) = r.get(super::gamedata::AbilityField::WeaponDelay) {
        if caster < viewers {
            let secs = r.duration().unwrap_or(0.0);
            let class_raw = combat.fighters[caster]
                .loadout
                .weapon_template
                .map(|w| w.weapon_class as u8)
                .unwrap_or(0);
            let bonus = r
                .bonus_damages
                .iter()
                .find(|(c, _)| *c == class_raw)
                .or_else(|| r.bonus_damages.iter().find(|(c, _)| *c == 0))
                .map(|(_, v)| *v)
                .unwrap_or(0.0);
            if bonus > 0.0 && secs > 0.0 {
                let f = &mut combat.fighters[caster];
                f.echo_until = Some(now + Duration::from_secs_f32(secs));
                f.echo_bonus = bonus;
                f.echo_delay = delay;
                info!(
                    "combat: slot {caster} ECHO WEAPON +{bonus:.1} after {delay:.2}s, \
                     for {secs:.1}s (weapon class {class_raw})"
                );
            }
        }
    }

    // **MAGICKA SURGE** — `_magickaRegenerationBonus` for `_duration`, then
    // `_noMagickaRegenDuration` of nothing at all. All three were unread: the spell
    // spent its cost and did literally nothing.
    if let Some(bonus) = r.get(super::gamedata::AbilityField::MagickaRegenerationBonus) {
        if bonus > 0.0 && caster < viewers {
            let surge_secs = r.get(super::gamedata::AbilityField::Duration).unwrap_or(0.0);
            let blackout_secs = r
                .get(super::gamedata::AbilityField::NoMagickaRegenDuration)
                .unwrap_or(0.0);
            let f = &mut combat.fighters[caster];
            f.magicka_surge_bonus = bonus;
            f.magicka_surge_until = Some(now + Duration::from_secs_f32(surge_secs));
            // The blackout begins when the surge ENDS, not at cast — it is the price
            // paid afterwards, not a concurrent penalty that would cancel the surge.
            f.no_magicka_regen_until =
                Some(now + Duration::from_secs_f32(surge_secs + blackout_secs));
            info!(
                "combat: slot {caster} MAGICKA SURGE +{bonus:.1}/s for {surge_secs:.1}s, \
                 then {blackout_secs:.1}s of no magicka regen ({ability_uuid})"
            );
        }
    }

    // `_bonusResistance` — a FLAT all-damage resistance granted to the CASTER.
    // Indomitable Smash ships 250 at rank 1 and it was read by nobody, so the
    // maneuver cured conditions and then did nothing else defensively.
    //
    // Lifetime: the ability ships no duration of its own, so it rides the same
    // `ABILITY_USE_MIN_WINDOW_SECS` window the Combat Focus / Willpower perks use for
    // a cast — the committed-animation window. That is a modelling choice, not an
    // authored number, and is called out here rather than buried.
    if let Some(bonus) = r.get(super::gamedata::AbilityField::BonusResistance) {
        if bonus > 0.0 && caster < viewers {
            let window = super::perks::ABILITY_USE_MIN_WINDOW_SECS;
            let expires = now + Duration::from_secs_f32(window);
            combat.fighters[caster]
                .transient_all_resistance
                .push((bonus, expires));
            info!(
                "combat: slot {caster} bonus resistance +{bonus:.1} for {window:.2}s ({ability_uuid})"
            );
        }
    }

    // `_resistanceBonus` + `_resistTypes` — a resistance to SPECIFIC damage types for
    // the caster. Frostbite ships 13.13 against resistTypes [1,2,3] (the three
    // physical tracks) while it channels, and both fields were unread: the spell did
    // its channelled damage and gave the caster none of the protection its
    // description promises.
    if let Some(bonus) = r.get(super::gamedata::AbilityField::ResistanceBonus) {
        if bonus > 0.0 && caster < viewers && !r.resist_types.is_empty() {
            // Lasts as long as the channel it protects.
            let secs = r
                .get(super::gamedata::AbilityField::ChannelMaxLength)
                .or_else(|| r.channel_duration())
                .unwrap_or(super::perks::ABILITY_USE_MIN_WINDOW_SECS);
            let expires = now + Duration::from_secs_f32(secs);
            let mut applied = 0;
            for raw in r.resist_types {
                use super::state::DamageType as DT;
                let ty = match *raw {
                    1 => Some(DT::Slashing),
                    2 => Some(DT::Cleaving),
                    3 => Some(DT::Bashing),
                    4 => Some(DT::Fire),
                    5 => Some(DT::Frost),
                    6 => Some(DT::Shock),
                    7 => Some(DT::Poison),
                    _ => None,
                };
                if let Some(ty) = ty {
                    combat.fighters[caster]
                        .transient_resistances
                        .push((ty, bonus, expires));
                    applied += 1;
                }
            }
            info!(
                "combat: slot {caster} resistance +{bonus:.1} on {applied} type(s) for \
                 {secs:.2}s ({ability_uuid})"
            );
        }
    }

    if let Some(cap) = r.maximum_damage_dodged() {
        if cap > 0.0 && caster < viewers {
            // `_dodgeDuration` is authored at **1.0 s** on all four dodge maneuvers
            // and was ignored: the pool was given the 3600 s "until consumed"
            // placeholder, so a Dodging Strike stayed armed for an hour and ate a hit
            // a round or more later. It is a one-second reactive window, not a
            // banked shield.
            let dodge_secs = r
                .get(super::gamedata::AbilityField::DodgeDuration)
                .filter(|v| *v > 0.0);
            let expires = match dodge_secs {
                Some(secs) => now + Duration::from_secs_f32(secs),
                None => until_consumed,
            };
            combat.fighters[caster].negation_pools.push(NegationPool {
                source: DamageNegationSource::Dodge,
                remaining: cap,
                expires_at: expires,
                // Adrenaline / Renewing / Focusing Dodge pay out only if the dodge
                // actually connects. Absent fields are 0, i.e. a plain Dodging Strike.
                on_absorb_restore: (
                    r.get(super::gamedata::AbilityField::MaximumHealthRestored).unwrap_or(0.0),
                    r.get(super::gamedata::AbilityField::MaximumMagickaRestored).unwrap_or(0.0),
                    r.get(super::gamedata::AbilityField::MaximumCooldownReduction).unwrap_or(0.0),
                ),
                bypass_types: &[],
                restoration_factor: 0.0,
                absorb_fraction: 1.0,
                elemental_only: false,
                consumes_overflow: false,
            });
            let obj = combat.fighters[caster].net_object_id;
            info!("combat: slot {caster} dodge pool +{cap:.1} ({ability_uuid})");
            // The apply carries the dodge's own duration, not 0.
            //
            // Retail is unambiguous: of 405 captured `Dodging` (12) op51 frames,
            // all 204 APPLIES carry duration 1.0 and all 201 REMOVES carry -0.0.
            // We sent 0.0 on the apply, so the client was told the dodge lasts no
            // time — and since we never send the remove either, its indicator has
            // nothing to clear on.
            //
            // `expires` above already bounds the pool server-side; this is the
            // client being told the same thing.
            let announced = dodge_secs.unwrap_or(0.0);
            let frame = messages::change_combat_status_effect(
                obj, true, StatusEffectType::Dodging, announced,
            );
            for v in 0..viewers {
                out.push((v, frame.clone()));
            }
        }
    }

    if let Some(shield) = r.shield_health() {
        if shield > 0.0 && caster < viewers {
            // `_damageAbsorptionPercent` = 0.50 at EVERY rank on all three storm
            // armors: the shield eats HALF of each hit until its 116-158 pool drains,
            // not the whole hit. Treating it as a full absorber made it twice as strong
            // per hit and drained it twice as fast — the gap this plan recorded as
            // "will not match retail exactly".
            let absorb = r
                .get(super::gamedata::AbilityField::DamageAbsorptionPercent)
                .unwrap_or(1.0);
            combat.fighters[caster].negation_pools.push(NegationPool {
                source: DamageNegationSource::Ward,
                remaining: shield,
                expires_at: until_consumed,
                restoration_factor: 0.0,
                absorb_fraction: absorb,
                on_absorb_restore: (0.0, 0.0, 0.0),
                // Storm-armor shields are not element-scoped and have no overflow
                // clause in their description — only Ward does.
                elemental_only: false,
                consumes_overflow: false,
                // `_vulnerableDamageTypes` — Blizzard Armor ships [Fire]. The ice
                // shield does not stop the element it is weak to. The data names the
                // TYPE and no magnitude, so "does not absorb it" is the faithful
                // reading; a "takes extra fire" multiplier would be invented.
                bypass_types: r.vulnerable_damage_types,
            });
            let obj = combat.fighters[caster].net_object_id;
            info!("combat: slot {caster} storm-armor shield +{shield:.1} ({ability_uuid})");
            // `ElementalStormArmor` = 16 (dump.cs:609812) — ONE shared status for all
            // three spells; the element lives on the ability, not the status.
            // Dump-recovered, NOT capture-confirmed: neither 16 nor Blind=8 appears in
            // the ~60k decrypted frames we hold, because nobody cast them in those
            // sessions. The dump is authoritative for name→value and propId 5 matched it
            // 2,965/2,965 across three sessions, so this is well-founded — but if the
            // shield visual does not show on device, this id is the first thing to check.
            let frame = messages::change_combat_status_effect(
                obj, true, StatusEffectType::ElementalStormArmor, 0.0,
            );
            for v in 0..viewers {
                out.push((v, frame.clone()));
            }
        }
    }

    // `_damageToCauseBlind` → the green fog on the VICTIM when the hit lands hard
    // enough. Exactly parallel to the already-wired `_damageToCauseParalyze`.
    //
    // There is NO server-side mechanic to model: `ActorStateType.StateId` has no blind
    // state (all 29 members read), so the fog — and a burning opponent staying visible
    // through it — is rendered client-side off `Blind` (8). The server sends the
    // status with the ability's duration and tracks that lifetime for removal/cures.
    //
    // Strictly greater, and never through an Absorb shield:
    // `AbilityApplyPoisonSpellDamage$$OnDamage@0x1e8ebb0` (`fcmp; b.le`, then
    // `!HasStatus(Absorb 17)`).
    if let Some(threshold) = r.damage_to_cause_blind() {
        let qualifies = last_hit_total > threshold
            && !target_absorbing
            && target_slot < viewers
            && !combat.fighters[target_slot].is_dead();
        // Already Blind: refresh the server's timer and send nothing. A PvP client
        // adds a SECOND instance for a repeated apply (`PvpAvatar$$AddStatusEffect
        // @0x17933ac` -> `ForceAddStatusEffect`, whose replace path goes through the
        // no-op `PvpPlayerActor$$RemoveStatusEffect@0x1a32aa8`), and the one remove we
        // send later clears only one of them, so the fog would never lift.
        let refreshed = qualifies && {
            let expires = now + Duration::from_secs_f32(r.duration().unwrap_or(0.0).max(0.0));
            match combat.fighters[target_slot]
                .effects
                .iter_mut()
                .find(|e| e.effect == StatusEffectType::Blind && now < e.expires_at)
            {
                Some(e) => {
                    e.expires_at = e.expires_at.max(expires);
                    debug!("combat: slot {target_slot} already Blind — timer refreshed, no re-send");
                    true
                }
                None => false,
            }
        };
        if qualifies && !refreshed {
            let secs = r.duration().unwrap_or(0.0);
            let obj = combat.fighters[target_slot].net_object_id;
            info!(
                "combat status: gsid={} target_slot={target_slot} target={} status=Blind hit={last_hit_total:.1} threshold={threshold:.1} duration={secs:.2}",
                combat.game_session_id,
                combat.fighters[target_slot].loadout.display_name,
            );
            if secs > 0.0 {
                // The op51 duration is presentation metadata, not a self-removing
                // timer. Track Blind like the elemental statuses so the normal
                // lapsed-status diff emits the required op51 remove and so
                // Indomitable Smash's shipped `[4,5,6,7,8]` cure can find it.
                combat.fighters[target_slot].effects.push(super::state::ActiveEffect {
                    effect: StatusEffectType::Blind,
                    damage_type: super::state::DamageType::None,
                    value: 0.0,
                    per_tick_damage: 0.0,
                    expires_at: now + Duration::from_secs_f32(secs),
                    last_tick: now,
                    is_transient_resist: false,
                });
            }
            let frame = messages::change_combat_status_effect(
                obj, true, StatusEffectType::Blind, secs,
            );
            for v in 0..viewers {
                out.push((v, frame.clone()));
            }
        }
    }

    // `_damageToCauseStagger` → stagger the VICTIM when the hit lands hard enough.
    // Structurally identical to the `_damageToCauseBlind` gate directly above, and
    // it lives HERE for a reason (tracker #24).
    //
    // It used to sit inside `resolve_ability_cast`'s `Paralyze | Damage | Generic`
    // arm, whose own comment named "IceSpike, StaggeringBash, Guardbreaker". But
    // only ONE of those three is a spell: `StaggeringBash` and `Guardbreaker` are
    // `AbilityKind::Maneuver` → `AbilityTag::Maneuver`, and the Maneuver arm ends
    // just before that block. So the stun fix of 2026-08-04 landed in the one arm
    // the two bashes cannot reach, and no maneuver could ever stagger anything.
    // Captured proof: gmid 51 fired 4x and 21x across the reporter's two sessions,
    // every instance targeting him, and `Staggered` was never sent once in either
    // direction in either session.
    //
    // Data (all 706 shipped ability ranks): exactly three abilities carry
    // `_damageToCauseStagger` — StaggeringBash (Maneuver, threshold 1.0 at every
    // one of its 13 ranks, `_stunDuration` 1.30…2.50 s), Guardbreaker (Maneuver,
    // same shape), and IceSpike (Spell, threshold 70.19…227.06). The first two are
    // what this move unblocks; IceSpike behaves exactly as before.
    //
    // `last_hit_total > 0.0` as well as the threshold: a threshold of 1.0 already
    // implies a landed hit, but a buff arm (Ward / Absorb / ResistElements / Perk)
    // sets `last_hit_total = 0.0`, and a future rank shipping threshold 0.0 must
    // not be able to stagger from a self-buff that never touched the target.
    //
    // **PER-ABILITY BLOCK CONDITION (tracker #31).** The 2026-08-17 move above applied
    // ONE uniform "damage ≥ threshold → stagger" rule to all three carriers. The
    // shipped descriptions say the two maneuvers are OPPOSITES, and both ship
    // `damageToCauseStagger: 1` at every rank, so the uniform rule was wrong for each
    // of them in a different direction:
    //
    // * `Ability.Maneuver.Guardbreaker.Description` — *"This Power Attack deals {0}
    //   extra damage … and stuns a target that **blocks** it."*
    // * `Ability.Maneuver.StaggeringBash.Description` — *"This Shield Bash deals {0}
    //   extra bashing damage and stuns a target that **does not block** it."*
    // * `Ability.Spell.IceSpike.Description` — *"Enemies that suffer more than {1}
    //   damage are stunned."* — a pure damage threshold, no block condition. Its
    //   behaviour is unchanged.
    //
    // Keyed on `editor_name`, not on a data field, because retail encodes the
    // condition in CODE and not in data: `AbilityDoGuardbreaker`
    // (`dump.cs:603647`) and `AbilityDoStaggeringBash` (`:604357`) are distinct
    // classes, each overriding `ApplyAdditionalEffects` with its own body, and their
    // shipped rank rows are otherwise identical (`damageToCauseStagger` 1.0,
    // `stunDuration` 1.30 → 2.50). There is no field that separates them.
    let block_condition_met = match super::gamedata::ability(ability_uuid)
        .map(|a| a.editor_name)
    {
        Some("Guardbreaker") => target_blocked,
        Some("StaggeringBash") => !target_blocked,
        _ => true,
    };
    if let Some(threshold) = r.damage_to_cause_stagger() {
        // Strictly greater, and never through an Absorb shield — Ice Spike
        // (`AbilityApplyIceSpikeDamage$$OnDamage@0x1e8ea14`), Staggering Bash
        // (`AbilityDoStaggeringBash$$ApplyAdditionalEffects@0x1e9a038`) and
        // Guardbreaker (`AbilityDoGuardbreaker$$ApplyAdditionalEffects@0x1e956bc`).
        if last_hit_total > 0.0
            && last_hit_total > threshold
            && !target_absorbing
            && block_condition_met
            && target_slot < viewers
            && !combat.fighters[target_slot].is_dead()
        {
            // Prefer the rank's OWN `_stunDuration` over the generic
            // `baseStaggerDuration` — StaggeringBash/Guardbreaker 1.30 s @ R1 rising
            // to 2.50 s, IceSpike 1.20 s.
            let secs = r
                .stun_duration()
                .unwrap_or(super::state::BASE_STAGGER_DURATION_SECS);
            // A refused stagger (Fury, paralysis) is not announced.
            if combat.fighters[target_slot].apply_stagger_for(now, secs) {
                let obj = combat.fighters[target_slot].net_object_id;
                info!(
                    "combat: slot {target_slot} STAGGERED {secs:.2}s \
                     (hit {last_hit_total:.1} > {threshold:.1}, {ability_uuid})"
                );
                let frame = messages::change_combat_status_effect(
                    obj, true, StatusEffectType::Staggered, secs,
                );
                for v in 0..viewers {
                    out.push((v, frame.clone()));
                }
            }
        }
    }

    if let Some(secs) = r.freeze_duration().or_else(|| r.paralyze_duration()) {
        if secs > 0.0 && target_slot < viewers && !combat.fighters[target_slot].is_dead() {
            let f = &mut combat.fighters[target_slot];
            f.paralyze_secs = secs;
            f.set_actor_state(ActorStateType::Paralyzed, now);
            f.clear_scheduled_states();
            f.blocking_until = None;
            // Frozen (5) is announced with the paralysis, but nothing tracked it, so its
            // op51 remove was never sent and the PvP client kept FlashFreeze's local
            // Slow and Stamina-regen block for the rest of the round (08 §8). Paralyzed
            // is removed by `reconcile_paralysis`; Frozen now lapses at the same instant.
            f.status_timers.retain(|(st, _)| *st != StatusEffectType::Frozen);
            f.status_timers
                .push((StatusEffectType::Frozen, now + Duration::from_secs_f32(secs)));
            let obj = f.net_object_id;
            info!("combat: slot {target_slot} FROZEN + PARALYZED {secs:.2}s ({ability_uuid})");
            for st in [StatusEffectType::Frozen, StatusEffectType::Paralyzed] {
                let frame = messages::change_combat_status_effect(obj, true, st, secs);
                for v in 0..viewers {
                    out.push((v, frame.clone()));
                }
            }
        }
    }
    out
}

/// Land `Paralyzed` on `target_slot` when the caster's Paralyze rank says the hit is
/// strong enough. The threshold is the **absolute** shipped `_damageToCauseParalyze`
/// (32.7 @ R1), and the lock lasts the rank's own `_duration` (2.0 s @ R1). [Phase 3.9]
///
/// `cast_poison` is the Poison damage THIS Paralyze cast actually delivered, AFTER
/// negation. It used to be `recent_element_damage(Poison)` — every poison point the
/// target had taken in the 5 s window, from any source. That was wrong twice over:
/// a Paralyze fully eaten by a Ward still paralysed (it contributed 0 damage but the
/// window was already over threshold), and a Poison Cloud ticking in the background
/// could arm someone else's Paralyze. Both make the lock land when the spell that is
/// supposed to cause it did nothing.
fn try_paralyze(
    combat: &mut MatchCombat,
    _caster: usize,
    target_slot: usize,
    rank: u8,
    cast_poison: f32,
    target_absorbing: bool,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{ActorStateType, StatusEffectType};
    let mut out = Vec::new();
    if !combat.fighters[target_slot].can_be_paralyzed
        || combat.fighters[target_slot].actor_state() == ActorStateType::Paralyzed
    {
        return out;
    }
    // Strictly greater, and never through an Absorb shield:
    // `AbilityApplyPoisonSpellDamage$$OnDamage@0x1e8ebb0` (`fcmp s0, s1; b.le`, then
    // `!HasStatus(Absorb 17)`).
    let threshold = super::state::paralyze_damage_threshold(rank);
    if cast_poison <= threshold || target_absorbing {
        return out;
    }
    let secs = super::state::paralyze_duration_secs(rank);
    let f = &mut combat.fighters[target_slot];
    f.set_actor_state(ActorStateType::Paralyzed, now);
    f.clear_scheduled_states();
    f.blocking_until = None;
    // The duration the op51 below announces is the duration the server must hold
    // the lock for. Without this the lock ran on whatever `paralyze_secs` already
    // held — rank 1's 2.0 s by default, or a previous FlashFreeze's value — so a
    // rank-12 paralysis froze the victim for 2.0 s while telling the client 3.1 s,
    // and `reconcile_paralysis` released them (and emitted the op51 remove) a second
    // early. That early release is what makes the freeze read as weaker than retail.
    f.paralyze_secs = secs;
    let obj = f.net_object_id;
    info!(
        "combat status: gsid={} target_slot={target_slot} target={} status=Paralyzed cast_poison={cast_poison:.1} threshold={threshold:.1} duration={secs}",
        combat.game_session_id,
        combat.fighters[target_slot].loadout.display_name,
    );
    let frame = messages::change_combat_status_effect(obj, true, StatusEffectType::Paralyzed, secs);
    for slot in 0..combat.fighters.len() {
        out.push((slot, frame.clone()));
    }
    out
}

/// Apply a resolved hit: drain negation, decrement the target (unless wholly negated),
/// record elemental conditioning + land status effects, build the `ReceiveDamage` (or
/// `DamageNegated`) for both players, and end the match if the target died.
fn emit_damage(
    combat: &mut MatchCombat,
    attacker_slot: usize,
    target_slot: usize,
    resolved: &ResolvedDamage,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();

    // Finish the mitigation pipeline: drain the DEFENDER's negation pools (Ward/Absorb/
    // Dodge) against this hit's components (mutates the pool, so it runs HERE, not in the
    // read-only damage model). Work on a local copy of the components so the wire frame
    // reflects the post-negation per-type damage. [status-resistance-spec §4]
    let mut components = resolved.components.clone();
    let neg = combat.fighters[target_slot].apply_negation_pools(&mut components);
    let total: f32 = components
        .iter()
        .filter(|(t, _)| super::damage::is_health_type(*t))
        .map(|(_, v)| *v)
        .sum();

    // Whole hit eaten by a Ward/Absorb pool → emit DamageNegated(66), apply the Absorb
    // heal-back, and DO NOT reduce HP (the hit dealt 0). [status-resistance-spec §4]
    // Pool restorations are paid whenever a pool ABSORBED something, whether or not
    // it swallowed the hit whole. The heal used to live inside the `negated` branch
    // below, so an Absorb that ate most of a big hit healed nothing, and the dodge
    // restorations (Adrenaline / Renewing / Focusing) would have had the same hole.
    if neg.heal > 0.0 {
        let f = &mut combat.fighters[target_slot];
        f.health = (f.health + neg.heal.round() as u32).min(f.max_health);
    }
    if neg.restore_magicka > 0.0 {
        let f = &mut combat.fighters[target_slot];
        f.magicka = (f.magicka + neg.restore_magicka.round() as u32).min(f.max_magicka);
        info!(
            "combat: slot {target_slot} dodge restored {:.0} magicka",
            neg.restore_magicka
        );
    }
    if neg.restore_cooldown_secs > 0.0 {
        let cut = Duration::from_secs_f32(neg.restore_cooldown_secs);
        let f = &mut combat.fighters[target_slot];
        for until in f.cooldowns.values_mut() {
            *until = until.checked_sub(cut).unwrap_or(*until);
        }
        info!(
            "combat: slot {target_slot} dodge cut {:.1}s off its cooldowns",
            neg.restore_cooldown_secs
        );
        // Focusing Dodge's refund reaches the dodger's HUD only as op83 with a negative
        // amount: `AbilityDoFocusingDodge$$OnDodge@0x1e95050` calls `ModifyCooldowns`,
        // which the PvP player actor ignores (`@0x1a32ad0`) (04-DG8 / 07-D8).
        out.push((
            target_slot,
            messages::modify_ability_cooldowns(
                combat.fighters[target_slot].net_object_id,
                -neg.restore_cooldown_secs,
            ),
        ));
    }

    if neg.negated {
        let defender_obj = combat.fighters[target_slot].net_object_id;
        info!(
            "combat damage: slot {attacker_slot} → slot {target_slot} | source {:?} side {:?} | \
             NEGATED by a pool (heal +{:.0}) → op66 DamageNegated, no HP loss",
            resolved.source, resolved.active_side, neg.heal,
        );
        let frame = messages::damage_negated(defender_obj);
        out.push((target_slot, frame.clone()));
        out.push((attacker_slot, frame));
        return out;
    }

    let hp_before = combat.fighters[target_slot].health;
    let max_hp = combat.fighters[target_slot].max_health;
    // `take_damage_at`, not `take_damage`: Reckless Fury floors the victim at 1 HP
    // for its window ("cannot be killed").
    combat.fighters[target_slot].take_damage_at(total.round().max(0.0) as u32, now);
    // The mirrored Stamina/Magicka tracks come off their pools BEFORE `packed_stats()`
    // is read for the frame, so the bars the client draws match the numbers the same
    // frame reports. [Fighter::drain_mirrored_pools]
    let (drained_stam, drained_mag) = combat.fighters[target_slot].drain_mirrored_pools(&components);
    // RAVAGE — a cut to the victim's MAXIMUM pools, taken per landed swing and given
    // back at the round boundary. Scaled by the share of physical damage the block
    // let through (`block_physical`); a dodged swing resolves no
    // hit and never arrives here. Nothing goes on the wire for it: pools are sent as
    // fractions of max, so the ceiling change is invisible to the bar — which is why
    // the game shows no opponent stamina bar and players count it in their heads.
    let ravage = combat.fighters[attacker_slot].loadout.ravage.clone();
    let (rav_s, rav_m, rav_h) =
        combat.fighters[target_slot].apply_ravage(&ravage, resolved.block_physical);
    // SHIELD ravage fires on the opposite event: "on a blocked attack or Shield Bash".
    // The defender's shield ravages whoever swung into the guard, so it is applied to
    // the ATTACKER, and only when the guard actually took the hit (`blocked`), at
    // full whatever the block let through.
    let (sr_s, sr_m, sr_h) = if resolved.blocked {
        let shield = combat.fighters[target_slot].loadout.shield_ravage.clone();
        combat.fighters[attacker_slot].apply_ravage(&shield, 1.0)
    } else {
        (0, 0, 0)
    };
    let hp_after = combat.fighters[target_slot].health;
    // Per-hit damage-vs-maxHP ratio (info-level so the ghost-verify on the box shows the
    // before→after HP without RUST_LOG=debug). NOTE: the 25% one-shot clamp is GONE for
    // arena — deep-combo hits are *earned* and can legitimately be large (§4.5).
    let pct = if max_hp > 0 { 100.0 * total / max_hp as f32 } else { 0.0 };
    let dealt = hp_before.saturating_sub(hp_after);
    info!(
        "combat event: gsid={} attacker_slot={attacker_slot} attacker={} target_slot={target_slot} target={} source={:?} side={:?} components={components:?} total={total:.1} pct_max_hp={pct:.1} hp={hp_before}->{hp_after} dealt={dealt} drained_stam={drained_stam} drained_mag={drained_mag} ravaged_stam={rav_s} ravaged_mag={rav_m} ravaged_hp={rav_h} shield_ravaged=({sr_s},{sr_m},{sr_h}) max_stam_now={} max_mag_now={}",
        combat.game_session_id,
        combat.fighters[attacker_slot].loadout.display_name,
        combat.fighters[target_slot].loadout.display_name,
        resolved.source,
        resolved.active_side,
        combat.fighters[target_slot].max_stamina,
        combat.fighters[target_slot].max_magicka,
    );

    let msg = {
        let damaged = &combat.fighters[target_slot];
        let attacker = &combat.fighters[attacker_slot];
        messages::receive_damage(
            damaged.net_object_id,
            NetObjectType::Avatar as u8,
            damaged.packed_stats(),
            attacker.packed_stats(),
            resolved.source,
            resolved.flags,
            total,
            // The ATTACKER's current combo depth. This was a hardcoded `0`, so all
            // 5,147 production op50 events reported comboCount 0 regardless of the
            // chain that produced them — which both lies to the client and destroys
            // our own ability to compare a recorded chain against retail, since the
            // depth is the x-axis of every combo-ramp comparison.
            i16::try_from(attacker.combo_count).unwrap_or(i16::MAX),
            resolved.active_side,
            resolved.most_resisted,
            &components,
        )
    };
    out.push((target_slot, msg.clone()));
    out.push((attacker_slot, msg));

    // Elemental conditioning + status land (after the hit resolved): record each
    // POST-NEGATION elemental component into the target's sliding window and check
    // thresholds → op51 ChangeCombatStatusEffect (a condition DoT lands) — including the
    // Paralyze poison→paralyse layering. [status-resistance-spec §5]
    out.extend(apply_status_conditioning(combat, target_slot, &components, now));

    // The DEFENDER's gear hits back. Emitted after the hit that provoked it and
    // before any death check, so a Revenge proc can itself be the killing blow —
    // which is how retail orders it (`op50 blocked` then `op50 src=Revenge`).
    // WALL OF FIRE: an attacker who lands a hit has "passed through" the wall and is
    // burned for its per-attack `_damage`; the caster pays `_selfDamagePercent` of
    // that for standing in their own fire.
    if attacker_slot != target_slot
        && combat.fighters[target_slot].firewall_until.is_some_and(|t| now < t)
    {
        let burn = combat.fighters[target_slot].firewall_damage;
        if burn > 0.0 {
            let self_hit = burn * combat.fighters[target_slot].firewall_self_pct;
            combat.fighters[attacker_slot].take_damage_at(burn.round().max(0.0) as u32, now);
            if self_hit > 0.0 {
                let f = &mut combat.fighters[target_slot];
                // The caster's own fire never kills them outright: floor at 1.
                let cost = self_hit.round().max(0.0) as u32;
                f.health = f.health.saturating_sub(cost).max(1.min(f.health));
            }
            let msg = {
                let hit = &combat.fighters[attacker_slot];
                let other = &combat.fighters[target_slot];
                messages::receive_damage(
                    hit.net_object_id,
                    NetObjectType::Avatar as u8,
                    hit.packed_stats(),
                    other.packed_stats(),
                    super::state::DamageSource::StatusEffect,
                    super::damage::flags::SHOW_DAMAGE | hit.optimal_block_flag(now),
                    burn,
                    0,
                    ActiveSide::None,
                    super::state::DamageType::Fire,
                    &[(super::state::DamageType::Fire, burn)],
                )
            };
            info!(
                "combat: slot {attacker_slot} walked through slot {target_slot}'s WALL OF \
                 FIRE for {burn:.1} (caster self {self_hit:.1})"
            );
            for v in 0..combat.fighters.len() {
                out.push((v, msg.clone()));
            }
        }
    }

    // ECHO WEAPON: the attacker's landed weapon hit is echoed after `_weaponDelay`.
    // Only a real weapon swing echoes — an echo cannot echo itself, and a spell is
    // not a weapon.
    if resolved.source == super::state::DamageSource::Attack
        && combat.fighters[attacker_slot].echo_until.is_some_and(|t| now < t)
    {
        let f = &combat.fighters[attacker_slot];
        let (bonus, delay) = (f.echo_bonus, f.echo_delay);
        if bonus > 0.0 {
            combat.pending_echoes.push(super::state::PendingEcho {
                sender: attacker_slot,
                target: target_slot,
                damage: bonus,
                due: now + Duration::from_secs_f32(delay),
            });
        }
    }

    // REFLECTING BASH: send part of what just landed back at the attacker, capped by
    // the remaining budget. Placed beside Revenge because it is the same shape — a
    // defender dealing damage back outside its own swing — and so shares its frame.
    if attacker_slot != target_slot && combat.fighters[target_slot].reflect_until.is_some_and(|t| now < t) {
        let budget = combat.fighters[target_slot].reflect_remaining;
        let back = total.min(budget).max(0.0);
        if back > 0.0 {
            combat.fighters[target_slot].reflect_remaining -= back;
            combat.fighters[attacker_slot].take_damage_at(back.round().max(0.0) as u32, now);
            let msg = {
                let hit = &combat.fighters[attacker_slot];
                let other = &combat.fighters[target_slot];
                messages::receive_damage(
                    hit.net_object_id,
                    NetObjectType::Avatar as u8,
                    hit.packed_stats(),
                    other.packed_stats(),
                    super::state::DamageSource::Revenge,
                    super::damage::flags::SHOW_DAMAGE
                        | super::damage::flags::HAS_ATTACKER
                        | hit.optimal_block_flag(now),
                    back,
                    0,
                    ActiveSide::None,
                    super::state::DamageType::None,
                    &[(super::state::DamageType::Health, back)],
                )
            };
            info!(
                "combat: slot {target_slot} REFLECTED {back:.1} back at slot {attacker_slot} \
                 ({:.1} of budget left)",
                combat.fighters[target_slot].reflect_remaining
            );
            for v in 0..combat.fighters.len() {
                out.push((v, msg.clone()));
            }
        }
    }

    out.extend(apply_revenge(
        combat,
        target_slot,
        attacker_slot,
        resolved.source,
        &components,
        now,
    ));

    // ONE round end, however many fighters this hit left dead. Both dead (the target
    // died and Reflecting Bash / Revenge killed the attacker) is a double KO, which
    // `on_round_ended` detects from the pools itself; calling it once per corpse
    // recorded the round twice and sent two results (CRE-SOAK).
    let target_dead = combat.fighters[target_slot].is_dead();
    let attacker_dead = combat.fighters[attacker_slot].is_dead();
    if target_dead || attacker_dead {
        let winner = if target_dead { attacker_slot } else { target_slot };
        out.extend(on_round_ending_death(combat, winner, now));
    }
    out
}

/// Elemental retaliation: the fighter who was just hit deals their gear's Revenge
/// damage back at whoever hit them.
///
/// The item text is explicit about the trigger: “Retaliates with up to … points of
/// any one-hit FIRE damage suffered” (and equivalently for Frost/Shock/Poison).
/// Therefore each enchant triggers only when the post-mitigation hit contains a
/// positive component of that same element. The earlier cross-element interpretation
/// came from pairing nearby Revenge frames in busy captures; it contradicted the
/// shipped description and could make a Fire necklace answer a Frost attack.
///
/// NO RECURSION: this emits a damage frame directly rather than re-entering the hit
/// pipeline, so an attacker's own Revenge cannot fire in response to being retaliated
/// against. Two wearers would otherwise ping-pong until one died.
fn apply_revenge(
    combat: &mut MatchCombat,
    defender_slot: usize,
    attacker_slot: usize,
    triggering_source: super::state::DamageSource,
    triggering_components: &[(super::state::DamageType, f32)],
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    if defender_slot == attacker_slot {
        return out;
    }
    // A one-hit Attack, Spell or Maneuver may trigger. A channeled spell/condition is
    // not “one-hit”; captures contain no Revenge after ContinuousSpell or StatusEffect
    // frames. A channeled Frostbite can therefore provoke at most its initial hit,
    // never one retaliation per 0.2 s tick.
    if matches!(
        triggering_source,
        super::state::DamageSource::ContinuousSpell
            | super::state::DamageSource::StatusEffect
            | super::state::DamageSource::Revenge
    ) {
        return out;
    }
    let entries = match combat.fighters.get(defender_slot) {
        Some(f) if !f.loadout.revenge.is_empty() => f.loadout.revenge.clone(),
        _ => return out,
    };

    for (ty, raw) in entries {
        if raw <= 0.0 {
            continue;
        }
        let suffered_same_element = triggering_components
            .iter()
            .any(|(incoming_ty, damage)| *incoming_ty == ty && *damage > 0.0);
        if !suffered_same_element {
            continue;
        }
        // Resistance is the attacker's, and it is what explains the gap between the
        // shipped 137.32 and the 137.21 seen on the wire.
        let resisted = {
            let a = &combat.fighters[attacker_slot];
            // No elemental piercing: that is a property of an ATTACK, and Revenge is
            // gear firing on its own, not a swing the wearer aimed.
            (raw - a.total_resistance_against(ty, 0.0, now)).max(0.0)
        };
        if resisted <= 0.0 {
            continue;
        }
        combat.fighters[attacker_slot].take_damage_at(resisted.round().max(0.0) as u32, now);
        let msg = {
            let hit = &combat.fighters[attacker_slot];
            let other = &combat.fighters[defender_slot];
            messages::receive_damage(
                hit.net_object_id,
                NetObjectType::Avatar as u8,
                hit.packed_stats(),
                other.packed_stats(),
                super::state::DamageSource::Revenge,
                super::damage::flags::SHOW_DAMAGE
                    | super::damage::flags::HAS_ATTACKER
                    | hit.optimal_block_flag(now),
                resisted,
                0,
                ActiveSide::None,
                super::state::DamageType::None,
                &[(ty, resisted)],
            )
        };
        info!(
            "combat event: gsid={} attacker_slot={defender_slot} attacker={} target_slot={attacker_slot} target={} source=Revenge element={ty:?} damage={resisted:.2} trigger_source={triggering_source:?}",
            combat.game_session_id,
            combat.fighters[defender_slot].loadout.display_name,
            combat.fighters[attacker_slot].loadout.display_name,
        );
        for v in 0..combat.fighters.len() {
            out.push((v, msg.clone()));
        }
    }
    out
}

/// `BURNING/FROZEN/ENERVATED/POISONED` DoT duration once landed — the shipped
/// `CombatParameters.elemental_status_data[].duration` (5 s, identical for all four).
/// [Phase 3.8]
const CONDITION_DURATION_SECS: f32 = super::gamedata::combat_params::ELEMENTAL_STATUS_DURATION;

/// The **per-element** `_percentHealthDamage` for an elemental status, straight from
/// `CombatParameters.elemental_status_data`:
///
/// | element | percent_health_damage |
/// |---|---|
/// | Fire | 0.02 |
/// | Frost | **0.0** |
/// | Shock | **0.0** |
/// | Poison | 0.02 |
///
/// **Phase 3.8 correction:** Frost and Shock are *control* statuses — they apply their
/// mirrored Stamina/Magicka drain, not a damage-over-time. Fire and Poison deal the
/// authored 2% as a total over the full condition, split across its scheduled ticks.
fn dot_percent_health(ty: super::state::DamageType) -> f32 {
    use super::gamedata::combat_params as cp;
    use super::state::DamageType;
    let status_type = match ty {
        DamageType::Fire => 4,
        DamageType::Frost => 5,
        DamageType::Shock => 6,
        DamageType::Poison => 7,
        _ => return 0.0,
    };
    cp::elemental_status(status_type)
        .map(|e| e.percent_health_damage)
        .unwrap_or(0.0)
}

/// DoT tick cadence — 1 tick per second (s506 packet timestamps confirm 1s intervals).
const DOT_TICK_INTERVAL: Duration = Duration::from_secs(1);

fn condition_tick_count(duration_secs: f32) -> u32 {
    (duration_secs / DOT_TICK_INTERVAL.as_secs_f32()).round().max(1.0) as u32
}

/// Regen tick cadence. We regen once per second and apply the video-ground-truth per-
/// second rates. A fractional tick (e.g. regen ~31 stamina/s from a 625 pool at L86)
/// is rounded to nearest integer to avoid float drift.
const REGEN_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// In-combat stamina/magicka regen rate as a fraction of the pool per second.
///
/// **Video ground-truth (s293)**: stamina and magicka both recover at ~5 %/s during
/// passive recovery phases (t=50..52 clean window: 5%→10%→15% over 2s).
/// [ground-truth: /tmp/arena-video-groundtruth.md §1; calibration flag]
///
/// PROVENANCE, CORRECTED (tracker #53, 2026-08-22). This comment used to say the
/// rates "are CDN `[ExcelVariable]` (`PlayerStats._staminaRegenRate` /
/// `_magickaRegenRate`)" and that 5 %/s "supersedes the UESP 4 %/s estimate" —
/// i.e. that the shipped asset field was a slightly-low measurement of THIS
/// number. It is not the same number at all.
///
/// A contributor decompiled the regeneration gate. `Actor` declares
/// `ShouldApplyRegeneration()` virtual, and exactly three classes override it:
/// `EnemyActor` (real logic — base conditions, non-lethal state, gameplay
/// manager), and **`PvpPlayerActor` and `PvpOpponentActor`, which both return
/// false unconditionally.** No conditions, no field reads. Confirmed against
/// `reference/il2cpp/dump.cs` — those are the only three overrides that exist.
///
/// So the client's passive regeneration — the system driven by
/// `ActorInnateStats._staminaRegenRate` / `_magickaRegenRate` / `_healthRegenRate`
/// — is switched off for BOTH actors in arena PvP. Bethesda wrote a dedicated
/// override for each to make sure of it. Whatever `PlayerStats` ships (4 %/s
/// stamina, 4 %/s magicka, 0.5 %/s health) answers a PvE/open-world question and
/// has no bearing here. Do not "reconcile" this constant with it.
///
/// It follows that every pool change a PvP client sees is server-authored, which
/// is what this engine already does.
///
/// **What that leaves genuinely open.** Two measurements of retail remain, and
/// they now provably measure the SAME server-driven signal:
///   * video HUD (s293)           — 5 %/s stamina, 5 %/s magicka
///   * captured `packedStats` wire — ~3.03 %/s stamina, ~2.93 %/s magicka
/// They cannot both be right. The wire is the finer instrument (10-bit pool
/// fractions, thousands of samples, versus reading a bar off video frames), but
/// the 5 %/s figure was an explicit owner call from the video and is left in
/// place here rather than changed on my own initiative. Raised with the owner.
///
/// **SET FROM THE WIRE, 2026-08-22, on the owner's call.** The video figure was
/// 5 %/s for both; the captured `packedStats` series says 3.03 %/s stamina and
/// 2.93 %/s magicka. Tracker #53 established that these are measurements of the
/// SAME quantity — `PvpPlayerActor::ShouldApplyRegeneration()` returns false
/// unconditionally, so the client applies no regeneration of its own in PvP and
/// every pool change a player sees is server-authored. Two readings of one
/// signal cannot both be right, and the wire is the finer instrument: 10-bit
/// pool fractions across thousands of samples, against reading a bar off video
/// frames. The owner made the call to take the wire.
///
/// This is a ~40 % nerf to both pools. Expect fights to run longer and stamina
/// management to matter more; if it feels wrong in play, the video number is one
/// line away and the argument for it is above.
const STAMINA_REGEN_RATE_PER_S: f32 = 0.0303;
const MAGICKA_REGEN_RATE_PER_S: f32 = 0.0293;

/// In-combat health regen: **modelled as ZERO — an approximation, not a rule.**
///
/// There is no *baseline* passive HP recovery in a fight: video ground-truth (s293)
/// shows health only changing on hits, and the old UESP-derived 0.5 %/s baseline was
/// wrong for arena PvP. Between rounds `reset_fighters_for_next_round` restores full
/// HP anyway.
///
/// Independently supported since (tracker #53): `PvpPlayerActor::
/// ShouldApplyRegeneration()` returns false unconditionally, so the client never
/// applies `ActorInnateStats._healthRegenRate` in PvP whatever it ships. The
/// 0.5 %/s in the `PlayerStats` asset is an open-world figure, not a PvP one.
///
/// **But health CAN rise mid-round.** A regen perk plus the right rings/armour gives
/// real in-round health recovery. It is rare, and on most builds too slow to matter,
/// which is why a flat zero is a good approximation of the field today — but it is
/// not a law of the game. Two things follow:
///   * do not write "health cannot increase in a round" anywhere. It can.
///   * when a regen build does show up, this becomes a per-fighter rate summed from
///     the perk and the equipped items, not a global constant.
/// [owner, 2026-08-02, correcting a claim this file previously stated as fact]
///
/// `BlockHealthRegen` status suppression is kept — it is what will gate that rate
/// once it is non-zero.
const HEALTH_REGEN_RATE_PER_S: f32 = 0.0;

/// **Phase 3.10 — the invented Ward / Resist-Elements constants are GONE.**
///
/// | deleted | was | shipped (R1) | error |
/// |---|---|---|---|
/// | `WARD_HEALTH_POOL` | 300.0 | `WardRank1._wardHealth` **120.54** | 2.5× |
/// | `WARD_ARMOR_FLAT` | 20.0 | `WardRank1._wardArmor` **67.2** | 3.4× (the other way) |
/// | `WARD_DURATION_SECS` | 60.0 | `WardRank1._wardDuration` **3.0** | 20× |
/// | `RESIST_ELEMENTS_FLAT_AMOUNT` | 50.0 | `ResistElementsRank1._resistanceAmount` **48.54** | ~3 % |
/// | `RESIST_ELEMENTS_DURATION_SECS` | 11.5 | `ResistElementsRank1._resistanceDuration` **10.0** | 15 % |
///
/// All five are now read per-rank from [`super::gamedata`].
///
/// `_wardDuration` (3 s) is a HARD expiry, not the "pool-managed, effectively
/// unbounded" model the 60 s constant implied — a Ward that is not consumed within
/// three seconds is simply gone.
fn ward_params(rank: u8) -> (f32, f32, f32) {
    match super::gamedata::ability_rank_clamped(super::gamedata::ids::WARD, rank.max(1) as u16) {
        Some(r) => (
            r.ward_health().unwrap_or(120.54),
            r.ward_armor().unwrap_or(67.2),
            r.ward_duration().unwrap_or(3.0),
        ),
        None => (120.54, 67.2, 3.0),
    }
}

/// `(resistance_amount, resistance_duration)` for a Resist-Elements rank.
fn resist_elements_params(rank: u8) -> (f32, f32) {
    match super::gamedata::ability_rank_clamped(super::gamedata::ids::RESIST_ELEMENTS, rank.max(1) as u16)
    {
        Some(r) => (
            r.resistance_amount().unwrap_or(48.54),
            r.resistance_duration().unwrap_or(10.0),
        ),
        None => (48.54, 10.0),
    }
}

/// `(maximum_amount_absorbed, restoration_factor, duration)` for an Absorb rank.
fn absorb_params(ability_uuid: &str, rank: u8) -> (f32, f32, f32) {
    use super::gamedata::AbilityField;
    match super::gamedata::ability_rank_clamped(ability_uuid, rank.max(1) as u16) {
        Some(r) => (
            r.maximum_amount_absorbed().unwrap_or(30.83),
            r.get(AbilityField::RestorationFactor).unwrap_or(1.0),
            r.duration().unwrap_or(1.5),
        ),
        None => (30.83, 1.0, 1.5),
    }
}

/// Record this hit's elemental components into the target's sliding `damage_history`
/// window, then run `CheckStatusEffectApplication` per element (§5.2): when accumulated
/// [element] damage crosses the condition threshold, the condition LANDS → emit op51
/// (apply, the source DamageType, the DoT duration). For POISON, a further crossing of
/// the absolute `_damageToCauseParalyze` (gated by `can_be_paralyzed` + the defender's
/// poison resist / Fortify-Poisoned / Ward) lands `Paralyzed` and locks the victim's
/// inputs for the duration. Idempotent within a window (won't re-apply an active
/// condition each tick). [status-resistance-spec §5.5]
fn apply_status_conditioning(
    combat: &mut MatchCombat,
    target_slot: usize,
    components: &[(super::state::DamageType, f32)],
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::damage::is_elemental;
    use super::state::{condition_for_element, DamageType};

    let mut out = Vec::new();
    let target_obj = combat.fighters[target_slot].net_object_id;
    let _ = combat.fighters[target_slot].max_health;

    // Collect this hit's elemental components (post-mitigation) before borrowing mut.
    let elementals: Vec<(DamageType, f32)> = components
        .iter()
        .filter(|(t, v)| is_elemental(*t) && *v > 0.0)
        .map(|(t, v)| (*t, *v))
        .collect();
    if elementals.is_empty() {
        return out;
    }

    for (ty, amount) in &elementals {
        combat.fighters[target_slot].record_element_damage(*ty, *amount, now);
        let Some(condition) = condition_for_element(*ty) else { continue };
        let recent = combat.fighters[target_slot].recent_element_damage(*ty);
        let threshold = combat.fighters[target_slot].condition_threshold(condition);
        if recent >= threshold {
            // The elemental condition lands. Emit op51 apply to both players (the
            // source DamageType = 0 for the elemental four). Idempotent: skip if this
            // condition is already active on the target.
            let already = combat.fighters[target_slot]
                .effects
                .iter()
                .any(|e| e.effect == condition && now < e.expires_at);
            if !already {
                let base_hp = combat.fighters[target_slot].base_max_health();
                // `_percentHealthDamage` is the TOTAL over the status, not a per-tick
                // fraction. It is authored against the same un-cheated character pool
                // as `_healthPercentToCauseStatus`; arena's x3 pacing bar must not
                // triple it. Retail s506's dominant Poison tick (3.87) is the expected
                // order of magnitude, while the old calculation produced 63 per tick
                // at L86.
                let total_dot = dot_percent_health(*ty) * base_hp as f32;
                let per_tick = total_dot / condition_tick_count(CONDITION_DURATION_SECS) as f32;
                combat.fighters[target_slot].effects.push(super::state::ActiveEffect {
                    effect: condition,
                    damage_type: *ty,
                    value: per_tick,
                    per_tick_damage: per_tick,
                    expires_at: now + Duration::from_secs_f32(CONDITION_DURATION_SECS),
                    last_tick: now,
                    is_transient_resist: false,
                });
                let frame = messages::change_combat_status_effect(
                    target_obj, true, condition, CONDITION_DURATION_SECS,
                );
                info!(
                    "combat status: gsid={} target_slot={target_slot} target={} status={condition:?} source_element={ty:?} recent_damage={recent:.1} threshold={threshold:.1} duration={CONDITION_DURATION_SECS} dot_per_tick={per_tick:.2}",
                    combat.game_session_id,
                    combat.fighters[target_slot].loadout.display_name,
                );
                for slot in 0..combat.fighters.len() {
                    out.push((slot, frame.clone()));
                }
            }


            // Frozen is not a second stagger/paralysis status. Retail's shipped
            // `slowStatusMultiplier` is 0.75 and its loading tip says Frozen slows the
            // target and stops stamina regeneration. op51 above drives the client-side
            // frost VFX/slow; `swing_cooldown_for` and `critical_hold_secs_for` mirror
            // that slower weapon timing authoritatively. Stamina suppression is in
            // `apply_regen_tick`. Do not manufacture a Staggered actor state here.
            // PARALYSE (poison only): the absolute poison threshold layered on top —
            // gated by can_be_paralyzed (player) + the defender's poison resist /
            // Fortify-Poisoned / Ward (all already folded into `recent` via mitigation
            // + into `threshold` via Fortify; Ward eats poison so it never accumulates).
            // **Phase 3.9:** the threshold is the shipped, ABSOLUTE
            // `ParalyzeAbility._damageToCauseParalyze` (32.7 @ R1) — not a fraction of
            // max HP — and the lock lasts the rank's own `_duration` (2.0 s @ R1).
            // PARALYSE USED TO BE LAYERED HERE, on any poison hit. It is gone.
            //
            // Paralysis comes from CASTING the Paralyze spell — not from taking poison
            // damage, whether or not the attacker has the spell equipped. `try_paralyze`
            // is called from the Paralyze tag arm on the cast itself and is a complete
            // duplicate of what stood here: same `can_be_paralyzed` gate, same
            // `recent_element_damage(Poison)`, same shipped threshold and duration. It
            // remains the only source, alongside FlashFreeze's own `_paralyzeDuration`.
            //
            // The spec is not evidence against this. "Paralyse = a Poison-damage SPELL
            // + a paralyse threshold. Proven in s506: every Paralyzed(3.1 s) apply is
            // immediately preceded by a big Poisoned(4.89 s) apply"
            // [arena-status-resistance-spec.md §5.4, dump+cap] describes the Paralyze
            // spell's OWN venom crossing its OWN threshold — which is exactly what
            // `try_paralyze` does on the cast. It does not say poison from any other
            // source paralyses, and s506's paralysis came from a cast.
            //
            // What stood here read the ATTACKER's `paralyze_rank` — 0 unless a Paralyze
            // ability is equipped — after which `paralyze_damage_threshold` silently
            // substituted rank 1 via `rank.max(1)`. So a plain poison WEAPON ENCHANT
            // paralysed people, and blocking was no defence: an optimal block zeroes
            // PHYSICAL damage but only rating-reduces elemental, so half the poison
            // still reached `damage_history`. With a 5 s poison window and a 2 s lock
            // the next tick re-paralysed — a stun-lock from behind a raised shield.
            // Reported as "first match I got paralysed from a block".
        }
    }
    out
}

/// Clear a lapsed `Paralyzed` actor-state back to Idle once the paralyse duration
/// (`PARALYZE_DURATION_SECS`) has elapsed since it was applied (`state_entered`) — so a
/// paralysed fighter regains its inputs. No-op for a non-paralysed fighter.
///
/// This used to add: "the client also times the status out via the op51 duration;
/// the un-paralyse op51 *remove* is a cosmetic nicety not emitted here — the apply
/// carried the duration." Both halves were wrong. The client does not time it out,
/// and the remove is not cosmetic: without it the effect renders forever. The
/// remove is now emitted by `emit_status_removals`, which diffs the state this
/// function mutates.
fn reconcile_paralysis(f: &mut super::state::Fighter, now: Instant) {
    use super::state::ActorStateType;
    if f.actor_state() == ActorStateType::Paralyzed
        && now.duration_since(f.state_entered) >= Duration::from_secs_f32(f.paralyze_secs.max(0.1))
    {
        f.set_actor_state(ActorStateType::Idle, now);
    }
    // Phase 3.13: a lapsed stagger also returns the actor to Idle.
    f.reconcile_stagger(now);
}

/// Deliver the due ticks of every in-flight channelled spell.
///
/// Retail streams a `_damagePerSecond` spell as a run of `ReceiveDamage` frames with
/// `DamageSource::ContinuousSpell (8)`, one per [`damage::CHANNEL_TICK_INTERVAL_SECS`]
/// (the shipped `GLOBAL_PVP_TICK_INTERVAL`), for `channelMaxLength` seconds. We used to
/// land the whole shipped total in ONE hit, which was wrong three ways: the damage
/// shape, the stagger interaction (one big hit can cross a stagger threshold a stream
/// never would), and the stamina trajectory, since Frost mirrors 1:1 onto stamina and
/// a lump empties the bar in a single frame.
///
/// Each tick re-enters `resolve_ability`, so block, resistance and the mirrored drain
/// are recomputed against the target's state at that instant rather than frozen at
/// cast time.
///
/// A channel ends early when its target dies or leaves — there is no separate
/// "release" input modelled, so a cast always runs its full `channelMaxLength`.
fn apply_channel_ticks(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    if combat.channels.is_empty() {
        return out;
    }

    let due: Vec<usize> = combat
        .channels
        .iter()
        .enumerate()
        .filter(|(_, c)| now >= c.next_tick_at)
        .map(|(i, _)| i)
        .collect();

    for i in due {
        // A tick below can end the round, and `on_round_ended` clears every channel.
        // The indices in `due` then point into a list that is gone; indexing it
        // panicked the arena thread and restarted the whole server mid-match
        // (resolve.rs:3628, 2026-09-24 12:36, a Wall of Fire kill). Nothing is owed
        // after a round boundary, so stop.
        if i >= combat.channels.len() {
            break;
        }
        let (caster, target, uuid, level, magicka_full_at_cast) = {
            let c = &combat.channels[i];
            (
                c.caster_slot,
                c.target_slot,
                c.ability_uuid.clone(),
                c.ability_level,
                c.magicka_full_at_cast,
            )
        };
        if target >= combat.fighters.len()
            || combat.fighters[target].is_dead()
            || caster >= combat.fighters.len()
            || combat.fighters[caster].is_dead()
        {
            combat.channels[i].remaining_ticks = 0;
            continue;
        }

        // Maximum Power is frozen at cast; Mettle and the caster's gear are re-read
        // live. Built from `CasterPerks::of` with only the one frozen field
        // overridden, rather than field-by-field: hand-building it here is what makes
        // "works on tick 1, nothing after" bugs — a field added to `of()` and
        // forgotten here silently stops applying partway through every channel. EDIR
        // and Fortify both had to be patched in two places because of this.
        let caster_perks = super::perks::CasterPerks {
            magicka_full: magicka_full_at_cast,
            ..super::perks::CasterPerks::of(&combat.fighters[caster])
        };
        let resolved = RetailDamageModel.resolve_ability(
            &uuid,
            level,
            &caster_perks,
            &combat.fighters[target],
            ActiveSide::Middle,
            now,
        );
        out.extend(emit_damage(combat, caster, target, &resolved, now));
        if i >= combat.channels.len() {
            // This tick ended the round (see the guard at the top of the loop).
            break;
        }

        // **CONSUMING INFERNO'S UPKEEP.** `_staminaCostPerSecond` (51.81) and
        // `_healthCostPerSecond` (31.11) are what the spell costs its CASTER for
        // every second it burns — two distinct drains, both unread, so the spell was
        // pure upside: huge channelled damage for a one-off magicka cost.
        //
        // Charged per TICK, scaled by the tick interval, so the per-second figure
        // stays a per-second figure whatever the tick rate is.
        if let Some(rank) = super::gamedata::ability_rank_clamped(&uuid, level as u16) {
            let per_tick = super::damage::CHANNEL_TICK_INTERVAL_SECS;
            let stam = rank
                .get(super::gamedata::AbilityField::StaminaCostPerSecond)
                .unwrap_or(0.0)
                * per_tick;
            let health = rank
                .get(super::gamedata::AbilityField::HealthCostPerSecond)
                .unwrap_or(0.0)
                * per_tick;
            if stam > 0.0 || health > 0.0 {
                let f = &mut combat.fighters[caster];
                if stam > 0.0 {
                    f.stamina = f.stamina.saturating_sub(stam.round().max(0.0) as u32);
                }
                if health > 0.0 {
                    // Never self-kill on upkeep: floor at 1. A channel that killed its
                    // own caster would end the round for the wrong player, and nothing
                    // in the data says the cost is lethal.
                    let cost = health.round().max(0.0) as u32;
                    f.health = f.health.saturating_sub(cost).max(1.min(f.health));
                }
                f.stats_seq = f.stats_seq.wrapping_add(1);
            }
        }

        let c = &mut combat.channels[i];
        c.remaining_ticks = c.remaining_ticks.saturating_sub(1);
        // Advance from the SCHEDULED time, never from `now`. `on_tick` fires a little
        // after the instant a tick was due, and rebasing on the late arrival lets that
        // slack compound: measured, it dropped 4 of 15 ticks outside the channel and
        // stretched a 3.0 s cast to 3.6 s.
        c.next_tick_at += Duration::from_secs_f32(c.interval_secs);
    }

    combat.channels.retain(|c| c.remaining_ticks > 0);
    out
}

/// Deliver continuous gear damage: Ebony Mail's poison, Rimelink's frost.
///
/// Before this, the property's arm in `apply_enchant` did not exist, so the effect
/// fell through `_ => {}`. Ebony Mail's "Does {0} poison damage per second" did
/// nothing in the arena.
///
/// Retail's shape, from the client (libil2cpp RVAs):
/// * `ContinuousDamageBonusInstance.Register` (`0x1D4AC18`) adds `GetContinuousDamage`
///   to the WEARER's `ContinuousAreaEffectDamageSource` list. The effect needs no hit
///   and no range check. It runs against the actor the wearer is fighting.
/// * `CombatManager.ResolveContinuousDamage` (`0x1BD3428`) returns early unless the
///   target is alive. `ResolveContinuousAreaEffectDamage` (`0x1BD34C4`) then builds
///   `sum(rates) x deltaTime` (`ApplyContinuousDamage`, `0x1BD3864`), and if the sum is
///   above 0 runs `target.ResolveDamageTaken(wearer, list, AreaEffect, unblockable:
///   false)` and `ApplyDamage(..., AreaEffect, ActiveSide.None)`.
/// * `GetContinuousDamage` (`0x1D4B3C0`) returns the rate only when the damage type is
///   in `_damageTypes`, the target passes `_damageEnemyGroup`, `_requiredArmor` is
///   worn and the target is not the wearer. Otherwise it returns 0.
///   `ActorFilterComponent.Contains` (`0x1D4B4F8`) maps a PlayerActor to `Player (1)`,
///   which is in Ebony Mail's `[Enemy, Player]`. The retail PvP server's combatants
///   are `ServerActor : PlayerActor`.
///
/// Health is integral, so the fractional part of each tick is carried to the next
/// (`Fighter::continuous_carry`). The wire frame reports the exact tick. The first
/// tick lands one interval after the round goes live, and the schedule advances from
/// the SCHEDULED instant, as `apply_channel_ticks` does.
fn apply_continuous_area_damage(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let interval = Duration::from_secs_f32(super::damage::CONTINUOUS_AREA_TICK_SECS);
    for wearer in 0..combat.fighters.len() {
        if combat.fighters[wearer].loadout.continuous_damage.is_empty() {
            continue;
        }
        if matches!(combat.phase, FlowState::RoundEnd | FlowState::NextState) {
            break;
        }
        let Some(due) = combat.fighters[wearer].continuous_next_tick_at else {
            combat.fighters[wearer].continuous_next_tick_at = Some(now + interval);
            continue;
        };
        if now < due {
            continue;
        }
        // Advance from the scheduled instant. After a long stall (a paused tick loop),
        // rebase instead of bursting a backlog of ticks all at once.
        let next = due + interval;
        combat.fighters[wearer].continuous_next_tick_at = Some(if next + interval < now {
            now + interval
        } else {
            next
        });

        let Some(target) = combat.opponent_of(wearer) else {
            continue;
        };
        if target == wearer
            || combat.fighters[wearer].is_dead()
            || combat.fighters[target].is_dead()
        {
            continue;
        }

        let entries = combat.fighters[wearer].loadout.continuous_damage.clone();
        for (ty, rate) in entries {
            let resolved = super::damage::resolve_continuous_area_tick(
                &combat.fighters[wearer].loadout,
                &combat.fighters[target],
                ty,
                rate,
                now,
            );
            if resolved.total <= 0.0 {
                continue;
            }
            let owed = combat.fighters[wearer].continuous_carry + resolved.total;
            let whole = owed.floor();
            combat.fighters[wearer].continuous_carry = owed - whole;
            let hp_before = combat.fighters[target].health;
            combat.fighters[target].take_damage_at(whole as u32, now);
            // Rimelink's frost mirrors onto stamina like any frost damage ("to Health
            // and Stamina"), so the bars in the frame below are post-drain.
            combat.fighters[target].drain_mirrored_pools(&resolved.components);
            let msg = {
                let hit = &combat.fighters[target];
                let wearing = &combat.fighters[wearer];
                messages::receive_damage(
                    hit.net_object_id,
                    NetObjectType::Avatar as u8,
                    hit.packed_stats(),
                    wearing.packed_stats(),
                    resolved.source,
                    resolved.flags,
                    resolved.total,
                    0,
                    ActiveSide::None,
                    resolved.most_resisted,
                    &resolved.components,
                )
            };
            debug!(
                "combat event: gsid={} attacker_slot={wearer} target_slot={target} source=AreaEffect element={ty:?} damage={:.3} hp={hp_before}->{}",
                combat.game_session_id,
                resolved.total,
                combat.fighters[target].health,
            );
            for v in 0..combat.fighters.len() {
                out.push((v, msg.clone()));
            }
            if combat.fighters[target].is_dead() {
                out.extend(on_round_ending_death(combat, wearer, now));
                return out;
            }
        }
    }
    out
}

/// op51 `ChangeCombatStatusEffect` with `apply = false` for every status that has
/// just lapsed, to both viewers.
///
/// THE BUG THIS FIXES. The engine emitted applies and never a remove — all 15
/// op51 call sites passed `apply = true`. The assumption, written down at
/// `reconcile_paralysis`, was that "the apply carried the duration" so the client
/// would time the effect out itself. It does not. A player high-blocked a bot,
/// saw it stunned, and then watched it swing at him and land hits while still
/// rendered mid-stun: the actor state had returned to Idle on the wire (op39),
/// but the status layer never heard the stun ended.
///
/// Retail sends the remove. Across 2,889 op51 messages in captures s615+s616 the
/// apply/remove counts are ~1:1 for every one of the nineteen effect types seen —
/// Staggered 132/140, Paralyzed 16/17, Frozen 80/83, Blocking 736/756. (Removes
/// slightly lead because the window catches some whose apply preceded it.)
///
/// Driven by diffing [`Fighter::drain_lapsed_statuses`] rather than emitting at
/// each expiry site, because expiry happens in three places and two of them are
/// input handlers with no route to the wire.
fn emit_status_removals(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    for slot in 0..combat.fighters.len() {
        let lapsed = combat.fighters[slot].drain_lapsed_statuses(now);
        if lapsed.is_empty() {
            continue;
        }
        let obj = combat.fighters[slot].net_object_id;
        for status in lapsed {
            debug!("combat: slot {slot} status {status:?} lapsed → op51 remove");
            // Duration on a remove is meaningless; retail carries 0.
            let frame = messages::change_combat_status_effect(obj, false, status, 0.0);
            for dest in 0..combat.fighters.len() {
                out.push((dest, frame.clone()));
            }
        }
    }
    out
}

/// Drive scheduled DoT ticks for all active elemental conditions. Fire and Poison
/// split `_percentHealthDamage × baseMaxHP` across the five authored one-second
/// ticks; Frost and Shock retain zero health damage and provide their control/drain
/// effects instead. A late engine pass catches up every tick due through expiry.
fn apply_dot_ticks(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageSource as DS, StatusEffectType};
    let mut out = Vec::new();

    for slot in 0..combat.fighters.len() {
        // Prune expired transient resistances.
        combat.fighters[slot].prune_transient_resistances(now);

        let opp_slot = combat.fighters[slot].arena_target;
        if combat.fighters[slot].is_dead() {
            continue;
        }

        // Collect every tick due up to the effect's expiry. Advance from the
        // SCHEDULED time rather than rebasing on a late engine tick, so scheduler
        // jitter cannot turn a five-tick effect into four ticks.
        let ticking: Vec<(usize, u32, f32, super::state::DamageType)> = combat.fighters[slot]
            .effects
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if !matches!(
                    e.effect,
                    StatusEffectType::Burning
                        | StatusEffectType::Frozen
                        | StatusEffectType::Enervated
                        | StatusEffectType::Poisoned
                ) {
                    return None;
                }
                let through = now.min(e.expires_at);
                let elapsed = through.checked_duration_since(e.last_tick).unwrap_or_default();
                let due = (elapsed.as_secs_f32() / DOT_TICK_INTERVAL.as_secs_f32()).floor() as u32;
                (due > 0).then_some((i, due, e.per_tick_damage, e.damage_type))
            })
            .collect();

        'effects: for (idx, due, tick_dmg, dmg_type) in ticking {
            combat.fighters[slot].effects[idx].last_tick += DOT_TICK_INTERVAL * due;

            if tick_dmg <= 0.0 {
                continue;
            }

            for _ in 0..due {
                let hp_before = combat.fighters[slot].health;
                let max_hp = combat.fighters[slot].max_health;
                combat.fighters[slot].take_damage_at(tick_dmg.round().max(0.0) as u32, now);
                let hp_after = combat.fighters[slot].health;
                let pct = if max_hp > 0 { 100.0 * tick_dmg / max_hp as f32 } else { 0.0 };
                info!(
                    "combat event: gsid={} target_slot={slot} target={} source=StatusEffect element={dmg_type:?} damage={tick_dmg:.2} pct_max_hp={pct:.2} hp={hp_before}->{hp_after}",
                    combat.game_session_id,
                    combat.fighters[slot].loadout.display_name,
                );

                // Emit ReceiveDamage (DamageSource::StatusEffect) to both players.
                let (defender_stats, attacker_stats) = {
                    let d = &combat.fighters[slot];
                    let a = combat.fighters.get(opp_slot).map(|f| f.packed_stats()).unwrap_or(0);
                    (d.packed_stats(), a)
                };
                let defender_obj = combat.fighters[slot].net_object_id;
                let frame = messages::receive_damage(
                    defender_obj,
                    super::state::NetObjectType::Avatar as u8,
                    defender_stats,
                    attacker_stats,
                    DS::StatusEffect,
                    // No HAS_ATTACKER for DoT. Bit 3 is the defender's optimal-guard
                    // STATE, sent on DoT frames too (03-D19, `ApplyDamage` 0x1bd2a24).
                    super::damage::flags::SHOW_DAMAGE
                        | combat.fighters[slot].optimal_block_flag(now),
                    tick_dmg,
                    0,
                    ActiveSide::None,
                    super::state::DamageType::None,
                    &[(dmg_type, tick_dmg)],
                );
                for dest in 0..combat.fighters.len() {
                    out.push((dest, frame.clone()));
                }

                if combat.fighters[slot].is_dead() {
                    // DoT killed the defender — score the round for the opponent.
                    out.extend(on_round_ending_death(combat, opp_slot, now));
                    break 'effects;
                }
            }
        }
        combat.fighters[slot].effects.retain(|e| now < e.expires_at);
    }
    out
}

/// Apply a Ward cast to `caster_slot`: push a Ward negation pool + optional armor
/// bonus onto the fighter and emit op51 `ChangeCombatStatusEffect` (Ward=15) to
/// both players. The pool drains on incoming elemental hits (existing
/// `apply_negation_pools` infrastructure); when fully drained, op66 DamageNegated
/// is emitted by the normal `emit_damage` path. [arena-status-resistance-spec §4.2]
fn apply_ward(
    combat: &mut MatchCombat,
    caster_slot: usize,
    rank: u8,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageNegationSource, NegationPool, StatusEffectType};
    let mut out = Vec::new();
    if caster_slot >= combat.fighters.len() {
        return out;
    }
    let (ward_health, ward_armor, ward_duration) = ward_params(rank);
    let f = &mut combat.fighters[caster_slot];
    let ward_expires = now + Duration::from_secs_f32(ward_duration);
    // Add the negation pool.
    f.negation_pools.push(NegationPool {
        source: DamageNegationSource::Ward,
        remaining: ward_health,
        expires_at: ward_expires,
        restoration_factor: 0.0, // Ward: pure negation, no heal-back
        absorb_fraction: 1.0,    // Ward swallows a hit whole until exhausted
        on_absorb_restore: (0.0, 0.0, 0.0),
        // `Ability.Spell.Ward.Description`: "negates up to {1} ELEMENTAL damage,
        // plus any EXCESS damage from the attack that destroys it". Ward's physical
        // protection is the Armor Rating pushed below, not this pool.
        elemental_only: true,
        consumes_overflow: true,
        bypass_types: &[],
    });
    // Add transient flat physical armor (subtracted from incoming physical as a
    // transient resistance on the caster — `DamageType::Health` is NOT physical;
    // Slashing/Cleaving/Bashing are. We model ward armor as flat resist on physical
    // types using the transient_resistances mechanism).
    use super::state::DamageType;
    for ty in [DamageType::Slashing, DamageType::Cleaving, DamageType::Bashing] {
        f.transient_resistances.push((ty, ward_armor, ward_expires));
    }
    let target_obj = f.net_object_id;
    info!(
        "combat: slot {caster_slot} WARD r{rank} applied (pool {ward_health:.2}, armor {ward_armor:.2}, duration {ward_duration}s)"
    );
    // op51 apply Ward=15 with the rank's real `_wardDuration` (was 0 = "pool-managed").
    let frame =
        messages::change_combat_status_effect(target_obj, true, StatusEffectType::Ward, ward_duration);
    for slot in 0..combat.fighters.len() {
        out.push((slot, frame.clone()));
    }
    out
}

/// Apply an **Absorb** cast to `caster_slot` (Phase 3.10/3.11): a negation pool of
/// `_maximumAmountAbsorbed` that HEALS the caster back by `_restorationFactor` of
/// whatever it eats, for `_duration` seconds. Emits op51 `Absorb`(17).
fn apply_absorb(
    combat: &mut MatchCombat,
    caster_slot: usize,
    rank: u8,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageNegationSource, NegationPool, StatusEffectType};
    let mut out = Vec::new();
    if caster_slot >= combat.fighters.len() {
        return out;
    }
    // The caster's Absorb ability uuid (Absorb / SiphonLife both map to this tag).
    let uuid = combat.fighters[caster_slot]
        .loadout
        .abilities
        .iter()
        .find(|a| a.tag == super::state::AbilityTag::Absorb)
        .map(|a| a.instance_uuid.clone())
        .unwrap_or_else(|| "4e760726-b012-4b25-bc92-0cd6312d6601".to_string());
    let (amount, restoration, duration) = absorb_params(&uuid, rank);
    let f = &mut combat.fighters[caster_slot];
    f.negation_pools.push(NegationPool {
        source: DamageNegationSource::Absorb,
        remaining: amount,
        expires_at: now + Duration::from_secs_f32(duration),
        restoration_factor: restoration,
        absorb_fraction: 1.0,
        on_absorb_restore: (0.0, 0.0, 0.0),
        elemental_only: false,
        consumes_overflow: false,
        bypass_types: &[],
    });
    let obj = f.net_object_id;
    info!("combat: slot {caster_slot} ABSORB r{rank} applied (pool {amount:.2}, heal ×{restoration}, {duration}s)");
    let frame =
        messages::change_combat_status_effect(obj, true, StatusEffectType::Absorb, duration);
    for slot in 0..combat.fighters.len() {
        out.push((slot, frame.clone()));
    }
    out
}

/// Apply Resist Elements to `caster_slot`: push four transient elemental resistances
/// (Fire/Frost/Shock/Poison) with the rank's shipped duration and emit four op51
/// `ChangeCombatStatusEffect` events (FireResistance=60 … PoisonResistance=63).
/// The flat subtraction is applied AFTER block by `total_resistance_against` in the
/// damage pipeline. [docs/arena-combat-fidelity-iteration.md §Mechanic-3]
fn apply_resist_elements(
    combat: &mut MatchCombat,
    caster_slot: usize,
    rank: u8,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageType, StatusEffectType};
    let mut out = Vec::new();
    if caster_slot >= combat.fighters.len() {
        return out;
    }
    let (resist_amount, resist_duration) = resist_elements_params(rank);
    let expires = now + Duration::from_secs_f32(resist_duration);
    let target_obj = combat.fighters[caster_slot].net_object_id;
    let resist_pairs = [
        (DamageType::Fire, StatusEffectType::FireResistance),
        (DamageType::Frost, StatusEffectType::FrostResistance),
        (DamageType::Shock, StatusEffectType::ShockResistance),
        (DamageType::Poison, StatusEffectType::PoisonResistance),
    ];
    for (dmg_ty, effect_ty) in resist_pairs {
        combat.fighters[caster_slot]
            .transient_resistances
            .push((dmg_ty, resist_amount, expires));
        // So the lapsed-status diff sends this resistance's op51 remove.
        let timers = &mut combat.fighters[caster_slot].status_timers;
        timers.retain(|(st, _)| *st != effect_ty);
        timers.push((effect_ty, expires));
        let frame =
            messages::change_combat_status_effect(target_obj, true, effect_ty, resist_duration);
        for slot in 0..combat.fighters.len() {
            out.push((slot, frame.clone()));
        }
    }
    info!(
        "combat: slot {caster_slot} RESIST ELEMENTS r{rank} applied (rating {resist_amount:.2}/elem, {resist_duration}s)"
    );
    out
}

/// `winner` defeated its opponent (the killing blow just landed). Score the round
/// (`rounds_won[winner] += 1`) then BRANCH on the best-of-3 (`MaxMatchRounds` = 3):
///
///   - **Match NOT yet won** (neither fighter at 2 wins) → this is a NON-final round
///     end: emit the round-end burst, set `MatchState`→`PostRound`(14), and put the
///     match into `FlowState::NextState` so [`super::engine::MatchInstance::on_tick`]
///     walks the BETWEEN-ROUNDS MatchState sequence `ChooseLoadout`(8)→
///     `AwaitingClientBackendSynchronization`(9)→`SynchronizingLoadout`(10)→
///     `OpponentShowcase`(11)→`PreRound`(12)→`InRound`(13), resets both fighters to
///     full HP, and re-enters the live round — the match LOOPS to round 2/3. [s506
///     round-0→round-1: 13→op79 RoundEnd→14 PostRound→8 ChooseLoadout(round=1)→9→10→
///     11→12→13.]
///   - **Match won** (a fighter just reached 2 round-wins) → the MATCH ends: same
///     round-end burst + `PostRound`(14), but `phase = RoundEnd` so the engine walks
///     the TERMINAL states `BackendMatchEnd(17)→PostMatch(16)→DisconnectingPlayers(19)`
///     and finishes — the client sees a clean result + returns to the lobby. [s506
///     final round, the match-ending blow.]
///
/// Both branches emit the capture-faithful burst (decoded byte-for-byte from prod
/// arena_udp_frames s506):
///   1. op29 `PlayerDeadStateChange` for the loser (capture-proven props-0-6 layout).
///   2. op79 flow `RoundEnd` on the Control net-object (the client echoes op80).
///   3. op48 `MatchPostRoundInfoMsg` — the round result.
///   4. Match net-object `MatchState` → `PostRound`(14).
fn on_round_ending_death(combat: &mut MatchCombat, winner: usize, now: Instant) -> Vec<(usize, Vec<u8>)> {
    on_round_ended(combat, winner, now, true)
}

/// End a live round because its authoritative 120-second clock expired. No fighter
/// is actually dead, so this emits the same cumulative result + MatchState walk as a
/// normal round end without fabricating an op29 death frame. The engine chooses the
/// winner by remaining HP fraction before calling this function.
pub(super) fn on_round_timeout(
    combat: &mut MatchCombat,
    winner: usize,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    on_round_ended(combat, winner, now, false)
}

fn on_round_ended(
    combat: &mut MatchCombat,
    winner: usize,
    now: Instant,
    ended_by_death: bool,
) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let loser = combat.opponent_of(winner).unwrap_or(winner);
    // Make the round boundary immediate and explicit. Old committed work may not
    // animate or land later, and op53 can leave a surviving caster visually
    // Channeling even though its logical actor_state is already Idle.
    combat.channels.clear();
    combat.pending_hits.clear();
    combat.pending_impacts.clear();
    for (slot, fighter) in combat.fighters.iter_mut().enumerate() {
        fighter.clear_scheduled_states();
        if !ended_by_death || slot != loser {
            fighter.force_actor_state(ActorStateType::Idle, now);
        }
    }
    // **Phase 3.14 — DOUBLE-KO.** Both fighters at 0 HP in the same resolution step:
    // nobody scores, the round is replayed. AUTHORED, not capture-derived — no
    // recorded match ends this way, so this is a designed rule.
    let double_ko = ended_by_death
        && combat.round_outcome() == super::state::RoundOutcome::DoubleKo;
    if double_ko {
        info!("combat: DOUBLE-KO — both fighters at 0 HP, round is replayed (no score)");
    } else if winner < combat.rounds_won.len() {
        combat.rounds_won[winner] += 1;
    }
    let match_won = combat.match_is_won();
    let burst = if ended_by_death {
        "op29 + op79 RoundEnd + op48 + MatchState→PostRound(14)"
    } else {
        "op79 RoundEnd + op48 + MatchState→PostRound(14)"
    };
    let loser_obj = combat.fighters.get(loser).map(|f| f.net_object_id).unwrap_or(0);
    let winner_obj = combat.fighters.get(winner).map(|f| f.net_object_id).unwrap_or(0);
    let loser_stats = combat.fighters.get(loser).map(|f| f.packed_stats()).unwrap_or(0);
    let winner_stats = combat.fighters.get(winner).map(|f| f.packed_stats()).unwrap_or(0);

    // 1) op29 PlayerDead for the loser, props 0-10 — only for an actual death.
    //
    //    Transition the loser to `Dead` FIRST. The state ring at propId 7 is what the
    //    client reads to pick a death animation, and its newest entry must equal the
    //    frame's own propId 6 — an invariant that holds in every retail frame decoded.
    //    Snapshotting before the transition would ship a ring whose tail is whatever
    //    the fighter was doing a moment ago, and a death that animates out of the
    //    wrong pose.
    let dead_frame = if ended_by_death {
        if let Some(f) = combat.fighters.get_mut(loser) {
            f.set_actor_state(ActorStateType::Dead, now);
        }
        let (loser_history, loser_time_in_prev) = combat
            .fighters
            .get(loser)
            .map(|f| (f.packed_state_history(), f.time_in_state(now)))
            .unwrap_or_default();
        // op29 carries the same Dead-tailed history the queued transition does, so
        // drop that transition: drained, it would reach the client as a 39 Dead
        // ahead of op29 and make op29 a no-op (`CheckShouldForceServerState@0x1792864`
        // returns false once the indices agree), losing the kneel/ragdoll parameters.
        if let Some(f) = combat.fighters.get_mut(loser) {
            let _ = f.take_state_changes();
        }
        Some(messages::player_dead(
            loser_obj,
            loser_stats,
            winner_stats,
            &loser_history,
            loser_time_in_prev,
        ))
    } else {
        None
    };
    // 3) op48 MatchPostRoundInfoMsg — the result (winner/loser char UUIDs + match id).
    //    matchId = the gameSessionId (the Match net-object's propId9). Carries the ACTUAL
    //    round number (so the client scores THIS round, not a fixed round-3 frame) and
    //    `is_match_ended` = whether this death won the match (best-of-3). [bug-1 fix]
    // Record THIS round's outcome, then send the cumulative array. op48 is
    // cumulative — the client tallies the score from the whole round-by-round list,
    // so every completed round must be present in order (capture-pinned, 375 frames).
    combat.round_winners.push(winner);
    let round_results: Vec<(String, String)> = combat
        .round_winners
        .iter()
        .map(|&w| {
            let l = 1 - w;
            (
                combat.fighters.get(w).map(|f| f.loadout.character_uuid.clone()).unwrap_or_default(),
                combat.fighters.get(l).map(|f| f.loadout.character_uuid.clone()).unwrap_or_default(),
            )
        })
        .collect();
    let result_frame = messages::match_post_round_info(
        combat.match_net_object_id,
        &round_results,
        &combat.game_session_id,
        match_won,
        false, // a death, not a concession
    );
    // 4) Match net-object → PostRound(14), timeout 3.0 (s506 obj 123 round end).
    let post_round_update = messages::update_match(
        combat.match_net_object_id,
        combat.fighters.len() as u8,
        MatchState::PostRound,
        MATCH_STATE_POST_ROUND_TIMEOUT,
        combat.round,
        &combat.game_session_id,
    );
    combat.match_state = MatchState::PostRound;
    combat.match_state_timeout_secs = MATCH_STATE_POST_ROUND_TIMEOUT;

    if match_won {
        // Final round → walk the terminal match-end states next.
        combat.winner = Some(winner);
        combat.matchend_step = 0;
        combat.phase = FlowState::RoundEnd;
        info!(
            "combat: MATCH-ending {} → winner slot {winner} (obj {winner_obj}) won the match \
             (score {:?}); emitting {burst} to {} player(s); \
             engine tick now walks PostRound→BackendMatchEnd→PostMatch→Disconnecting",
            if ended_by_death { "death" } else { "round timeout" },
            combat.rounds_won,
            combat.fighters.len(),
        );
    } else {
        // Non-final round → loop to the next round (best-of-3). The engine's NextState
        // branch walks ChooseLoadout(8)→…→InRound(13) + resets HP + re-enters the round.
        combat.interround_step = 0;
        combat.phase = FlowState::NextState;

        // Return both actors to Idle NOW, as the round ends — not six interround
        // steps later when round 2 goes live.
        //
        // `reset_fighters_for_next_round` already does this, but it runs only at
        // `InRound(13)`, the last step of the walk. So a fighter caught mid-cast when
        // the round ended kept that pose for the whole break. Report #113, in the
        // reporter's own words: the opponent's cast pose held "from the end of a round
        // right through the break and up until the first strikes of the next round
        // began" — which is exactly where the old reset fired.
        //
        // This is additive: it tells the clients earlier and removes nothing. Retail
        // interleaves actor-state (gmid 39) with the match-state walk rather than
        // confining it to round start — 856 gmid-39 frames sit among the 277 gmid-79
        // state changes in session 615 — so an Idle inside the walk is the shape the
        // client already expects.
        //
        // The LOSER of a death is exempt: it stays Dead, and its op29 below must be the
        // first state frame the clients see for it (12-D5).
        combat.reset_actor_animations_except(now, ended_by_death.then_some(loser));
        out.extend(drain_state_changes(combat, now));
        info!(
            "combat: round-ending {} (round {}) → winner slot {winner} (obj {winner_obj}), loser slot {loser} \
             (obj {loser_obj}); score {:?} (no fighter at {} wins yet) — LOOPING to the next round; \
             emitting {burst}, then the engine walks \
             ChooseLoadout(8)→…→InRound(13) and resets both fighters to full HP",
            if ended_by_death { "death" } else { "timeout" },
            combat.round,
            combat.rounds_won,
            super::state::ROUND_WINS_TO_WIN_MATCH,
        );
    }
    if let Some(dead_frame) = &dead_frame {
        debug!("combat op29 PlayerDead {} bytes: {}", dead_frame.len(), hex(dead_frame));
    }
    debug!("combat op48 result {} bytes: {}", result_frame.len(), hex(&result_frame));

    for slot in 0..combat.fighters.len() {
        if let Some(dead_frame) = &dead_frame {
            out.push((slot, dead_frame.clone()));
        }
        // 2) op79 flow "RoundEnd" on the Control net-object.
        if let Some(m) = messages::flow_state(combat.flow_controller_id, FlowState::RoundEnd) {
            out.push((slot, m));
        }
        out.push((slot, result_frame.clone()));
        out.push((slot, post_round_update.clone()));
    }
    out
}

/// `CurrentMatchStateTimeout` (Match propId6) sent with the `PostRound`(14) update at
/// a round-ending death — s506 obj 123 final round: 3.0 s.
const MATCH_STATE_POST_ROUND_TIMEOUT: f32 = 3.0;

/// Lowercase hex of an emitted frame, for logging the UNVERIFIED s2c layouts
/// (op29/op49) so the next capture can validate the exact bytes the server sent.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Per-second Stamina/Magicka regen for all alive fighters. Called from `on_tick`
/// once per `REGEN_TICK_INTERVAL`.
///
/// **Video ground-truth (s293):** health has ZERO in-round passive regen — HP only
/// changes on hits.  Stamina and magicka recover at ~5 %/s (video-pinned from t=50..52
/// and the t=113..117 confirming window).  Between-round HP reset is handled separately
/// by `reset_fighters_for_next_round`; no in-round HP regen is applied here.
///
/// Block-regen status effects suppress per-stat regen:
///   - `BlockHealthRegen`(50) — kept for future out-of-arena paths; no-op here (0.0 rate)
///   - `BlockStaminaRegen`(51) → no stamina regen (Frozen)
///   - `BlockMagickaRegen`(52) → no magicka regen (Enervated)
///
/// After all fighters are ticked, emits `PlayerStatsUpdate`(65) for any fighter
/// whose pools changed. [video-ground-truth §1; /tmp/arena-video-groundtruth.md]
pub(super) fn apply_regen_tick(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    use super::state::StatusEffectType;

    let mut out = Vec::new();

    for slot in 0..combat.fighters.len() {
        let f = &mut combat.fighters[slot];
        if f.is_dead() {
            continue;
        }

        // Which regen channels are suppressed by an active status.
        //
        // Frozen(5) suppresses STAMINA and Enervated(6) suppresses MAGICKA as an
        // INTRINSIC property of those statuses — retail does not send a companion
        // status to say so. Measured over 7,051 op51 messages in six sessions,
        // BlockHealthRegen(50)/BlockStaminaRegen(51)/BlockMagickaRegen(52) were
        // never sent once, yet the packed pools prove the block is in force:
        // during a Frozen window, stamina sat pinned at exactly 26 across five
        // consecutive samples while health AND magicka rose in the very same
        // frames, then resumed the instant Frozen was removed. Enervated mirrors
        // it — magicka pinned at 79 for nine samples while stamina climbed
        // 160→252. So the block is real; the announcement is not.
        //
        // 50/51/52 are still honoured because an item property can set them
        // (BlockMagickaRegenerationPropertyLogic), but nothing in combat emits
        // them, and nothing should — that would invent traffic retail never sent.
        //
        // Note a pool can still be SPENT while blocked; only regeneration stops.
        let suppresses_stamina = |e: &super::state::ActiveEffect| {
            matches!(
                e.effect,
                StatusEffectType::BlockStaminaRegen | StatusEffectType::Frozen
            )
        };
        let suppresses_magicka = |e: &super::state::ActiveEffect| {
            matches!(
                e.effect,
                StatusEffectType::BlockMagickaRegen | StatusEffectType::Enervated
            )
        };
        let block_stam = f
            .effects
            .iter()
            .any(|e| suppresses_stamina(e) && now < e.expires_at);
        let block_mag = f
            .effects
            .iter()
            .any(|e| suppresses_magicka(e) && now < e.expires_at);

        let before_s = f.stamina;
        let before_m = f.magicka;
        let before_h = f.health;

        // A potion in flight. Drained here so it shares the tick's existing
        // stats-update emit, and so a restoration and a regen landing in the
        // same second produce ONE frame rather than two.
        //
        // Health is restored even though passive health regen is zero: a potion
        // is not regeneration, and `ShouldApplyRegeneration` returning false in
        // PvP says nothing about drinking one.
        if let Some(mut pr) = f.pending_restore.take() {
            let give = pr.per_tick.min(pr.remaining);
            let amount = give.round() as u32;
            match pr.affected_stat {
                0 => f.health = (f.health + amount).min(f.max_health),
                1 => f.stamina = (f.stamina + amount).min(f.max_stamina),
                2 => f.magicka = (f.magicka + amount).min(f.max_magicka),
                _ => {}
            }
            pr.remaining -= give;
            // Keep it only while there is something left to give; the rounding
            // above can leave a sub-point remainder that would otherwise tick
            // forever handing over zero.
            if pr.remaining >= 1.0 {
                f.pending_restore = Some(pr);
            }
        }

        // PASSIVE health regen stays at zero. `HEALTH_REGEN_RATE_PER_S` is still
        // referenced by no code but its own pinning test: `ShouldApplyRegeneration`
        // returns false unconditionally in PvP, and switching universal health regen
        // on would change how every fight feels. That remains an owner decision.
        //
        // HEALING SURGE is a different thing and is applied here. It is a PERK —
        // "Increases Health regeneration while Stamina is high, by up to {0} per
        // second" — so it pays out only for a player who bought it, at the rank they
        // bought. That reconciles the owner's report that health regenerates in a
        // fight with the deliberate zero above: the measured wire range across 272
        // fighters (1.4-14.2 HP/s) sits inside this perk's shipped ceiling (8.4 at
        // rank 1 to 15.4 at rank 8), and the fighters showing no regen are the ones
        // without the perk.
        //
        // `BlockHealthRegen`(50) suppresses it, exactly as 51/52 suppress the two
        // pools. Nothing in combat emits 50, but an item property can set it.
        let block_health = f
            .effects
            .iter()
            .any(|e| e.effect == StatusEffectType::BlockHealthRegen && now < e.expires_at);
        if !block_health && f.health < f.max_health && f.max_stamina > 0 {
            // `Stamina.BoundedPercent` — against the pool's FULL maximum, the same
            // reading Maximum Power uses (ravage does not lower `Maximum`).
            let stamina_fraction =
                f.stamina as f32 / f.max_stamina.saturating_add(f.ravaged_stamina) as f32;
            let rate = f.loadout.perks.healing_surge_rate(stamina_fraction);
            // REGEN_TICK_INTERVAL is 1 s, so a per-second rate IS the per-tick
            // amount. Rounded, and not floored to a minimum of 1: an unperked
            // fighter must gain exactly nothing.
            let heal = rate.round() as u32;
            if heal > 0 {
                f.health = (f.health + heal).min(f.max_health);
            }
        }

        // Stamina regen: 3.03 %/s — the captured wire rate (see the constant).
        if !block_stam && f.stamina < f.max_stamina {
            let regen = ((STAMINA_REGEN_RATE_PER_S * f.max_stamina as f32).round() as u32).max(1);
            f.stamina = (f.stamina + regen).min(f.max_stamina);
        }
        // Magicka Surge's BLACKOUT: for `_noMagickaRegenDuration` after the surge
        // ends, magicka does not regenerate at all. This is the drawback that pays
        // for the surge and it is authored, not invented.
        let surge_blackout = f.no_magicka_regen_until.is_some_and(|t| now < t);
        // Magicka regen: 2.93 %/s — the captured wire rate (see the constant) — plus
        // Magicka Surge's flat `_magickaRegenerationBonus` while it is up.
        if !block_mag && !surge_blackout && f.magicka < f.max_magicka {
            let mut regen =
                ((MAGICKA_REGEN_RATE_PER_S * f.max_magicka as f32).round() as u32).max(1);
            if f.magicka_surge_until.is_some_and(|t| now < t) {
                // REGEN_TICK_INTERVAL is 1 s, so a per-second rate is the per-tick
                // amount (the same equivalence the health block above relies on).
                regen += f.magicka_surge_bonus.round().max(0.0) as u32;
            }
            f.magicka = (f.magicka + regen).min(f.max_magicka);
        }

        let changed = f.stamina != before_s || f.magicka != before_m || f.health != before_h;
        if changed {
            f.stats_seq = f.stats_seq.wrapping_add(1);
            let packed = f.packed_stats();
            let obj_id = f.net_object_id;
            // propId 5 is `_pvpOtherActorStats` — the OPPONENT of the avatar at
            // propId 0. It used to be a hardcoded `1`, which decodes to
            // all-pools-zero, and this tick fires ~1/s per fighter for the whole
            // fight. `packed_stats()` is a pure read: it does not bump the
            // opponent's own `stats_seq`.
            let opp_slot = combat.fighters[slot].arena_target;
            let other_packed = combat
                .fighters
                .get(opp_slot)
                .map(|o| o.packed_stats())
                .unwrap_or(packed);
            let frame = messages::player_stats_update(obj_id, packed, other_packed);
            debug!(
                "combat regen: slot {slot} stam {before_s}→{}/{} mag {before_m}→{}/{}",
                combat.fighters[slot].stamina, combat.fighters[slot].max_stamina,
                combat.fighters[slot].magicka, combat.fighters[slot].max_magicka,
            );
            for dest in 0..combat.fighters.len() {
                out.push((dest, frame.clone()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Actor-state broadcast — the animation stream
// ---------------------------------------------------------------------------

/// How long a BOT winds up before its swing lands — the `Charging` → `PlayerAutoAttack`
/// gap. Capture-measured on the opponent's avatar: median 383 ms (the capturing
/// player's own median is 318 ms, and the minimum anywhere in the corpus is 215 ms).
///
/// A human's wind-up is however long they hold the button; only a bot needs a
/// synthetic one. Without it the bot's charge and swing drain in the same tick and the
/// client has nothing to animate — which is exactly what "I still don't see the
/// opponent's swing" looked like.
const BOT_CHARGE_WINDUP: Duration = Duration::from_millis(350);

/// Delay from `PlayerAutoAttackStateChange` (52) to `PlayerFollowThroughStateChange`
/// (43). **Capture-pinned**: the measured 52→43 gaps in retail are 49, 49, 49, 53 and
/// 65 ms, and the 43 frame's own `_timeInPreviousState` is 0.050 — the message states
/// its own delay, and the two agree.
pub(super) const FOLLOW_THROUGH_DELAY: Duration = Duration::from_millis(50);

/// A pointer-driven `PlayerAttack` stays in its collision/trail state longer than an
/// auto attack. This is both authored and captured: the APK's
/// `PlayerCombatParameters._attackStateMinimumTime` is 0.150000006 s, and the gmid
/// 43 immediately following every decoded gmid 40 reference carries prop 8 ≈ 0.15.
const MANUAL_ATTACK_FOLLOW_THROUGH_DELAY: Duration = Duration::from_millis(150);

/// Delay from `PlayerFollowThroughStateChange` (43) to `PlayerRecoveryStateChange`
/// (44) — one 60 Hz frame. Measured retail gaps: 16, 17, 17, 20, 21 ms, against a
/// `_timeInPreviousState` of 1/60 s on the 44 frame.
const RECOVERY_DELAY: Duration = Duration::from_millis(17);

/// Turn every queued actor-state transition into its s2c frame, for **both** viewers.
///
/// This is the one place the animation stream is produced. It is deliberately not a
/// dozen `emit` calls next to the dozen state assignments: writers push onto
/// `Fighter::pending_state_changes` via [`super::state::Fighter::set_actor_state`] and
/// this drains them, so a new writer cannot forget to notify the client — which is the
/// failure mode that left the whole family unsent in the first place.
///
/// Called once at the end of each `MatchInstance::on_c2s` / `on_tick`, so no early
/// return in this module can skip it.
///
/// Retail's mapping of state → message, from the decoded corpus:
/// * `Blocking` → 41, the frame that raises the shield;
/// * `PlayerAttack` / `PlayerAutoAttack` → 40 / 52, the manual-slash and fallback
///   first beats; both continue through 43 / 44;
/// * `PlayerDraining` → 42;
/// * everything else → 39, the generic member. That includes `Idle`, which is how a
///   block **ends**: there is no shield-down variant of 41 (all 248 decoded 41 frames
///   carry prop6 = Blocking), so the guard comes down with a 39 carrying stateId 0.
///
/// `Charging` → 45, the wind-up. It is part of the same walk rather than a special
/// case: every one of the 593 decoded retail swings begins with a 45 for the same
/// avatar 300-400 ms before its 52.
pub fn drain_state_changes(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    drain_state_changes_for(combat, now, None)
}

/// As [`drain_state_changes`], but optionally for ONE slot.
///
/// Exists for ordering. Retail sends the actor-state frame BEFORE the status frame
/// that accompanies it — measured at 90/90 on high-block stuns in s615/s616, with no
/// exceptions. Our status frames are emitted inline where the effect is applied, while
/// state frames come from the end-of-tick drain, which put us in the opposite order on
/// every single one. A caller that emits a status can drain its own actor's state first
/// and restore retail's order without giving up the single-seam drain for everything
/// else: the end-of-tick call then finds nothing left for that slot.
pub fn drain_state_changes_for(
    combat: &mut MatchCombat,
    now: Instant,
    only: Option<usize>,
) -> Vec<(usize, Vec<u8>)> {
    let viewers = combat.fighters.len();
    let mut out = Vec::new();
    for slot in 0..viewers {
        if matches!(only, Some(s) if s != slot) {
            continue;
        }
        let changes = combat.fighters[slot].take_state_changes();
        if changes.is_empty() {
            continue;
        }
        let own = combat.fighters[slot].packed_stats();
        let opponent = combat
            .opponent_of(slot)
            .and_then(|o| combat.fighters.get(o))
            .map(|f| f.packed_stats())
            .unwrap_or(0);
        let actor_net_object_id = combat.fighters[slot].net_object_id;
        let manual_gesture = combat.fighters[slot].active_manual_attack;
        let emitted_manual_attack = changes
            .iter()
            .any(|change| change.to == ActorStateType::PlayerAttack);
        // `InitialActiveSide` for the swing family. The three beats of one swing share
        // the side the swing was committed on, which is what `last_combo_side` holds
        // until the next swing replaces it.
        let swing_side = combat.fighters[slot].last_combo_side;
        for change in changes {
            let ctx = StateFrame {
                actor_net_object_id,
                own_packed_stats: own,
                opponent_packed_stats: opponent,
                state_history: &change.history,
            };
            let t = change.time_in_previous;
            let bytes = match change.to {
                ActorStateType::Charging => {
                    // The wind-up. Its side is the one classified at the press, which
                    // is what retail carries through the whole swing.
                    let side = combat.fighters[slot].charge_side.unwrap_or(swing_side);
                    messages_state::player_charging_state_change(&ctx, side, t)
                }
                ActorStateType::Blocking => {
                    // prop 10 `OptimalBlockAllowed`: retail sent `true` in 231 of 248
                    // frames and no decoded correlation explains the other 17, so the
                    // majority value is sent rather than a guessed derivation.
                    messages_state::player_blocking_state_change(&ctx, t, true)
                }
                ActorStateType::PlayerAutoAttack => {
                    // prop 10 `Direction`: (0,0) in 21 of 25 retail frames. We have
                    // pointer samples, but they are screen coordinates, not the unit
                    // swipe vector the field carries — so send the value retail
                    // overwhelmingly sent rather than a converted guess.
                    messages_state::player_auto_attack_state_change(
                        &ctx,
                        swing_side,
                        (0.0, 0.0),
                        t,
                    )
                }
                ActorStateType::PlayerAttack => {
                    // `begin_swing_animation` enters this state only with a gesture.
                    // Keep a defensive zero fallback so a future direct state writer
                    // still emits a well-formed frame instead of dropping the whole
                    // animation stream.
                    let gesture = manual_gesture.unwrap_or(ManualAttackGesture {
                        direction: (0.0, 0.0),
                        execution_point: (0.0, 0.0),
                    });
                    messages_state::player_attack_state_change(
                        &ctx,
                        gesture.direction,
                        gesture.execution_point,
                        swing_side,
                        t,
                    )
                }
                ActorStateType::PlayerFollowThrough => {
                    messages_state::player_follow_through_state_change(&ctx, swing_side, t)
                }
                ActorStateType::PlayerRecovery => {
                    messages_state::player_recovery_state_change(&ctx, swing_side, t)
                }
                ActorStateType::PlayerDraining => {
                    messages_state::player_draining_state_change(&ctx, swing_side, t)
                }
                other => messages_state::player_state_change(&ctx, other, t),
            };
            debug!(
                "combat: slot {slot} actor state {:?} → {:?} (t_prev {t:.4}s) → gmid broadcast",
                change.from, change.to,
            );
            for viewer in 0..viewers {
                out.push((viewer, bytes.clone()));
            }
        }
        if emitted_manual_attack {
            combat.fighters[slot].active_manual_attack = None;
        }
    }
    // The Blocking (1) status, after the actor-state frames so the op41 that raises
    // the shield (or the op39 that lowers it) goes out first, the order retail uses
    // for a state and its status. Synced here, at the seam every c2s and every tick
    // passes, because a guard is raised and dropped on many paths.
    for slot in 0..viewers {
        if matches!(only, Some(s) if s != slot) {
            continue;
        }
        if let Some(up) = combat.fighters[slot].sync_blocking_status(now) {
            let obj = combat.fighters[slot].net_object_id;
            debug!("combat: slot {slot} Blocking status → {}", if up { "apply" } else { "remove" });
            // Retail's Blocking applies carry duration 0 (651 of 651, status spec §5.3).
            let frame = messages::change_combat_status_effect(
                obj, up, super::state::StatusEffectType::Blocking, 0.0,
            );
            for viewer in 0..viewers {
                out.push((viewer, frame.clone()));
            }
        }
    }
    out
}

/// Walk the attacker through the three beats of a swing.
///
/// `PlayerAttack` (manual slash) or `PlayerAutoAttack` (fallback) now, then
/// `PlayerFollowThrough` and `PlayerRecovery` on the capture-measured delays, then
/// back to `Idle` at the template's
/// `attackDelay + recoveryToNeutralTime`. A combo is legal earlier, at
/// `attackDelay + recoveryToComboTime`, so the animation and input gates deliberately
/// use separate values. The transitions land on the outbox;
/// [`drain_state_changes`] puts them on the wire.
///
/// Retail's per-session counts corroborate one of each per swing: s503 sent 330 × gmid
/// 52, 325 × 43 and 291 × 44 — near-1:1, with 44 slightly lower because a swing that
/// is interrupted never reaches recovery.
fn begin_swing_animation(
    combat: &mut MatchCombat,
    slot: usize,
    manual_attack: Option<ManualAttackGesture>,
    now: Instant,
) -> Duration {
    let neutral = combat.fighters[slot].loadout.neutral_interval();
    let f = &mut combat.fighters[slot];
    // A new combo may start while the previous swing is still recovering. Drop that
    // swing's pending Idle transition so it cannot interrupt the new animation.
    f.clear_scheduled_states();
    f.active_manual_attack = manual_attack;
    let (first_state, follow_delay) = if manual_attack.is_some() {
        (ActorStateType::PlayerAttack, MANUAL_ATTACK_FOLLOW_THROUGH_DELAY)
    } else {
        (ActorStateType::PlayerAutoAttack, FOLLOW_THROUGH_DELAY)
    };
    f.set_actor_state(first_state, now);
    f.schedule_state(now + follow_delay, ActorStateType::PlayerFollowThrough);
    f.schedule_state(
        now + follow_delay + RECOVERY_DELAY,
        ActorStateType::PlayerRecovery,
    );
    // Never idle earlier than the Recovery beat, even for a special template with
    // unusually short authored values.
    let idle_at = (now + neutral).max(now + follow_delay + RECOVERY_DELAY * 2);
    f.schedule_state(idle_at, ActorStateType::Idle);
    follow_delay
}

/// How long after a round goes live before a bot may take its first action.
///
/// **AUTHORED — this is not a shipped game-data value.** It was searched for and does
/// not exist, because retail arena is human-vs-human: there is no bot in the shipped
/// client, so Bethesda had nothing to tune. Specifically:
///
///   * `PvpDefaultSettings` (`dump.cs:427009`) is nine constants — health multiplier,
///     stamina reduction, block/stagger timings, block multipliers — none round-start.
///   * `PvpParameters` (`dump.cs:611404`) is thirteen fields — sidestep idle, charge
///     anim modifier, `serverHitTime`, spawn distance, consumables — none round-start.
///   * `CombatParameters`' only timing fields are `_baseStaggerDuration`,
///     `_endCombatTime`, `_endEncounterTime` and the IK/animation times.
///   * The only AI-reaction data anywhere in the dump is
///     `EnemyCombatAIParameters._reactionTime` (`dump.cs:624152`), the PvE dungeon
///     brain — 0.2–1.0 s bands across 667 enemy variants. Wrong domain.
///   * The inter-round state table (`engine::MATCH_STATE_INTERROUND_PROGRESSION`)
///     stops at `InRound`: its 4 s hold is consumed BEFORE the live round begins, and
///     `PreRound`'s 4.0 s is burned by the client's own READY/FIGHT HUD sequence
///     (`PvpHUDMenu.PREROUND_*`, `dump.cs:667036`). Nothing shipped covers the window
///     AFTER `InRound`.
///
/// So a number had to be chosen. 1.0 s, anchored two ways:
///
///   * **Upper bound from shipped precedent.** Retail demonstrably DOES stagger a
///     round's first action: `ActiveAbility._initialCooldown` (`dump.cs:607776`,
///     "cooldown charged at the start of a fight") runs 0.5 s (Lightning Bolt) to
///     2.75 s (Power Attack, Frostbite, Paralyze, Guardbreaker) across the arena
///     abilities — see `docs/arena-cooldowns-authoritative.md`. 1.0 s sits near the
///     bottom of that band. This is an ANALOGY, not a derivation: `_initialCooldown`
///     gates abilities, not weapon swings.
///   * **Lower bound from what the mechanic requires.** With this delay the opening
///     blow cannot land sooner than 1.0 + 0.35 + 0.05 = 1.4 s into the round, which is
///     the budget the player needs to register that the round went live, press block,
///     and have the c2s gmid 46 cross WireGuard. The previous behaviour gave 400 ms
///     total, of which none was available for the first two steps.
///
/// Kept deliberately near the bottom of the precedent band: the goal is a blockable
/// opener, not a passive bot. It is also consistent with [`BOT_SWING_COOLDOWN`], the
/// bot's other cadence knob, which is authored for the same reason.
///
/// **This is not a substitute for the telegraph.** `BOT_CHARGE_WINDUP` (350 ms) +
/// [`FOLLOW_THROUGH_DELAY`] (50 ms) = 400 ms matches retail's measured 383 ms median
/// across 593 decoded swings and must stay exactly where it is — widening the wind-up
/// to make the opener blockable would move us AWAY from retail. The defect was that
/// there was ZERO opening delay, and this is the only thing that changes.
pub(super) const ROUND_START_ENGAGE_DELAY: Duration = Duration::from_millis(1000);

/// A bot fighter's auto-swing cadence. Slower than a human's `SWING_COOLDOWN` so the
/// player wins comfortably but sees real incoming damage — a fight, not a static dummy.
const BOT_SWING_COOLDOWN: Duration = Duration::from_millis(1800);

/// How often a bot may cast an ability. AUTHORED, like `BOT_SWING_COOLDOWN` —
/// retail arena is human-vs-human, so there is no shipped bot cadence to copy.
///
/// Slower than the swing cadence on purpose: the bot should still read as a
/// melee opponent that occasionally casts, not a spell turret. Ability cooldowns
/// gate individual abilities on top of this.
const BOT_CAST_COOLDOWN: Duration = Duration::from_millis(4500);

/// How long after its own swing a bot raises its guard.
///
/// AUTHORED, but the value is not arbitrary — it is pinned by two shipped constants.
/// A re-raise within `OPTIMAL_BLOCK_RECOVERY_SECS` (0.8 s) of the last drop is
/// downgraded to a LATE block, so raising sooner than that would guarantee the bot
/// only ever blocks low. 900 ms clears it, which leaves the guard up for the ~900 ms
/// remaining of `BOT_SWING_COOLDOWN` (1.8 s) — comfortably inside the 2 s
/// `BLOCK_OPTIMAL_TIME_SECS` window, so the guard is a genuine HIGH block for its
/// whole life.
///
/// That matters because the high-block stun fires on the ATTACKER. Until the bot
/// blocked, a human could never be stunned by one: the stun needs the DEFENDER to
/// block high, and bots never guarded.
const BOT_GUARD_RAISE_DELAY: Duration = Duration::from_millis(900);

/// Drop a bot's guard the way a human's release does, so the client sees the same
/// exit: clear the window and let `reconcile_block` map Blocking → Idle, which the
/// drain emits as a gmid 39 carrying stateId 0. (Retail ends 199 of 225 blocks this
/// way rather than with a second gmid 41.)
fn bot_lower_guard(f: &mut super::state::Fighter, now: Instant) {
    if f.actor_state() == ActorStateType::Blocking {
        f.blocking_until = None;
        f.reconcile_block(now);
    }
}

/// Choose the bot's next ability: the one it has cast FEWEST times this match,
/// ties broken by loadout order.
///
/// Least-cast-first rather than random, for two reasons. It maximises coverage —
/// the point of a bot match is to exercise mechanics, and a uniform random pick
/// leaves the long tail of a loadout untouched for a long time. And it keeps the
/// engine deterministic: there is no RNG anywhere in combat resolution, which is
/// what lets the scenario tests assert exact sequences. Adding one here would cost
/// that for no gain.
///
/// `Perk` is skipped because a perk is passive and never activates.
fn bot_next_ability(f: &super::state::Fighter) -> Option<String> {
    f.loadout
        .abilities
        .iter()
        .filter(|a| a.tag != super::state::AbilityTag::Perk)
        .enumerate()
        .min_by_key(|(i, a)| (*f.bot_cast_counts.get(&a.instance_uuid).unwrap_or(&0), *i))
        .map(|(_, a)| a.instance_uuid.clone())
}

/// Tick-driven combat. Drives any BOT fighters (slots at/after `expected_peers`,
/// which have no real ENet peer — a solo-vs-bot match's 2nd fighter) to auto-swing
/// at their opponent on `BOT_SWING_COOLDOWN`. Real players are input-driven
/// (`on_c2s_input`); only bots act on the tick. (DoT/status-effect ticks will also
/// plug in here once that path is wired.)
///
/// `debug_hold` is the `ARENA_DEBUG_HOLD` freeze flag: when set, NO bot swings
/// (return empty). This is belt-and-suspenders — with HOLD on the FSM never reaches
/// `StateTimeout` so this guard is already satisfied below, but we make the no-bot
/// intent explicit and robust to any future tick path.
pub fn on_tick(combat: &mut MatchCombat, now: Instant, debug_hold: bool) -> Vec<(usize, Vec<u8>)> {
    if debug_hold {
        return Vec::new();
    }
    if !matches!(combat.phase, FlowState::StateTimeout) {
        return Vec::new();
    }
    // Expire any lapsed block windows on the tick too (a human victim of a bot may be
    // blocking with no inbound input to reconcile it).
    for f in combat.fighters.iter_mut() {
        f.reconcile_block(now);
        // Clear a lapsed stagger/stun back to Idle. A BOT has no input path, so
        // without this a bot stunned by a high block (tracker #31) would sit in
        // `Staggered` on both clients until something else moved it.
        f.reconcile_stagger(now);
        // A bot may receive no c2s input for the whole paralysis window, so expiry
        // cannot depend on the human-input path. This also lets the status diff below
        // emit the required op51 remove at the authored duration.
        reconcile_paralysis(f, now);
        // Advance any in-flight swing: AutoAttack → FollowThrough → Recovery → Idle.
        // The tick is the ONLY thing that moves it for a player who stops sending
        // input mid-swing, so this must run here as well as on the input path.
        f.reconcile_scheduled_states(now);
    }
    let mut out = Vec::new();

    // Land any swings whose FollowThrough beat has arrived, BEFORE the DoT ticks and
    // the bot's turn, so a swing thrown last tick resolves in the order it would have
    // if it had landed instantly (tracker #21).
    out.extend(land_due_hits(combat, now));
    out.extend(land_due_echoes(combat, now));
    out.extend(land_due_impacts(combat, now));
    if matches!(combat.phase, FlowState::RoundEnd | FlowState::NextState) {
        // A landing blow just ended the round.
        return out;
    }

    // DoT ticks — one tick per second per active condition instance, independent of
    // whether a bot or player is the source. Runs BEFORE bot swings so a DoT killing
    // blow is processed before the bot's turn. Tick first at the expiry boundary,
    // then diff statuses: otherwise pruning the condition would drop its fifth tick.
    // [§Mechanic-2]
    out.extend(apply_dot_ticks(combat, now));
    out.extend(emit_status_removals(combat, now));
    out.extend(apply_channel_ticks(combat, now));
    out.extend(apply_continuous_area_damage(combat, now));
    if matches!(combat.phase, FlowState::RoundEnd | FlowState::NextState) {
        // A DoT killing blow just ended the round — no bot swings this tick.
        return out;
    }

    // Regen tick — once per second, regenerate HP/Stamina/Magicka for all alive
    // fighters. Runs AFTER DoT (DoT damage may deplete a pool; regen brings it back up).
    // Guarded against DoT-ending the round (the RoundEnd/NextState check above).
    if now.duration_since(combat.last_regen_tick) >= REGEN_TICK_INTERVAL {
        combat.last_regen_tick = now;
        out.extend(apply_regen_tick(combat, now));
    }

    let bot_slots: Vec<usize> = (combat.expected_peers..combat.fighters.len()).collect();
    for bot in bot_slots {
        if combat.fighters[bot].is_dead() {
            continue;
        }
        let Some(target) = combat.opponent_of(bot) else {
            continue;
        };
        if combat.fighters[target].is_dead() {
            continue;
        }
        // A STUNNED bot cannot act (tracker #31). The human input path has enforced
        // this since Phase 3.13 (`is_staggered` gate in `on_c2s_input`), but the bot
        // loop never did — so a bot stunned by a high block would keep swinging and
        // the whole mechanic would be invisible for exactly the case the report
        // describes ("the AI swings into my high block"). Its queued wind-up is
        // dropped too: `apply_stagger_for` already cleared the scheduled actor states,
        // and letting `bot_swing_at` survive would land a swing out of a stun.
        if combat.fighters[bot].is_staggered(now) {
            combat.fighters[bot].bot_swing_at = None;
            continue;
        }
        // Paralysis locks bot input just as it locks human input. `pending_hits` is
        // deliberately untouched: a swing already COMMITTED before the paralysis
        // still lands, while this pre-commit Charging wind-up is cancelled.
        if combat.fighters[bot].is_paralyzed() {
            combat.fighters[bot].bot_swing_at = None;
            continue;
        }
        // OPENING DELAY. `ready` below falls back to `true` when `last_swing` is
        // `None`, which at round start it always is — so the bot charged on tick 0 of
        // the round and the opening blow landed `BOT_CHARGE_WINDUP` +
        // `FOLLOW_THROUGH_DELAY` = 400 ms into a round the player had not yet seen go
        // live. Blocking it required pressing block and getting the c2s gmid 46 across
        // WireGuard inside that window: not reachable, and the opener was in practice
        // unblockable.
        //
        // The knob is this delay, NOT the telegraph. 350 ms + 50 ms matches retail's
        // measured 383 ms median across 593 decoded swings — widening the wind-up
        // would move us away from retail, so it stays exactly where it is. Precisely:
        // the gap was not literally zero, it was 400 ms of swing ANIMATION and nothing
        // else. What was missing is any opening delay BEYOND the animation, i.e. any
        // time in which the player can register that the round is live before the
        // telegraph starts.
        if now.duration_since(combat.phase_entered) < ROUND_START_ENGAGE_DELAY {
            continue;
        }
        // Cast before swinging. A bot that only ever swung was why a human opponent
        // never received a status effect: every stun/freeze/paralyse in a bot match
        // flowed one way, because only the human side ever cast anything.
        // Swing once before the first cast. Starting every round with the loadout's
        // strongest ready ability made the authored AI much more clinical than a
        // human opponent, especially when that first cast was a maneuver.
        let cast_ready = combat.fighters[bot].last_swing.is_some()
            && combat.fighters[bot]
                .bot_last_cast
                .map(|t| now.duration_since(t) >= BOT_CAST_COOLDOWN)
                .unwrap_or(true);
        if cast_ready && combat.fighters[bot].bot_swing_at.is_none() {
            if let Some(uuid) = bot_next_ability(&combat.fighters[bot]) {
                // Go through the SAME path a human cast takes — synthesise the frame a
                // client would have sent rather than maintain a second cast
                // implementation that could drift. `resolve_ability_cast` still applies
                // the per-ability cooldown and resource cost, so an unaffordable or
                // still-cooling ability simply produces nothing here.
                bot_lower_guard(&mut combat.fighters[bot], now);
                let frame = messages::request_execute_ability(
                    combat.fighters[bot].net_object_id,
                    &uuid,
                );
                if let Some(ea) = input::parse_execute_ability(&frame) {
                    let before = out.len();
                    out.extend(resolve_ability_cast(combat, bot, target, &frame, &ea, now));
                    if out.len() > before {
                        // Only count a cast that actually resolved, so an ability that
                        // is on cooldown or unaffordable does not get "used up" and
                        // starve the rest of the loadout.
                        *combat.fighters[bot]
                            .bot_cast_counts
                            .entry(uuid)
                            .or_insert(0) += 1;
                        combat.fighters[bot].bot_last_cast = Some(now);
                        continue;
                    }
                }
            }
        }

        // A bot swings in TWO steps, because retail's swing is two steps.
        //
        // Step 1, the wind-up: enter `Charging` and note when the swing should land.
        // Step 2, `BOT_CHARGE_WINDUP` later: resolve the swing, which walks
        // AutoAttack → FollowThrough → Recovery → Idle.
        //
        // Doing it in one tick is what made the opponent's swing invisible: the client
        // received the charge and the attack in the same breath, with no wind-up to
        // play. Retail never does that — all 593 decoded swings have a 300-400 ms gap.
        if let Some(at) = combat.fighters[bot].bot_swing_at {
            if now >= at {
                combat.fighters[bot].bot_swing_at = None;
                // Bots don't hold a button — always ×1.0 (no held-charge crit). Use
                // the side announced at wind-up instead of the synthetic fallback,
                // so the authored bot pattern below is also the damage-model input.
                let side = combat.fighters[bot].charge_side;
                out.extend(resolve_swing_with_side(combat, bot, target, 1.0, side, now));
            }
            continue;
        }
        let ready = combat.fighters[bot]
            .last_swing
            .map(|t| now.duration_since(t) >= BOT_SWING_COOLDOWN)
            .unwrap_or(true);
        if !ready {
            // The gap between swings is when a real player guards, so the bot does
            // too. Raising here (rather than on a timer of its own) is what keeps the
            // block INSIDE the optimal window: see `BOT_GUARD_RAISE_DELAY`.
            let since_swing = combat.fighters[bot].last_swing.map(|t| now.duration_since(t));
            let due = since_swing.map(|d| d >= BOT_GUARD_RAISE_DELAY).unwrap_or(false);
            let f = &mut combat.fighters[bot];
            if due && f.actor_state() != ActorStateType::Blocking && f.block_phase(now).is_none() {
                // Same fields the human block-zone press sets, so the drain emits the
                // identical gmid 41 and the block resolves through the identical path.
                f.set_actor_state(ActorStateType::Blocking, now);
                f.blocking_side = ActiveSide::Middle; // retail: propId 9 == 1 in 578/578
                f.blocking_until = Some(now + BLOCK_LEAK_GUARD);
                f.block_raised_at = Some(now);
                // info!, not debug!: prod runs RUST_LOG=info. A played match showed
                // 17 stuns, all one-directional, and this line — the only evidence of
                // whether the bot ever guarded — produced nothing either way.
                info!("combat: slot {bot} bot guard UP");
            }
            continue;
        }
        {
            // Swinging ends the guard, exactly as an attack press does for a human.
            bot_lower_guard(&mut combat.fighters[bot], now);
            // The side is decided now and carried across the whole swing (593/593 in
            // retail). The old synthetic fallback alternated perfectly forever,
            // guaranteeing the maximum combo ramp. A human-like deterministic
            // pattern alternates once, then repeats a side to reset the chain:
            // ×1, ×combo, ×1, ×combo… rather than climbing to the cap every round.
            let side = if combat.fighters[bot].combo_count == 0 {
                match combat.fighters[bot].last_combo_side {
                    ActiveSide::Right => ActiveSide::Left,
                    _ => ActiveSide::Right,
                }
            } else {
                combat.fighters[bot].last_combo_side
            };
            combat.fighters[bot].charge_side = Some(side);
            combat.fighters[bot].set_actor_state(ActorStateType::Charging, now);
            combat.fighters[bot].bot_swing_at = Some(now + BOT_CHARGE_WINDUP);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Unit tests (spec §IMPLEMENT: focused tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    /// The regen rates are PINNED, not derived.
    ///
    /// The other regen tests compute their expectation from the same constant
    /// they check, so they follow any edit silently — they verify the arithmetic,
    /// not the number. This figure has already flipped once (video 5 %/s -> wire
    /// 3.03 %/s, owner's call 2026-08-22) and is exactly the kind of value that
    /// gets "tidied" back. Changing it should mean changing this test and saying
    /// why.
    #[test]
    fn the_regen_rates_are_the_measured_wire_values() {
        assert_eq!(
            STAMINA_REGEN_RATE_PER_S, 0.0303,
            "stamina regen is the captured 3.03 %/s, not the video 5 %/s",
        );
        assert_eq!(
            MAGICKA_REGEN_RATE_PER_S, 0.0293,
            "magicka regen is the captured 2.93 %/s, not the video 5 %/s",
        );
        // Health is still zero AND still unwired — no code reads this constant.
        // Asserting both halves so "I set the constant" cannot be mistaken for
        // "health now regenerates".
        assert_eq!(HEALTH_REGEN_RATE_PER_S, 0.0);
    }

    /// Advance past the FollowThrough beat so a committed swing lands.
    ///
    /// Tracker #21 moved the moment of impact to match the animation. These tests
    /// were updated to ADVANCE A CLOCK, not to relax assertions — every damage
    /// number below is unchanged.
    fn land(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
        super::land_due_hits(combat, now + super::FOLLOW_THROUGH_DELAY + Duration::from_millis(1))
    }

    /// Commit a swing and land it.
    fn swing_and_land(
        combat: &mut MatchCombat,
        sender: usize,
        target: usize,
        factor: f32,
        now: Instant,
    ) -> Vec<(usize, Vec<u8>)> {
        let mut out = super::resolve_swing(combat, sender, target, factor, now);
        out.extend(land(combat, now));
        out
    }
    use super::*;
    use super::super::messages::{self, frame_for_test};
    use super::super::state::{
        AbilityTag, BlockPhase, DamageType, EquippedAbility, Fighter, FlowState, MatchCombat,
    };
    use arena_proto::NetDataWriter;

    // -----------------------------------------------------------------------
    // Bug 3: Block input must emit ZERO damage
    // -----------------------------------------------------------------------

    /// A `PlayerBlockingStateChange` (41) c2s frame must set the block state and return
    /// NO s2c damage frames — raising the shield must never produce a ReceiveDamage. [spec bug 3]
    #[test]
    fn block_input_emits_zero_damage() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        // Build a realistic c2s op41 PlayerBlockingStateChange (Right side = 3).
        let block_frame = {
            let mut w = NetDataWriter::new();
            w.int(0, 120).byte(1, 55).byte(2, 3).byte(3, 41).byte(4, 3);
            let mut f = frame_for_test(w.finish());
            f[0] = 0x84; // c2s marker
            f
        };

        let resolved = on_c2s_input(&mut combat, 0, &block_frame, now);
        assert!(
            resolved.is_empty(),
            "resolution itself emits nothing for a block — no damage, and the gmid-41 \
             relay is the drain's job"
        );

        // The relay comes from the actor-state drain, which the engine runs at the end
        // of every on_c2s/on_tick. It is what raises the shield on screen; this test
        // used to assert zero frames anywhere, which is what kept the shield down
        // (report #5).
        //
        // Each viewer gets the gmid-41 relay and then the Blocking (1) status op51:
        // retail sends both (736 Blocking applies in s615+s616), state frame first.
        let out = drain_state_changes(&mut combat, now);
        assert_eq!(
            out.len(),
            2 * combat.fighters.len(),
            "block must relay PlayerBlockingStateChange + op51(Blocking) to every viewer, \
             got {} frame(s)",
            out.len()
        );
        let (relay, status) = out.split_at(combat.fighters.len());
        for (_, f) in relay {
            let nd = arena_proto::parse_netdata(&f[2..]);
            assert_eq!(
                nd.int(3),
                Some(41),
                "the state relay comes first — never damage"
            );
            assert_eq!(nd.int(6), Some(1), "prop6 = ActorStateType::Blocking");
        }
        for (_, f) in status {
            let nd = arena_proto::parse_netdata(&f[2..]);
            assert_eq!(nd.int(3), Some(51), "then the status op51");
            assert_eq!(nd.int(5), Some(1), "StatusEffectType::Blocking");
            assert_eq!(
                nd.props.get(&4),
                Some(&arena_proto::NetDataValue::Bool(true)),
                "an apply"
            );
        }
        // And the fighter should now be in the Blocking state.
        assert_eq!(
            combat.fighters[0].actor_state(),
            super::super::state::ActorStateType::Blocking,
            "block input must put fighter 0 into Blocking state"
        );
    }

    // -----------------------------------------------------------------------
    // Bug 2: Under-funded ability cast is rejected
    // -----------------------------------------------------------------------

    /// An ability cast when the caster has LESS stamina than required must be
    /// rejected — no cooldown set, no damage emitted. [spec bug 2 / §1 cost gate]
    ///
    /// It is no longer *silent*: report #109 showed that emitting nothing leaves the
    /// client's own prediction uncorrected, so the player sees a free cast. The
    /// rejection now answers with the caster's unchanged pools (op65) and nothing
    /// else — which is what this test asserts, and it is the only thing about the
    /// rejection that changed.
    #[test]
    fn underfunded_cast_is_rejected_no_damage_no_cooldown() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        // Give fighter 0 a Quick Strikes (eb0cb7e6…, R1 cost = 150 stamina).
        // Then DRAIN stamina to zero so it can't afford the cast.
        let qs_uuid = "eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13";
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: qs_uuid.to_string(),
            level: 1,
            tag: AbilityTag::Generic,
        });
        combat.fighters[0].stamina = 0; // completely empty

        let ability_frame = make_ability_frame(120, qs_uuid);
        let mut out = on_c2s_input(&mut combat, 0, &ability_frame, now);
        out.extend(land(&mut combat, now));

        // No cast echo (38), no damage (50) — the ONLY thing a rejection emits is
        // the op65 pool correction, one per player.
        for (_, f) in &out {
            assert_eq!(
                messages::user_message_gmid(f),
                Some(65),
                "underfunded cast must emit nothing but the op65 pool correction"
            );
        }
        assert_eq!(
            out.len(),
            combat.fighters.len(),
            "one op65 per player and no other frame, got {} frame(s)",
            out.len()
        );
        // Cooldown must NOT be set — the cast was rejected before the commit point.
        assert!(
            combat.fighters[0].cooldowns.get(qs_uuid).is_none(),
            "rejected cast must not set the ability cooldown"
        );
    }

    // -----------------------------------------------------------------------
    // Initial cooldown: the round-start first-use delay
    // -----------------------------------------------------------------------

    /// Reckless Fury must not be castable on the round's opening tick.
    ///
    /// `ActiveAbility._initialCooldown` is charged when a round goes live, and
    /// Reckless Fury's is 10.5 s — the longest in the game, 3.8x the 2.75 s tier
    /// below it. The field was extracted into `gamedata.rs` and never read, so the
    /// gate did not exist: the AI opened round 2 of match `7c8f70ac` with Reckless
    /// Fury **1.35 s in** (round live 19:36:03.271, cast 19:36:04.626).
    ///
    /// Stamina could not stand in for the gate — the between-round reset refills
    /// pools to full, so the 425 cost was affordable on the first tick. This test
    /// therefore funds the cast deliberately: the ONLY thing that may reject it is
    /// the initial cooldown.
    #[test]
    fn reckless_fury_is_not_castable_on_the_rounds_opening_tick() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        let rf_uuid = "0cfe29cd-89d9-42ad-9227-8308e2f87c7f";
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: rf_uuid.to_string(),
            level: 1,
            tag: AbilityTag::Maneuver,
        });
        // Fund it past the 425 stamina cost, so cost can never be the reason.
        combat.fighters[0].max_stamina = 660;
        combat.fighters[0].stamina = 660;

        // The round goes live.
        combat.charge_initial_cooldowns(now);

        // 1.35 s in — exactly where the reported cast landed.
        let at_report = now + Duration::from_millis(1350);
        let frame = make_ability_frame(120, rf_uuid);
        let out = on_c2s_input(&mut combat, 0, &frame, at_report);
        assert!(
            out.is_empty(),
            "Reckless Fury at 1.35 s must be refused by the 10.5 s initial cooldown, \
             got {} frame(s)",
            out.len()
        );
        assert_eq!(
            combat.fighters[0].stamina, 660,
            "a refused cast must not spend stamina"
        );

        // The control: the same cast, funded identically, once the 10.5 s has run.
        // Without this the test would also pass if the ability were simply broken.
        let after = now + Duration::from_millis(10_600);
        let out = on_c2s_input(&mut combat, 0, &frame, after);
        assert!(
            !out.is_empty(),
            "once the initial cooldown expires the same cast must go through"
        );
        assert_eq!(
            combat.fighters[0].stamina,
            660 - 425,
            "the accepted cast spends its 425 stamina"
        );
    }

    /// The control on the seeding itself: an ability that ships NO `_initialCooldown`
    /// must not be gated at round start, or charging them would silently freeze
    /// abilities retail leaves available on the opening tick.
    #[test]
    fn an_ability_without_an_initial_cooldown_is_open_at_round_start() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        // Quick Strikes ships an initialCooldown (2.08 s); Chaotic Strike does not.
        let qs_uuid = "eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13";
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: qs_uuid.to_string(),
            level: 1,
            tag: AbilityTag::Maneuver,
        });
        combat.fighters[0].max_stamina = 660;
        combat.fighters[0].stamina = 660;
        combat.charge_initial_cooldowns(now);

        assert!(
            combat.fighters[0].cooldowns.contains_key(qs_uuid),
            "Quick Strikes ships a 2.08 s initial cooldown and must be charged"
        );

        // An ability the table has no initial cooldown for must be left uncharged.
        let unknown = "00000000-0000-4000-8000-000000000000";
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: unknown.to_string(),
            level: 1,
            tag: AbilityTag::Generic,
        });
        combat.charge_initial_cooldowns(now);
        assert!(
            !combat.fighters[0].cooldowns.contains_key(unknown),
            "an ability with no shipped initial cooldown must stay open at round start"
        );
    }

    /// An ability cast when the caster HAS enough stamina succeeds: cooldown is set,
    /// stamina is deducted, an op65 PlayerStatsUpdate (ch1) is emitted. [spec §1]
    #[test]
    fn funded_cast_deducts_stamina_and_emits_op65() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        let qs_uuid = "eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13"; // Quick Strikes R1 = 150 stam
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: qs_uuid.to_string(),
            level: 1,
            tag: AbilityTag::Generic,
        });
        // Ensure full stamina (set by Fighter::new from pool_for_level).
        let stam_before = combat.fighters[0].stamina;
        assert!(stam_before >= 150, "fighter must have ≥ 150 stamina for this test");

        let ability_frame = make_ability_frame(120, qs_uuid);
        let mut out = on_c2s_input(&mut combat, 0, &ability_frame, now);
        out.extend(land(&mut combat, now));

        // Stamina must be deducted by the R1 cost (150).
        let stam_after = combat.fighters[0].stamina;
        assert_eq!(
            stam_before - stam_after,
            150,
            "Quick Strikes R1 must cost exactly 150 stamina"
        );

        // Cooldown must be set.
        assert!(
            combat.fighters[0].cooldowns.contains_key(qs_uuid),
            "funded cast must set the ability cooldown"
        );

        // At least one op65 PlayerStatsUpdate (GMID 65) must be emitted.
        let has_op65 = out.iter().any(|(_, frame)| {
            frame.len() >= 2
                && frame[1] == 0x36
                && messages::user_message_gmid(frame) == Some(65)
        });
        assert!(
            has_op65,
            "funded cast must emit at least one PlayerStatsUpdate (op65) to update HUD bars"
        );
    }

    // -----------------------------------------------------------------------
    // Regen tick: 5%/s stamina+magicka, ZERO in-round health regen (video-proven)
    // -----------------------------------------------------------------------

    /// Video ground-truth (s293 §1): stamina and magicka recover at ~5 %/s.
    /// One regen tick on a half-depleted pool must add ≈5% of max and emit op65.
    #[test]
    fn regen_tick_raises_stamina_at_5pct_per_second() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        // Drain stamina to simulate a spent ability.
        let max_stam = combat.fighters[0].max_stamina;
        combat.fighters[0].stamina = max_stam / 2;
        let stam_before = combat.fighters[0].stamina;

        // Advance time by exactly one regen interval.
        let tick_now = now + REGEN_TICK_INTERVAL;
        combat.last_regen_tick = now; // ensure the tick fires

        let out = apply_regen_tick(&mut combat, tick_now);

        let stam_after = combat.fighters[0].stamina;
        // Must increase by ~5% of max (±1 for rounding).
        let expected_regen = ((STAMINA_REGEN_RATE_PER_S * max_stam as f32).round() as u32).max(1);
        assert_eq!(
            stam_after - stam_before, expected_regen,
            "regen tick must add ~5% of max stamina ({} expected), stam {stam_before}→{stam_after}",
            expected_regen,
        );

        // op65 PlayerStatsUpdate must be emitted (HUD update for both players).
        let has_op65 = out.iter().any(|(_, frame)| {
            frame.len() >= 2
                && frame[1] == 0x36
                && messages::user_message_gmid(frame) == Some(65)
        });
        assert!(
            has_op65,
            "regen tick must emit at least one PlayerStatsUpdate (op65)"
        );
    }

    /// Video ground-truth (s293 §1): magicka recovers at ~5 %/s, symmetric with stamina.
    #[test]
    fn regen_tick_raises_magicka_at_5pct_per_second() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        let max_mag = combat.fighters[0].max_magicka;
        combat.fighters[0].magicka = max_mag / 4; // 25% of max
        let mag_before = combat.fighters[0].magicka;

        let tick_now = now + REGEN_TICK_INTERVAL;
        let out = apply_regen_tick(&mut combat, tick_now);

        let mag_after = combat.fighters[0].magicka;
        let expected_regen = ((MAGICKA_REGEN_RATE_PER_S * max_mag as f32).round() as u32).max(1);
        assert_eq!(
            mag_after - mag_before, expected_regen,
            "regen tick must add ~5% of max magicka ({expected_regen} expected), mag {mag_before}→{mag_after}",
        );
        let _ = out; // op65 emission already verified in the stamina test
    }

    /// Video ground-truth (s293 §1): health has ZERO in-round passive regen.
    /// A regen tick must NOT increase health, even when the fighter is damaged.
    #[test]
    fn regen_tick_does_not_regen_health() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        // Damage the fighter so health is below max.
        let max_hp = combat.fighters[0].max_health;
        combat.fighters[0].health = max_hp / 2;
        let hp_before = combat.fighters[0].health;

        let tick_now = now + REGEN_TICK_INTERVAL;
        let out = apply_regen_tick(&mut combat, tick_now);

        let hp_after = combat.fighters[0].health;
        assert_eq!(
            hp_after, hp_before,
            "in-round health must NOT regen (video-proven zero): hp was {hp_before}, got {hp_after}"
        );
        // The tick may still emit op65 if stamina/magicka changed, but HP must be static.
        let _ = out;
    }

    /// tracker #24: op65 propId 5 is `_pvpOtherActorStats` — the OPPONENT of the
    /// avatar named at propId 0, exactly as `receive_damage` and
    /// `player_channeling_state_change` already fill it.
    ///
    /// It shipped as the literal `1`. `PackedStats` puts the stat word in the HIGH
    /// 32 bits and the sequence id in the LOW 32, so `1` decodes to
    /// `health 0 / stamina 0 / magicka 0, seq 1` — every op65 told both clients the
    /// other fighter's three bars were empty. The regen tick emits one ~1/s per
    /// fighter with a moved pool, i.e. for essentially the whole fight.
    #[test]
    fn regen_tick_op65_carries_the_opponents_real_stats_not_a_placeholder() {
        use super::super::state::PackedStats;
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        // Only slot 0's pool moves, so the single op65 is named on slot 0's avatar
        // and its propId 5 must therefore describe slot 1.
        combat.fighters[0].stamina = combat.fighters[0].max_stamina / 2;
        combat.last_regen_tick = now;
        let out = apply_regen_tick(&mut combat, now + REGEN_TICK_INTERVAL);

        let (_, frame) = out
            .iter()
            .find(|(_, f)| messages::user_message_gmid(f) == Some(65))
            .expect("the regen tick emits an op65 PlayerStatsUpdate");
        let nd = arena_proto::parse_netdata(&frame[2..]);
        assert_eq!(
            nd.int(0),
            Some(combat.fighters[0].net_object_id as i64),
            "propId 0 names slot 0's avatar, so propId 5 is slot 1's",
        );
        let other = match nd.get(5) {
            Some(arena_proto::NetDataValue::ULong(v)) => *v,
            got => panic!("propId 5 must be a ULong, got {got:?}"),
        };
        assert_eq!(
            other,
            combat.fighters[1].packed_stats(),
            "propId 5 must be the OPPONENT's packed_stats()",
        );
        let (h, s, m, _) = PackedStats::unpack(other);
        assert!(
            h > 0 && s > 0 && m > 0,
            "an untouched opponent must read as full, not empty — got h={h} s={s} m={m} \
             (the old hardcoded `1` decoded to 0/0/0)",
        );
    }

    // -----------------------------------------------------------------------
    // Bug 4: Held-charge crit (arena-charge-decode.md §5)
    // -----------------------------------------------------------------------

    /// Build a synthetic op46 (`PlayerCombatInputActivate`, carrier `0x2e`) frame.
    ///
    /// Wire layout per `arena-charge-decode.md` §2:
    /// ```
    /// [0x84][0x2e] + netObjId(4 bytes LE) + blockZone(1) + separator(0xcc) +
    /// chargeTimePacked(4 bytes, MSB=b[11])
    /// ```
    /// `held=true` → bit0 of b[11] = 1 (button DOWN).
    /// `held=false` → bit0 of b[11] = 0 (button UP / commit).
    fn make_op46_frame(net_obj_id: u32, held: bool) -> Vec<u8> {
        let mut frame = vec![
            0x84u8, // C2S marker
            0x2eu8, // carrier = 0x2e (GameMessageId::PlayerCombatInputActivate = 46)
        ];
        // netObjectId u32 LE (4 bytes)
        frame.extend_from_slice(&net_obj_id.to_le_bytes());
        // _isWithinBlockZone byte + structural separator
        frame.push(0x00); // blockZone (not decoded, any value)
        frame.push(0xcc); // separator
        // _clientChargeTime f32 LE packed with _held in bit0 of MSB (byte [11]).
        // Use a representative chargeTime of 52.22s (s293 swing1 chargeTime, both directions).
        // DOWN: raw bytes e1 e2 50 43; UP: e1 e2 50 42 (bit0 of MSB flipped).
        let (b8, b9, b10, b11): (u8, u8, u8, u8) = if held {
            (0xe1, 0xe2, 0x50, 0x43) // DOWN: bit0 of MSB = 1
        } else {
            (0xe1, 0xe2, 0x50, 0x42) // UP: bit0 of MSB = 0
        };
        frame.extend_from_slice(&[b8, b9, b10, b11]);
        frame
    }

    /// Op46 DOWN (button press): records `charge_press_at`, emits ZERO damage, and
    /// broadcasts op45 `PlayerChargingStateChange` — the charge/combo circle — to
    /// BOTH viewers. Retail sends op45 on every charge (13,060 captured frames);
    /// sending none is why a plain swing showed no circle.
    #[test]
    fn op46_down_broadcasts_charging_state_and_no_damage() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        let down_frame = make_op46_frame(0x1234_5678, true);
        let resolved = on_c2s_input(&mut combat, 0, &down_frame, now);
        assert!(resolved.is_empty(), "the press itself emits nothing — the drain does");

        assert!(
            combat.fighters[0].charge_press_at.is_some(),
            "op46 DOWN must record charge_press_at"
        );
        assert_eq!(
            combat.fighters[0].actor_state(),
            super::super::state::ActorStateType::Charging,
            "op46 DOWN must enter the Charging wind-up"
        );

        // Both viewers get it: the charging player (own circle) and the opponent
        // (sees the wind-up). The frames come from the actor-state drain, which the
        // engine runs at the end of every on_c2s/on_tick.
        let out = drain_state_changes(&mut combat, now);
        assert_eq!(out.len(), 2, "op45 must go to both viewers");
        let viewers: Vec<usize> = out.iter().map(|(v, _)| *v).collect();
        assert!(viewers.contains(&0), "the charging player gets its own circle");
        assert!(viewers.contains(&1), "the opponent sees the wind-up");

        for (_, body) in &out {
            assert_eq!(body[1], 0x36, "carrier 0x36");
            let nd = arena_proto::parse_netdata(&body[2..]);
            assert_eq!(nd.int(3), Some(45), "gmid 45 PlayerChargingStateChange");
            assert_eq!(nd.int(1), Some(56), "Avatar");
            assert_eq!(nd.int(6), Some(2), "ActorStateType charging = 2 (constant in all captures)");
            assert!(
                matches!(nd.int(9), Some(2) | Some(3)),
                "ActiveSide must be Left(2)/Right(3) — captures never show Middle here"
            );
            // Not damage: no ReceiveDamage anywhere in the burst.
            assert_ne!(nd.int(3), Some(50), "op46 DOWN must not emit damage");
        }
    }

    /// Build a 2-player combat with pure physical weapon (no enchants), allowing exact
    /// damage-ratio checks without the enchant track's fixed contribution diluting the ratio.
    fn make_live_combat_no_enchant(now: Instant, weight: super::super::tables::Weight) -> MatchCombat {
        use super::super::loadout::starter;
        let mut combat = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj_id = combat.alloc_net_object_id();
            let mut f = Fighter::new(slot, obj_id, starter(), now);
            f.loadout.weapon = super::super::state::WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 113.82)],
                weight: Some(weight),
            };
            f.loadout.weapon_template = None; // synthetic profile → fallback cadence
            // No enchants → pure physical, ratio of crit:uncharged == swing_factor exactly.
            f.loadout.enchants = vec![];
            combat.fighters.push(f);
        }
        combat.match_net_object_id = combat.alloc_net_object_id();
        combat.phase = FlowState::StateTimeout;
        combat.phase_entered = now;
        combat
    }

    // -----------------------------------------------------------------------
    // Report #24: a shield bash must NOT emit op53 PlayerChannelingStateChange
    // -----------------------------------------------------------------------

    /// The five bash abilities, by shipped template UUID.
    const BASHES: [(&str, &str); 5] = [
        ("cc768bae-a063-4885-8207-f39c6542fb36", "Guardbreaker"),
        ("69ffa3fd-deb7-4824-bab6-ac6450f19676", "Harrying Bash"),
        ("ba61ce46-163f-4a61-8ede-f5b7ae365e40", "Reflecting Bash"),
        ("f9a2373b-a84f-4716-90ce-165baa2dd6ed", "Shield Bash"),
        ("9b915ec3-c63b-4b62-b417-4c5436d45fc1", "Staggering Bash"),
    ];
    const FIREBALL: &str = "d07a8d30-9a1c-49b0-866d-97a8aa1534cf";

    /// Cast `uuid` from slot 0 at slot 1 through the real cast path and return the
    /// emitted frames.
    fn cast(
        combat: &mut MatchCombat,
        uuid: &str,
        tag: AbilityTag,
        now: Instant,
    ) -> Vec<(usize, Vec<u8>)> {
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: uuid.to_string(),
            level: 1,
            tag,
        });
        let frame = messages::request_execute_ability(
            combat.fighters[0].net_object_id,
            uuid,
        );
        let ea = input::parse_execute_ability(&frame).expect("synthesised op37 must parse");
        resolve_ability_cast(combat, 0, 1, &frame, &ea, now)
    }

    fn gmids(out: &[(usize, Vec<u8>)]) -> Vec<u8> {
        out.iter()
            .filter_map(|(_, f)| messages::user_message_gmid(f))
            .collect()
    }

    /// The production classifier must call every bash a `Maneuver`, or the gate in
    /// `resolve_ability_cast` never fires on a real loadout and the fix is inert in
    /// production while the test below still passes.
    #[test]
    fn the_five_bashes_classify_as_maneuvers() {
        for (uuid, name) in BASHES {
            assert_eq!(
                super::super::loadout::ability_tag_for_template(uuid),
                AbilityTag::Maneuver,
                "{name} must classify as a Maneuver for the op53 gate to apply",
            );
        }
        assert_eq!(
            super::super::loadout::ability_tag_for_template(FIREBALL),
            AbilityTag::Damage,
            "Fireball is the control - it must NOT be a Maneuver",
        );
    }

    /// The wire split, measured over 60 decrypted sessions: after a bash op38 echo
    /// retail sends op58 `PlayerManeuverStateChange` 785 times out of 788 and op53
    /// zero times; after a spell echo it sends op53 1,324 of 1,330 and op58 zero.
    /// We sent op53 for both and never sent op58 at all, so a bash had no animation
    /// frame on the wire — report #24's missing shield-bash animation.
    #[test]
    fn a_bash_animates_on_op58_not_op53() {
        for (uuid, name) in BASHES {
            let now = Instant::now();
            let mut combat = make_live_combat(now);
            let out = cast(&mut combat, uuid, AbilityTag::Maneuver, now);
            let ids = gmids(&out);

            // Non-vacuity: the cast must actually have resolved. Without this, an
            // ability rejected on cost or cooldown would emit nothing at all and
            // the op53 assertion would pass for entirely the wrong reason.
            assert!(
                ids.contains(&38),
                "{name}: expected the op38 cast echo, got gmids {ids:?}",
            );
            assert!(
                ids.contains(&58),
                "{name}: a bash must animate on op58 — retail sends one after 100% \
                 of 788 captured bash echoes. Got gmids {ids:?}",
            );
            assert!(
                !ids.contains(&53),
                "{name}: a bash must not emit op53 — retail sends none across those \
                 same 788 echoes. Got gmids {ids:?}",
            );
        }
    }

    /// The op58 must carry the right animation, or the client plays the wrong one —
    /// which from the player's seat is indistinguishable from playing none.
    /// `ShieldBashBegin` (26) is what all four shield bashes send in every captured
    /// frame; Guardbreaker sends its own member (13) and is the discriminator here,
    /// since a mapping that returned 26 for everything would otherwise pass.
    #[test]
    fn op58_carries_the_captured_actor_animation() {
        // (uuid, name, propId-10 value observed in EVERY captured op58 for it)
        let pinned: [(&str, &str, u8); 6] = [
            ("f9a2373b-a84f-4716-90ce-165baa2dd6ed", "Shield Bash", 26),
            ("9b915ec3-c63b-4b62-b417-4c5436d45fc1", "Staggering Bash", 26),
            ("69ffa3fd-deb7-4824-bab6-ac6450f19676", "Harrying Bash", 26),
            ("ba61ce46-163f-4a61-8ede-f5b7ae365e40", "Reflecting Bash", 26),
            ("cc768bae-a063-4885-8207-f39c6542fb36", "Guardbreaker", 13),
            ("eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13", "Quick Strikes", 5),
        ];
        for (uuid, name, want) in pinned {
            let got = super::super::loadout::actor_animation_for_maneuver(uuid)
                .unwrap_or_else(|| panic!("{name}: no ActorAnimation resolved"));
            assert_eq!(
                got as u8, want,
                "{name}: op58 propId 10 is {want} in every captured frame, got {}",
                got as u8,
            );
        }
    }

    /// A spell must not resolve a maneuver animation — the guard that stops a future
    /// edit from emitting op58 for everything.
    #[test]
    fn a_spell_resolves_no_maneuver_animation() {
        assert!(
            super::super::loadout::actor_animation_for_maneuver(FIREBALL).is_none(),
            "Fireball is a spell — it animates on op53 and must resolve no ActorAnimation",
        );
    }

    /// The control, and what makes the test above non-vacuous: a real channelled
    /// spell on the same path still gets its op53. A gate that suppressed op53
    /// wholesale would pass the bash test and fail this one.
    #[test]
    fn a_spell_still_emits_its_channeling_frame() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let out = cast(&mut combat, FIREBALL, AbilityTag::Damage, now);
        let ids = gmids(&out);
        assert!(
            ids.contains(&38),
            "Fireball: expected the op38 cast echo, got gmids {ids:?}",
        );
        assert!(
            ids.contains(&53),
            "Fireball is channelled - it must still emit op53. Got gmids {ids:?}",
        );
    }

    // -----------------------------------------------------------------------
    // Cast time: a spell lands after its wind-up, not at the button press
    // -----------------------------------------------------------------------

    const ICE_SPIKE: &str = "cfee0b02-6d91-4d34-869c-a7e54329060d";
    const FROSTBITE: &str = "4be1d681-c35d-4540-b255-c2910ac80664";
    const PARALYZE: &str = "9fdc4d52-ce90-44f8-9b5d-21f31e27dbda";
    const STAGGERING_BASH: &str = "9b915ec3-c63b-4b62-b417-4c5436d45fc1";
    const PIERCING_STRIKES: &str = "cdab44fb-6ff6-4701-a4ec-d19cce79e49f";

    /// The delays are read from shipped rank data, so pin the actual numbers. If a
    /// gamedata regeneration changes them, that should be a visible decision rather
    /// than a silent shift in how the game feels.
    #[test]
    fn the_shipped_cast_times_are_what_we_defer_by() {
        for (uuid, name, want_ms) in [
            (ICE_SPIKE, "Ice Spike", 1120u64),
            (PARALYZE, "Paralyze", 1500),
            (FROSTBITE, "Frostbite", 0),
            (STAGGERING_BASH, "Staggering Bash", 500),
        ] {
            let got = super::ability_impact_delay(uuid, 1).as_millis() as u64;
            assert_eq!(
                got, want_ms,
                "{name}: shipped channelDuration says {want_ms} ms, got {got} ms",
            );
        }
    }

    /// Every retail op58 carries a state-history blob whose newest state is the
    /// maneuver itself. Production classified Piercing Strikes correctly and sent
    /// animation 15, but omitted this property; the client displayed a spell-like
    /// fallback instead of the strike animation.
    #[test]
    fn piercing_strikes_op58_carries_maneuver_state_history() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let out = cast(&mut combat, PIERCING_STRIKES, AbilityTag::Maneuver, now);
        let frame = out
            .iter()
            .map(|(_, f)| f)
            .find(|f| messages::user_message_gmid(f) == Some(58))
            .expect("Piercing Strikes must emit op58");
        let nd = arena_proto::parse_netdata(&frame[2..]);
        let blob = match nd.props.get(&7) {
            Some(arena_proto::NetDataValue::ByteArray(blob)) => blob,
            other => panic!("op58 propId 7 must be state history, got {other:?}"),
        };
        assert_eq!(blob.last().copied(), Some(ActorStateType::Maneuver as u8));
        assert_eq!(nd.int(10), Some(15), "Piercing Strikes animation id");
    }

    /// A shield bash's authored 0.50 s is its guarding phase, not a guard that begins
    /// after the damage. Staggering Bash may stun an unblocking target only when its
    /// second, striking phase lands.
    #[test]
    fn staggering_bash_guards_first_then_strikes_and_staggers() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let hp_before = combat.fighters[1].health;

        let cast_frames = cast(
            &mut combat,
            STAGGERING_BASH,
            AbilityTag::Maneuver,
            now,
        );
        let cast_ids = gmids(&cast_frames);
        assert!(cast_ids.contains(&38));
        assert!(cast_ids.contains(&58), "compound bash animation starts immediately");
        assert!(!cast_ids.contains(&50), "the first phase must deal no damage");
        assert_eq!(combat.fighters[1].health, hp_before);
        assert!(!combat.fighters[1].is_staggered(now));
        assert_eq!(combat.fighters[0].block_phase(now), Some(BlockPhase::Optimal));

        let early = super::land_due_impacts(&mut combat, now + Duration::from_millis(499));
        assert!(early.is_empty(), "the strike cannot land during the guard phase");
        assert_eq!(combat.fighters[1].health, hp_before);

        let impact = now + Duration::from_millis(501);
        let landed = super::land_due_impacts(&mut combat, impact);
        assert!(gmids(&landed).contains(&50), "the second phase deals weapon damage");
        assert!(combat.fighters[1].health < hp_before);
        assert!(
            combat.fighters[1].is_staggered(impact),
            "an unblocking target is staggered only at the strike phase",
        );
    }

    /// Report from live play: "Ice Spike now stuns, but it does it instantaneously,
    /// and in recordings there is a delay from sending the spike to them being
    /// stunned."
    ///
    /// Ice Spike ships `channelDuration` 1.12 s. We applied its damage and stun at
    /// the instant of the cast, so the target was stunned before the spike had
    /// visibly left the caster's hand.
    #[test]
    fn ice_spike_impact_waits_for_its_cast_time() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let hp_before = combat.fighters[1].health;

        let out = cast(&mut combat, ICE_SPIKE, AbilityTag::Damage, now);
        let ids = gmids(&out);

        // Non-vacuity: the cast itself must have resolved.
        assert!(ids.contains(&38), "expected the op38 cast echo, got {ids:?}");
        // ...but nothing may have LANDED yet.
        assert!(
            !ids.contains(&50),
            "Ice Spike must not deal damage at cast time — it ships a 1.12 s wind-up. \
             Got gmids {ids:?}",
        );
        assert_eq!(
            combat.fighters[1].health, hp_before,
            "target health must be untouched during the wind-up",
        );

        // Just before the wind-up completes: still nothing.
        let early = super::land_due_impacts(&mut combat, now + Duration::from_millis(1100));
        assert!(early.is_empty(), "impact landed early: {:?}", gmids(&early));

        // After it: the damage arrives.
        let late = super::land_due_impacts(&mut combat, now + Duration::from_millis(1130));
        assert!(
            gmids(&late).contains(&50),
            "Ice Spike must land once its 1.12 s wind-up elapses, got {:?}",
            gmids(&late),
        );
        assert!(
            combat.fighters[1].health < hp_before,
            "target must actually take the damage on impact",
        );
    }

    /// The control, and the reason this is not just "delay everything": Frostbite
    /// ships NO `channelDuration` — it is a channelled stream, not a projectile — so
    /// it must still land immediately. A change that deferred every ability would
    /// pass the test above and fail this one.
    #[test]
    fn an_ability_with_no_cast_time_still_lands_immediately() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let out = cast(&mut combat, FROSTBITE, AbilityTag::Damage, now);
        let ids = gmids(&out);
        assert!(ids.contains(&38), "expected the op38 cast echo, got {ids:?}");
        assert!(
            ids.contains(&50),
            "Frostbite ships no cast time and must land at once, got {ids:?}",
        );
        assert!(
            combat.pending_impacts.is_empty(),
            "an ability with no wind-up must not be queued",
        );
    }

    /// A queued cast must not be lost if the round ends first, and must not fire
    /// against a slot that no longer exists.
    #[test]
    fn a_queued_impact_is_drained_only_once() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        cast(&mut combat, ICE_SPIKE, AbilityTag::Damage, now);
        assert_eq!(combat.pending_impacts.len(), 1);

        let first = super::land_due_impacts(&mut combat, now + Duration::from_millis(1200));
        assert!(gmids(&first).contains(&50));
        assert!(combat.pending_impacts.is_empty(), "queue must be emptied on delivery");

        let second = super::land_due_impacts(&mut combat, now + Duration::from_millis(2000));
        assert!(second.is_empty(), "a delivered impact must not fire twice");
    }

    /// Op46 UP after a FULL-CHARGE hold (≥ the equipped backswing) → crit ×1.325 on a Light weapon.
    /// Damage must be GREATER than an uncharged swing (×1.0) on the same fighter.
    /// Ratio must be ≈×1.325 (within 1% — integer rounding tolerance on an exact formula).
    #[test]
    fn op46_full_charge_light_weapon_applies_crit_multiplier() {
        let now = Instant::now();
        // No-enchant combat so the physical damage ratio is clean (not diluted by fixed enchant).
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);

        // Simulate a full-charge hold: press at t=0, release after the template gate.
        let press_time = now;
        combat.fighters[0].charge_press_at = Some(press_time);
        let expected_factor = combat.fighters[0].loadout.critical_damage_factor();
        let release_time =
            press_time + Duration::from_secs_f32(critical_hold_secs(&combat.fighters[0]) + 0.5);

        let up_frame = make_op46_frame(0x1234_5678, false);
        let mut out = on_c2s_input(&mut combat, 0, &up_frame, release_time);
        out.extend(land(&mut combat, release_time));

        // Must emit ReceiveDamage frames (not empty).
        assert!(!out.is_empty(), "full-charge op46 UP must emit damage frames");

        // charge_press_at must be cleared after the commit.
        assert!(
            combat.fighters[0].charge_press_at.is_none(),
            "charge_press_at must be cleared after op46 UP commit"
        );

        // Measure the Slashing damage from the ReceiveDamage: compare against an
        // uncharged swing resolved directly via resolve_swing(×1.0).
        // The crit (×1.325 Light) must produce strictly MORE damage than ×1.0.
        let mut uncharged_combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);
        let _uncharged_out = swing_and_land(&mut uncharged_combat, 0, 1, 1.0, now);

        // The charged combat emitted frames → the target (slot 1) received some HP reduction.
        let crit_hp_after = combat.fighters[1].health;
        let norm_hp_after = uncharged_combat.fighters[1].health;
        let crit_dealt = combat.fighters[1].max_health.saturating_sub(crit_hp_after);
        let norm_dealt = uncharged_combat.fighters[1].max_health.saturating_sub(norm_hp_after);

        assert!(
            crit_dealt > norm_dealt,
            "full-charge crit (×{expected_factor}) must deal MORE damage than an uncharged swing: \
             crit dealt {crit_dealt}, uncharged dealt {norm_dealt}"
        );

        // The ratio must be approximately this template's maxDamageFactor (1.325),
        // within 2% (rounding tolerance).
        // No enchants → ratio is pure physical = swing_factor (1.325 crit / 1.0 normal).
        let ratio = crit_dealt as f32 / norm_dealt as f32;
        let _ = out; // suppress unused warning
        assert!(
            (ratio - expected_factor).abs() < 0.02,
            "damage ratio must be ≈×{expected_factor} (Light crit), got ×{ratio:.4} \
             (crit={crit_dealt}, normal={norm_dealt})"
        );
    }

    /// Op46 UP after a FULL-CHARGE hold with a Heavy weapon → crit ×1.987.
    #[test]
    fn op46_full_charge_heavy_weapon_applies_crit_multiplier() {
        let now = Instant::now();
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Heavy);

        let expected_factor = combat.fighters[0].loadout.critical_damage_factor();
        combat.fighters[0].charge_press_at = Some(
            now - Duration::from_secs_f32(critical_hold_secs(&combat.fighters[0]) + 0.3),
        );

        let up_frame = make_op46_frame(0x1234_5678, false);
        let mut out = on_c2s_input(&mut combat, 0, &up_frame, now);
        out.extend(land(&mut combat, now));

        assert!(!out.is_empty(), "full-charge Heavy op46 UP must emit damage");

        // Compare against uncharged heavy.
        let mut uncharged = make_live_combat_no_enchant(now, super::super::tables::Weight::Heavy);
        let _ = swing_and_land(&mut uncharged, 0, 1, 1.0, now);

        let crit_dealt = combat.fighters[1].max_health.saturating_sub(combat.fighters[1].health);
        let norm_dealt = uncharged.fighters[1].max_health.saturating_sub(uncharged.fighters[1].health);

        let ratio = crit_dealt as f32 / norm_dealt as f32;
        assert!(
            (ratio - expected_factor).abs() < 0.02,
            "Heavy crit ratio must be ≈×{expected_factor}, got ×{ratio:.4}"
        );
    }

    /// A hold of the length players actually use must be able to CRIT.
    ///
    /// The old flat threshold was 1.2 s against a measured MAXIMUM hold of 1.73 s and
    /// a median of 0.317 s, so a crit was very nearly unreachable. This fails if the
    /// threshold is ever put back above what a human actually holds.
    #[test]
    fn a_typical_player_hold_can_crit() {
        const MEASURED_MEDIAN_HOLD: f32 = 0.3167;
        for (weight, name) in [
            (super::super::tables::Weight::Light, "Light"),
            (super::super::tables::Weight::Versatile, "Versatile"),
            (super::super::tables::Weight::Heavy, "Heavy"),
        ] {
            let now = Instant::now();
            let combat = make_live_combat_no_enchant(now, weight);
            let factor = charge_crit_factor(&combat.fighters[0], MEASURED_MEDIAN_HOLD);
            assert!(
                factor > 1.0,
                "{name}: a median-length hold ({MEASURED_MEDIAN_HOLD}s) must crit, got x{factor}"
            );
            let below = critical_hold_secs(&combat.fighters[0]) * 0.5;
            assert_eq!(
                charge_crit_factor(&combat.fighters[0], below),
                1.0,
                "{name}: a hold below the backswing must NOT crit"
            );
        }
    }

    /// Op46 UP after a SHORT hold (< the equipped backswing) → normal swing ×1.0 (no crit).
    /// Damage must equal an uncharged swing (no crit boost applied).
    #[test]
    fn op46_short_hold_partial_charge_no_crit() {
        let now = Instant::now();
        // No-enchant so the comparison is exact (no rounding from fixed enchant contribution).
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);

        // Press at t=0, release halfway to the template's full-charge gate.
        let press_time = now;
        combat.fighters[0].charge_press_at = Some(press_time);
        let release_time =
            press_time + Duration::from_secs_f32(critical_hold_secs(&combat.fighters[0]) / 2.0);

        let up_frame = make_op46_frame(0x1234_5678, false);
        let _ = on_c2s_input(&mut combat, 0, &up_frame, release_time);
        let _ = land(&mut combat, release_time);

        // Resolve an uncharged swing on a fresh combat at the same `release_time`.
        let mut uncharged = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);
        let _ = swing_and_land(&mut uncharged, 0, 1, 1.0, release_time);

        let partial_dealt = combat.fighters[1].max_health.saturating_sub(combat.fighters[1].health);
        let normal_dealt = uncharged.fighters[1].max_health.saturating_sub(uncharged.fighters[1].health);

        // Partial charge must be equal to uncharged (×1.0, no crit boost).
        assert_eq!(
            partial_dealt, normal_dealt,
            "partial hold (below the weapon threshold) must NOT crit: partial dealt {partial_dealt}, \
             uncharged dealt {normal_dealt}"
        );
    }

    /// `parse_op46_held` unit tests — verify the bit extraction from the wire bytes.
    #[test]
    fn parse_op46_held_detects_held_flag() {
        // Exact s293 DOWN frame bytes: e1 e2 50 43 → b[11]=0x43, bit0=1 → DOWN
        let down = make_op46_frame(0x1FEDC7B1, true);
        assert_eq!(parse_op46_held(&down), Some(true), "s293-derived DOWN frame: held=1");

        // Exact s293 UP frame bytes: e1 e2 50 42 → b[11]=0x42, bit0=0 → UP
        let up = make_op46_frame(0x1FEDC7B1, false);
        assert_eq!(parse_op46_held(&up), Some(false), "s293-derived UP frame: held=0");

        // Non-op46 frame (carrier 0x36) must return None.
        let non46 = vec![0x84u8, 0x36u8, 0x00u8, 0x00u8, 0x00u8, 0x00u8,
                         0x00u8, 0x00u8, 0x00u8, 0x00u8, 0x00u8, 0x00u8];
        assert_eq!(parse_op46_held(&non46), None, "non-op46 carrier must return None");

        // Frame too short must return None.
        let short = vec![0x84u8, 0x2eu8, 0x01u8];
        assert_eq!(parse_op46_held(&short), None, "too-short op46 frame must return None");
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a minimal 2-player `MatchCombat` already in the live `StateTimeout` phase.
    pub(super) fn make_live_combat(now: Instant) -> MatchCombat {
        use super::super::loadout::starter;
        let mut combat = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj_id = combat.alloc_net_object_id();
            let mut f = Fighter::new(slot, obj_id, starter(), now);
            // Give fighters full weapon base so damage resolves properly.
            f.loadout.weapon = super::super::state::WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 113.82)],
                weight: Some(super::super::tables::Weight::Light),
            };
            f.loadout.weapon_template = None; // synthetic profile → fallback cadence
            combat.fighters.push(f);
        }
        combat.match_net_object_id = combat.alloc_net_object_id();
        combat.phase = FlowState::StateTimeout;
        combat.phase_entered = now;
        combat
    }

    /// Build a synthetic `RequestExecuteAbility` (GMID 37) c2s frame for the given
    /// ability `uuid`, using the same builder as the production bot path.
    fn make_ability_frame(obj_id: i32, uuid: &str) -> Vec<u8> {
        messages::request_execute_ability(obj_id, uuid)
    }

    // -----------------------------------------------------------------------
    // BUG 3: per-weapon-class swing cadence (no more swing-spam)
    // -----------------------------------------------------------------------

    /// A second swing that arrives BEFORE the weapon's swing interval has elapsed is
    /// REJECTED (no damage), and one that arrives AFTER lands. Proves attacks resolve at
    /// the weapon cadence, not instantly.
    #[test]
    fn second_swing_before_cooldown_is_rejected() {
        use super::super::tables::Weight;
        let now = Instant::now();
        let mut combat = make_live_combat_no_enchant(now, Weight::Light);
        let interval = tables::fallback_swing_interval(Weight::Light); // 400 ms

        // First swing lands.
        let out1 = swing_and_land(&mut combat, 0, 1, 1.0, now);
        assert!(!out1.is_empty(), "first swing lands (emits ReceiveDamage)");
        let hp_after_first = combat.fighters[1].health;

        // Second swing HALF an interval later → rejected, no additional damage.
        let too_soon = now + interval / 2;
        let out2 = swing_and_land(&mut combat, 0, 1, 1.0, too_soon);
        assert!(out2.is_empty(), "a swing before the weapon cadence elapses is rejected");
        assert_eq!(combat.fighters[1].health, hp_after_first, "rejected swing deals no damage");

        // A swing just past the interval lands again.
        let ok_time = now + interval + Duration::from_millis(1);
        let out3 = swing_and_land(&mut combat, 0, 1, 1.0, ok_time);
        assert!(!out3.is_empty(), "a swing after the cadence elapses lands");
        assert!(combat.fighters[1].health < hp_after_first, "the cadence-legal swing deals damage");
    }

    /// Spamming N swing inputs in a short window resolves only the cadence-allowed
    /// number. Fire 20 swings across 1 second on a Light weapon (400 ms interval) → at
    /// most 3 land (t=0, ~0.4s, ~0.8s), not 20.
    #[test]
    fn spamming_swings_resolves_only_cadence_allowed_count() {
        use super::super::tables::Weight;
        let now = Instant::now();
        let mut combat = make_live_combat_no_enchant(now, Weight::Light);

        let window = Duration::from_secs(1);
        let n = 20u32;
        let mut landed = 0u32;
        for i in 0..n {
            // 20 evenly-spaced inputs across the 1s window (~50 ms apart — spam).
            let t = now + window * i / n;
            if !swing_and_land(&mut combat, 0, 1, 1.0, t).is_empty() {
                landed += 1;
            }
        }
        // 400 ms cadence over 1s → t=0, 0.4, 0.8 = 3 landed swings. Certainly not 20.
        assert_eq!(landed, 3, "only cadence-allowed swings land (400 ms over 1 s = 3), not the {n} spammed");
    }

    /// A Heavy weapon swings SLOWER than a Light one: at a time inside the Light cadence
    /// but before the Heavy cadence, a Light fighter's second swing lands while a Heavy
    /// fighter's is still rejected.
    #[test]
    fn heavy_weapon_swings_slower_than_light() {
        use super::super::tables::Weight;
        let now = Instant::now();
        assert!(
            tables::fallback_swing_interval(Weight::Heavy) > tables::fallback_swing_interval(Weight::Light),
            "Heavy cadence must be slower than Light"
        );

        let mut light = make_live_combat_no_enchant(now, Weight::Light);
        let mut heavy = make_live_combat_no_enchant(now, Weight::Heavy);
        // First swing for both.
        assert!(!swing_and_land(&mut light, 0, 1, 1.0, now).is_empty());
        assert!(!swing_and_land(&mut heavy, 0, 1, 1.0, now).is_empty());

        // A time past the Light interval but before the Heavy interval.
        let t = now + tables::fallback_swing_interval(Weight::Light) + Duration::from_millis(1);
        assert!(t < now + tables::fallback_swing_interval(Weight::Heavy), "test time is inside the Heavy cadence");
        assert!(!swing_and_land(&mut light, 0, 1, 1.0, t).is_empty(), "Light can swing again");
        assert!(swing_and_land(&mut heavy, 0, 1, 1.0, t).is_empty(), "Heavy is still on cadence — rejected");
    }

    /// The spell/ability cooldown gate: a second cast of the SAME ability before its
    /// authoritative cooldown elapses is rejected (no `PerformExecuteAbility` echo);
    /// after the cooldown it fires again. (Fireball = 3540 ms.)
    #[test]
    fn ability_cast_is_cooldown_gated() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let fireball = "d07a8d30-9a1c-49b0-866d-97a8aa1534cf";
        let cd = ability_cooldown(fireball, 1); // shipped FireballRank1._cooldown = 3.54 s
        let frame = make_ability_frame(combat.fighters[0].net_object_id, fireball);

        let out1 = on_c2s_input(&mut combat, 0, &frame, now);
        assert!(!out1.is_empty(), "first cast fires (PerformExecuteAbility + damage)");

        let too_soon = now + cd / 2;
        let out2 = on_c2s_input(&mut combat, 0, &frame, too_soon);
        assert!(out2.is_empty(), "a re-cast before the ability cooldown elapses is rejected");

        let after = now + cd + Duration::from_millis(1);
        let out3 = on_c2s_input(&mut combat, 0, &frame, after);
        assert!(!out3.is_empty(), "the ability fires again once its cooldown elapses");
    }

    /// Report #109 (WolfWalker): "the Magic/Stamina cost is displayed, but the
    /// resources are not actually deducted … spells and abilities can be cast
    /// repeatedly without consuming resources", and "abilities have no initial
    /// cooldown".
    ///
    /// Both readings come from ONE server behaviour: a cast the resource gate
    /// refused used to `return Vec::new()`. Nothing went back to the client — no
    /// op65, so the caster's own bars never moved, and no cooldown, so the icon
    /// never greyed out. The client runs its own prediction; with the authority
    /// silent there is nothing to correct it, so the fight looks free.
    ///
    /// His own numbers make this reachable rather than theoretical: at level 89
    /// `pool_for_level` gives 640 stamina and 640 magicka, while his equipped
    /// Reckless Fury costs 425 stamina at rank 1 and Ward 205 magicka — two casts
    /// and the gate starts firing on every one.
    ///
    /// The gate itself is unchanged: the cast is still refused, still free, still
    /// sets no cooldown. It just says so now.
    #[test]
    fn a_cast_refused_for_want_of_magicka_still_reports_the_real_pools() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let fireball = "d07a8d30-9a1c-49b0-866d-97a8aa1534cf";
        // Shipped FireballRank1._magickaCost = 90.
        let (_stam, mag_cost) = tables::ability_cost(fireball, 1);
        assert!(mag_cost > 0, "the fixture ability must actually cost magicka");

        combat.fighters[0].magicka = mag_cost - 1;
        let before = combat.fighters[0].magicka;
        let frame = make_ability_frame(combat.fighters[0].net_object_id, fireball);

        let out = on_c2s_input(&mut combat, 0, &frame, now);

        // THE regression. Before the fix this was empty.
        assert!(!out.is_empty(), "a refused cast must still answer the client");
        let stats: Vec<_> = out
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(65))
            .collect();
        assert_eq!(
            stats.len(),
            combat.fighters.len(),
            "the pools go to every player, as on the commit path"
        );
        // Still refused: no cast echo, no damage, nothing but the correction.
        assert_eq!(out.len(), stats.len(), "a refused cast emits ONLY the stats frame");
        assert_eq!(
            combat.fighters[0].magicka, before,
            "the refusal must not spend the magicka it refused over"
        );

        // And no cooldown was set, so the ability is castable the moment the pool
        // is back — the refusal is not a silent lockout.
        combat.fighters[0].magicka = combat.fighters[0].max_magicka;
        let out2 = on_c2s_input(&mut combat, 0, &frame, now);
        assert!(
            out2.iter().any(|(_, f)| messages::user_message_gmid(f) == Some(38)),
            "with the pool restored the same cast fires immediately (no cooldown was set)"
        );
        assert!(
            combat.fighters[0].magicka < combat.fighters[0].max_magicka,
            "and NOW the cost is deducted — the control that makes the assertions above \
             about the refusal path rather than about a gate that never charges"
        );
    }

    /// The same for the stamina half of the gate, because the two branches are
    /// separate `if`s and only one of them was covered by the magicka test.
    #[test]
    fn a_cast_refused_for_want_of_stamina_still_reports_the_real_pools() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        // Reckless Fury — one of WolfWalker's own equipped maneuvers, 425 stamina
        // at rank 1.
        let reckless_fury = "0cfe29cd-89d9-42ad-9227-8308e2f87c7f";
        let (stam_cost, _mag) = tables::ability_cost(reckless_fury, 1);
        assert_eq!(stam_cost, 425, "his rank-1 cost, from the shipped RecklessFuryRank1");

        combat.fighters[0].stamina = stam_cost - 1;
        let before = combat.fighters[0].stamina;
        let frame = make_ability_frame(combat.fighters[0].net_object_id, reckless_fury);

        let out = on_c2s_input(&mut combat, 0, &frame, now);
        assert!(
            out.iter().any(|(_, f)| messages::user_message_gmid(f) == Some(65)),
            "a stamina refusal must report the pools too"
        );
        assert_eq!(combat.fighters[0].stamina, before, "and spend nothing");
    }

    /// A level-89 fighter — WolfWalker's level — gets 640 of each pool from
    /// `pool_for_level`, and that is a PLACEHOLDER curve, not the retail one: it
    /// ignores the attribute split entirely. He has spent 46 points on stamina and
    /// 3 on magicka, and our formula hands him the same 640 of both.
    ///
    /// This test does not assert the right answer, because the right answer is in
    /// `ActorInnateStats` / `LevelUpData` in the APK and is not extracted yet. It
    /// pins the CURRENT number so that wiring the real curve is a deliberate change
    /// with a failing test attached, rather than something that drifts silently
    /// underneath the resource gate.
    #[test]
    fn the_level_89_pool_is_still_the_placeholder_curve() {
        use super::super::state::pool_for_level;
        assert_eq!(pool_for_level(89), 640, "200 + 5 * (level - 1)");
        // Reckless Fury rank 1 alone is two thirds of it.
        let (stam_cost, _) = tables::ability_cost("0cfe29cd-89d9-42ad-9227-8308e2f87c7f", 1);
        assert!(
            stam_cost * 2 > pool_for_level(89),
            "one rank-1 maneuver costs more than half the whole pool ({stam_cost} of {})",
            pool_for_level(89)
        );
    }

    // -----------------------------------------------------------------------
    // Tracker #31 — "Frostbite gives no damage and does not freeze the opponent"
    //
    // These drive the REAL emission path (c2s op37 → resolve → s2c frames), not
    // the damage model in isolation, and assert against the retail wire recorded
    // in capture session 615 (2026-06-27 21:00:19, `docs/arena-status-resistance-spec.md`
    // §5): a Frostbite cast produces `ReceiveDamage` frames with
    // `DamageSource::ContinuousSpell (8)` carrying `Frost + Stamina` in equal
    // measure, and an op51 `Frozen (5)` apply lands within the channel.
    // -----------------------------------------------------------------------

    /// Frostbite (`4be1d681…`) — `ability_type: spell`, `damage_type: frost`,
    /// `damagePerSecond` per rank, `channelMaxLength: 3`, no `_damage` field.
    const FROSTBITE_UUID: &str = "4be1d681-c35d-4540-b255-c2910ac80664";
    /// Rank 4: `damagePerSecond = 95.80`, `magickaCost = 235` — the rank the
    /// project owner cast in prod session 911 (arena-server logged `mag=235`).
    const FROSTBITE_RANK: u8 = 4;

    /// The exact fighter pair from the reporter's prod match (arena-server,
    /// 2026-08-18 05:46:47): the caster is L86 (`maxHP 3150`, `pool 625`) and the
    /// target is L89 (`maxHP 3240`). The levels matter: the `Frozen` trigger is a
    /// fraction of the TARGET's max HP, so a low-level test fighter freezes where
    /// a real arena opponent does not.
    fn make_prod_scale_combat(now: Instant) -> MatchCombat {
        use super::super::loadout::starter;
        let mut combat = MatchCombat::new(2, 2, now);
        for (slot, level) in [(0usize, 86u16), (1, 89)] {
            let obj_id = combat.alloc_net_object_id();
            let mut lo = starter();
            lo.level = level;
            let mut f = Fighter::new(slot, obj_id, lo, now);
            f.loadout.weapon = super::super::state::WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 113.82)],
                weight: Some(super::super::tables::Weight::Light),
            };
            f.loadout.weapon_template = None;
            combat.fighters.push(f);
        }
        combat.match_net_object_id = combat.alloc_net_object_id();
        combat.phase = FlowState::StateTimeout;
        combat.phase_entered = now;
        combat
    }

    /// Cast Frostbite through `on_c2s_input` and return the s2c frames.
    fn cast_frostbite(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: FROSTBITE_UUID.to_string(),
            level: FROSTBITE_RANK,
            tag: super::super::loadout::ability_tag_for_template(FROSTBITE_UUID),
        });
        let frame = make_ability_frame(120, FROSTBITE_UUID);
        let mut out = on_c2s_input(combat, 0, &frame, now);
        out.extend(land(combat, now));
        out
    }

    /// Drive `on_tick` across a full `channelMaxLength`, returning the frames the
    /// channel emits AFTER its first tick. Steps at the real tick interval so the
    /// schedule under test is the one production uses.
    fn run_channel(combat: &mut MatchCombat, start: Instant) -> Vec<(usize, Vec<u8>)> {
        let iv = super::super::damage::CHANNEL_TICK_INTERVAL_SECS;
        let steps = (3.0 / iv).round() as u32 + 2; // channelMaxLength + slack
        let mut out = Vec::new();
        for i in 1..=steps {
            let t = start + Duration::from_secs_f32(iv * i as f32);
            out.extend(super::on_tick(combat, t, false));
        }
        out
    }

    /// Report #107 (Taheen): "on the ragdoll finish … in the case of a hit or bash
    /// or even some spells, the opponent should ragdoll, but in other cases the
    /// opponent simply collapses to the ground as if he fainted".
    ///
    /// He asked whether the death reaction is two functions or one function reading
    /// the damage. It is one, and it reads `ReceiveDamage` propId 6 (`DamageSource`)
    /// and propId 10 (`ActiveSide`) off the killing frame — so a wrong source byte
    /// gives the wrong death.
    ///
    /// Measured over 168 decoded retail op50 messages in the stored captures:
    ///
    /// ```text
    /// source 1 Attack          side 2 Left (15) / 3 Right (21)   — NEVER Middle
    /// source 3 WeaponManeuver  side 1 Middle (13)                — only ever Middle
    /// source 2 Spell           side 1 Middle (13) / 0 None (2)
    /// source 4 StatusEffect    side 0 (73)
    /// source 7 AreaEffect      side 1 (14) / 0 (2)
    /// source 8 ContinuousSpell side 1 (6)
    /// source 6 Revenge         side 0 (4)
    /// ```
    ///
    /// The maneuver arm sent `(1 Attack, 1 Middle)` — a pair that occurs **0 times
    /// in 168**, because Attack is the only source that carries a side at all and it
    /// is always Left or Right.
    ///
    /// The whole existing suite passed with the wrong byte, which is why this test
    /// exists rather than a wider one.
    #[test]
    fn a_maneuver_hit_is_labelled_weapon_maneuver_not_attack() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        // Quick Strikes — a maneuver that ships no `_damage`, so it takes the weapon
        // path this arm exists for.
        let quick_strikes = "eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13";
        combat.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: quick_strikes.to_string(),
            level: 1,
            tag: AbilityTag::Maneuver,
        });
        let frame = make_ability_frame(combat.fighters[0].net_object_id, quick_strikes);

        let mut out = on_c2s_input(&mut combat, 0, &frame, now);
        out.extend(land(&mut combat, now));

        let hits = damage_frames(&out);
        assert!(!hits.is_empty(), "the maneuver must land a damage frame at all");
        for (source, total, _) in &hits {
            assert_eq!(
                *source,
                super::super::state::DamageSource::WeaponManeuver as u8,
                "a maneuver must ride WeaponManeuver (3), not Attack (1)"
            );
            assert!(*total > 0.0, "and still deal its weapon damage");
        }

        // The side half of the pair, read straight off the frame.
        for (_, f) in out.iter().filter(|(_, f)| messages::user_message_gmid(f) == Some(50)) {
            let nd = arena_proto::parse_netdata(&f[2..]);
            assert_eq!(
                nd.int(10).unwrap_or(-1),
                super::super::state::ActiveSide::Middle as i64,
                "a maneuver is a Middle hit — the pair must be (3, Middle)"
            );
        }
    }

    /// The control that makes the test above about the maneuver arm rather than about
    /// every damage frame: a plain swing must STILL be `Attack`, and on a real side.
    #[test]
    fn a_plain_swing_is_still_attack_on_a_left_or_right_side() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);
        let out = swing_and_land(&mut combat, 0, 1, 1.0, now);
        let hits = damage_frames(&out);
        assert!(!hits.is_empty(), "the swing must land");
        for (source, _, _) in &hits {
            assert_eq!(
                *source,
                super::super::state::DamageSource::Attack as u8,
                "an ordinary swing is Attack (1) — unchanged"
            );
        }
        for (_, f) in out.iter().filter(|(_, f)| messages::user_message_gmid(f) == Some(50)) {
            let nd = arena_proto::parse_netdata(&f[2..]);
            let side = nd.int(10).unwrap_or(-1);
            assert!(
                side == super::super::state::ActiveSide::Left as i64
                    || side == super::super::state::ActiveSide::Right as i64,
                "Attack carries Left or Right, never Middle — got {side}"
            );
        }
    }

    /// Decode every `ReceiveDamage` (50) frame in `out` into
    /// `(source, total, [(damage_type, value)])`.
    fn damage_frames(out: &[(usize, Vec<u8>)]) -> Vec<(u8, f32, Vec<(u8, f32)>)> {
        let mut got = Vec::new();
        for (_, f) in out {
            if messages::user_message_gmid(f) != Some(50) {
                continue;
            }
            let nd = arena_proto::parse_netdata(&f[2..]);
            let n = nd.int(12).unwrap_or(0) as u8;
            let mut comps = Vec::new();
            for k in 0..n {
                let base = 13 + 2 * k;
                let ty = nd.int(base).unwrap_or(0) as u8;
                let v = match nd.get(base + 1) {
                    Some(arena_proto::NetDataValue::Float(v)) => *v,
                    _ => 0.0,
                };
                comps.push((ty, v));
            }
            let total = match nd.get(8) {
                Some(arena_proto::NetDataValue::Float(v)) => *v,
                _ => 0.0,
            };
            got.push((nd.int(6).unwrap_or(0) as u8, total, comps));
        }
        got
    }

    /// The HEALTH half of a Frostbite cast must reach the wire with a non-zero
    /// Frost component AND take the health off the target — the mirrored Stamina
    /// drain must not be the only thing that lands.
    #[test]
    fn report31_frostbite_health_damage_reaches_the_wire() {
        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let hp_before = combat.fighters[1].health;
        let out = cast_frostbite(&mut combat, now);

        let dmg = damage_frames(&out);
        assert!(!dmg.is_empty(), "a Frostbite cast must emit at least one op50 ReceiveDamage");
        let (_src, total, comps) = &dmg[0];

        let frost: f32 = comps.iter().filter(|(t, _)| *t == 5).map(|(_, v)| *v).sum();
        let stam: f32 = comps.iter().filter(|(t, _)| *t == 8).map(|(_, v)| *v).sum();
        assert!(frost > 0.0, "Frost (health) component must be non-zero on the wire, got {comps:?}");
        assert!(stam > 0.0, "the mirrored Stamina drain must be on the wire, got {comps:?}");
        assert!(
            (frost - stam).abs() < 0.01,
            "frostDamageToStaminaDamage = 1 → the two tracks are equal ({frost} vs {stam})"
        );
        assert!(*total > 0.0, "totalDamage (health sum) must be non-zero");
        assert!(
            combat.fighters[1].health < hp_before,
            "the target's HP must actually drop ({hp_before} → {})",
            combat.fighters[1].health
        );
    }

    /// **Report #31, "gives no damage".** The magnitude is the rank's own
    /// `damagePerSecond × channelMaxLength` — NOT `× ELEMENTAL_STATUS_DURATION`,
    /// which is a different shipped constant (the elemental-condition DoT length)
    /// that happens to live in the same module. Frostbite ships
    /// `channelMaxLength = 3`; using 5.0 inflated every rank by 5/3.
    #[test]
    fn report31_frostbite_total_is_dps_times_its_own_channel_length() {
        use super::super::gamedata;
        let r = gamedata::ability_rank_clamped(FROSTBITE_UUID, FROSTBITE_RANK as u16)
            .expect("Frostbite rank 4 is in the shipped table");
        let dps = r.damage_per_second().expect("Frostbite ships damagePerSecond");
        let channel = r
            .get(gamedata::AbilityField::ChannelMaxLength)
            .expect("Frostbite ships channelMaxLength");
        assert_eq!(channel, 3.0, "shipped channelMaxLength");

        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let mut out = cast_frostbite(&mut combat, now);
        out.extend(run_channel(&mut combat, now));
        let dmg = damage_frames(&out);

        // The channel is a STREAM, so the shipped total is the sum over its ticks —
        // one frame carries dps × the tick interval, not the whole cast.
        let ticks: Vec<&(u8, f32, Vec<(u8, f32)>)> = dmg
            .iter()
            .filter(|(src, _, _)| *src == super::super::state::DamageSource::ContinuousSpell as u8)
            .collect();
        let expected_ticks = super::super::damage::channel_ticks(FROSTBITE_UUID, FROSTBITE_RANK)
            .expect("Frostbite is channelled");
        // `emit_damage` sends each hit to BOTH viewers, so the frame count is
        // ticks × fighters. Counting frames as ticks silently doubles it.
        let viewers = combat.fighters.len();
        assert_eq!(
            ticks.len(),
            expected_ticks as usize * viewers,
            "a {channel}s channel at {}s per tick is {expected_ticks} ticks to {viewers} viewers, \
             got {} frames",
            super::super::damage::CHANNEL_TICK_INTERVAL_SECS,
            ticks.len(),
        );

        // Sum one viewer's copy only, for the same reason.
        let frost: f32 = ticks
            .iter()
            .flat_map(|(_, _, comps)| comps.iter())
            .filter(|(t, _)| *t == 5)
            .map(|(_, v)| *v)
            .sum::<f32>()
            / viewers as f32;
        assert!(
            (frost - dps * channel).abs() < 0.5,
            "Frostbite R{FROSTBITE_RANK} must deal dps({dps}) × channelMaxLength({channel}) = {} \
             summed over its {expected_ticks} ticks, got {frost}",
            dps * channel
        );
    }

    /// **The channel is a STREAM, at the shipped PvP tick.**
    ///
    /// The engine used to land a channelled spell's whole total in ONE hit. Retail
    /// sends a run of `ContinuousSpell` frames — s615/s616 carry 118 of them across 74
    /// cast runs, none spanning more than the shipped `channelMaxLength = 3 s`, the
    /// longest run 13 ticks.
    ///
    /// This pins the two properties that make it a stream rather than a lump: more
    /// than one tick, and every tick the same size (`dps x the tick interval`). It
    /// also pins the SCHEDULE, which is where the first cut was wrong — advancing
    /// `next_tick_at` from the delivery instant instead of the scheduled one let slack
    /// compound, dropping 4 of 15 ticks and stretching a 3.0 s channel to 3.6 s.
    /// THE REPORTED BUG: "Frostbite and Ice spike quite surely damage too little."
    ///
    /// Every other frost test in this module fires at a defender with ZERO resistance
    /// — `make_prod_scale_combat` uses `starter()`, which pushes no `resistances` entry
    /// — which is precisely why the suite was blind to this. Give the target one
    /// Resist Frost affix and the old model collapsed the whole channel to ~14 damage:
    /// `resistance_reduction` subtracts a FLAT rating, and a channel re-entered it once
    /// per tick, 15 times, slamming every tick into the 95% cap.
    ///
    /// The captures disagree, and they are the same captures this model was built from:
    /// 118 ContinuousSpell frames across s615+s616 carry per-tick magnitudes of
    /// 39.33-45.00, i.e. `dps x 0.2` with nothing subtracted. A defender with a t4
    /// resist could not have produced a 44.997 tick under the old model — the ceiling
    /// was 2.25.
    #[test]
    fn report31_frost_resistance_is_charged_once_per_cast_not_once_per_tick() {
        use super::super::gamedata;
        use super::super::state::DamageType;
        let r = gamedata::ability_rank_clamped(FROSTBITE_UUID, FROSTBITE_RANK as u16)
            .expect("Frostbite rank 4 is in the shipped table");
        let dps = r.damage_per_second().expect("Frostbite ships damagePerSecond");
        let channel = r
            .get(gamedata::AbilityField::ChannelMaxLength)
            .expect("Frostbite ships channelMaxLength");
        let unresisted = dps * channel;

        // One Resist Frost t4 affix, as resolved by loadout.rs (1941 x 0.018090).
        const RATING: f32 = 35.11;

        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        combat.fighters[1].loadout.resistances = vec![(DamageType::Frost, RATING)];

        let mut out = cast_frostbite(&mut combat, now);
        out.extend(run_channel(&mut combat, now));
        let dmg = damage_frames(&out);
        let viewers = combat.fighters.len();
        let frost: f32 = dmg
            .iter()
            .filter(|(src, _, _)| *src == super::super::state::DamageSource::ContinuousSpell as u8)
            .flat_map(|(_, _, comps)| comps.iter())
            .filter(|(t, _)| *t == 5)
            .map(|(_, v)| *v)
            .sum::<f32>()
            / viewers as f32;

        // The whole channel pays the resistance ONCE, the same as a single hit of the
        // same total would. `continuous` also applies the shipped 0.75 effectiveness.
        let expected_loss = RATING
            * gamedata::combat_params::REDUCTION_PER_RESISTANCE_RATING
            * gamedata::combat_params::CONTINUOUS_DAMAGE_RESISTANCE_EFFECTIVENESS;
        assert!(
            (frost - (unresisted - expected_loss)).abs() < 1.0,
            "a resisted Frostbite channel must lose the rating ONCE ({expected_loss:.1} off \
             {unresisted:.1}), got {frost:.1}"
        );
        // The load-bearing half: it must not be the old ~14.
        assert!(
            frost > unresisted * 0.75,
            "one resist affix must not delete the spell — {frost:.1} of {unresisted:.1}"
        );
    }

    /// EDIR — "Elemental Damage Ignores Resistance" — must work on elemental SPELLS.
    ///
    /// It did not. `resolve_ability` built a `Loadout::default()` and copied only the
    /// caster's perks, so `elem_resist_piercing_rating` was 0 on every spell and every
    /// channel tick. A frost build wearing four EDIR pieces got nothing from any of
    /// them; the WEAPON path (`resolve.rs`, which clones the real loadout) had honoured
    /// them the whole time. Reported by a player running exactly that build.
    #[test]
    fn edir_gear_pierces_resistance_on_a_frost_channel() {
        use super::super::gamedata;
        use super::super::state::DamageType;
        const RATING: f32 = 35.11; // one Resist Frost t4 affix on the defender

        let channel_frost = |edir: f32| -> f32 {
            let now = Instant::now();
            let mut combat = make_prod_scale_combat(now);
            combat.fighters[1].loadout.resistances = vec![(DamageType::Frost, RATING)];
            combat.fighters[0].loadout.elem_resist_piercing_rating = edir;
            let mut out = cast_frostbite(&mut combat, now);
            out.extend(run_channel(&mut combat, now));
            let viewers = combat.fighters.len();
            damage_frames(&out)
                .iter()
                .filter(|(src, _, _)| {
                    *src == super::super::state::DamageSource::ContinuousSpell as u8
                })
                .flat_map(|(_, _, comps)| comps.iter())
                .filter(|(t, _)| *t == 5)
                .map(|(_, v)| *v)
                .sum::<f32>()
                / viewers as f32
        };

        let bare = channel_frost(0.0);
        let pierced = channel_frost(RATING); // enough EDIR to cancel the affix

        assert!(
            pierced > bare,
            "EDIR must raise damage through resistance: {pierced:.1} vs {bare:.1}"
        );

        let r = gamedata::ability_rank_clamped(FROSTBITE_UUID, FROSTBITE_RANK as u16)
            .expect("Frostbite rank 4");
        let unresisted = r.damage_per_second().expect("dps")
            * r.get(gamedata::AbilityField::ChannelMaxLength).expect("channel");
        assert!(
            (pierced - unresisted).abs() < 1.0,
            "EDIR equal to the defender's rating should fully cancel it: {pierced:.1} vs \
             {unresisted:.1} unresisted"
        );
    }

    #[test]
    fn a_channel_tick_that_ends_the_round_does_not_crash_the_server() {
        // 2026-09-24 12:36: a Wall of Fire tick killed its target, the round ended,
        // `on_round_ended` cleared the channel list, and the loop then indexed the
        // channel it had just ticked — index out of bounds, arena restarted.
        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let _ = cast_frostbite(&mut combat, now);
        assert!(!combat.channels.is_empty(), "control: the cast opened a channel");
        let target = combat.channels[0].target_slot;
        combat.fighters[target].health = 1;
        let due = combat.channels[0].next_tick_at;
        let _ = super::apply_channel_ticks(&mut combat, due);
        assert!(
            combat.channels.is_empty() || combat.fighters[target].is_dead(),
            "the killing tick must end the channel without panicking"
        );
    }

    #[test]
    fn report31_frostbite_streams_evenly_over_its_channel() {
        use super::super::gamedata;
        let r = gamedata::ability_rank_clamped(FROSTBITE_UUID, FROSTBITE_RANK as u16).unwrap();
        let dps = r.damage_per_second().unwrap();
        let iv = super::super::damage::CHANNEL_TICK_INTERVAL_SECS;

        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let mut out = cast_frostbite(&mut combat, now);
        out.extend(run_channel(&mut combat, now));

        let viewers = combat.fighters.len();
        let per_tick: Vec<f32> = damage_frames(&out)
            .iter()
            .filter(|(src, _, _)| *src == super::super::state::DamageSource::ContinuousSpell as u8)
            .map(|(_, _, comps)| comps.iter().filter(|(t, _)| *t == 5).map(|(_, v)| *v).sum())
            .collect();

        assert!(
            per_tick.len() > viewers,
            "a channel must be MORE than one hit — got {} frame(s), i.e. {} tick(s)",
            per_tick.len(),
            per_tick.len() / viewers,
        );
        let want = dps * iv;
        for (i, v) in per_tick.iter().enumerate() {
            assert!(
                (v - want).abs() < 0.5,
                "tick {i} carried {v}, expected dps({dps}) x interval({iv}) = {want} — \
                 every tick of a channel is the same size",
            );
        }

        // And the channel must be DONE by its shipped length: nothing may still be
        // owed once `channelMaxLength` has passed.
        assert!(
            combat.channels.is_empty(),
            "{} channel(s) still owed ticks after channelMaxLength elapsed",
            combat.channels.len(),
        );
    }

    /// **Retail wire fidelity.** s615 #4394011: a Frostbite tick is
    /// `DamageSource = 8 (ContinuousSpell)`, not `2 (Spell)`. The client renders a
    /// channelled spell's damage off this discriminator.
    #[test]
    fn report31_frostbite_is_a_continuous_spell_on_the_wire() {
        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let out = cast_frostbite(&mut combat, now);
        let dmg = damage_frames(&out);
        assert_eq!(
            dmg[0].0,
            super::super::state::DamageSource::ContinuousSpell as u8,
            "a channelled dps spell rides DamageSource::ContinuousSpell (8), per s615 #4394011"
        );
    }

    /// **Report #31, the stamina half.** `CombatParameters.frostDamageToStaminaDamage = 1`
    /// means Frost drains the target's STAMINA pool one-for-one. The component was
    /// already written to the wire; nothing ever subtracted it, so a Frostbite
    /// landed on a full stamina bar and left it full.
    #[test]
    fn report31_frost_drains_the_targets_stamina_pool() {
        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let stam_before = combat.fighters[1].stamina;
        assert!(stam_before > 0, "the target starts with stamina to drain");

        let out = cast_frostbite(&mut combat, now);
        let dmg = damage_frames(&out);
        let stam_component: f32 = dmg[0].2.iter().filter(|(t, _)| *t == 8).map(|(_, v)| *v).sum();
        assert!(stam_component > 0.0, "the wire carries a Stamina component");

        let expected = stam_before.saturating_sub(stam_component.round() as u32);
        assert_eq!(
            combat.fighters[1].stamina, expected,
            "the Stamina component must come off the pool ({stam_before} − {stam_component:.1})"
        );
    }

    /// **Report #31, "does not freeze the opponent".** Frostbite ships no
    /// `_freezeDuration`, so the `apply_shipped_effects` gate can never fire for
    /// it — the freeze is the ELEMENTAL STATUS (`Frozen`, status id 5), landed by
    /// the conditioning accumulator. Retail lands it within ~1 s of every
    /// Frostbite cast (s615: casts at 21:00:19 / 21:00:32 / 21:05:21 → op51
    /// `apply=1 status=5` at 21:00:20 / 21:00:33 / 21:05:22).
    #[test]
    fn report31_frostbite_lands_the_frozen_status() {
        let now = Instant::now();
        let mut combat = make_prod_scale_combat(now);
        let mut out = cast_frostbite(&mut combat, now);
        // Frozen is the ELEMENTAL-STATUS accumulator crossing 25% of the target's max
        // HP. A lump crossed it on the cast frame; a stream has to build up to it, so
        // the channel must actually run. That is the retail shape: s615 casts at
        // 21:00:19 / 21:00:32 land op51 Frozen at 21:00:20 / 21:00:33 — about a second
        // IN, not on the cast.
        out.extend(run_channel(&mut combat, now));

        let frozen: Vec<_> = out
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(51))
            .filter(|(_, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(4) == Some(1)
                    && nd.int(5)
                        == Some(super::super::state::StatusEffectType::Frozen as u16 as i64)
            })
            .collect();
        assert_eq!(
            frozen.len(),
            combat.fighters.len(),
            "op51 Frozen(5) apply must go to every viewer, got {} frame(s)",
            frozen.len()
        );
        let nd = arena_proto::parse_netdata(&frozen[0].1[2..]);
        let dur = match nd.get(6) {
            Some(arena_proto::NetDataValue::Float(v)) => *v,
            _ => 0.0,
        };
        assert!(
            (dur - super::super::gamedata::combat_params::ELEMENTAL_STATUS_DURATION).abs() < 0.01,
            "the shipped elemental-status duration (5 s), got {dur}"
        );
    }

}

#[cfg(test)]
mod cooldown_data_tests {
    use super::*;

    /// Cooldowns now come from the shipped per-RANK assets (Phase 3.11), so they are
    /// exact floats rather than the hand-rounded milliseconds of the old table.
    #[test]
    fn authoritative_per_ability_cooldowns() {
        let ms = |u: &str, r: u8| ability_cooldown(u, r).as_secs_f32();
        assert!((ms("d07a8d30-9a1c-49b0-866d-97a8aa1534cf", 1) - 3.54).abs() < 1e-3); // Fireball
        assert!((ms("ce6b63e9-9f18-49c4-aee0-51f7985f9892", 1) - 8.09).abs() < 1e-2); // Power Attack
        assert!((ms("65ede044-d68a-4b2b-8f0c-02075ad133cc", 1) - 7.5).abs() < 1e-3); // Ward
        // The old table had Thunderstorm under a fabricated uuid, so it silently fell
        // back to 3 s; the real id now resolves.
        assert_ne!(ability_cooldown("2ab06506-2114-4738-bd87-f6f402d3ce2e", 1), ABILITY_COOLDOWN);
        assert_eq!(ability_cooldown("not-a-real-uuid", 1), ABILITY_COOLDOWN); // fallback
    }
}

// ---------------------------------------------------------------------------
// Phase 4.3 — consumables
// ---------------------------------------------------------------------------

/// Spend one of `slot`'s per-round consumable charges.
///
/// `PvpParameters.consumablesPerRound` is **1**, and the budget resets in
/// `MatchCombat::reset_fighters_for_next_round`. Returns `false` (and does nothing)
/// once the budget is spent.
///
/// Driven by [`on_consume_consumable`], the wire trigger. (The earlier note here
/// claimed no `UseConsumable` GameMessageId exists — that was wrong: retail uses a
/// REQUEST/PERFORM pair, `RequestConsumeConsumable`(63) c2s → `PerformConsumeConsumable`
/// (64) s2c, both present in the corpus. See [`on_consume_consumable`].)
pub fn use_consumable(combat: &mut MatchCombat, slot: usize, _now: Instant) -> bool {
    match combat.fighters.get_mut(slot) {
        Some(f) => {
            let ok = f.try_use_consumable();
            if !ok {
                debug!(
                    "combat: slot {slot} consumable REJECTED — consumablesPerRound ({}) already spent",
                    super::state::CONSUMABLES_PER_ROUND,
                );
            }
            ok
        }
        None => false,
    }
}

/// The WIRE TRIGGER for consumables: handle a client's `RequestConsumeConsumable`
/// (63) and answer with `PerformConsumeConsumable` (64) to both players.
///
/// Capture-established protocol (269 op63 + 554 op64 prod frames; s433 shows the
/// pairing directly — c2s op63 on avatar 199 is answered by an s2c op64 on avatar
/// 199 carrying that avatar's declared consumable UUID, every time):
///   1. c2s op56 `EquipAbilitiesAndConsumables` declares `{consumableUuid, charges}`.
///   2. c2s op63 `RequestConsumeConsumable` — bare NetObjectInfo + gmid, NO item id.
///   3. s2c op64 `PerformConsumeConsumable` — the same avatar, plus the UUID from (1).
///
/// The server is authoritative on whether the drink happens: the request is refused
/// (silently, no op64) when the fighter's `consumablesPerRound` budget is already
/// spent, or when no op56 has named a consumable yet — the UUID is never fabricated.
///
/// **Not wired here:** the potion's actual EFFECT. The shipped consumable items are
/// not in `gamedata.rs` (none of the observed consumable UUIDs appear there), so
/// there is no authoritative heal/restore magnitude to apply, and guessing one would
/// desync the HUD from the real game's numbers. The charge accounting and the visual
/// are faithful; the stat change is a documented gap.
fn on_consume_consumable(
    combat: &mut MatchCombat,
    sender: usize,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    if sender >= combat.fighters.len() {
        return Vec::new();
    }
    // Resolve the item id BEFORE spending the charge, so a request we cannot answer
    // does not silently burn the round's only consumable.
    let Some(uuid) = combat.fighters[sender].equipped_consumable.clone() else {
        debug!(
            "combat: slot {sender} op63 ignored — no consumable declared yet \
             (no EquipAbilitiesAndConsumables seen)"
        );
        return Vec::new();
    };
    if !use_consumable(combat, sender, now) {
        return Vec::new();
    }
    let obj = combat.fighters[sender].net_object_id;

    // Apply what the potion actually restores (tracker #29). Until this, the
    // charge was spent and the animation played and NOTHING happened, because
    // the magnitude was not known to the engine. It is now a generated table
    // joined from the shipped item data — see `gamedata::RESTORATIONS`.
    //
    // Spread over the tier's own duration rather than granted in one lump:
    // every shipped tier is 2.5 s, and a 225-point heal arriving instantly is
    // a different thing to fight against than one arriving over two and a half
    // seconds. `apply_regen_tick` drains it.
    match super::gamedata::restoration(&uuid) {
        Some(r) => {
            let ticks = (r.duration / REGEN_TICK_INTERVAL.as_secs_f32()).max(1.0);
            combat.fighters[sender].pending_restore = Some(super::state::PendingRestore {
                affected_stat: r.affected_stat,
                remaining: r.value,
                per_tick: r.value / ticks,
            });
            info!(
                "combat: slot {sender} consumed {uuid} (op63 → op64) — \
                 restoring {:.0} to stat {} over {:.1}s",
                r.value, r.affected_stat, r.duration
            );
        }
        None => {
            // Every non-restoration consumable: resist potions and weakness
            // poisons carry an AlchemyInfo instead, which is not modelled. The
            // drink is still spent and still animates, as before.
            info!("combat: slot {sender} consumed {uuid} (op63 → op64) — no restoration in data");
        }
    }

    let frame = messages::perform_consume_consumable(obj, &uuid);
    let mut out: Vec<(usize, Vec<u8>)> =
        (0..combat.fighters.len()).map(|s| (s, frame.clone())).collect();

    // ...and the drink VISUAL. gmid 78 `PlayerPlayVFX` existed in the opcode enum
    // and was emitted by nothing, so a potion healed silently. All 284 captured
    // op78 frames are potion effects, which is what makes this the right home for
    // it. Sent to both players: propId 4 is the drinker's first-person effect,
    // propId 5 the third-person one their opponent sees.
    if let Some(vfx) = potion_vfx_for_stat(&uuid) {
        let f = messages::player_play_vfx(obj, vfx.0, vfx.1);
        for slot in 0..combat.fighters.len() {
            out.push((slot, f.clone()));
        }
    }
    out
}

/// The (first-person, third-person) VFX pair for a restoration potion.
///
/// Only health and magicka appear in the corpus. **Stamina returns `None` on
/// purpose:** no captured op78 names a stamina effect, and a guessed uuid is
/// dropped silently by the client — which looks like a working feature that does
/// nothing. Better to send no visual than a fabricated one.
fn potion_vfx_for_stat(item_uuid: &str) -> Option<(&'static str, &'static str)> {
    match super::gamedata::restoration(item_uuid)?.affected_stat {
        0 => Some((
            "71396acd-1caa-414b-a249-57e35e1e69b6", // VFX_AlchemyVFX_PotionVFX_Health
            "0fef0efe-57b5-46c1-814a-47211103a673", // ..._Health_3rd
        )),
        2 => Some((
            "ca784a31-a574-4a8d-8840-6b5d432cbbb4", // VFX_AlchemyVFX_PotionVFX_Magicka
            "2f5405f6-054d-451b-8671-3b21f9a7ef9e", // ..._Magicka_3rd
        )),
        _ => None, // stamina: not in the corpus, so not invented
    }
}

#[cfg(test)]
mod potion_tests {
    use super::super::gamedata;

    /// Tracker #29: "potion had no effect". The engine spent the charge and
    /// played the animation and applied nothing, because no magnitude was known
    /// to it. These pin the table that fixed that.
    ///
    /// Values are the shipped ones, not chosen here: Health Potion tier 9 —
    /// the tier the reporter was actually carrying — restores 225.
    #[test]
    fn the_reporters_potion_restores_its_shipped_amount() {
        // Items.Name.Potion.Restoration.Health.Tier9
        let r = gamedata::restoration("61b31323-8ba2-49f2-befe-f43111c6e2c7")
            .expect("the health potion must be in the table");
        assert_eq!(r.affected_stat, 0, "health");
        assert_eq!(r.value, 225.0);
        assert_eq!(r.duration, 2.5);
    }

    /// END TO END: drinking must actually PUT the frame on the wire.
    ///
    /// The other tests here check `potion_vfx_for_stat` and the frame builder in
    /// isolation, and red-proofing showed that is not enough — with the emission
    /// deleted from `on_consume_consumable`, all of them still passed. This one
    /// drives the real consumable path and looks for gmid 78 in what comes out.
    #[test]
    fn drinking_a_health_potion_emits_the_vfx_frame() {
        use super::super::messages;
        let now = std::time::Instant::now();
        let mut combat = super::tests::make_live_combat(now);
        combat.fighters[0].equipped_consumable =
            Some("61b31323-8ba2-49f2-befe-f43111c6e2c7".to_string()); // Health Tier 9

        let out = super::on_consume_consumable(&mut combat, 0, now);
        let ids: Vec<u8> = out
            .iter()
            .filter_map(|(_, f)| messages::user_message_gmid(f))
            .collect();

        // Non-vacuity: the drink must actually have resolved.
        assert!(
            ids.contains(&64),
            "expected the op64 consume confirmation, got {ids:?}",
        );
        assert!(
            ids.contains(&78),
            "drinking must emit the op78 visual — retail sends one on every potion. \
             Got gmids {ids:?}",
        );

        // ...and it must carry the health pair, not just any op78.
        let vfx = out
            .iter()
            .find(|(_, f)| messages::user_message_gmid(f) == Some(78))
            .expect("op78 present");
        let nd = arena_proto::parse_netdata(&vfx.1[2..]);
        assert_eq!(
            nd.string(4).as_deref(),
            Some("71396acd-1caa-414b-a249-57e35e1e69b6"),
            "first-person health potion effect",
        );
    }

    /// gmid 78 `PlayerPlayVFX` existed in the opcode enum and was emitted by
    /// nothing, so a potion healed in silence. All 284 captured op78 frames are
    /// potion effects — health and magicka, each with a first- and third-person
    /// variant.
    #[test]
    fn a_health_potion_names_its_captured_vfx_pair() {
        // Items.Name.Potion.Restoration.Health.Tier9
        let (first, third) = super::potion_vfx_for_stat("61b31323-8ba2-49f2-befe-f43111c6e2c7")
            .expect("a health potion must have a visual");
        assert_eq!(first, "71396acd-1caa-414b-a249-57e35e1e69b6");
        assert_eq!(third, "0fef0efe-57b5-46c1-814a-47211103a673");
        assert_ne!(first, third, "the drinker and the opponent see different effects");
    }

    /// Stamina potions get NO visual, deliberately. No captured op78 names a
    /// stamina effect, and the client drops an unknown uuid silently — which
    /// looks like a working feature doing nothing. This asserts the absence so
    /// nobody "completes the set" with a guessed id.
    #[test]
    fn a_stamina_potion_has_no_invented_vfx() {
        let stamina: Vec<&str> = gamedata::RESTORATIONS
            .iter()
            .filter(|r| r.affected_stat == 1)
            .map(|r| r.uuid)
            .collect();
        assert!(!stamina.is_empty(), "there are stamina potions to check");
        for uuid in stamina {
            assert!(
                super::potion_vfx_for_stat(uuid).is_none(),
                "stamina potion {uuid} must not carry a fabricated effect uuid",
            );
        }
    }

    /// Magicka is the other half of the captured pair, and the control that stops
    /// `potion_vfx_for_stat` from being "health or nothing".
    #[test]
    fn a_magicka_potion_names_the_magicka_pair() {
        let magicka = gamedata::RESTORATIONS
            .iter()
            .find(|r| r.affected_stat == 2)
            .expect("magicka potions exist");
        let (first, third) =
            super::potion_vfx_for_stat(magicka.uuid).expect("magicka has a visual");
        assert_eq!(first, "ca784a31-a574-4a8d-8840-6b5d432cbbb4");
        assert_eq!(third, "2f5405f6-054d-451b-8671-3b21f9a7ef9e");
    }

    /// The frame itself, against the shape all 284 captured frames share.
    #[test]
    fn the_play_vfx_frame_matches_the_captured_shape() {
        let f = super::super::messages::player_play_vfx(431, "aaa", "bbb");
        let nd = arena_proto::parse_netdata(&f[2..]);
        assert_eq!(nd.int(0), Some(431), "avatar object id");
        assert_eq!(nd.int(1), Some(56), "NetObjectType::Avatar");
        assert_eq!(nd.int(2), Some(1), "NetRole::Authority");
        assert_eq!(nd.int(3), Some(78), "gmid");
        assert_eq!(nd.int(6), Some(1), "constant in all 284 frames");
        assert_eq!(nd.int(7), Some(1), "constant in all 284 frames");
        assert_eq!(nd.int(8), Some(2), "constant in all 284 frames");
    }

    /// Every restoration consumable is present: three pools, ten tiers each.
    #[test]
    fn all_thirty_restorations_are_present() {
        assert_eq!(gamedata::RESTORATIONS.len(), 30);
        let mut per_stat = [0usize; 3];
        for r in gamedata::RESTORATIONS.iter() {
            per_stat[r.affected_stat as usize] += 1;
        }
        assert_eq!(per_stat, [10, 10, 10], "ten tiers of health, stamina, magicka");
    }

    /// The lookup is a binary search, so the table MUST stay uuid-sorted. A
    /// generator change that reordered it would silently start returning None
    /// for real potions.
    #[test]
    fn the_table_is_uuid_sorted() {
        let mut prev = "";
        for r in gamedata::RESTORATIONS.iter() {
            assert!(r.uuid > prev, "out of order at {}", r.uuid);
            prev = r.uuid;
        }
    }

    /// Every value is positive and finite — a potion that restores nothing, or
    /// NaN, would be worse than the old do-nothing behaviour.
    #[test]
    fn every_restoration_is_a_real_amount() {
        for r in gamedata::RESTORATIONS.iter() {
            assert!(r.value > 0.0 && r.value.is_finite(), "{} value {}", r.uuid, r.value);
            assert!(r.duration > 0.0 && r.duration.is_finite(), "{} duration", r.uuid);
            assert!(r.affected_stat <= 2, "{} stat {}", r.uuid, r.affected_stat);
        }
    }

    /// A non-restoration consumable resolves to nothing rather than to a
    /// default. Resist potions and weakness poisons carry an AlchemyInfo, which
    /// this table deliberately does not model.
    #[test]
    fn a_resist_potion_has_no_restoration() {
        // Prime Elixir of Resist Frost — a real consumable players drink.
        assert!(gamedata::restoration("c4e0de4f-813c-45b9-9ed7-943b4ac2e729").is_none());
    }

    #[test]
    fn an_unknown_item_has_no_restoration() {
        assert!(gamedata::restoration("00000000-0000-0000-0000-000000000000").is_none());
    }
}

#[cfg(test)]
mod phase4_tests {

    /// Advance past the FollowThrough beat so a committed swing lands.
    ///
    /// Tracker #21 moved the moment of impact to match the animation. These tests
    /// were updated to ADVANCE A CLOCK, not to relax assertions — every damage
    /// number below is unchanged.
    fn land(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
        super::land_due_hits(combat, now + super::FOLLOW_THROUGH_DELAY + Duration::from_millis(1))
    }

    /// Commit a swing and land it.
    fn swing_and_land(
        combat: &mut MatchCombat,
        sender: usize,
        target: usize,
        factor: f32,
        now: Instant,
    ) -> Vec<(usize, Vec<u8>)> {
        let mut out = super::resolve_swing(combat, sender, target, factor, now);
        out.extend(land(combat, now));
        out
    }
    use super::*;
    use crate::arena::combat::state::Fighter;

    // ---------------------------------------------------------------------
    // Phase 4.1 — swing-side classification from real client input geometry
    // ---------------------------------------------------------------------
    //
    // (The former `active_side_decodes_only_from_combat_input_frames` test lived
    // here. It asserted that a 0..=3 NetData prop above the header was an
    // `ActiveSide`. Prod ground truth disproved that hypothesis outright — there is
    // no `activeSide` field on the c2s wire — so the test was replaced rather than
    // relaxed: it was pinning a decode that does not exist.)

    /// A prod-shaped `PlayerCombatInputPosition` (gmid 47) on the generic 0x36
    /// carrier: `{0:Int obj · 1:Byte 56 · 2:Byte 3 · 3:Byte 47 · 4:Float x ·
    /// 5:Float y · 6:Float frameDelta · 7:Float charge · 8:Int flags}`.
    fn make_pos_frame_with_flags(x: f32, y: f32, charge: f32, flags: i32) -> Vec<u8> {
        let mut w = arena_proto::NetDataWriter::new();
        w.int(0, 565)
            .byte(1, 56)
            .byte(2, 3)
            .byte(3, 47)
            .float(4, x)
            .float(5, y)
            .float(6, 0.033_334)
            .float(7, charge)
            .int(8, flags);
        let mut f = messages::frame_for_test(w.finish());
        f[0] = 0x84; // c2s marker
        f
    }

    /// `410` is the capture-observed non-attack control value used by the existing
    /// phase-4 tests. It has no `START_TRIGGER_FLAG` bit.
    fn make_pos_frame(x: f32, y: f32, charge: f32) -> Vec<u8> {
        make_pos_frame_with_flags(x, y, charge, 410)
    }

    /// A prod-shaped `PlayerCombatInputActivate` (gmid 46) on the generic 0x36
    /// carrier: `{… 3:Byte 46 · 4:Bool held · 5:Float charge · 6:Bool blockZone}`.
    fn make_act_frame(held: bool, charge: f32, block_zone: bool) -> Vec<u8> {
        let mut w = arena_proto::NetDataWriter::new();
        w.int(0, 565)
            .byte(1, 56)
            .byte(2, 3)
            .byte(3, 46)
            .bool(4, held)
            .float(5, charge)
            .bool(6, block_zone);
        let mut f = messages::frame_for_test(w.finish());
        f[0] = 0x84;
        f
    }

    /// Report #113, the phantom hit: releasing a held block struck the opponent.
    ///
    /// In the reporter's words — a player who opens every fight with a raised
    /// shield — "Every time I released that first block, it seemed like I was
    /// quickly hitting my opponent, though I did not push a button for this nor
    /// see the strike animation. All that I saw was my opponent taking the hit."
    ///
    /// The path: a guard press clears `charge_press_at` (a block is never a
    /// swing), so a RELEASE that is not classified as a block falls through to the
    /// swing commit — with no press recorded, no `Charging` wind-up, and therefore
    /// no animation. The block/swing split is positional and admits misses by
    /// construction: 344 of 351 captured block presses sit below pointer X 0.5,
    /// so 7 did not.
    ///
    /// Damage is checked AFTER `land_due_impacts`, because a swing schedules its
    /// impact rather than applying it inline — an assertion taken at commit time
    /// reads 0 damage for a real swing too, and would pass whatever the code did.
    #[test]
    fn a_release_with_no_press_does_not_swing() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        let before = combat.fighters[1].health;

        // Guard up, then a release the classifier fails to label as a block.
        on_c2s_input(&mut combat, 0, &make_act_frame(true, 0.0, true), now);
        assert_eq!(combat.fighters[0].actor_state(), ActorStateType::Blocking);
        on_c2s_input(&mut combat, 0, &make_act_frame(false, 0.0, false), now);
        land(&mut combat, now);

        assert_eq!(
            combat.fighters[1].health, before,
            "releasing a block must not damage the opponent"
        );
    }

    /// The control, and the reason this is a guard rather than a deletion: an
    /// ordinary press-then-release must still swing and still land. A fix that
    /// silenced the phantom hit by silencing swings would pass the test above and
    /// break every fight.
    #[test]
    fn an_ordinary_press_then_release_still_swings() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        let before = combat.fighters[1].health;

        on_c2s_input(&mut combat, 0, &make_pos_frame(0.8, 0.5, 0.0), now);
        on_c2s_input(&mut combat, 0, &make_act_frame(true, 0.0, false), now);
        on_c2s_input(&mut combat, 0, &make_act_frame(false, 0.1, false), now);
        land(&mut combat, now);

        assert!(
            combat.fighters[1].health < before,
            "a real swing must still land: {} -> {}",
            before,
            combat.fighters[1].health
        );
    }

    /// A bare release with no prior input at all — a reconnect, or a dropped
    /// press — is also not a swing.
    #[test]
    fn a_bare_release_with_no_history_does_not_swing() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        let before = combat.fighters[1].health;
        on_c2s_input(&mut combat, 0, &make_act_frame(false, 0.0, false), now);
        land(&mut combat, now);
        assert_eq!(combat.fighters[1].health, before);
    }

    /// Report #113: a fighter caught mid-cast when the round ended held that pose
    /// for the whole inter-round walk.
    ///
    /// `reset_fighters_for_next_round` did reset the actor, but it runs at
    /// `InRound(13)` — the LAST of the six interround steps — so the pose survived
    /// until round 2 went live. The reporter described exactly that boundary: the
    /// cast pose held "right through the break and up until the first strikes of the
    /// next round began".
    ///
    /// The round end must therefore put both actors back to Idle itself.
    #[test]
    fn a_round_ending_returns_both_actors_to_idle() {
        let now = Instant::now();
        let mut combat = live_combat(now);

        // Both fighters mid-animation when the round ends.
        combat.fighters[0].set_actor_state(ActorStateType::Channeling, now);
        combat.fighters[1].set_actor_state(ActorStateType::Channeling, now);
        assert_ne!(combat.fighters[0].actor_state(), ActorStateType::Idle);
        assert_ne!(combat.fighters[1].actor_state(), ActorStateType::Idle);

        combat.reset_actor_animations(now);

        assert_eq!(
            combat.fighters[0].actor_state(),
            ActorStateType::Idle,
            "the winner's actor must stop animating when the round ends"
        );
        assert_eq!(
            combat.fighters[1].actor_state(),
            ActorStateType::Idle,
            "the loser's actor must stop animating when the round ends"
        );

        // And the clients must be TOLD — a server-side field change nobody sends is
        // what left the pose stuck on screen in the first place.
        let frames = drain_state_changes(&mut combat, now);
        assert!(
            !frames.is_empty(),
            "the reset must emit actor-state frames, not just mutate server state"
        );
    }

    /// The control: an actor already Idle must not emit a redundant change. Without
    /// this the fix would spam an op39 at every round end for a fighter that was
    /// simply standing still, which is not what retail does.
    #[test]
    fn an_already_idle_actor_emits_nothing() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        assert_eq!(combat.fighters[0].actor_state(), ActorStateType::Idle);
        assert_eq!(combat.fighters[1].actor_state(), ActorStateType::Idle);

        combat.reset_actor_animations(now);
        let frames = drain_state_changes(&mut combat, now);
        assert!(
            frames.is_empty(),
            "an actor that was already Idle must not produce a state change: {frames:?}"
        );
    }

    fn live_combat(now: Instant) -> MatchCombat {
        use super::super::loadout::starter;
        let mut combat = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = combat.alloc_net_object_id();
            let mut f = Fighter::new(slot, obj, starter(), now);
            f.loadout.weapon = super::super::state::WeaponProfile {
                primary_type: Some(super::super::state::DamageType::Slashing),
                base_by_type: vec![(super::super::state::DamageType::Slashing, 113.82)],
                weight: Some(super::super::tables::Weight::Light),
            };
            f.loadout.weapon_template = None;
            combat.fighters.push(f);
        }
        combat.match_net_object_id = combat.alloc_net_object_id();
        combat.phase = FlowState::StateTimeout;
        combat.phase_entered = now;
        combat
    }

    /// Swing `sender` by press→release at normalised X `x`, `dt` after `t0`.
    /// Returns the resulting combo count.
    fn swing_at(combat: &mut MatchCombat, sender: usize, x: f32, t: Instant) -> u32 {
        on_c2s_input(combat, sender, &make_pos_frame(x, 0.5, 0.0), t);
        on_c2s_input(combat, sender, &make_act_frame(true, 0.0, false), t);
        on_c2s_input(combat, sender, &make_act_frame(false, 0.1, false), t);
        combat.fighters[sender].combo_count
    }

    /// The decoders read the real prod NetData layout.
    #[test]
    fn combat_input_frames_decode_prod_layout() {
        let pos = parse_input_position(&make_pos_frame(0.7946, 0.4528, 0.4169))
            .expect("gmid 47 decodes");
        assert!((pos.x - 0.7946).abs() < 1e-4, "propId 4 is normalised screen X");
        assert!((pos.y - 0.4528).abs() < 1e-4, "propId 5 is normalised screen Y");
        assert!((pos.client_charge.unwrap() - 0.4169).abs() < 1e-4, "propId 7 is charge secs");
        assert!(!pos.start_attack_trigger_ready, "410 has no start-attack bit");
        let swipe = parse_input_position(&make_pos_frame_with_flags(
            0.729_055,
            0.337_963,
            0.1,
            410 | POS_START_ATTACK_TRIGGER_FLAG,
        ))
        .expect("flagged gmid 47 decodes");
        assert!(swipe.start_attack_trigger_ready, "bit 512 is the start-attack trigger");

        let down = parse_input_activate(&make_act_frame(true, 0.0, true)).expect("gmid 46 decodes");
        assert!(down.held, "propId 4 true = press");
        assert_eq!(down.block_zone, Some(true), "propId 6 = _isWithinBlockZone");
        let up = parse_input_activate(&make_act_frame(false, 2.81, false)).expect("gmid 46 decodes");
        assert!(!up.held, "propId 4 false = release");
        assert!((up.client_charge.unwrap() - 2.81).abs() < 1e-3);

        // Neither decoder fires on a foreign frame.
        assert!(parse_input_position(&make_act_frame(true, 0.0, false)).is_none());
        assert!(parse_input_activate(&make_pos_frame(0.5, 0.5, 0.0)).is_none());
        assert!(parse_input_position(&[0x84, 0x36]).is_none());
        assert!(parse_input_activate(&[0x84, 0x36]).is_none());
    }

    /// Report #171: the client explicitly marks the pointer segment that passed its
    /// authored swipe-speed/angle gates. That path must enter retail's manual
    /// `PlayerAttack` state (gmid 40), preserving the slash geometry used by the
    /// weapon trail/collision effects, rather than flattening every swing to gmid 52.
    /// Damage remains server-owned and lands at the capture-measured 150 ms beat.
    #[test]
    fn a_flagged_swipe_emits_manual_attack_and_lands_at_its_follow_through() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        let before = combat.fighters[1].health;

        // Real s616 geometry, rounded to six decimals. The segment from previous to
        // current normalises to approximately (0.305514, 0.952187), exactly the
        // direction carried on the corresponding retail gmid 40.
        on_c2s_input(
            &mut combat,
            0,
            &make_pos_frame_with_flags(0.760_250, 0.435_185, 0.0, 410),
            now,
        );
        on_c2s_input(&mut combat, 0, &make_act_frame(true, 0.0, false), now);
        let windup = drain_state_changes(&mut combat, now);
        assert_eq!(
            windup
                .iter()
                .filter(|(_, frame)| messages::user_message_gmid(frame) == Some(45))
                .count(),
            2,
            "both viewers must receive the Charging wind-up",
        );

        let release = now + Duration::from_millis(400);
        on_c2s_input(
            &mut combat,
            0,
            &make_pos_frame_with_flags(
                0.729_055,
                0.337_963,
                0.4,
                410 | POS_START_ATTACK_TRIGGER_FLAG,
            ),
            release - Duration::from_millis(1),
        );
        on_c2s_input(
            &mut combat,
            0,
            &make_act_frame(false, 0.4, false),
            release,
        );
        let swing = drain_state_changes(&mut combat, release);
        assert_eq!(
            swing
                .iter()
                .filter(|(_, frame)| messages::user_message_gmid(frame) == Some(40))
                .count(),
            2,
            "both viewers must receive PlayerAttackStateChange",
        );
        assert!(
            swing
                .iter()
                .all(|(_, frame)| messages::user_message_gmid(frame) != Some(52)),
            "a recognised manual slash must not be downgraded to auto-attack",
        );

        land_due_hits(
            &mut combat,
            release + MANUAL_ATTACK_FOLLOW_THROUGH_DELAY - Duration::from_millis(1),
        );
        assert_eq!(combat.fighters[1].health, before, "manual damage landed too early");
        land_due_hits(
            &mut combat,
            release + MANUAL_ATTACK_FOLLOW_THROUGH_DELAY + Duration::from_millis(1),
        );
        assert!(
            combat.fighters[1].health < before,
            "manual damage must land with the 150 ms follow-through",
        );
    }

    /// A normal tap with no client swipe trigger keeps the already-working gmid 52
    /// fallback. This prevents the visual fix from inventing geometry for bots or
    /// clients that do not report a qualifying path.
    #[test]
    fn an_unflagged_tap_keeps_the_auto_attack_fallback() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        on_c2s_input(&mut combat, 0, &make_pos_frame(0.8, 0.5, 0.0), now);
        on_c2s_input(&mut combat, 0, &make_act_frame(true, 0.0, false), now);
        drain_state_changes(&mut combat, now);
        on_c2s_input(
            &mut combat,
            0,
            &make_act_frame(false, 0.1, false),
            now + Duration::from_millis(100),
        );
        let swing = drain_state_changes(&mut combat, now + Duration::from_millis(100));
        assert_eq!(
            swing
                .iter()
                .filter(|(_, frame)| messages::user_message_gmid(frame) == Some(52))
                .count(),
            2,
        );
        assert!(
            swing
                .iter()
                .all(|(_, frame)| messages::user_message_gmid(frame) != Some(40)),
        );
    }

    /// The X midpoint splits Left from Right; garbage coordinates classify to nothing
    /// (so the caller falls back rather than trusting a hostile frame).
    #[test]
    fn x_midpoint_classifies_side() {
        // Prod class medians (n = 3 277 ground-truth attack hits).
        assert_eq!(classify_side_from_x(0.213), Some(ActiveSide::Left));
        assert_eq!(classify_side_from_x(0.814), Some(ActiveSide::Right));
        // Exactly on the cut-point resolves Right (>= is the documented rule).
        assert_eq!(classify_side_from_x(SIDE_CLASSIFY_X_MIDPOINT), Some(ActiveSide::Right));
        // Never Middle: a weapon Attack is always Left or Right in the corpus.
        for x in [0.0, 0.05, 0.49, 0.51, 0.99, 1.0] {
            let s = classify_side_from_x(x).unwrap();
            assert!(matches!(s, ActiveSide::Left | ActiveSide::Right), "got {s:?} for x={x}");
        }
        // Out of range / non-finite → no classification.
        assert_eq!(classify_side_from_x(-0.1), None);
        assert_eq!(classify_side_from_x(1.5), None);
        assert_eq!(classify_side_from_x(f32::NAN), None);
    }

    /// A `PlayerCombatInputPosition` is a 30 Hz POINTER STREAM: it records geometry
    /// and must never itself resolve a swing.
    #[test]
    fn pointer_stream_updates_geometry_and_never_swings() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        let before = combat.fighters[1].health;
        for i in 0..30 {
            let out = on_c2s_input(
                &mut combat,
                0,
                &make_pos_frame(0.80, 0.45, 0.0),
                now + Duration::from_millis(i * 33),
            );
            assert!(out.is_empty(), "a pointer sample must emit nothing (frame {i})");
        }
        assert_eq!(combat.fighters[1].health, before, "no damage from pointer samples alone");
        assert_eq!(combat.fighters[0].last_swing, None, "no swing was committed");
        assert!((combat.fighters[0].last_input_x.unwrap() - 0.80).abs() < 1e-4);
    }

    /// **The Phase 4.1 crux.** The combo ramp must follow what the player actually
    /// did. Tapping the SAME side over and over never advances the combo; alternating
    /// does. Under the old synthetic alternator both cases ramped identically.
    #[test]
    fn combo_follows_the_players_real_sides() {
        let now = Instant::now();
        let step = Duration::from_millis(900); // > weapon cadence

        // (a) Repeating the RIGHT side: combo stays pinned at 0 forever.
        let mut same = live_combat(now);
        for i in 1..=5u32 {
            let combo = swing_at(&mut same, 0, 0.814, now + step * i);
            assert_eq!(combo, 0, "repeating one side must not build combo (swing {i})");
            assert_eq!(same.fighters[0].last_combo_side, ActiveSide::Right);
        }

        // (b) Alternating Right/Left/Right/…: the combo ramps 0,1,2,3,4.
        let mut alt = live_combat(now);
        for i in 1..=5u32 {
            let x = if i % 2 == 1 { 0.814 } else { 0.213 };
            let combo = swing_at(&mut alt, 0, x, now + step * i);
            assert_eq!(combo, i - 1, "alternating sides must ramp the combo (swing {i})");
        }

        // The two must diverge — that is the behavioural fix.
        assert!(alt.fighters[0].combo_count > same.fighters[0].combo_count);
    }

    /// A left-half press is a LEFT swing and a right-half press is a RIGHT swing,
    /// end-to-end through `on_c2s_input`.
    #[test]
    fn press_position_drives_the_committed_side() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        swing_at(&mut combat, 0, 0.213, now + Duration::from_millis(900));
        assert_eq!(combat.fighters[0].last_combo_side, ActiveSide::Left);
        swing_at(&mut combat, 0, 0.814, now + Duration::from_millis(1800));
        assert_eq!(combat.fighters[0].last_combo_side, ActiveSide::Right);
    }

    /// FALLBACK: with no pointer stream at all (a bot, or a client that sends only
    /// bare carrier-54 bodies) the synthetic alternation still drives the fight, so
    /// nothing can hang.
    #[test]
    fn no_pointer_stream_falls_back_to_alternation() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        assert!(classified_side_for(&combat.fighters[0], now).is_none(), "no sample yet");
        let step = Duration::from_millis(900);
        let mut sides = Vec::new();
        for i in 1..=4u32 {
            // Bare unstructured carrier-54 swing — no geometry anywhere.
            let _ = on_c2s_input(&mut combat, 0, &[0x84, 0x36], now + step * i);
            // Commit no longer emits damage — it queues the hit. Assert on the queue,
            // which is what "the swing was accepted" now means.
            assert!(
                !combat.pending_hits.is_empty(),
                "the fallback must still commit a swing"
            );
            // The side is settled at commit, but land the hit so the combo advances
            // exactly as it did before the impact moved.
            land(&mut combat, now + step * i);
            sides.push(combat.fighters[0].last_combo_side);
        }
        assert_eq!(
            sides,
            vec![ActiveSide::Right, ActiveSide::Left, ActiveSide::Right, ActiveSide::Left],
            "fallback alternates so a bot match progresses"
        );
    }

    /// A pointer sample older than the TTL is stale and must not classify a swing
    /// (the client only streams gmid 47 while a finger is down, so a survivor from a
    /// previous gesture would be misleading).
    #[test]
    fn stale_pointer_sample_is_ignored() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        on_c2s_input(&mut combat, 0, &make_pos_frame(0.213, 0.5, 0.0), now);
        assert_eq!(
            classified_side_for(&combat.fighters[0], now + SIDE_CLASSIFY_SAMPLE_TTL),
            Some(ActiveSide::Left),
            "still fresh at exactly the TTL"
        );
        assert_eq!(
            classified_side_for(
                &combat.fighters[0],
                now + SIDE_CLASSIFY_SAMPLE_TTL + Duration::from_millis(1)
            ),
            None,
            "past the TTL the sample is dropped and the fallback takes over"
        );
    }

    /// Round reset clears the geometry, so the first swing of a new round can never
    /// inherit the previous round's pointer position.
    #[test]
    fn round_reset_clears_pointer_geometry() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        on_c2s_input(&mut combat, 0, &make_pos_frame(0.9, 0.5, 0.0), now);
        assert!(combat.fighters[0].last_input_x.is_some());
        combat.reset_fighters_for_next_round(now);
        assert_eq!(combat.fighters[0].last_input_x, None);
        assert_eq!(combat.fighters[0].last_input_at, None);
        assert_eq!(classified_side_for(&combat.fighters[0], now), None);
    }

    /// **The charge timer is SERVER-measured, never client-claimed.** A client that
    /// reports a full 2.8 s charge on an instantaneous tap gets no crit: the damage
    /// is identical to an honest uncharged swing. The client value is kept only as
    /// telemetry.
    #[test]
    fn client_claimed_charge_cannot_buy_a_crit() {
        let now = Instant::now();
        let t = now + Duration::from_millis(900);

        // Honest: press and release in the same instant (0 s hold), no claim.
        let mut honest = live_combat(now);
        on_c2s_input(&mut honest, 0, &make_pos_frame(0.814, 0.5, 0.0), t);
        on_c2s_input(&mut honest, 0, &make_act_frame(true, 0.0, false), t);
        on_c2s_input(&mut honest, 0, &make_act_frame(false, 0.0, false), t);
        land(&mut honest, t);
        let honest_dmg = honest.fighters[1].health;

        // Cheating: identical timing, but the client CLAIMS a 2.8 s charge.
        let mut liar = live_combat(now);
        on_c2s_input(&mut liar, 0, &make_pos_frame(0.814, 0.5, 0.0), t);
        on_c2s_input(&mut liar, 0, &make_act_frame(true, 2.817, false), t);
        on_c2s_input(&mut liar, 0, &make_act_frame(false, 2.817, false), t);
        land(&mut liar, t);

        assert_eq!(
            liar.fighters[1].health, honest_dmg,
            "a client-claimed charge must not increase damage"
        );
        // …but the claim IS recorded for telemetry.
        assert!((liar.fighters[0].last_client_charge.unwrap() - 2.817).abs() < 1e-3);

        // And an honest, genuinely-held charge DOES crit (server-measured).
        let mut real = live_combat(now);
        on_c2s_input(&mut real, 0, &make_pos_frame(0.814, 0.5, 0.0), t);
        on_c2s_input(&mut real, 0, &make_act_frame(true, 0.0, false), t);
        let held_to =
            t + Duration::from_secs_f32(critical_hold_secs(&real.fighters[0]) + 0.1);
        on_c2s_input(&mut real, 0, &make_act_frame(false, 1.3, false), held_to);
        land(&mut real, held_to);
        assert!(
            real.fighters[1].health < honest_dmg,
            "a real server-measured full hold must crit for more than an uncharged swing"
        );
    }

    /// Phase 4.3: `consumablesPerRound` is 1 and the budget resets between rounds.
    #[test]
    fn consumables_are_gated_per_round() {
        let now = Instant::now();
        let mut combat = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = combat.alloc_net_object_id();
            combat.fighters.push(Fighter::new(slot, obj, super::super::loadout::starter(), now));
        }
        assert_eq!(super::super::state::CONSUMABLES_PER_ROUND, 1);
        assert!(use_consumable(&mut combat, 0, now), "the first consumable is allowed");
        assert!(!use_consumable(&mut combat, 0, now), "the second is refused (1 per round)");
        assert!(use_consumable(&mut combat, 1, now), "the budget is per FIGHTER");
        combat.reset_fighters_for_next_round(now);
        assert!(use_consumable(&mut combat, 0, now), "the budget resets between rounds");
        assert!(!use_consumable(&mut combat, 9, now), "an out-of-range slot is refused");
    }

    /// Build a c2s `EquipAbilitiesAndConsumables` (56) declaring `uuid` for the avatar
    /// net object `obj` — the same wire shape as prod s127 #954909.
    fn make_equip_consumable_frame(obj: i32, uuid: &str, charges: i32) -> Vec<u8> {
        let mut w = arena_proto::NetDataWriter::new();
        w.int(0, obj)
            .byte(1, 56)
            .byte(2, 3) // Autonomous (c2s)
            .byte(3, arena_proto::GameMessageId::EquipAbilitiesAndConsumables as u8)
            .string(4, uuid)
            .int(5, charges);
        let mut v = vec![0xBEu8, 0x36];
        v.extend_from_slice(&w.finish());
        v
    }

    /// Build a c2s `RequestConsumeConsumable` (63) for avatar net object `obj` — the
    /// bare NetObjectInfo + gmid shape of prod s127 #962747.
    fn make_request_consume_frame(obj: i32) -> Vec<u8> {
        let mut w = arena_proto::NetDataWriter::new();
        w.int(0, obj)
            .byte(1, 56)
            .byte(2, 3)
            .byte(3, arena_proto::GameMessageId::RequestConsumeConsumable as u8);
        let mut v = vec![0xBEu8, 0x36];
        v.extend_from_slice(&w.finish());
        v
    }

    /// The consumable WIRE path end-to-end: op56 declares the item, op63 spends the
    /// round's single charge and is answered with an op64 to BOTH players carrying that
    /// item's UUID; a second op63 in the same round is refused; the budget resets next
    /// round. Also proves op63 is no longer mis-resolved as a weapon swing.
    #[test]
    fn consumable_request_is_answered_with_perform_and_gated_per_round() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        let obj = combat.fighters[0].net_object_id;
        const POTION: &str = "d826ea12-e583-47c1-a50f-4de608281735";

        // With no op56 yet, an op63 is refused — the UUID is never fabricated, and no
        // charge is burned.
        assert!(on_c2s_input(&mut combat, 0, &make_request_consume_frame(obj), now).is_empty());
        assert_eq!(combat.fighters[0].consumables_used, 0);

        // op56 latches the equipped item.
        assert!(on_c2s_input(&mut combat, 0, &make_equip_consumable_frame(obj, POTION, 6), now)
            .is_empty());
        assert_eq!(combat.fighters[0].equipped_consumable.as_deref(), Some(POTION));

        // op63 → op64 to both players.
        let target_hp_before = combat.fighters[1].health;
        let out = on_c2s_input(&mut combat, 0, &make_request_consume_frame(obj), now);
        // Assert on the op64 frames specifically rather than on a bare length: a
        // drink now also emits the op78 visual, so a length check silently couples
        // this test to how many OTHER frames the path happens to send.
        let op64: Vec<_> = out
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(64))
            .collect();
        assert_eq!(op64.len(), 2, "op64 goes to both players");
        let expect = messages::perform_consume_consumable(obj, POTION);
        assert_eq!(op64[0].1, expect, "byte-identical to the op64 builder");
        assert_eq!(op64[1].1, expect);
        // ...and the drink visual accompanies it, to both players.
        let op78 = out
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(78))
            .count();
        assert_eq!(op78, 2, "the op78 drink visual also goes to both players");
        assert_eq!(
            combat.fighters[1].health, target_hp_before,
            "an op63 must NOT be resolved as a phantom weapon swing"
        );

        // consumablesPerRound is 1 → the second request in the same round is refused.
        assert!(on_c2s_input(&mut combat, 0, &make_request_consume_frame(obj), now).is_empty());

        // …and the budget (plus the latched item) survives into the next round.
        combat.reset_fighters_for_next_round(now);
        combat.phase = FlowState::StateTimeout;
        let out2 = on_c2s_input(&mut combat, 0, &make_request_consume_frame(obj), now);
        let op64_again = out2
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(64))
            .count();
        assert_eq!(op64_again, 2, "the budget resets between rounds");
    }

    /// op56 is a loadout declaration, so it must latch even OUTSIDE the live round —
    /// retail uploads it during round-start setup, before `StateTimeout` opens.
    #[test]
    fn equipped_consumable_latches_before_the_live_round() {
        let now = Instant::now();
        let mut combat = live_combat(now);
        combat.phase = FlowState::BackendMatchCreated;
        let obj = combat.fighters[0].net_object_id;
        const POTION: &str = "819094ad-e749-4c02-9210-38c3bb1ec535";
        assert!(on_c2s_input(&mut combat, 0, &make_equip_consumable_frame(obj, POTION, 3), now)
            .is_empty());
        assert_eq!(combat.fighters[0].equipped_consumable.as_deref(), Some(POTION));
    }

    /// A cast now emits op53 `PlayerChannelingStateChange` to BOTH players, right after
    /// the op38 echo — the cast-animation feedback that was previously missing. The
    /// channel time is the CASTER'S OWN ability's shipped `_channelDuration`, looked up
    /// per-UUID (never a hard-coded one).
    #[test]
    fn ability_cast_emits_channeling_state_change_for_the_casters_own_ability() {
        const FIREBALL: &str = "d07a8d30-9a1c-49b0-866d-97a8aa1534cf";
        const LIGHTNING: &str = "7fc15804-1637-40a9-8dcc-3ea1eb0f778d";

        let cast = |uuid: &str| -> Vec<(usize, Vec<u8>)> {
            let now = Instant::now();
            let mut combat = live_combat(now);
            // Give the caster plenty of magicka so the resource gate passes.
            combat.fighters[0].magicka = combat.fighters[0].max_magicka;
            let mut frame = vec![
                0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, 0x02, 0x00, 0x00, 0x38, 0x03,
                0x25, 0x24, 0x00,
            ];
            frame.extend_from_slice(uuid.as_bytes());
            on_c2s_input(&mut combat, 0, &frame, now)
        };

        for (uuid, want_secs) in [(FIREBALL, 0.9f32), (LIGHTNING, 0.5f32)] {
            let out = cast(uuid);
            let chan: Vec<&(usize, Vec<u8>)> = out
                .iter()
                .filter(|(_, f)| messages::user_message_gmid(f) == Some(53))
                .collect();
            assert_eq!(chan.len(), 2, "op53 goes to both players ({uuid})");
            assert_eq!(chan[0].0, 0, "the caster gets one");
            assert_eq!(chan[1].0, 1, "the opponent gets one");
            assert_eq!(chan[0].1, chan[1].1, "both receive identical bytes");

            let nd = arena_proto::parse_netdata(&chan[0].1[2..]);
            assert!(nd.ok);
            assert_eq!(nd.int(1), Some(56), "on the Avatar net object");
            assert_eq!(nd.int(2), Some(1), "Authority");
            assert_eq!(nd.string(9), Some(uuid), "carries the cast ability's own UUID");
            let secs = match nd.props.get(&8) {
                Some(arena_proto::NetDataValue::Float(v)) => *v,
                other => panic!("propId 8 must be a Float, got {other:?}"),
            };
            assert!(
                (secs - want_secs).abs() < 1e-6,
                "{uuid}: propId 8 must be that ability's shipped _channelDuration \
                 ({want_secs}), got {secs}"
            );

            // The op38 cast echo must still precede the op53 (retail ordering).
            let i38 = out.iter().position(|(_, f)| messages::user_message_gmid(f) == Some(38));
            let i53 = out.iter().position(|(_, f)| messages::user_message_gmid(f) == Some(53));
            assert!(i38 < i53, "retail sends op38 before op53");
        }
    }

    /// Phase 3.13: a staggered fighter's combat inputs are dropped, and the stagger
    /// lasts `CombatParameters.baseStaggerDuration` (1.5 s).
    #[test]
    fn stagger_locks_inputs_for_the_shipped_duration() {
        use super::super::state::BASE_STAGGER_DURATION_SECS;
        // PvP value (PvpDefaultSettings.BASE_STAGGER_DURATION), not the PvE 1.5.
        assert!((BASE_STAGGER_DURATION_SECS - 2.5).abs() < 1e-6);
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, super::super::loadout::starter(), now);
        f.apply_stagger(now);
        assert!(f.is_staggered(now));
        assert_eq!(f.actor_state(), super::super::state::ActorStateType::Staggered);
        assert!(f.blocking_until.is_none(), "a stagger drops the guard");
        // Still locked just before the duration, recovered just after.
        assert!(f.is_staggered(now + Duration::from_millis(2400)));
        assert!(!f.is_staggered(now + Duration::from_millis(2600)));
        assert!(f.reconcile_stagger(now + Duration::from_millis(2600)));
        assert_eq!(f.actor_state(), super::super::state::ActorStateType::Idle);
    }

    /// Phase 3.14: a simultaneous double-KO scores nothing; a 1-1 draw at the final
    /// round is broken on remaining HP fraction, then on the lower `pvpTrophies`.
    #[test]
    fn double_ko_scores_nothing_and_the_draw_tiebreak_is_ordered() {
        use super::super::state::RoundOutcome;
        let now = Instant::now();
        let mut combat = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = combat.alloc_net_object_id();
            combat.fighters.push(Fighter::new(slot, obj, super::super::loadout::starter(), now));
        }
        assert_eq!(combat.round_outcome(), RoundOutcome::Ongoing);
        combat.fighters[1].take_damage(u32::MAX);
        assert_eq!(combat.round_outcome(), RoundOutcome::Win { winner: 0 });
        combat.fighters[0].take_damage(u32::MAX);
        assert_eq!(combat.round_outcome(), RoundOutcome::DoubleKo);

        // Neither side scores on a double-KO.
        let before = combat.rounds_won;
        let _ = on_round_ending_death(&mut combat, 0, now);
        assert_eq!(combat.rounds_won, before, "a double-KO scores nothing");

        // Tiebreak: higher remaining HP fraction first.
        combat.reset_fighters_for_next_round(now);
        combat.fighters[1].take_damage(100);
        assert_eq!(combat.draw_tiebreak_winner((0, 0)), 0, "more HP left wins");
        // Equal HP → the LOWER pvpTrophies (the underdog) wins.
        combat.reset_fighters_for_next_round(now);
        assert_eq!(combat.draw_tiebreak_winner((900, 100)), 1);
        assert_eq!(combat.draw_tiebreak_winner((100, 900)), 0);
        assert_eq!(combat.draw_tiebreak_winner((500, 500)), 0, "fully tied → slot 0");
    }
}

#[cfg(test)]
mod shipped_effects_tests {
    use super::*;
    use super::super::state::{DamageNegationSource, Fighter, StatusEffectType};
    use super::super::loadout;

    fn combat2(now: Instant) -> MatchCombat {
        let mut c = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            c.fighters.push(Fighter::new(slot, obj, loadout::starter(), now));
        }
        c.phase = FlowState::StateTimeout;
        c
    }

    /// Seven abilities used to spend a resource and produce nothing. These assert the
    /// shipped numbers now land, by UUID lookup rather than hardcoded values, so the
    /// test tracks the game data instead of restating it.
    fn uuid_of(editor: &str) -> &'static str {
        super::super::gamedata::ABILITIES
            .iter()
            .find(|a| a.editor_name == editor)
            .map(|a| a.uuid)
            .unwrap_or_else(|| panic!("{editor} missing from the shipped table"))
    }

    /// THE SYSTEMIC MANEUVER BUG: all 17 maneuvers dealt identical damage because
    /// `parameters.bonusDamage` and the grip multipliers were generated into the
    /// tables and read by nobody. Guardbreaker (131.15) and Quick Strikes (13.78)
    /// cannot be the same hit.
    ///
    /// Differential across maneuvers, so it cannot pass on a hardcoded constant.
    #[test]
    fn maneuvers_carry_their_own_authored_bonus_damage() {
        let r = |editor: &str| {
            super::super::gamedata::ability_rank_clamped(uuid_of(editor), 1)
                .unwrap_or_else(|| panic!("{editor} rank 1"))
        };
        // One-handed (a shield is equipped).
        let gb = maneuver_bonus_damage(&r("Guardbreaker"), false);
        let qs = maneuver_bonus_damage(&r("QuickStrikes"), false);
        assert!(gb > 0.0, "Guardbreaker must contribute a bonus, got {gb}");
        assert!(qs > 0.0, "QuickStrikes must contribute a bonus, got {qs}");
        assert!(
            gb > qs * 2.0,
            "Guardbreaker ({gb:.2}) must hit far harder than QuickStrikes ({qs:.2})"
        );
    }

    /// The power-attack family is authored at its TWO-handed figure and halved in one
    /// hand; the quick-strikes family is the other way round and gets NOTHING in two
    /// hands. A single shared multiplier would fail one of these two assertions.
    #[test]
    fn grip_multipliers_follow_the_authored_family() {
        let r = |editor: &str| {
            super::super::gamedata::ability_rank_clamped(uuid_of(editor), 1)
                .unwrap_or_else(|| panic!("{editor} rank 1"))
        };
        let pa_1h = maneuver_bonus_damage(&r("PowerAttack"), false);
        let pa_2h = maneuver_bonus_damage(&r("PowerAttack"), true);
        assert!(
            pa_2h > pa_1h && (pa_1h * 2.0 - pa_2h).abs() < 0.01,
            "PowerAttack one-handed ({pa_1h:.2}) must be half of two-handed ({pa_2h:.2})"
        );

        let qs_1h = maneuver_bonus_damage(&r("QuickStrikes"), false);
        let qs_2h = maneuver_bonus_damage(&r("QuickStrikes"), true);
        assert!(qs_1h > 0.0, "QuickStrikes one-handed must get its bonus");
        assert_eq!(qs_2h, 0.0, "…and two-handed must get none (2H multiplier is 0)");
    }

    /// Reckless Fury is a BUFF: it ships bonusDamage 0 with both multipliers 0, so it
    /// must contribute no swing damage at all. The server used to resolve it as an
    /// ordinary Middle weapon hit.
    #[test]
    fn reckless_fury_contributes_no_swing_damage() {
        let r = super::super::gamedata::ability_rank_clamped(uuid_of("RecklessFury"), 1)
            .expect("RecklessFury rank 1");
        assert_eq!(maneuver_bonus_damage(&r, false), 0.0);
        assert_eq!(maneuver_bonus_damage(&r, true), 0.0);
    }

    /// THE PRODUCTION BUG (match fffe01ca-9b20-4cb8-bd8c-a7ce1cfeaf29): the AI cast
    /// Reckless Fury at 18:38:48 and was stunned at 18:38:50 — two seconds into a
    /// five-second window that is supposed to make stunning impossible. Fury had no
    /// persistent state at all, so nothing was there to prevent it.
    #[test]
    fn reckless_fury_cannot_be_stunned_for_its_window() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_reckless_fury(&mut c, 0, 1, now);
        assert!(c.fighters[0].has_reckless_fury(now), "precondition: Fury is up");

        // The exact production timing: the stun arrives 2s in.
        let at_stun = now + Duration::from_secs(2);
        c.fighters[0].apply_stagger_for(at_stun, 2.5);
        assert!(
            !c.fighters[0].is_staggered(at_stun),
            "a fighter in Reckless Fury must not be stunnable"
        );

        // …and is stunnable again once the 5s window has lapsed, so the guard is a
        // window and not a permanent immunity.
        let after = now + Duration::from_secs_f32(5.5);
        assert!(!c.fighters[0].has_reckless_fury(after), "the window has closed");
        c.fighters[0].apply_stagger_for(after, 2.5);
        assert!(c.fighters[0].is_staggered(after), "and normal stuns resume");
    }

    /// "…cannot be killed." Floors at 1 HP for the window, and dies normally after.
    #[test]
    fn reckless_fury_prevents_death_for_its_window() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_reckless_fury(&mut c, 0, 1, now);
        c.fighters[0].take_damage_at(u32::MAX, now);
        assert!(!c.fighters[0].is_dead(), "Fury must survive a lethal hit");
        assert_eq!(c.fighters[0].health, 1, "…at exactly 1 HP");

        let after = now + Duration::from_secs_f32(5.5);
        c.fighters[0].take_damage_at(u32::MAX, after);
        assert!(c.fighters[0].is_dead(), "and dies normally once Fury lapses");
    }

    /// Fury is a self-buff: casting it must NOT produce a weapon hit. The generic
    /// maneuver arm gave the caster a free phantom attack on every cast.
    #[test]
    fn casting_reckless_fury_emits_no_damage_to_the_target() {
        let now = Instant::now();
        let mut c = combat2(now);
        let before = c.fighters[1].health;
        apply_reckless_fury(&mut c, 0, 1, now);
        assert_eq!(c.fighters[1].health, before, "the opponent must take no damage");
    }

    /// The window carries the rank's authored `_duration` and a weapon-class bonus
    /// picked from `bonusDamages`, not a hardcoded number.
    #[test]
    fn reckless_fury_uses_its_authored_duration_and_bonus() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_reckless_fury(&mut c, 0, 1, now);
        assert!(c.fighters[0].reckless_fury_bonus > 0.0, "a class bonus was chosen");
        // 5.0s authored: up just before, down just after.
        assert!(c.fighters[0].has_reckless_fury(now + Duration::from_secs_f32(4.9)));
        assert!(!c.fighters[0].has_reckless_fury(now + Duration::from_secs_f32(5.1)));
    }

    /// op53 `PlayerChannelingStateChange` announces a CHANNEL. An ability that does
    /// not channel must not send one — we used to send a 0.0-second frame for every
    /// non-maneuver, which is the same malformed message that made bashes animate
    /// wrongly (report #24) and is the likely "spell pose with no spell text".
    ///
    /// The predicate is "ships a channel field at all", not "ships
    /// `_channelDuration`": Frostbite and Consuming Inferno channel via
    /// `_channelMaxLength` and retail does send op53 for them. This asserts the split
    /// against every player ability whose retail op53 behaviour was measured — ten
    /// carriers and four silent ones — so it cannot pass by accident in one direction.
    #[test]
    fn only_channelled_abilities_announce_a_channel() {
        let channels = |editor: &str| {
            let r = super::super::gamedata::ability_rank_clamped(uuid_of(editor), 1)
                .unwrap_or_else(|| panic!("{editor} rank 1"));
            r.channel_duration().is_some_and(|v| v > 0.0)
                || r.get(super::super::gamedata::AbilityField::ChannelMaxLength)
                    .is_some_and(|v| v > 0.0)
        };
        for editor in [
            "ResistElements", "LightningBolt", "Fireball", "IceSpike", "Frostbite",
            "Paralyze", "PosionCloud", "DelayedLightningBolt", "Blind", "ConsumingInferno",
        ] {
            assert!(channels(editor), "{editor} carries an op53 in retail and must send one");
        }
        for editor in ["Ward", "Absorb", "MagickaSurge", "BlizzardArmor"] {
            assert!(
                !channels(editor),
                "{editor} sends NO op53 in retail (0 across 144 Ward and 30 Absorb cast \
                 echoes) — it is an instant buff, not a channel"
            );
        }
    }

    /// A dodge is a ONE-SECOND reactive window, not a banked shield. `_dodgeDuration`
    /// is authored at 1.0 s on all four dodge maneuvers and was ignored — the pool got
    /// the 3600 s "until consumed" placeholder, so a Dodging Strike stayed armed for an
    /// hour and could eat a hit a round later.
    #[test]
    fn a_dodge_pool_expires_after_its_authored_second() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, uuid_of("DodgingStrike"), 1, 500.0, false, false, now);
        let pool = c.fighters[0].negation_pools.first().expect("a dodge pool").clone();
        assert!(
            pool.expires_at <= now + Duration::from_secs_f32(1.05),
            "the dodge must lapse after its authored ~1.0s, not an hour"
        );
        assert!(pool.expires_at > now, "…but it is armed now");

        c.fighters[0].prune_negation_pools(now + Duration::from_secs_f32(1.5));
        assert!(c.fighters[0].negation_pools.is_empty(), "and is gone a second later");
    }

    /// Delayed Lightning Bolt trades time for damage: `_delayDuration` 4.0 is ADDITIVE
    /// to `_channelDuration` 1.3, so it lands at ~5.3 s. The delay field was read by
    /// nobody, so it landed after ~1.3 s — indistinguishable from the plain bolt.
    ///
    /// Differential against the ordinary Lightning Bolt so it cannot pass on a
    /// constant.
    #[test]
    fn delayed_lightning_bolt_waits_for_its_authored_delay() {
        let delayed = super::ability_impact_delay(uuid_of("DelayedLightningBolt"), 1);
        let plain = super::ability_impact_delay(uuid_of("LightningBolt"), 1);
        assert!(
            delayed >= Duration::from_secs_f32(5.0),
            "delayed bolt must wait channel+delay (~5.3s), got {delayed:?}"
        );
        assert!(
            delayed > plain + Duration::from_secs(3),
            "it must land far later than the plain bolt ({plain:?}), not alongside it"
        );
    }

    /// Indomitable Smash ships `_bonusResistance` 250 and it was read by nobody, so
    /// the maneuver cured conditions and then did nothing defensively at all.
    #[test]
    fn indomitable_smash_grants_its_authored_resistance() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, uuid_of("IndomitableSmash"), 1, 500.0, false, false, now);
        let total: f32 = c.fighters[0].transient_all_resistance.iter().map(|(v, _)| *v).sum();
        assert!(total >= 250.0, "expected the authored 250 flat resistance, got {total}");
    }

    /// Frostbite ships `_resistanceBonus` 13.13 against `_resistTypes` [1,2,3] — the
    /// three physical tracks — for the caster while it channels. Both fields were
    /// unread, so the spell dealt its damage and gave none of the protection its
    /// description promises.
    #[test]
    fn frostbite_grants_its_authored_physical_resistance_to_the_caster() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, uuid_of("Frostbite"), 1, 500.0, false, false, now);
        use super::super::state::DamageType;
        for ty in [DamageType::Slashing, DamageType::Cleaving, DamageType::Bashing] {
            let got: f32 = c.fighters[0]
                .transient_resistances
                .iter()
                .filter(|(t, _, _)| *t == ty)
                .map(|(_, v, _)| *v)
                .sum();
            assert!(got > 0.0, "{ty:?} must be resisted while Frostbite channels");
        }
        // …and NOT the elemental tracks: resistTypes is [1,2,3], physical only.
        let fire: f32 = c.fighters[0]
            .transient_resistances
            .iter()
            .filter(|(t, _, _)| *t == DamageType::Fire)
            .map(|(_, v, _)| *v)
            .sum();
        assert_eq!(fire, 0.0, "resistTypes is physical-only; Fire must not be covered");
    }

    /// Magicka Surge: a flat regen bonus for its duration, THEN a blackout window in
    /// which magicka does not regenerate at all. All three fields were unread, so the
    /// spell spent its cost and did nothing.
    #[test]
    fn magicka_surge_grants_regen_then_a_blackout() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, uuid_of("MagickaSurge"), 1, 500.0, false, false, now);
        let f = &c.fighters[0];
        assert!(f.magicka_surge_bonus > 0.0, "a regen bonus was granted");
        let surge_end = f.magicka_surge_until.expect("surge window");
        let blackout_end = f.no_magicka_regen_until.expect("blackout window");
        assert!(surge_end > now, "the surge is live now");
        assert!(
            blackout_end > surge_end,
            "the blackout must START when the surge ENDS, not run concurrently — \
             otherwise the drawback cancels the spell it is supposed to pay for"
        );
    }

    /// Consuming Inferno charges its caster stamina AND health for every second it
    /// burns. Both `_staminaCostPerSecond` and `_healthCostPerSecond` were unread, so
    /// the spell was pure upside.
    #[test]
    fn consuming_inferno_drains_its_caster_while_it_burns() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("ConsumingInferno");
        let (stam0, hp0) = (c.fighters[0].stamina, c.fighters[0].health);
        c.channels.push(super::super::state::ActiveChannel {
            caster_slot: 0,
            target_slot: 1,
            ability_uuid: u.to_string(),
            ability_level: 1,
            remaining_ticks: 3,
            magicka_full_at_cast: false,
            next_tick_at: now,
            interval_secs: super::super::damage::CHANNEL_TICK_INTERVAL_SECS,
        });
        let _ = super::apply_channel_ticks(&mut c, now);
        assert!(c.fighters[0].stamina < stam0, "stamina must drain: {stam0} -> {}", c.fighters[0].stamina);
        assert!(c.fighters[0].health < hp0, "health must drain: {hp0} -> {}", c.fighters[0].health);
        assert!(!c.fighters[0].is_dead(), "upkeep must never self-kill");
    }

    /// Thunderstorm is three bolts over nine seconds, not one immediate hit. It ships
    /// a per-BOLT `_damage`, so the DoT scheduler (which keys off `_damagePerSecond`)
    /// never saw it.
    #[test]
    fn thunderstorm_schedules_its_authored_bolts() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("Thunderstorm");
        let out = apply_ability_impact(
            &mut c, 0, 1, u, 1, super::super::state::AbilityTag::Damage, false, now,
        );
        assert!(!out.is_empty(), "the first bolt lands immediately");
        let ch = c.channels.iter().find(|ch| ch.ability_uuid == u).expect("bolts scheduled");
        assert_eq!(ch.remaining_ticks, 2, "3 bolts total = 1 now + 2 scheduled");
        assert!(
            (ch.interval_secs - 3.0).abs() < 0.01,
            "9s / 3 bolts = 3s apart, got {}",
            ch.interval_secs
        );
    }

    /// A Blizzard Armor is weak to fire: `_vulnerableDamageTypes` is [Fire], so the
    /// shield does not stop it. The shipped data names the TYPE and no magnitude, so
    /// "does not absorb" is the reading that invents nothing.
    #[test]
    fn a_blizzard_armor_does_not_absorb_fire() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, uuid_of("BlizzardArmor"), 1, 500.0, false, false, now);
        let pool = c.fighters[0].negation_pools.first().expect("a shield pool");
        assert!(
            pool.bypass_types.contains(&(super::super::state::DamageType::Fire as i32)),
            "Fire must bypass the ice shield, got {:?}",
            pool.bypass_types
        );
        use super::super::state::DamageType;
        let mut fire = vec![(DamageType::Fire, 80.0)];
        c.fighters[0].apply_negation_pools(&mut fire);
        assert_eq!(fire[0].1, 80.0, "fire passes through in full");
        let mut frost = vec![(DamageType::Frost, 80.0)];
        c.fighters[0].apply_negation_pools(&mut frost);
        assert!(frost[0].1 < 80.0, "…but frost is still absorbed");
    }

    #[test]
    fn a_dodge_ability_gives_the_caster_a_dodge_pool() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("DodgingStrike");
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, false, false, now);
        let pools = &c.fighters[0].negation_pools;
        assert_eq!(pools.len(), 1, "one dodge pool");
        assert_eq!(pools[0].source, DamageNegationSource::Dodge);
        assert!(pools[0].remaining > 0.0, "the shipped cap must be positive");
        // Dodging (12) is a pinned status id, so this one DOES get an op51 — to both.
        assert_eq!(out.len(), 2, "op51 Dodging to both viewers");
    }

    /// The op51 apply must carry the dodge's own duration, not 0.
    ///
    /// Measured, and unambiguous: of 405 captured `Dodging` (12) op51 frames,
    /// **all 204 applies carry duration 1.0** and all 201 removes carry -0.0. We
    /// sent 0.0 on the apply, so the client was told the dodge lasts no time at
    /// all. The pool was already bounded server-side; this is the client being
    /// told the same thing.
    ///
    /// Report #113 asked what a dodge looks like from our side. This is the half
    /// the player can actually see.
    #[test]
    fn the_dodge_status_announces_its_shipped_duration() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("DodgingStrike");
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, false, false, now);

        // op51 layout: propId 4 apply, 5 status, 6 duration (messages.rs).
        let durations: Vec<f32> = out
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(51))
            .filter_map(|(_, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                let status = nd.int(5)?;
                if status != StatusEffectType::Dodging as i64 {
                    return None;
                }
                match nd.get(6) {
                    Some(arena_proto::NetDataValue::Float(v)) => Some(*v),
                    _ => None,
                }
            })
            .collect();
        assert!(!durations.is_empty(), "an op51 Dodging must be emitted");
        for d in &durations {
            assert!(
                (*d - 1.0).abs() < 1e-3,
                "retail announces 1.0 on every one of 204 captured applies, got {d}"
            );
        }
    }

    /// The control: the pool's server-side expiry must still match what is
    /// announced, so the client and the server disagree about nothing. A fix that
    /// only changed the wire number would leave the two out of step.
    #[test]
    fn the_announced_duration_matches_the_pool_expiry() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("DodgingStrike");
        apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, false, false, now);
        let pool = &c.fighters[0].negation_pools[0];

        // Alive just inside the window, gone just outside it.
        assert!(pool.expires_at > now + Duration::from_millis(900));
        assert!(pool.expires_at < now + Duration::from_millis(1_100));

        c.fighters[0].prune_negation_pools(now + Duration::from_millis(1_500));
        assert!(
            c.fighters[0].negation_pools.is_empty(),
            "the dodge window must close a second after it opened"
        );
    }

    /// THE DODGE MUST BE TAKEN BACK WHEN THE WINDOW CLOSES.
    ///
    /// Retail sends the remove: of 405 captured `Dodging` (12) op51 frames, 204
    /// are applies and **201 are removes**. We sent only the apply, so the dodge
    /// indicator on the client had nothing to clear on — the same shape as the
    /// stun that stuck forever, which is why `emit_status_removals` exists.
    ///
    /// The fix is to let `tracked_statuses` SEE the dodge window, so the existing
    /// per-tick diff does the announcing. Report #113.
    #[test]
    fn the_dodge_window_closing_sends_an_op51_remove() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("DodgingStrike");
        apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, false, false, now);

        // A tick inside the window records the dodge as announced, and takes
        // nothing back — the dodge is still up.
        let during = emit_status_removals(&mut c, now + Duration::from_millis(500));
        assert!(
            dodging_removes(&during).is_empty(),
            "a live dodge must not be un-announced"
        );

        // A tick after it lapses takes it back, to both viewers.
        let after = emit_status_removals(&mut c, now + Duration::from_millis(1_500));
        assert_eq!(
            dodging_removes(&after).len(),
            2,
            "the closed dodge window must send an op51 remove to both viewers"
        );
    }

    /// The same remove is owed when the dodge CONNECTS, not only when it times
    /// out — `apply_negation_pools` drops a drained pool, so the window is over
    /// early and the client must hear about it.
    #[test]
    fn a_dodge_that_eats_a_hit_also_sends_its_remove() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("DodgingStrike");
        apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, false, false, now);
        emit_status_removals(&mut c, now);

        // A hit far bigger than the pool drains it outright.
        let mut components = [(crate::arena::combat::state::DamageType::Slashing, 5_000.0f32)];
        let neg = c.fighters[0].apply_negation_pools(&mut components);
        assert!(neg.negated || components[0].1 < 5_000.0, "the dodge must have eaten some of it");
        assert!(
            c.fighters[0].negation_pools.is_empty(),
            "a drained pool is dropped — the window is over"
        );

        let out = emit_status_removals(&mut c, now + Duration::from_millis(100));
        assert_eq!(
            dodging_removes(&out).len(),
            2,
            "a spent dodge must be taken back as well as an expired one"
        );
    }

    /// THE CONTROL. The storm-armor shields live in the same `negation_pools`
    /// vector but are announced from elsewhere; if the new scan were not
    /// source-scoped, the very first diff would emit a bogus `Dodging` remove for
    /// a shield — exactly the trap `announced_statuses` is documented against.
    #[test]
    fn a_shield_pool_is_never_announced_as_a_dodge() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, uuid_of("FirestormArmor"), 1, 500.0, false, false, now);
        assert!(
            c.fighters[0].negation_pools.iter().any(|p| p.source != DamageNegationSource::Dodge),
            "the fixture must actually hold a non-dodge pool, or this proves nothing"
        );
        assert!(
            !c.fighters[0].tracked_statuses(now).contains(&StatusEffectType::Dodging),
            "a storm-armor shield is not a dodge"
        );
        for t in [0u64, 500, 1_500, 5_000] {
            let out = emit_status_removals(&mut c, now + Duration::from_millis(t));
            assert!(
                dodging_removes(&out).is_empty(),
                "a shield must never produce a Dodging remove (t={t}ms)"
            );
        }
    }

    /// op51 `Dodging` REMOVE frames in a batch (propId 4 apply, 5 status).
    fn dodging_removes(out: &[(usize, Vec<u8>)]) -> Vec<usize> {
        out.iter()
            .enumerate()
            .filter(|(_, (_, f))| messages::user_message_gmid(f) == Some(51))
            .filter(|(_, (_, f))| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(5) == Some(StatusEffectType::Dodging as i64)
                    && matches!(nd.get(4), Some(arena_proto::NetDataValue::Bool(false)))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The three *Armor spells get a real shield. No op51: the elemental-armor status
    /// id is not pinned, and a guessed id is dropped silently by the client.
    #[test]
    fn an_armor_spell_gives_a_shield_pool_but_no_guessed_status() {
        let now = Instant::now();
        for name in ["FirestormArmor", "BlizzardArmor", "TempestArmor"] {
            let mut c = combat2(now);
            let out = apply_shipped_effects(&mut c, 0, 1, uuid_of(name), 1, 500.0, false, false, now);
            assert_eq!(c.fighters[0].negation_pools.len(), 1, "{name}: a shield pool");
            assert!(c.fighters[0].negation_pools[0].remaining >= 100.0, "{name}: shipped ~116");
            assert_eq!(out.len(), 2, "{name}: emits its now-known status id");
        }
    }

    /// PARALYSIS MUST LAST WHAT IT ANNOUNCES.
    ///
    /// `try_paralyze` puts the rank's `_duration` on the wire in the op51 apply, but
    /// used not to store it — so `reconcile_paralysis` released the victim on
    /// `paralyze_secs`, which nobody had set. At rank 12 that meant a 2.0 s freeze
    /// against an announced 3.1 s, released (and un-announced) a second early. The
    /// owner's report was that paralysis lands but "the visual is not as clear as it
    /// was in retail".
    ///
    /// Differential across ranks, so it cannot pass on a hard-coded constant.
    #[test]
    fn a_paralysis_lasts_its_own_ranks_duration_not_rank_ones() {
        use super::super::state::DamageType;

        let paralyse_at = |rank: u8| -> f32 {
            let now = Instant::now();
            let mut c = combat2(now);
            // A cast_poison well above even the top rank's threshold: these tests are
            // about the DURATION of the lock, not about what arms it.
            c.fighters[1].record_element_damage(DamageType::Poison, 500.0, now);
            let out = try_paralyze(&mut c, 0, 1, rank, 1_000.0, false, now);
            assert!(!out.is_empty(), "rank {rank} did not paralyse — the test would be vacuous");
            assert!(c.fighters[1].is_paralyzed(), "rank {rank} target not locked");
            c.fighters[1].paralyze_secs
        };

        let r1 = paralyse_at(1);
        let r12 = paralyse_at(12);

        // The shipped ranks: 2.0 at rank 1, 3.1 at rank 12.
        assert!((r1 - 2.0).abs() < 0.001, "rank 1 should hold for 2.0s, got {r1}");
        assert!((r12 - 3.1).abs() < 0.001, "rank 12 should hold for 3.1s, got {r12}");
        assert!(r12 > r1, "a higher rank must freeze for longer");
    }

    /// …and the victim must still BE paralysed when the announced window is most of
    /// the way through, then be released after it. This is the half that a stored-but-
    /// unused field would not catch.
    #[test]
    fn a_rank12_paralysis_still_holds_at_three_seconds() {
        use super::super::state::DamageType;
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].record_element_damage(DamageType::Poison, 500.0, now);
        try_paralyze(&mut c, 0, 1, 12, 1_000.0, false, now);

        // 2.5 s in: rank 1's window has long lapsed, rank 12's has not.
        reconcile_paralysis(&mut c.fighters[1], now + Duration::from_millis(2500));
        assert!(
            c.fighters[1].is_paralyzed(),
            "a rank-12 paralysis must still hold at 2.5s — it announced 3.1s"
        );

        // Past 3.1 s: released.
        reconcile_paralysis(&mut c.fighters[1], now + Duration::from_millis(3200));
        assert!(!c.fighters[1].is_paralyzed(), "it must release after its own duration");
    }

    /// The actual cast path must consume the effective jewellery-raised rank, not
    /// fall back to the base skill rank. The owner's base-2 + two-ring loadout is
    /// rank 12 and therefore announces/holds the shipped 3.1 second paralysis.
    #[test]
    fn an_equipped_rank12_paralyze_cast_uses_rank12_damage_and_duration() {
        use super::super::state::{AbilityTag, EquippedAbility};
        let now = Instant::now();
        let mut c = combat2(now);
        let uuid = uuid_of("Paralyze");
        c.fighters[0].loadout.abilities = vec![EquippedAbility {
            instance_uuid: uuid.into(),
            level: 12,
            tag: AbilityTag::Paralyze,
        }];
        c.fighters[0].loadout.paralyze_rank = 12;
        c.fighters[0].magicka = 10_000;
        let frame = messages::request_execute_ability(c.fighters[0].net_object_id, uuid);
        let ea = input::parse_execute_ability(&frame).expect("synthetic cast parses");
        let mut out = resolve_ability_cast(&mut c, 0, 1, &frame, &ea, now);
        out.extend(land_due_impacts(&mut c, now + Duration::from_secs(2)));

        assert!(c.fighters[1].is_paralyzed(), "rank-12 cast locks the victim");
        assert!((c.fighters[1].paralyze_secs - 3.1).abs() < 0.001);
        let status = out.iter().find(|(_, f)| {
            let nd = arena_proto::parse_netdata(&f[2..]);
            nd.int(3) == Some(51) && nd.int(5) == Some(9)
        }).expect("op51 Paralyzed reaches the wire");
        let nd = arena_proto::parse_netdata(&status.1[2..]);
        assert_eq!(nd.int(0), Some(c.fighters[1].net_object_id as i64), "victim Avatar");
        let duration = match nd.props.get(&6) {
            Some(arena_proto::NetDataValue::Float(v)) => *v,
            other => panic!("Paralyzed duration must be Float, got {other:?}"),
        };
        assert!((duration - 3.1).abs() < 0.001);
    }

    /// A paralysis must not inherit the duration of whatever last froze this fighter.
    /// `paralyze_secs` persists on the Fighter, so a FlashFreeze earlier in the round
    /// used to leak its duration into the next Paralyze.
    #[test]
    fn a_paralysis_does_not_inherit_a_previous_freezes_duration() {
        use super::super::state::DamageType;
        let now = Instant::now();
        let mut c = combat2(now);
        // Pretend an earlier effect left a long duration behind.
        c.fighters[1].paralyze_secs = 99.0;
        c.fighters[1].record_element_damage(DamageType::Poison, 500.0, now);

        try_paralyze(&mut c, 0, 1, 1, 1_000.0, false, now);
        assert!(
            (c.fighters[1].paralyze_secs - 2.0).abs() < 0.001,
            "rank 1 must set its OWN 2.0s, not keep the stale 99.0"
        );
    }

    /// A Paralyze that a Ward ate must NOT paralyse. The threshold used to be checked
    /// against `recent_element_damage(Poison)` — every poison point taken in the 5 s
    /// window from any source — so a cast that delivered literally zero damage still
    /// landed the lock as long as the victim happened to be poisoned already.
    ///
    /// Fails on the old code: the 500 seeded into the window armed it.
    #[test]
    fn a_fully_negated_paralyze_does_not_paralyze() {
        use super::super::state::DamageType;
        let now = Instant::now();
        let mut c = combat2(now);
        // The victim is already poisoned — the window is way over any threshold.
        c.fighters[1].record_element_damage(DamageType::Poison, 500.0, now);
        // …but THIS cast was fully negated, so it delivered nothing.
        try_paralyze(&mut c, 0, 1, 1, 0.0, false, now);
        assert!(
            !c.fighters[1].is_paralyzed(),
            "a cast that dealt no poison must not paralyse, however poisoned the target is"
        );
    }

    /// The complement: background poison alone must not arm someone else's Paralyze.
    /// A cast landing UNDER the rank's own `_damageToCauseParalyze` does nothing even
    /// when the sliding window is saturated.
    #[test]
    fn background_poison_does_not_arm_a_weak_paralyze() {
        use super::super::state::DamageType;
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].record_element_damage(DamageType::Poison, 500.0, now);
        let threshold = super::super::state::paralyze_damage_threshold(1);
        try_paralyze(&mut c, 0, 1, 1, threshold - 1.0, false, now);
        assert!(!c.fighters[1].is_paralyzed(), "under its own threshold: no lock");
        // …and one point over it does land, so the test cannot pass vacuously.
        try_paralyze(&mut c, 0, 1, 1, threshold + 1.0, false, now);
        assert!(c.fighters[1].is_paralyzed(), "over its own threshold: locked");
    }

    /// FlashFreeze locks the TARGET, not the caster, for the rank's own duration.
    #[test]
    fn flashfreeze_locks_the_target_for_its_shipped_duration() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("FlashFreeze"), 1, 500.0, false, false, now);
        assert!(c.fighters[1].is_paralyzed(), "the TARGET is locked");
        assert!(!c.fighters[0].is_paralyzed(), "the caster is not");
        assert!(c.fighters[1].paralyze_secs >= 2.0, "the rank's own duration, not the default");
        // Frozen (5) and Paralyzed (9) are both pinned → 2 statuses × 2 viewers.
        assert_eq!(out.len(), 4, "op51 Frozen + Paralyzed to both viewers");
    }

    /// ShieldOfMania / ReflectingBash cut incoming damage by a FLAT rating for the
    /// block window. The plan flagged fraction-vs-flat as undecidable from the field
    /// name; the shipped ranges (50→139, 111→182) settle it, and `_blockDuration` 0.50 s
    /// supplies the expiry I had thought was missing.
    #[test]
    fn a_block_buff_gives_a_timed_flat_reduction() {
        let now = Instant::now();
        for name in ["ShieldOfMania", "ReflectingBash"] {
            let mut c = combat2(now);
            begin_ability_guard(&mut c, 0, uuid_of(name), 1, now);
            let tr = &c.fighters[0].transient_resistances;
            assert!(!tr.is_empty(), "{name}: a reduction must land");
            assert!(tr.iter().all(|(_, amt, _)| *amt >= 50.0), "{name}: flat rating, not a fraction");
            // The window is short on purpose — half a second, not a standing buff.
            let expiry = tr[0].2;
            assert!(expiry > now && expiry <= now + Duration::from_secs(1), "{name}: ~0.5s window");
        }
    }

    /// Blind: the green fog on the VICTIM, gated on the hit landing hard enough.
    /// `Blind = 8` and there is no blind ACTOR state, so presentation comes from op51;
    /// the server also tracks its lifetime for expiry and cure removal.
    #[test]
    fn blind_fires_only_when_the_hit_clears_its_threshold() {
        let now = Instant::now();
        let u = uuid_of("Blind");
        // A big hit → blinded.
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, 9_999.0, false, false, now);
        let blind = out.iter().filter(|(_, f)| {
            let nd = arena_proto::parse_netdata(&f[2..]);
            nd.int(3) == Some(51) && nd.int(5) == Some(8)
        }).count();
        assert_eq!(blind, 2, "op51 Blind (8) to both viewers");
        for (_, frame) in out.iter().filter(|(_, f)| {
            let nd = arena_proto::parse_netdata(&f[2..]);
            nd.int(3) == Some(51) && nd.int(5) == Some(8)
        }) {
            let nd = arena_proto::parse_netdata(&frame[2..]);
            assert_eq!(nd.int(0), Some(c.fighters[1].net_object_id as i64), "victim Avatar");
            assert_eq!(nd.int(7), Some(255), "Blind is not an elemental-source status");
        }
        assert!(
            c.fighters[1].effects.iter().any(|e| e.effect == StatusEffectType::Blind),
            "Blind must be tracked so it can expire or be cured",
        );
        assert!(emit_status_removals(&mut c, now).is_empty(), "fresh Blind has not lapsed");
        let secs = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.duration())
            .expect("Blind ships a duration");
        let removed = emit_status_removals(
            &mut c,
            now + Duration::from_secs_f32(secs) + Duration::from_millis(1),
        );
        let blind_removes = removed.iter().filter(|(_, f)| {
            let nd = arena_proto::parse_netdata(&f[2..]);
            nd.int(3) == Some(51)
                && nd.props.get(&4) == Some(&arena_proto::NetDataValue::Bool(false))
                && nd.int(5) == Some(8)
        }).count();
        assert_eq!(blind_removes, 2, "op51 Blind remove must reach both viewers");

        // A hit of zero → no blind. A threshold effect must not fire on a cast that
        // did not land.
        let mut c2 = combat2(now);
        let out2 = apply_shipped_effects(&mut c2, 0, 1, u, 1, 0.0, false, false, now);
        assert!(
            !out2.iter().any(|(_, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(8)
            }),
            "no damage means no blindness"
        );
    }

    #[test]
    fn a_real_blind_cast_emits_the_victim_effect_to_both_players() {
        use super::super::state::{AbilityTag, EquippedAbility};
        let now = Instant::now();
        let mut c = combat2(now);
        let uuid = uuid_of("Blind");
        c.fighters[0].loadout.abilities = vec![EquippedAbility {
            instance_uuid: uuid.into(),
            level: 1,
            tag: AbilityTag::Damage,
        }];
        c.fighters[0].magicka = 10_000;
        let frame = messages::request_execute_ability(c.fighters[0].net_object_id, uuid);
        let ea = input::parse_execute_ability(&frame).expect("synthetic cast parses");
        let mut out = resolve_ability_cast(&mut c, 0, 1, &frame, &ea, now);
        out.extend(land_due_impacts(&mut c, now + Duration::from_secs(2)));

        let effects: Vec<_> = out
            .iter()
            .filter(|(_, frame)| {
                let nd = arena_proto::parse_netdata(&frame[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(8)
            })
            .collect();
        assert_eq!(effects.len(), 2, "one Blind apply per viewer");
        assert!(effects.iter().all(|(_, frame)| {
            arena_proto::parse_netdata(&frame[2..]).int(0)
                == Some(c.fighters[1].net_object_id as i64)
        }));
    }

    /// Indomitable Smash ships Blind in its cure list. Once the explicit cure remove
    /// is emitted, the ordinary expiry diff must not emit a duplicate next tick.
    #[test]
    fn indomitable_smash_cures_blind_once() {
        let now = Instant::now();
        let mut c = combat2(now);
        let blind = uuid_of("Blind");
        apply_shipped_effects(&mut c, 0, 1, blind, 1, 500.0, false, false, now);
        assert!(emit_status_removals(&mut c, now).is_empty());

        let indomitable_smash = uuid_of("IndomitableSmash");
        let removed = apply_status_cures(&mut c, 1, indomitable_smash, 1, now);
        assert_eq!(
            removed
                .iter()
                .filter(|(_, frame)| {
                    let nd = arena_proto::parse_netdata(&frame[2..]);
                    nd.int(3) == Some(51)
                        && nd.int(5) == Some(StatusEffectType::Blind as u16 as i64)
                        && nd.props.get(&4) == Some(&arena_proto::NetDataValue::Bool(false))
                })
                .count(),
            2,
            "one Blind removal per viewer",
        );
        assert!(
            !c.fighters[1]
                .effects
                .iter()
                .any(|e| e.effect == StatusEffectType::Blind),
        );
        assert!(
            emit_status_removals(&mut c, now + Duration::from_millis(1)).is_empty(),
            "the cure was already acknowledged on the wire",
        );
    }

    /// Bridge the shipped rank data to the live resistance store and all four wire
    /// effects. This catches a working subtraction helper that the cast path forgot
    /// to populate.
    #[test]
    fn resist_elements_cast_applies_every_element_at_its_shipped_rank() {
        use super::super::state::DamageType;
        let now = Instant::now();
        let mut c = combat2(now);
        let rank = 12;
        let (amount, duration) = resist_elements_params(rank);
        let out = apply_resist_elements(&mut c, 0, rank, now);

        assert_eq!(c.fighters[0].transient_resistances.len(), 4);
        assert_eq!(out.len(), 8, "four op51 effects to two viewers");
        for ty in [
            DamageType::Fire,
            DamageType::Frost,
            DamageType::Shock,
            DamageType::Poison,
        ] {
            assert!(
                (c.fighters[0].total_resistance_against(ty, 0.0, now) - amount).abs() < 0.001,
                "{ty:?} must receive the rank-{rank} shipped resistance",
            );
            assert_eq!(
                c.fighters[0].total_resistance_against(
                    ty,
                    0.0,
                    now + Duration::from_secs_f32(duration) + Duration::from_millis(1),
                ),
                0.0,
                "{ty:?} must expire after the shipped duration",
            );
        }
    }

    /// `_percentHealthDamage` is the whole five-second condition, not an amount to
    /// charge once per second. It is also authored against the character's own health,
    /// not arena's x3 pacing bar. A delayed engine tick still owes all five scheduled
    /// ticks rather than silently dropping the last one at expiry.
    #[test]
    fn burning_splits_two_percent_of_base_health_across_five_ticks() {
        use super::super::state::DamageType;
        let now = Instant::now();
        let mut c = combat2(now);
        let threshold = c.fighters[1].condition_threshold(StatusEffectType::Burning);
        apply_status_conditioning(
            &mut c,
            1,
            &[(DamageType::Fire, threshold + 1.0)],
            now,
        );
        let effect = c.fighters[1]
            .effects
            .iter()
            .find(|e| e.effect == StatusEffectType::Burning)
            .expect("Burning must land");
        let expected = 0.02 * c.fighters[1].base_max_health() as f32 / 5.0;
        assert!(
            (effect.per_tick_damage - expected).abs() < 0.001,
            "one tick is one fifth of the authored total",
        );

        let hp_before = c.fighters[1].health;
        let out = apply_dot_ticks(&mut c, now + Duration::from_secs(5));
        let damage_frames = out.iter().filter(|(_, frame)| {
            let nd = arena_proto::parse_netdata(&frame[2..]);
            nd.int(3) == Some(50) && nd.int(6) == Some(4)
        }).count();
        assert_eq!(damage_frames, 10, "five ticks, broadcast to two viewers");
        assert_eq!(
            hp_before - c.fighters[1].health,
            expected.round() as u32 * 5,
            "the complete condition deals 2% of base health, subject to wire rounding",
        );
        assert!(
            !c.fighters[1].effects.iter().any(|e| e.effect == StatusEffectType::Burning),
            "the fifth tick and expiry happen in the same boundary pass",
        );
    }

    /// 03-D19: bit 3 of op50 is the defender's `IsOptimalBlocking`, read for every
    /// source (`CombatManager$$ApplyDamage@0x1bd2770`, 0x1bd2a24). A Burning tick on
    /// a defender whose guard is optimal carries flags 0x9; the DoT itself is never
    /// blocked, so the damage is unchanged. Control: the guard down, flags 0x1.
    #[test]
    fn a_dot_tick_on_an_optimal_guard_carries_bit_3() {
        use super::super::state::DamageType;
        let tick_flags = |guard: bool| {
            let now = Instant::now();
            let mut c = combat2(now);
            let threshold = c.fighters[1].condition_threshold(StatusEffectType::Burning);
            apply_status_conditioning(&mut c, 1, &[(DamageType::Fire, threshold + 1.0)], now);
            let at = now + Duration::from_secs(1);
            if guard {
                let f = &mut c.fighters[1];
                f.set_actor_state(ActorStateType::Blocking, at);
                f.blocking_side = ActiveSide::Middle;
                f.block_raised_at = Some(at);
                f.blocking_until = Some(at + Duration::from_secs(8));
            }
            let hp = c.fighters[1].health;
            let out = apply_dot_ticks(&mut c, at);
            let flags: Vec<i64> = out
                .iter()
                .map(|(_, frame)| arena_proto::parse_netdata(&frame[2..]))
                .filter(|nd| nd.int(3) == Some(50) && nd.int(6) == Some(4))
                .filter_map(|nd| nd.int(7))
                .collect();
            (flags, hp - c.fighters[1].health)
        };
        let (guarded, lost_guarded) = tick_flags(true);
        let (open, lost_open) = tick_flags(false);
        assert!(!guarded.is_empty());
        assert!(guarded.iter().all(|f| *f == 0x9), "{guarded:?}");
        assert!(open.iter().all(|f| *f == 0x1), "{open:?}");
        assert_eq!(lost_guarded, lost_open, "a DoT tick is never blocked");
    }

    /// The elemental condition itself is the client's VFX instruction. Every one
    /// must be an op51 on the victim's Avatar, marked with retail's elemental source
    /// kind (0), and broadcast to both perspectives.
    #[test]
    fn all_elemental_condition_visuals_target_the_victim_and_reach_both_viewers() {
        use super::super::state::DamageType;
        let now = Instant::now();
        for (damage, status) in [
            (DamageType::Fire, StatusEffectType::Burning),
            (DamageType::Frost, StatusEffectType::Frozen),
            (DamageType::Shock, StatusEffectType::Enervated),
            (DamageType::Poison, StatusEffectType::Poisoned),
        ] {
            let mut c = combat2(now);
            let victim_obj = c.fighters[1].net_object_id as i64;
            let threshold = c.fighters[1].condition_threshold(status);
            let out = apply_status_conditioning(
                &mut c,
                1,
                &[(damage, threshold + 1.0)],
                now,
            );
            let frames: Vec<_> = out.iter().filter(|(_, frame)| {
                let nd = arena_proto::parse_netdata(&frame[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(status as u16 as i64)
            }).collect();
            assert_eq!(frames.len(), 2, "{status:?}: one op51 per viewer");
            let mut destinations: Vec<_> = frames.iter().map(|(dest, _)| *dest).collect();
            destinations.sort_unstable();
            assert_eq!(destinations, vec![0, 1], "{status:?}: both perspectives");
            for (_, frame) in frames {
                let nd = arena_proto::parse_netdata(&frame[2..]);
                assert_eq!(nd.int(0), Some(victim_obj), "{status:?}: victim Avatar");
                assert_eq!(nd.int(7), Some(0), "{status:?}: elemental VFX source kind");
            }
        }
    }

    /// The three *Armor spells now DO emit their status — `ElementalStormArmor` = 16,
    /// one shared value for all three (the element is on the ability, not the status).
    #[test]
    fn an_armor_spell_emits_the_storm_armor_status() {
        let now = Instant::now();
        for name in ["FirestormArmor", "BlizzardArmor", "TempestArmor"] {
            let mut c = combat2(now);
            let out = apply_shipped_effects(&mut c, 0, 1, uuid_of(name), 1, 0.0, false, false, now);
            let n = out.iter().filter(|(_, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(16)
            }).count();
            assert_eq!(n, 2, "{name}: op51 ElementalStormArmor to both viewers");
        }
    }

    /// A plain damage spell must not pick up any of this — the pass is additive.
    #[test]
    fn a_plain_damage_spell_gains_nothing() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("Fireball"), 1, 500.0, false, false, now);
        assert!(c.fighters[0].negation_pools.is_empty());
        assert!(!c.fighters[1].is_paralyzed());
        assert!(out.is_empty());
    }

    // -----------------------------------------------------------------------
    // tracker #24: maneuvers could not stagger
    // -----------------------------------------------------------------------

    /// How many op51 `ChangeCombatStatusEffect` frames in `out` carry `status`.
    /// gmid is propId 3, the `StatusEffectType` is propId 5.
    fn status_frames(out: &[(usize, Vec<u8>)], status: super::super::state::StatusEffectType) -> usize {
        out.iter()
            .filter(|(_, f)| {
                if f.len() <= 2 || f[1] != 0x36 {
                    return false;
                }
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(status as u16 as i64)
            })
            .count()
    }

    /// The routing fact the old placement got wrong, asserted from the shipped data
    /// so it cannot drift: of the 706 ability ranks, exactly three abilities carry
    /// `_damageToCauseStagger`, and TWO of them are maneuvers — which is why a gate
    /// living in the `Paralyze | Damage | Generic` arm was unreachable for them.
    #[test]
    fn the_stagger_field_is_carried_mostly_by_maneuvers() {
        use super::super::gamedata::{ABILITIES, AbilityKind, ability_rank_clamped};
        let carriers: Vec<(&str, AbilityKind)> = ABILITIES
            .iter()
            .filter(|a| {
                (1..=a.maximum_level).any(|lvl| {
                    ability_rank_clamped(a.uuid, lvl)
                        .and_then(|r| r.damage_to_cause_stagger())
                        .is_some()
                })
            })
            .map(|a| (a.editor_name, a.kind))
            .collect();
        assert_eq!(
            carriers.len(),
            3,
            "expected StaggeringBash + Guardbreaker + IceSpike, got {carriers:?}",
        );
        let maneuvers: Vec<&str> = carriers
            .iter()
            .filter(|(_, k)| *k == AbilityKind::Maneuver)
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(maneuvers.len(), 2, "two of the three are maneuvers: {maneuvers:?}");
        for name in &maneuvers {
            assert_eq!(
                super::super::loadout::ability_tag_for_template(uuid_of(name)),
                super::super::state::AbilityTag::Maneuver,
                "{name} routes to the Maneuver arm, which the old gate sat after",
            );
        }
    }

    /// A maneuver that ships `_damageToCauseStagger` staggers its target, for the
    /// rank's own `_stunDuration`, and tells both viewers.
    ///
    /// This is what the reporter never saw: gmid 51 fired 4x and 21x across his two
    /// sessions and `Staggered` was never sent once in either direction.
    ///
    /// tracker #31: each maneuver is now driven at the block state its own shipped
    /// description names — Guardbreaker "stuns a target that blocks it", Staggering
    /// Bash "stuns a target that does not block it".
    #[test]
    fn a_maneuver_that_ships_a_stagger_threshold_staggers_the_target() {
        use super::super::state::StatusEffectType;
        for (name, target_blocked) in [("StaggeringBash", false), ("Guardbreaker", true)] {
            let now = Instant::now();
            let mut c = combat2(now);
            let u = uuid_of(name);
            let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
                .and_then(|r| r.damage_to_cause_stagger())
                .unwrap_or_else(|| panic!("{name} R1 ships _damageToCauseStagger"));
            let stun = super::super::gamedata::ability_rank_clamped(u, 1)
                .and_then(|r| r.stun_duration())
                .unwrap_or_else(|| panic!("{name} R1 ships _stunDuration"));

            let out = apply_shipped_effects(&mut c, 0, 1, u, 1, threshold + 1.0, target_blocked, false, now);
            assert!(c.fighters[1].is_staggered(now), "{name}: the TARGET is staggered");
            assert!(!c.fighters[0].is_staggered(now), "{name}: the caster is not");
            assert_eq!(
                c.fighters[1].actor_state(),
                ActorStateType::Staggered,
                "{name}: the actor state follows",
            );
            // The rank's OWN duration, not the generic baseStaggerDuration.
            assert!(
                c.fighters[1].is_staggered(now + Duration::from_secs_f32(stun * 0.9)),
                "{name}: still staggered at 90% of its own {stun}s",
            );
            assert!(
                !c.fighters[1].is_staggered(now + Duration::from_secs_f32(stun * 1.1)),
                "{name}: over by 110% of its own {stun}s",
            );
            assert_eq!(
                status_frames(&out, StatusEffectType::Staggered),
                c.fighters.len(),
                "{name}: op51 Staggered to both viewers",
            );
        }
    }

    /// IceSpike is the one SPELL that ships the field. It reached the old gate and
    /// must still work — the move must not trade one arm for the other. Its
    /// threshold is real (70.19 @ R1), so a weak hit still must not stagger.
    #[test]
    fn icespike_still_staggers_and_still_respects_its_threshold() {
        use super::super::state::StatusEffectType;
        let u = uuid_of("IceSpike");
        let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.damage_to_cause_stagger())
            .expect("IceSpike R1 ships _damageToCauseStagger");
        assert!(threshold > 1.0, "IceSpike's threshold is a real damage figure, got {threshold}");

        let now = Instant::now();
        let mut hard = combat2(now);
        let out = apply_shipped_effects(&mut hard, 0, 1, u, 1, threshold + 0.1, false, false, now);
        assert!(hard.fighters[1].is_staggered(now), "a hit over the threshold staggers");
        assert_eq!(status_frames(&out, StatusEffectType::Staggered), hard.fighters.len());

        // EXACTLY at the threshold does not: the client's test is strictly greater
        // (`AbilityApplyIceSpikeDamage$$OnDamage@0x1e8ea14`, "more than {1} damage").
        // This used to assert the opposite.
        let mut at = combat2(now);
        let out = apply_shipped_effects(&mut at, 0, 1, u, 1, threshold, false, false, now);
        assert!(!at.fighters[1].is_staggered(now), "a hit AT the threshold does not stagger");
        assert_eq!(status_frames(&out, StatusEffectType::Staggered), 0);

        let mut soft = combat2(now);
        let out = apply_shipped_effects(&mut soft, 0, 1, u, 1, threshold - 0.1, false, false, now);
        assert!(!soft.fighters[1].is_staggered(now), "a hit under the threshold does not");
        assert_eq!(status_frames(&out, StatusEffectType::Staggered), 0);
    }

    /// A self-buff arm sets `last_hit_total = 0.0`. StaggeringBash's threshold is
    /// 1.0 at every rank, so without the extra `> 0.0` gate a cast that never
    /// touched the target could still stagger it once the block moved out of the
    /// damage-only arm.
    #[test]
    fn a_cast_that_dealt_no_damage_cannot_stagger() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("StaggeringBash"), 1, 0.0, false, false, now);
        assert!(!c.fighters[1].is_staggered(now), "no damage → no stagger");
        assert_eq!(status_frames(&out, super::super::state::StatusEffectType::Staggered), 0);
    }

    /// RECOVERY STRIKES MUST WORK THROUGH A STUN. That is the entire ability.
    ///
    /// `Ability.Maneuver.RecoveryStrikes.Description`: *"These Quick Strikes can be
    /// performed at any time, except when Paralyzed."* It is the ONLY ability in the
    /// shipped description corpus that says so — surveying every `*.Description`
    /// string for "at any time" / "staggered" returns Recovery Strikes and nothing
    /// else.
    ///
    /// The staggered-input gate dropped every frame from a staggered fighter, so a
    /// level-25, 5-ability-point maneuver bought purely to act through a stun did
    /// nothing through a stun. Reported by Taheen (#113) as being unable to act out
    /// of a stun.
    #[test]
    fn recovery_strikes_can_be_performed_while_staggered() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[0].apply_stagger_for(now, 2.5);
        assert!(c.fighters[0].is_staggered(now), "the fixture must actually stun slot 0");

        let obj = c.fighters[0].net_object_id;
        let frame = messages::request_execute_ability(obj, RECOVERY_STRIKES_UUID);
        let out = on_c2s_input(&mut c, 0, &frame, now + Duration::from_millis(100));

        assert!(
            !out.is_empty(),
            "a staggered fighter must still be able to perform Recovery Strikes"
        );
    }

    /// THE DODGES ACT THROUGH A STUN TOO — measured, since the data does not say.
    ///
    /// Counting every ability execution (gmid 37/38) inside a `Staggered` op51
    /// apply→remove window across 11 retail sessions:
    ///
    /// ```text
    ///   RecoveryStrikes  64 stunned / 221 free  22.5%   (documented stun-proof)
    ///   DodgingStrike    50 / 218               18.7%
    ///   AdrenalineDodge  44 / 314               12.3%
    ///   QuickStrikes      0 / 294                0.0%
    ///   LightningBolt     0 / 219                0.0%
    ///   Ward              0 / 70                 0.0%
    /// ```
    ///
    /// The hard zeros at comparable sample sizes are what make this a real
    /// distinction rather than a measurement floor. A lagging stagger-remove would
    /// be the obvious confound; measuring position WITHIN the window rules it out
    /// (30 % of DodgingStrike's stunned uses fall in the first half, vs Recovery
    /// Strikes' 14 %), and StaggeringBash — which CAUSES the stagger — lands at
    /// median position 0.10, confirming the window logic.
    ///
    /// Report #113 (Taheen), who said dodges free you from a stun. They do.
    #[test]
    fn the_dodge_maneuvers_can_also_be_performed_while_staggered() {
        let now = Instant::now();
        let at = now + Duration::from_millis(100);
        for name in ["DodgingStrike", "AdrenalineDodge", "RenewingDodge", "FocusingDodge"] {
            let mut c = combat2(now);
            c.fighters[0].apply_stagger_for(now, 2.5);
            assert!(c.fighters[0].is_staggered(now), "{name}: the fixture must stun slot 0");
            let obj = c.fighters[0].net_object_id;
            let frame = messages::request_execute_ability(obj, &uuid_of(name));
            assert!(
                !on_c2s_input(&mut c, 0, &frame, at).is_empty(),
                "{name} must be performable while staggered"
            );
        }
    }

    /// THE CONTROL, and it is the one that matters: the exception must stay NARROW.
    /// A gate that simply stopped blocking staggered input would pass both tests
    /// above while deleting the stun from the game.
    #[test]
    fn a_stagger_still_blocks_every_other_input() {
        let now = Instant::now();
        let at = now + Duration::from_millis(100);

        // Abilities measured at 0 uses inside a stun window across 11 retail
        // sessions are still refused: QuickStrikes 0/294, LightningBolt 0/219,
        // Ward 0/70. QuickStrikes in particular is a MANEUVER, so this also pins
        // that the rule is not "maneuvers pass, spells do not".
        for name in ["QuickStrikes", "LightningBolt", "Ward"] {
            let mut c = combat2(now);
            c.fighters[0].apply_stagger_for(now, 2.5);
            let obj = c.fighters[0].net_object_id;
            let frame = messages::request_execute_ability(obj, &uuid_of(name));
            assert!(
                on_c2s_input(&mut c, 0, &frame, at).is_empty(),
                "{name} must still be blocked by a stagger"
            );

            // …and the un-staggered control proves the fixture is not simply inert:
            // the same frame DOES act with no stagger applied.
            let mut free = combat2(now);
            assert!(
                !on_c2s_input(&mut free, 0, &frame, at).is_empty(),
                "{name} must act when NOT staggered — otherwise this proves nothing"
            );
        }

        // …and an ordinary weapon swing is still blocked.
        let mut c2 = combat2(now);
        c2.fighters[0].apply_stagger_for(now, 2.5);
        assert!(
            on_c2s_input(&mut c2, 0, &[0xBE, 0x36], at).is_empty(),
            "a stagger must still block a plain swing"
        );
    }

    /// The uuid the gate pins must still be Recovery Strikes in the shipped table, so
    /// a regenerated gamedata.rs cannot silently unhook the exception.
    #[test]
    fn the_recovery_strikes_uuid_still_names_recovery_strikes() {
        let a = super::super::gamedata::ABILITIES
            .iter()
            .find(|a| a.uuid == RECOVERY_STRIKES_UUID)
            .expect("the pinned uuid must exist in the shipped ability table");
        assert_eq!(a.editor_name, "RecoveryStrikes");
    }

    /// And a dead target is not staggered — the pre-existing guard, kept.
    #[test]
    fn a_dead_target_is_not_staggered() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].health = 0;
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("StaggeringBash"), 1, 500.0, false, false, now);
        assert!(!c.fighters[1].is_staggered(now));
        assert_eq!(status_frames(&out, super::super::state::StatusEffectType::Staggered), 0);
    }
}

#[cfg(test)]
mod piercing_tests {
    use super::*;
    use super::super::damage::{DamageModel, RetailDamageModel};
    use super::super::state::{ActiveSide, DamageSource, Fighter};
    use super::super::loadout;

    fn armored_target(now: Instant) -> Fighter {
        let mut f = Fighter::new(1, 2, loadout::starter(), now);
        f.loadout.armor_rating = 300.0;
        f
    }

    /// Skullcrusher's 225.00 armor pierce must actually cut through armor. The rating
    /// was already consumed by the damage pipeline; nothing set it from the ability,
    /// so the field did nothing.
    #[test]
    fn armor_piercing_increases_damage_through_armor() {
        let now = Instant::now();
        let m = RetailDamageModel;
        let mut lo = loadout::starter();
        lo.weapon.base_by_type = vec![(super::super::state::DamageType::Slashing, 200.0)];

        let plain = m.resolve_attack(&lo, &armored_target(now), DamageSource::Attack,
                                     ActiveSide::Middle, 1.0, 0, now).total;
        let mut pierce = lo.clone();
        pierce.armor_piercing_rating += 225.0;
        let pierced = m.resolve_attack(&pierce, &armored_target(now), DamageSource::Attack,
                                       ActiveSide::Middle, 1.0, 0, now).total;
        assert!(
            pierced > plain,
            "225 armor pierce must beat 300 armor: {pierced:.1} should exceed {plain:.1}"
        );
    }

    /// ADDITIVE, which is the whole safety argument for touching this pipeline: zero
    /// piercing must reproduce today's numbers exactly. The s506 differentials are the
    /// real proof; this pins it directly.
    #[test]
    fn zero_piercing_changes_nothing() {
        let now = Instant::now();
        let m = RetailDamageModel;
        let lo = loadout::starter();
        let base = m.resolve_attack(&lo, &armored_target(now), DamageSource::Attack,
                                    ActiveSide::Middle, 1.0, 0, now).total;
        let mut zero = lo.clone();
        zero.armor_piercing_rating += 0.0;
        zero.elem_resist_piercing_rating += 0.0;
        let same = m.resolve_attack(&zero, &armored_target(now), DamageSource::Attack,
                                    ActiveSide::Middle, 1.0, 0, now).total;
        assert_eq!(base.to_bits(), same.to_bits(), "zero piercing must be bit-identical");
    }

    /// A LOW block must be weaker against a block-piercing attack. Skullcrusher ships
    /// 60.00 physical block pierce, PiercingStrikes 122.40 elemental — both dead until
    /// now, because `block_outcome` had no piercing input at all.
    #[test]
    fn block_piercing_weakens_a_late_block() {
        use super::super::damage::block_outcome;
        use super::super::state::{ActorStateType, BlockPhase, DamageType};
        let now = Instant::now();
        let mut d = Fighter::new(1, 2, loadout::starter(), now);
        d.loadout.block_rating = 400.0;
        d.set_actor_state(ActorStateType::Blocking, now);
        d.blocking_side = ActiveSide::Right;
        // LATE, not optimal: re-raised inside the recovery window.
        d.last_block_dropped_at = Some(now);
        d.block_raised_at = Some(now);
        d.blocking_until = Some(now + Duration::from_secs(5));
        assert_eq!(d.block_phase(now), Some(BlockPhase::Late), "precondition: LATE block");

        let plain = block_outcome(&d, &loadout::starter(), ActiveSide::Right, now);
        let mut pierce = loadout::starter();
        pierce.block_piercing_rating = 60.0;
        pierce.elem_block_piercing_rating = 122.40;
        let pierced = block_outcome(&d, &pierce, ActiveSide::Right, now);

        let hit = |b: &super::super::damage::BlockOutcome| {
            let mut c = vec![(DamageType::Slashing, 1000.0_f32), (DamageType::Fire, 1000.0_f32)];
            b.apply(&mut c, false);
            c
        };
        let (p, q) = (hit(&plain), hit(&pierced));
        // Low block, R 400: physical cut 400 · 0.16 = 64, elemental 400 · 0.082 = 32.8.
        // Pierced: (400 − 60) · 0.16 = 54.4 and (400 − 122.4) · 0.082 = 22.76.
        assert!((p[0].1 - 936.0).abs() < 0.01 && (q[0].1 - 945.6).abs() < 0.01, "{p:?} {q:?}");
        assert!((p[1].1 - 967.2).abs() < 0.01 && (q[1].1 - 977.237).abs() < 0.01, "{p:?} {q:?}");
    }

    /// ADDITIVE — the whole safety argument for touching the block stage. A hit with no
    /// piercing must produce a bit-identical factor to before the parameter existed.
    /// The s506 block differentials are the real proof; this pins it directly.
    #[test]
    fn zero_block_piercing_is_bit_identical() {
        use super::super::damage::block_outcome;
        use super::super::state::{ActorStateType, DamageType};
        let now = Instant::now();
        let mut d = Fighter::new(1, 2, loadout::starter(), now);
        d.loadout.block_rating = 400.0;
        d.set_actor_state(ActorStateType::Blocking, now);
        d.blocking_side = ActiveSide::Right;
        d.block_raised_at = Some(now);
        d.blocking_until = Some(now + Duration::from_secs(5));

        let b = block_outcome(&d, &loadout::starter(), ActiveSide::Right, now);
        let comps = vec![
            (DamageType::Slashing, 200.0_f32),
            (DamageType::Fire, 90.0),
            (DamageType::Frost, 45.0),
            (DamageType::Stamina, 30.0),
        ];
        let mut with = comps.clone();
        b.apply(&mut with, false);
        // Re-deriving with the piercing fields explicitly zeroed must give the same bits.
        let mut zero = b;
        zero.block_piercing = 0.0;
        zero.elem_block_piercing = 0.0;
        let mut without = comps.clone();
        zero.apply(&mut without, false);
        for ((ty, a), (_, z)) in with.iter().zip(without.iter()) {
            assert_eq!(a.to_bits(), z.to_bits(), "{ty:?} must be identical");
        }
    }

    /// The piercing lives on a per-cast CLONE, so a maneuver cannot leak it into the
    /// fighter's later auto-attacks.
    #[test]
    fn piercing_does_not_persist_on_the_fighter() {
        let now = Instant::now();
        let f = Fighter::new(0, 1, loadout::starter(), now);
        let before = f.loadout.armor_piercing_rating;
        let mut cast = f.loadout.clone();
        cast.armor_piercing_rating += 225.0;
        assert_eq!(f.loadout.armor_piercing_rating, before, "the fighter is untouched");
        assert!(cast.armor_piercing_rating > before, "only the cast's clone pierces");
    }
}

// ---------------------------------------------------------------------------
// tracker #31 — the high-block stun, the bash's own guard, and the two
// maneuvers whose block conditions are opposites.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod report_31_high_block_stun {
    use super::*;
    use super::super::damage::flags;
    use super::super::loadout::starter;
    use super::super::state::{
        AbilityTag, ActorStateType, BlockPhase, EquippedAbility, BASE_STAGGER_DURATION_SECS,
        BLOCK_OPTIMAL_TIME_SECS, Fighter, FlowState, MatchCombat, StatusEffectType, WeaponProfile,
    };

    /// Two fighters with a plain 113.82 Slashing blade, live round.
    /// `expected_peers` fighters are humans; the rest are bots.
    fn combat(now: Instant, expected_peers: usize) -> MatchCombat {
        let mut c = MatchCombat::new(2, expected_peers, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            let mut f = Fighter::new(slot, obj, starter(), now);
            f.loadout.weapon = WeaponProfile {
                primary_type: Some(super::super::state::DamageType::Slashing),
                base_by_type: vec![(super::super::state::DamageType::Slashing, 113.82)],
                weight: Some(super::super::tables::Weight::Light),
            };
            f.loadout.weapon_template = None;
            f.loadout.block_rating = 379.5;
            c.fighters.push(f);
        }
        c.match_net_object_id = c.alloc_net_object_id();
        c.phase = FlowState::StateTimeout;
        c.phase_entered = now;
        c
    }

    /// Raise `slot`'s guard at `at`, the way both production block-raise paths do.
    fn raise_guard(c: &mut MatchCombat, slot: usize, at: Instant, window: Duration) {
        let f = &mut c.fighters[slot];
        f.set_actor_state(ActorStateType::Blocking, at);
        f.blocking_side = ActiveSide::Middle;
        f.blocking_until = Some(at + window);
        f.block_raised_at = Some(at);
    }

    /// How many op51 `ChangeCombatStatusEffect` frames in `out` carry `status`.
    fn status_frames(out: &[(usize, Vec<u8>)], status: StatusEffectType) -> usize {
        out.iter()
            .filter(|(_, f)| {
                if f.len() <= 2 || f[1] != 0x36 {
                    return false;
                }
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(status as u16 as i64)
            })
            .count()
    }

    fn uuid_of(editor: &str) -> &'static str {
        super::super::gamedata::ABILITIES
            .iter()
            .find(|a| a.editor_name == editor)
            .map(|a| a.uuid)
            .unwrap_or_else(|| panic!("{editor} missing from the shipped table"))
    }

    /// Commit slot `sender`'s swing and land it on the FollowThrough beat.
    fn swing_and_land(
        c: &mut MatchCombat,
        sender: usize,
        target: usize,
        now: Instant,
    ) -> (Vec<(usize, Vec<u8>)>, Instant) {
        let mut out = super::resolve_swing(c, sender, target, 1.0, now);
        let impact = now + super::FOLLOW_THROUGH_DELAY + Duration::from_millis(1);
        out.extend(super::land_due_hits(c, impact));
        (out, impact)
    }

    // -- (B) the stun itself -------------------------------------------------

    /// `UI.Help.Blocking.Description`: *"When a weapon attack is blocked high, the
    /// attacker gets stunned."* This is the whole of tracker #31's first half — the
    /// reporter's "when the AI swings into my high block it does not get stunned".
    #[test]
    fn a_weapon_attack_blocked_high_stuns_the_attacker() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        raise_guard(&mut c, 0, now, Duration::from_secs(2));

        let (out, impact) = swing_and_land(&mut c, 1, 0, now);

        assert_eq!(
            c.fighters[0].block_phase(impact),
            Some(BlockPhase::Optimal),
            "precondition: the guard is still HIGH when the swing lands",
        );
        assert!(
            c.fighters[1].is_staggered(impact),
            "the ATTACKER is stunned by the high block",
        );
        assert_eq!(
            c.fighters[1].actor_state(),
            ActorStateType::Staggered,
            "and its actor state follows, so the client animates it",
        );
        assert!(!c.fighters[0].is_staggered(impact), "the BLOCKER is not stunned");
        assert_eq!(
            status_frames(&out, StatusEffectType::Staggered),
            c.fighters.len(),
            "op51 Staggered goes to both viewers",
        );
    }

    /// The duration is shipped data: `PvpDefaultSettings.BASE_STAGGER_DURATION = 2.5`
    /// (`dump.cs:427016`), which is also the arena help text's "stun opponents for
    /// longer" — `CombatParameters.baseStaggerDuration` (PvE) is 1.5 s.
    #[test]
    fn the_high_block_stun_lasts_the_shipped_pvp_stagger_duration() {
        assert!(
            (BASE_STAGGER_DURATION_SECS - 2.5).abs() < 1e-6,
            "PvpDefaultSettings.BASE_STAGGER_DURATION is 2.5 s, got {BASE_STAGGER_DURATION_SECS}",
        );
        let now = Instant::now();
        let mut c = combat(now, 2);
        raise_guard(&mut c, 0, now, Duration::from_secs(2));
        let (_, impact) = swing_and_land(&mut c, 1, 0, now);

        let d = Duration::from_secs_f32(BASE_STAGGER_DURATION_SECS);
        assert!(
            c.fighters[1].is_staggered(impact + d - Duration::from_millis(50)),
            "still stunned just before {BASE_STAGGER_DURATION_SECS}s",
        );
        assert!(
            !c.fighters[1].is_staggered(impact + d + Duration::from_millis(50)),
            "recovered just after {BASE_STAGGER_DURATION_SECS}s",
        );
    }

    /// A LOW block — the same guard, just held past `BLOCK_OPTIMAL_TIME` — must NOT
    /// stun. "Blocking high ALSO protects you more effectively": the stun is the
    /// high-block reward, not a blocking reward.
    #[test]
    fn a_weapon_attack_blocked_low_does_not_stun_the_attacker() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        raise_guard(&mut c, 0, now, Duration::from_secs(8));
        let swing_at = now + Duration::from_secs_f32(BLOCK_OPTIMAL_TIME_SECS + 0.5);

        let (out, impact) = swing_and_land(&mut c, 1, 0, swing_at);

        assert_eq!(
            c.fighters[0].block_phase(impact),
            Some(BlockPhase::Late),
            "precondition: the guard has dropped to LOW",
        );
        assert!(!c.fighters[1].is_staggered(impact), "a low block does not stun");
        assert_eq!(status_frames(&out, StatusEffectType::Staggered), 0);
    }

    /// And an unblocked swing obviously does not stun its owner.
    #[test]
    fn an_unblocked_weapon_attack_does_not_stun_the_attacker() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let (out, impact) = swing_and_land(&mut c, 1, 0, now);
        assert!(!c.fighters[1].is_staggered(impact));
        assert_eq!(status_frames(&out, StatusEffectType::Staggered), 0);
    }

    /// THE REPORTED BUG: "RE shows a 0 damage effect on the opponent."
    ///
    /// An ability shipping neither `_damage` nor `_damagePerSecond` used to be routed
    /// through the damage model anyway, resolve to exactly 0.0, and be handed to
    /// `emit_damage` — which has no zero guard and addresses op50 to the TARGET. So a
    /// buff cast on yourself painted a floating `0` on your opponent.
    ///
    /// `MagickaSurge` and `EchoWeapon` are the shipped examples: `kind: Spell`,
    /// `damage_type: None`, so they classify as `Generic` and land in the damage arm.
    /// Red before the `ships_damage` guard — each emitted one op50 per viewer.
    #[test]
    fn a_buff_that_ships_no_damage_number_emits_no_damage_frame() {
        for editor in ["MagickaSurge", "EchoWeapon"] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            // Above every shipped cost — MagickaSurge alone is 425, and the harness
            // fighter's pool is 345, so `max_magicka` silently fails the resource gate
            // and the cast never reaches the damage arm at all.
            c.fighters[0].magicka = 100_000;
            c.fighters[0].stamina = 100_000;
            let out = super::on_c2s_input(&mut c, 0, &cast_frame(uuid_of(editor)), now);
            assert!(
                !out.is_empty(),
                "precondition: {editor} must actually CAST — an empty frame list means \
                 the resource gate rejected it and this test proves nothing"
            );
            assert_eq!(
                op50_count(&out),
                0,
                "{editor} ships no damage number — it must not paint a 0 on the opponent"
            );
        }
    }

    /// Non-vacuity control for the guard above: an ability that DOES ship a damage
    /// number must still hit. Without this, deleting the damage arm entirely would pass.
    #[test]
    fn an_ability_that_ships_damage_still_emits_its_damage_frame() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        c.fighters[0].magicka = c.fighters[0].max_magicka;
        let cast = super::on_c2s_input(&mut c, 0, &cast_frame(uuid_of("IceSpike")), now);
        // Ice Spike ships `ChannelDuration 1.12`, so the impact is SCHEDULED, not
        // inline — the cast frame carries no op50 and the hit arrives on a later tick.
        assert_eq!(op50_count(&cast), 0, "the wind-up defers the hit");
        let landed = super::land_due_impacts(&mut c, now + Duration::from_millis(1200));
        assert!(
            op50_count(&landed) > 0,
            "Ice Spike ships `_damage` — it must still land a hit once its wind-up ends"
        );
    }

    /// The predicate itself, against the shipped table: damage spells and channels are
    /// in, buffs are out, and an ability gamedata does not know at all is out — we have
    /// no number for it, and a fabricated 0 is worse than silence.
    #[test]
    fn ships_damage_reads_the_shipped_table() {
        use super::super::damage::ships_damage;
        assert!(ships_damage(uuid_of("IceSpike"), 1), "direct damage");
        assert!(ships_damage(uuid_of("Frostbite"), 1), "damage per second");
        assert!(!ships_damage(uuid_of("MagickaSurge"), 1), "a buff");
        assert!(!ships_damage(uuid_of("EchoWeapon"), 1), "a buff");
        assert!(
            !ships_damage("00000000-0000-0000-0000-000000000000", 1),
            "an ability gamedata does not know"
        );
    }

    /// A cast whose uuid is not in the equipped loadout used to be blindly relabelled
    /// `Generic` — i.e. treated as a damage SPELL. A maneuver's damage comes from the
    /// WEAPON and it ships no ability damage field, so under the `ships_damage` guard
    /// that relabelling would have silently deleted every mis-looked-up bash. Classify
    /// from gamedata instead, so a maneuver stays a maneuver.
    ///
    /// This is not hypothetical: `Loadout::default()` carries no abilities, so every
    /// cast in this test module takes the fallback path.
    #[test]
    fn an_unknown_cast_is_classified_from_gamedata_not_assumed_to_be_a_spell() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        c.fighters[0].stamina = c.fighters[0].max_stamina;
        assert!(
            c.fighters[0].loadout.abilities.is_empty(),
            "precondition: the lookup must miss, or this proves nothing"
        );
        let out = super::on_c2s_input(&mut c, 0, &cast_frame(uuid_of("ShieldBash")), now);
        assert!(
            op50_count(&out) == 0,
            "the first, guarding half of a shield bash must not deal damage"
        );
        let landed = super::land_due_impacts(&mut c, now + Duration::from_millis(501));
        assert!(
            op50_count(&landed) > 0,
            "the shield bash's second half still lands its weapon hit — it must not be \
             mistaken for a damage spell and dropped for shipping no damage field"
        );
    }

    /// op37 cast frame for `uuid` — the NetData layout from `input.rs`.
    fn cast_frame(uuid: &str) -> Vec<u8> {
        let mut f = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, 0x02, 0x00, 0x00, 0x38, 0x03, 0x25,
            0x24, 0x00,
        ];
        f.extend_from_slice(uuid.as_bytes());
        f
    }

    fn op50_count(out: &[(usize, Vec<u8>)]) -> usize {
        out.iter()
            .filter(|(_, f)| {
                f.len() > 2
                    && f[1] == 0x36
                    && arena_proto::parse_netdata(&f[2..]).int(3) == Some(50)
            })
            .count()
    }

    fn a_maneuver_blocked_high_does_not_stun_its_caster() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        // Slot 1 holds a fresh HIGH guard; slot 0 bashes into it.
        raise_guard(&mut c, 1, now, Duration::from_secs(2));
        c.fighters[0].stamina = c.fighters[0].max_stamina;

        let u = uuid_of("ShieldBash");
        let mut frame = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, 0x02, 0x00, 0x00, 0x38, 0x03, 0x25,
            0x24, 0x00,
        ];
        frame.extend_from_slice(u.as_bytes());
        let out = super::on_c2s_input(&mut c, 0, &frame, now);

        let blocked = out.iter().any(|(_, f)| {
            if f.len() <= 2 || f[1] != 0x36 {
                return false;
            }
            let nd = arena_proto::parse_netdata(&f[2..]);
            nd.int(3) == Some(50)
                && nd
                    .int(7)
                    .map(|v| v as u8 & flags::WAS_OPTIMAL_BLOCKING != 0)
                    .unwrap_or(false)
        });
        assert!(blocked, "precondition: the bash was blocked HIGH (op50 carries the flag)");
        assert!(
            !c.fighters[0].is_staggered(now),
            "an ability attack blocked high must NOT stun its caster",
        );
    }

    /// The bot loop had no stagger gate, so a stunned bot kept swinging and the whole
    /// mechanic was invisible for exactly the case in the report.
    #[test]
    fn a_stunned_bot_stops_swinging() {
        let now = Instant::now();
        // expected_peers = 1 → slot 1 is the bot.
        let mut c = combat(now, 1);
        c.fighters[1].apply_stagger_for(now, BASE_STAGGER_DURATION_SECS);

        super::on_tick(&mut c, now + Duration::from_millis(10), false);
        assert!(
            c.fighters[1].bot_swing_at.is_none(),
            "a stunned bot must not queue a wind-up",
        );
        assert_ne!(
            c.fighters[1].actor_state(),
            ActorStateType::Charging,
            "…nor enter Charging",
        );

        // Once the stun lapses the bot resumes, and the tick clears it back to Idle.
        let after = now + Duration::from_secs_f32(BASE_STAGGER_DURATION_SECS + 0.1);
        super::on_tick(&mut c, after, false);
        assert!(!c.fighters[1].is_staggered(after));
        assert!(c.fighters[1].bot_swing_at.is_some(), "the bot swings again after the stun");
    }

    #[test]
    fn a_paralyzed_bot_cannot_queue_actions_and_recovers_on_tick() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        c.fighters[1].record_element_damage(
            super::super::state::DamageType::Poison,
            500.0,
            now,
        );
        let _ = super::try_paralyze(&mut c, 0, 1, 12, 1_000.0, false, now);
        assert!(c.fighters[1].is_paralyzed());

        super::on_tick(&mut c, now + Duration::from_millis(10), false);
        assert!(c.fighters[1].bot_swing_at.is_none());
        assert_eq!(c.fighters[1].actor_state(), ActorStateType::Paralyzed);

        let after = now + Duration::from_millis(3150);
        super::on_tick(&mut c, after, false);
        assert!(!c.fighters[1].is_paralyzed(), "tick path must release a bot at 3.1 s");
        assert!(c.fighters[1].bot_swing_at.is_some(), "the bot may act after expiry");
    }

    /// Retail has two distinct stages: Charging is still cancellable input, while a
    /// released swing has been committed to the server. Paralysis must not erase the
    /// latter from `pending_hits`; otherwise a visibly released attack disappears.
    #[test]
    fn a_committed_swing_lands_after_its_attacker_is_paralyzed() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        let target_hp = c.fighters[0].health;

        super::resolve_swing_with_side(
            &mut c,
            1,
            0,
            1.0,
            Some(ActiveSide::Right),
            now,
        );
        assert_eq!(c.pending_hits.len(), 1, "precondition: swing is committed");

        c.fighters[1].record_element_damage(
            super::super::state::DamageType::Poison,
            500.0,
            now + Duration::from_millis(5),
        );
        let _ = super::try_paralyze(&mut c, 0, 1, 12, 1_000.0, false, now + Duration::from_millis(5));
        assert!(c.fighters[1].is_paralyzed());
        assert_eq!(c.pending_hits.len(), 1, "paralysis must retain a committed hit");

        super::on_tick(
            &mut c,
            now + super::FOLLOW_THROUGH_DELAY + Duration::from_millis(1),
            false,
        );
        assert!(c.fighters[0].health < target_hp, "the committed hit must still land");
        assert!(c.fighters[1].is_paralyzed(), "landing must not end the 3.1 s lock");
    }

    // -----------------------------------------------------------------------
    // Revenge — elemental retaliation from gear
    // -----------------------------------------------------------------------

    /// Being hit makes the DEFENDER's gear hit back, and the magnitude is the shipped
    /// enchantment value — validated against the wire, not invented.
    ///
    /// Frost Revenge t10 is `7591 * ENCHANT_DAMAGE_PER_VALUE = 137.32`, and **137.21
    /// is an observed value in s615**, the remainder being the target's resistance.
    #[test]
    fn being_hit_retaliates_with_the_gears_element() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        c.fighters[1].loadout.revenge = vec![(super::super::state::DamageType::Frost, 137.32)];
        let hp_before = c.fighters[0].health;

        let out = super::apply_revenge(
            &mut c,
            1,
            0,
            super::super::state::DamageSource::Attack,
            &[(super::super::state::DamageType::Frost, 10.0)],
            now,
        );

        assert!(c.fighters[0].health < hp_before, "the attacker must take the retaliation");
        let rev = out
            .iter()
            .map(|(_s, b)| b)
            .find(|b| b.len() > 2 && b[1] == 0x36
                  && arena_proto::parse_netdata(&b[2..]).int(3) == Some(50))
            .expect("a Revenge op50 must be emitted");
        let p = arena_proto::parse_netdata(&rev[2..]);
        assert_eq!(p.int(6), Some(6), "DamageSource must be Revenge(6)");
        assert_eq!(p.int(7), Some(3), "flags must be SHOW|ATTACKER — retail never sets OPTIMAL here");
        assert_eq!(p.int(0), Some(c.fighters[0].net_object_id as i64),
                   "the frame must address the ATTACKER, who is the one taking it");
    }

    /// The shipped item text explicitly limits retaliation to damage of the same
    /// element. A Fire hit therefore cannot trigger a Frost Revenge necklace.
    #[test]
    fn revenge_does_not_cross_elements() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        c.fighters[1].loadout.revenge =
            vec![(super::super::state::DamageType::Frost, 50.0)];
        let attacker_hp_before = c.fighters[0].health;
        let fire_hit = ResolvedDamage {
            source: super::super::state::DamageSource::Spell,
            active_side: ActiveSide::Middle,
            flags: flags::SHOW_DAMAGE | flags::HAS_ATTACKER,
            components: vec![(super::super::state::DamageType::Fire, 10.0)],
            total: 10.0,
            most_resisted: super::super::state::DamageType::None,
            negated: false,
            heal: 0.0,
            block_physical: 1.0,
            blocked: false,
        };

        let out = super::emit_damage(&mut c, 0, 1, &fire_hit, now);

        assert!(
            c.fighters[0].health == attacker_hp_before,
            "a Fire spell must not provoke the defender's Frost Revenge",
        );
        assert!(!out.iter().any(|(_, frame)| {
            let nd = arena_proto::parse_netdata(&frame[2..]);
            nd.int(3) == Some(50) && nd.int(6) == Some(6)
        }));
    }

    /// Two fighters both wearing Revenge must not ping-pong retaliation forever.
    #[test]
    fn revenge_does_not_retaliate_against_revenge() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let frost = super::super::state::DamageType::Frost;
        c.fighters[0].loadout.revenge = vec![(frost, 50.0)];
        c.fighters[1].loadout.revenge = vec![(frost, 50.0)];

        // One retaliation resolves and stops; it does not re-enter the hit pipeline.
        let out = super::apply_revenge(
            &mut c,
            1,
            0,
            super::super::state::DamageSource::Attack,
            &[(frost, 10.0)],
            now,
        );
        let n = out
            .iter()
            .filter(|(_s, b)| b.len() > 2 && b[1] == 0x36
                    && arena_proto::parse_netdata(&b[2..]).int(6) == Some(6))
            .count();
        assert_eq!(n, c.fighters.len(), "exactly one Revenge frame per viewer, no cascade");
    }

    /// Gear without a Revenge enchantment retaliates for nothing.
    #[test]
    fn no_revenge_gear_means_no_retaliation() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let hp = c.fighters[0].health;
        let out = super::apply_revenge(
            &mut c,
            1,
            0,
            super::super::state::DamageSource::Attack,
            &[(super::super::state::DamageType::Frost, 10.0)],
            now,
        );
        assert!(out.is_empty());
        assert_eq!(c.fighters[0].health, hp);
    }

    /// Retail s615/s616 has no Revenge after ContinuousSpell or StatusEffect damage.
    /// In particular, Frostbite's 0.2 s stream must not turn one Fire Revenge
    /// enchantment into five retaliations per second.
    #[test]
    fn continuous_spells_and_status_ticks_do_not_provoke_revenge() {
        let now = Instant::now();
        for source in [
            super::super::state::DamageSource::ContinuousSpell,
            super::super::state::DamageSource::StatusEffect,
        ] {
            let mut c = combat(now, 2);
            c.fighters[1].loadout.revenge =
                vec![(super::super::state::DamageType::Fire, 43.68)];
            let hp = c.fighters[0].health;
            let out = super::apply_revenge(
                &mut c,
                1,
                0,
                source,
                &[(super::super::state::DamageType::Fire, 10.0)],
                now,
            );
            assert!(out.is_empty(), "{source:?} must not provoke Revenge");
            assert_eq!(c.fighters[0].health, hp, "{source:?} must deal no retaliation");
        }
    }

    /// Retaliation must fire from a REAL hit, not just when called directly.
    ///
    /// The direct-call tests above cannot catch the wiring being absent — removing the
    /// `apply_revenge` call from `emit_damage` leaves them all green. This drives an
    /// actual swing so the hit path itself is under test.
    #[test]
    fn a_real_swing_provokes_the_defenders_revenge() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        // The starter weapon carries Shock damage, so a Shock Revenge enchant is the
        // same-element control for the full swing pipeline.
        c.fighters[1].loadout.revenge =
            vec![(super::super::state::DamageType::Shock, 137.32)];
        let attacker_hp_before = c.fighters[0].health;

        // Slot 0 swings at slot 1; the hit lands after the follow-through beat.
        let _ = super::resolve_swing(&mut c, 0, 1, 1.0, now);
        let out = super::land_due_hits(
            &mut c,
            now + super::FOLLOW_THROUGH_DELAY + Duration::from_millis(1),
        );

        let revenge_frames = out
            .iter()
            .filter(|(_s, b)| b.len() > 2 && b[1] == 0x36
                    && arena_proto::parse_netdata(&b[2..]).int(3) == Some(50)
                    && arena_proto::parse_netdata(&b[2..]).int(6) == Some(6))
            .count();
        assert!(
            revenge_frames > 0,
            "a landed hit must provoke the defender's Revenge — the wiring in \
             emit_damage is what this asserts",
        );
        assert!(
            c.fighters[0].health < attacker_hp_before,
            "and the attacker must actually lose health to it",
        );
    }

    // -----------------------------------------------------------------------
    // Bot blocking — the other half of the high-block stun
    // -----------------------------------------------------------------------

    /// A bot raises its guard in the gap between swings, and the guard is a genuine
    /// HIGH (optimal) block rather than a low one.
    ///
    /// This is what lets a human be stunned at all. The high-block stun fires on the
    /// ATTACKER when the DEFENDER blocks high — so with bots that never guarded, a
    /// player could inflict that stun but never receive it.
    #[test]
    fn a_bot_raises_a_high_guard_between_swings() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        let start = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(10);

        // Land a swing so the cooldown (and therefore the gap) starts.
        c.fighters[1].last_swing = Some(start);

        // Too soon: inside OPTIMAL_BLOCK_RECOVERY_SECS, so no guard yet.
        super::on_tick(&mut c, start + Duration::from_millis(300), false);
        assert_ne!(
            c.fighters[1].actor_state(),
            ActorStateType::Blocking,
            "raising inside the 0.8s recovery would only ever produce a LATE block",
        );

        // After the raise delay the guard goes up, and it is OPTIMAL.
        let guarded = start + super::BOT_GUARD_RAISE_DELAY + Duration::from_millis(10);
        super::on_tick(&mut c, guarded, false);
        assert_eq!(c.fighters[1].actor_state(), ActorStateType::Blocking, "guard must be up");
        assert_eq!(
            c.fighters[1].block_phase(guarded),
            Some(super::super::state::BlockPhase::Optimal),
            "the bot's guard must be a HIGH block, or it cannot stun the attacker",
        );
    }

    /// The payoff: a human swinging into that guard is STUNNED.
    #[test]
    fn a_human_who_swings_into_the_bot_guard_is_stunned() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        let start = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(10);
        c.fighters[1].last_swing = Some(start);

        let guarded = start + super::BOT_GUARD_RAISE_DELAY + Duration::from_millis(10);
        super::on_tick(&mut c, guarded, false);
        assert_eq!(c.fighters[1].block_phase(guarded), Some(super::super::state::BlockPhase::Optimal));

        // Slot 0 (the human) swings into it and the hit lands.
        let swing_at = guarded + Duration::from_millis(20);
        let _ = super::resolve_swing(&mut c, 0, 1, 1.0, swing_at);
        let land_at = swing_at + super::FOLLOW_THROUGH_DELAY + Duration::from_millis(1);
        let _ = super::land_due_hits(&mut c, land_at);

        assert!(
            c.fighters[0].is_staggered(land_at),
            "the ATTACKER must be stunned by the bot's high block — this is the thing \
             a player could never experience before bots guarded",
        );
    }

    /// The guard comes down to swing, so blocking cannot deadlock the attack cadence.
    #[test]
    fn the_bot_lowers_its_guard_to_swing() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        let start = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(10);
        c.fighters[1].last_swing = Some(start);

        super::on_tick(&mut c, start + super::BOT_GUARD_RAISE_DELAY + Duration::from_millis(10), false);
        assert_eq!(c.fighters[1].actor_state(), ActorStateType::Blocking);

        // Once the swing cooldown expires the bot drops the guard and winds up.
        let swing_ready = start + super::BOT_SWING_COOLDOWN + Duration::from_millis(10);
        super::on_tick(&mut c, swing_ready, false);

        // Asserting on the actor state alone would be VACUOUS: the wind-up sets
        // `Charging`, so the state moves off `Blocking` whether or not the guard was
        // actually released. The real defect is a guard window left standing while the
        // bot charges — it would keep resolving incoming hits as blocked, and keep
        // stunning the attacker, from behind a shield that is visually down.
        assert!(
            c.fighters[1].blocking_until.is_none(),
            "the guard WINDOW must be cleared, not just the actor state",
        );
        assert!(
            c.fighters[1].block_phase(swing_ready).is_none(),
            "a bot mid-wind-up must not still be blocking",
        );
        assert!(c.fighters[1].bot_swing_at.is_some(), "and the wind-up must start");
    }

    /// The high-block stun must put the ACTOR-STATE frame before the STATUS frame.
    ///
    /// Measured in retail: across every staggering high block in s615/s616 the order is
    /// op39 `Staggered` then op51 `Staggered` — **90 of 90, no exceptions**. Our status
    /// frames are emitted inline while state frames came from the end-of-tick drain,
    /// which put us in the opposite order on every stun we have ever sent.
    #[test]
    fn the_high_block_stun_sends_actor_state_before_status() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let out = super::stun_the_blocked_attacker(&mut c, 0, 1, now);

        let mut i39 = None;
        let mut i51 = None;
        for (n, (_slot, bytes)) in out.iter().enumerate() {
            if bytes.len() < 3 || bytes[1] != 0x36 {
                continue;
            }
            let p = arena_proto::parse_netdata(&bytes[2..]);
            match p.int(3) {
                Some(39) if i39.is_none() => i39 = Some(n),
                Some(51) if i51.is_none() => i51 = Some(n),
                _ => {}
            }
        }

        let a = i39.expect("the stun must emit an op39 actor-state frame");
        let b = i51.expect("the stun must emit an op51 status frame");
        assert!(
            a < b,
            "retail sends op39 before op51 (90/90); got op39 at {a} and op51 at {b}",
        );
    }

    /// The death frame's state ring must END in `Dead`, which means the loser has to
    /// be transitioned BEFORE the ring is snapshotted.
    ///
    /// The client picks its death animation from the pose the fighter was in when it
    /// died — the tail of this ring. Snapshotting first would ship a ring ending in
    /// whatever they were doing a moment earlier, and the corpse would animate out of
    /// the wrong pose. Every retail death frame decoded holds the invariant that the
    /// newest ring entry equals the frame's own propId 6.
    #[test]
    fn a_death_frame_carries_a_state_history_ending_in_dead() {
        let now = Instant::now();
        let mut c = combat(now, 2);

        // Give the loser some history to ring: a couple of real transitions first.
        c.fighters[1].set_actor_state(ActorStateType::Charging, now);
        c.fighters[1].set_actor_state(ActorStateType::Blocking, now + Duration::from_millis(200));

        let out = super::on_round_ending_death(&mut c, 0, now + Duration::from_millis(400));

        // Find the op29 among the emitted frames and decode it.
        let death = out
            .iter()
            .map(|(_, bytes)| bytes)
            .find(|b| {
                b.len() > 2
                    && b[1] == 0x36
                    && arena_proto::parse_netdata(&b[2..]).int(3) == Some(29)
            })
            .expect("a round-ending death must emit an op29");

        let p = arena_proto::parse_netdata(&death[2..]);
        assert_eq!(p.int(6), Some(3), "propId 6 must be ActorStateType::Dead");

        let ring = match p.get(7) {
            Some(arena_proto::NetDataValue::ByteArray(b)) => b.clone(),
            other => panic!("propId 7 must carry the state ring, got {other:?}"),
        };
        assert_eq!(ring.len(), ring[0] as usize + 3, "ring framing: len == count + 3");
        assert_eq!(
            *ring.last().unwrap(),
            3,
            "the ring must END in Dead — otherwise the client animates the wrong pose",
        );
        // And the two ActorDeadState bools retail always sends false.
        assert_eq!(p.get(9), Some(&arena_proto::NetDataValue::Bool(false)));
        assert_eq!(p.get(10), Some(&arena_proto::NetDataValue::Bool(false)));
    }

    // -----------------------------------------------------------------------
    // Bot ability casting — coverage rig
    // -----------------------------------------------------------------------

    /// Give the bot a loadout of real abilities, ordered so that "first in the list"
    /// and "least cast" are different answers once anything has been cast.
    fn bot_with_abilities(c: &mut MatchCombat) {
        use super::super::state::{AbilityTag, EquippedAbility};
        c.fighters[1].loadout.abilities = vec![
            EquippedAbility {
                instance_uuid: "4be1d681-c35d-4540-b255-c2910ac80664".into(), // Frostbite
                level: 4,
                tag: AbilityTag::Damage,
            },
            EquippedAbility {
                instance_uuid: "cfee0b02-6d91-4d34-869c-a7e54329060d".into(), // Ice Spike
                level: 4,
                tag: AbilityTag::Damage,
            },
            EquippedAbility {
                instance_uuid: "9fdc4d52-ce90-44f8-9b5d-21f31e27dbda".into(), // Paralyze
                level: 4,
                tag: AbilityTag::Paralyze,
            },
        ];
    }

    /// A bot with abilities CASTS one. Before this, bots only ever swung, which is why
    /// a human opponent never received a status effect: every stun/freeze/paralyse in a
    /// bot match flowed one way, because only the human side ever cast anything.
    #[test]
    fn a_bot_casts_an_ability_and_does_not_only_swing() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        bot_with_abilities(&mut c);
        // The small combat() fixture allocates only Avatar objects (564, 565), while
        // a real MatchInstance allocates Avatar, Player and Ability objects per slot,
        // making the bot Avatar 567. Pin the production-shaped id so a captured-id
        // constant can never satisfy this test by accident.
        c.fighters[1].net_object_id = 567;

        let at = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(10);
        super::on_tick(&mut c, at, false); // queue the required first swing
        super::on_tick(&mut c, at + super::BOT_CHARGE_WINDUP + Duration::from_millis(1), false);
        let out = super::on_tick(
            &mut c,
            at + super::BOT_CHARGE_WINDUP + Duration::from_millis(2),
            false,
        );

        assert!(!out.is_empty(), "the tick must produce frames");
        assert!(
            c.fighters[1].bot_last_cast.is_some(),
            "the bot must have cast an ability, not just queued a swing",
        );
        assert_eq!(
            c.fighters[1].bot_cast_counts.values().sum::<u32>(),
            1,
            "exactly one cast is counted for one cast",
        );
        let bot_obj = c.fighters[1].net_object_id as i64;
        let seen: Vec<_> = out
            .iter()
            .map(|(dest, frame)| {
                let nd = frame.get(2..).map(arena_proto::parse_netdata);
                (
                    *dest,
                    messages::user_message_gmid(frame),
                    nd.and_then(|n| n.int(0)),
                )
            })
            .collect();
        for gmid in [38, 53] {
            let frames: Vec<_> = out
                .iter()
                .filter(|(_, frame)| messages::user_message_gmid(frame) == Some(gmid))
                .collect();
            assert_eq!(frames.len(), 2, "gmid {gmid} must reach both viewers; saw {seen:?}");
            for (_, frame) in frames {
                let nd = arena_proto::parse_netdata(&frame[2..]);
                assert_eq!(
                    nd.int(0),
                    Some(bot_obj),
                    "gmid {gmid} must animate the bot's actual Avatar net object",
                );
            }
        }
    }

    /// Selection is least-cast-first, so a bot match exercises the WHOLE loadout
    /// instead of hammering whichever ability sorts first. This is the coverage
    /// property the rig exists for.
    #[test]
    fn the_bot_picks_the_least_cast_ability() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        bot_with_abilities(&mut c);

        // Nothing cast yet → first in loadout order.
        assert_eq!(
            super::bot_next_ability(&c.fighters[1]).as_deref(),
            Some("4be1d681-c35d-4540-b255-c2910ac80664"),
        );

        // Cast it twice and the SECOND ability becomes the least-cast one.
        c.fighters[1]
            .bot_cast_counts
            .insert("4be1d681-c35d-4540-b255-c2910ac80664".into(), 2);
        assert_eq!(
            super::bot_next_ability(&c.fighters[1]).as_deref(),
            Some("cfee0b02-6d91-4d34-869c-a7e54329060d"),
        );

        // Level the first two and the untouched third wins — the long tail of a
        // loadout gets reached, which uniform random selection would not guarantee.
        c.fighters[1]
            .bot_cast_counts
            .insert("cfee0b02-6d91-4d34-869c-a7e54329060d".into(), 2);
        assert_eq!(
            super::bot_next_ability(&c.fighters[1]).as_deref(),
            Some("9fdc4d52-ce90-44f8-9b5d-21f31e27dbda"),
        );
    }

    /// A perk is passive and never activates, so it must never be selected — otherwise
    /// the bot would burn its cast slot on something that cannot fire.
    #[test]
    fn the_bot_never_selects_a_perk() {
        use super::super::state::{AbilityTag, EquippedAbility};
        let now = Instant::now();
        let mut c = combat(now, 1);
        c.fighters[1].loadout.abilities = vec![
            EquippedAbility {
                instance_uuid: "00000000-0000-0000-0000-0000000000aa".into(),
                level: 1,
                tag: AbilityTag::Perk,
            },
            EquippedAbility {
                instance_uuid: "4be1d681-c35d-4540-b255-c2910ac80664".into(),
                level: 4,
                tag: AbilityTag::Damage,
            },
        ];
        assert_eq!(
            super::bot_next_ability(&c.fighters[1]).as_deref(),
            Some("4be1d681-c35d-4540-b255-c2910ac80664"),
            "the perk sorts first but must be skipped",
        );
    }

    /// A bot with no abilities at all still swings — the cast path must not deadlock
    /// the melee behaviour that already worked.
    #[test]
    fn a_bot_without_abilities_still_swings() {
        let now = Instant::now();
        let mut c = combat(now, 1);
        assert!(super::bot_next_ability(&c.fighters[1]).is_none());

        let at = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(10);
        super::on_tick(&mut c, at, false);
        assert!(
            c.fighters[1].bot_swing_at.is_some(),
            "with nothing to cast the bot must fall through to its swing",
        );
    }



    // -- The opening swing must be blockable -------------------------------
    //
    // `drive_bots` computed readiness as
    //   `last_swing.map(|t| now - t >= BOT_SWING_COOLDOWN).unwrap_or(true)`
    // and at round start `last_swing` is `None`, so the fallback said READY and the
    // bot charged on tick 0 of the round. Impact landed `BOT_CHARGE_WINDUP` (350 ms)
    // + `FOLLOW_THROUGH_DELAY` (50 ms) = 400 ms later — into which the player had to
    // see the round go live, press block, and get the c2s gmid 46 across WireGuard.
    // The opening hit was unblockable.

    /// The bot must not act on tick 0 of a live round, and the opening delay is a
    /// PER-ROUND property — round 2 gets it too.
    #[test]
    fn a_bot_cannot_act_before_the_round_start_delay() {
        let now = Instant::now();
        // expected_peers = 1 → slot 1 is the bot; the round goes live at `now`.
        let mut c = combat(now, 1);

        // Tick 0 of the live round.
        super::on_tick(&mut c, now, false);
        assert!(
            c.fighters[1].bot_swing_at.is_none(),
            "the bot must not queue a wind-up on tick 0 of the round",
        );
        assert_ne!(
            c.fighters[1].actor_state(),
            ActorStateType::Charging,
            "…nor enter Charging on tick 0",
        );

        // Nor at any instant before the opening delay has elapsed.
        let just_before = now + super::ROUND_START_ENGAGE_DELAY - Duration::from_millis(1);
        super::on_tick(&mut c, just_before, false);
        assert!(
            c.fighters[1].bot_swing_at.is_none(),
            "the bot must not act 1 ms before the opening delay expires",
        );

        // Once it has, the bot engages exactly as before.
        let after = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(1);
        super::on_tick(&mut c, after, false);
        let swing_at = c.fighters[1]
            .bot_swing_at
            .expect("the bot engages once the opening delay has elapsed");
        assert_eq!(
            c.fighters[1].actor_state(),
            ActorStateType::Charging,
            "…and the wind-up is a real telegraph, not an instant hit",
        );

        // The FIX IS THE OPENING DELAY, NOT A WIDER TELEGRAPH. 350 ms + 50 ms = 400 ms
        // matches retail's measured 383 ms median across 593 decoded swings; widening
        // it would move us AWAY from retail. This pins it so a future "fix" for an
        // unblockable opener cannot reach for the telegraph instead.
        assert_eq!(
            super::BOT_CHARGE_WINDUP,
            Duration::from_millis(350),
            "the telegraph must stay at retail's measured value — the opening delay is \
             the knob, not the wind-up",
        );

        // The earliest the opening blow can LAND is delay + wind-up + follow-through.
        let impact = swing_at + super::FOLLOW_THROUGH_DELAY;
        assert!(
            impact.duration_since(now)
                >= super::ROUND_START_ENGAGE_DELAY
                    + super::BOT_CHARGE_WINDUP
                    + super::FOLLOW_THROUGH_DELAY,
            "the opening blow must not be able to land before delay + wind-up + \
             follow-through",
        );

        // …and it is PER ROUND. Round 2 re-enters the live phase with a fresh
        // `phase_entered`, so the opening delay must apply again — a bot that opened
        // round 2 instantly would be the same defect with one round of warning.
        let r2 = after + Duration::from_secs(10);
        c.reset_fighters_for_next_round(r2);
        c.phase_entered = r2;
        super::on_tick(&mut c, r2, false);
        assert!(
            c.fighters[1].bot_swing_at.is_none(),
            "the opening delay is per-ROUND: the bot must not act on tick 0 of round 2",
        );
    }

    /// Match-level schedules are still round-scoped. Before this guard, a Frostbite
    /// channel from round 1 produced fresh-round ticks and each one could provoke the
    /// opponent's Revenge enchantment before either fighter took a new action.
    #[test]
    fn round_reset_drops_channels_hits_and_spell_impacts() {
        use super::super::state::{AbilityTag, ActiveChannel, PendingHit, PendingImpact};
        let t0 = Instant::now();
        let mut c = combat(t0, 2);
        c.channels.push(ActiveChannel {
            caster_slot: 0,
            target_slot: 1,
            ability_uuid: "4be1d681-c35d-4540-b255-c2910ac80664".into(),
            ability_level: 4,
            remaining_ticks: 6,
            next_tick_at: t0 + Duration::from_millis(200),
            magicka_full_at_cast: true,
                    interval_secs: crate::arena::combat::damage::CHANNEL_TICK_INTERVAL_SECS,
        });
        c.pending_hits.push(PendingHit {
            sender: 0,
            target: 1,
            side: ActiveSide::Left,
            swing_factor: 1.0,
            combo_count: 0,
            due: t0 + Duration::from_millis(50),
        });
        c.pending_impacts.push(PendingImpact {
            sender: 0,
            target: 1,
            ability_uuid: "9fdc4d52-ce90-44f8-9b5d-21f31e27dbda".into(),
            level: 2,
            tag: AbilityTag::Paralyze,
            due: t0 + Duration::from_millis(1500),
                    magicka_full_at_cast: false,
        });

        c.reset_fighters_for_next_round(t0 + Duration::from_secs(1));

        assert!(c.channels.is_empty(), "round 1 channels must not tick in round 2");
        assert!(c.pending_hits.is_empty(), "round 1 swings must not land in round 2");
        assert!(c.pending_impacts.is_empty(), "round 1 spells must not land in round 2");
    }

    // -- Frozen slows; it does not paralyse -------------------------------

    /// The shipped 0.75 slow multiplier lengthens weapon cadence and charge time
    /// while leaving the fighter able to act. The old implementation layered a
    /// five-second Staggered state on Frozen and completely rejected input.
    #[test]
    fn frozen_slows_weapon_timing_without_locking_input_or_inventing_stagger() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(30);
        let mut c = combat(t0, 1);
        let normal_cadence = super::swing_cooldown_for(&c.fighters[1], now);
        let normal_charge = super::critical_hold_secs_for(&c.fighters[1], now);
        let out = super::apply_status_conditioning(
            &mut c,
            1,
            &[(super::super::state::DamageType::Frost, 1.0e5)],
            now,
        );
        let froze = out.iter().any(|(_, f)| {
            messages::user_message_gmid(f) == Some(51) && {
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(4) == Some(1)
                    && nd.int(5) == Some(StatusEffectType::Frozen as u16 as i64)
            }
        });
        assert!(froze, "precondition: the op51 Frozen(5) apply must land");
        assert!(c.fighters[1].is_frozen(now));
        assert!(!c.fighters[1].is_staggered(now));
        assert_eq!(c.fighters[1].actor_state(), ActorStateType::Idle);
        assert!(c.fighters[1].take_state_changes().is_empty());

        let slow = super::super::gamedata::combat_params::SLOW_STATUS_MULTIPLIER;
        let frozen_cadence = super::swing_cooldown_for(&c.fighters[1], now);
        let frozen_charge = super::critical_hold_secs_for(&c.fighters[1], now);
        assert!((frozen_cadence.as_secs_f32() - normal_cadence.as_secs_f32() / slow).abs() < 1e-5);
        assert!((frozen_charge - normal_charge / slow).abs() < 1e-5);

        // expected_peers=1 makes slot 1 a bot. It still begins a wind-up.
        super::on_tick(&mut c, now + Duration::from_millis(10), false);
        assert!(
            c.fighters[1].bot_swing_at.is_some(),
            "Frozen is a slow, so a frozen fighter remains able to attack",
        );

        let frost_secs = super::super::gamedata::combat_params::elemental_status(5)
            .expect("Frost (status_type 5) is in the shipped ELEMENTAL_STATUSES table")
            .duration;
        let thawed = now + Duration::from_secs_f32(frost_secs + 0.1);
        assert!(!c.fighters[1].is_frozen(thawed));
        assert_eq!(super::swing_cooldown_for(&c.fighters[1], thawed), normal_cadence);
    }

    // -- (C) the bash's own 0.5 s guard window -------------------------------

    /// Every ShieldBash-family maneuver ships `blockDuration: 0.5` at every rank, and
    /// retail's `AbilityDoShieldBash` (`dump.cs:604149`) holds `_blockDuration`,
    /// `_appliedBlock`, `_removedBlock` and a `_blockingEffect`. We now raise it.
    #[test]
    fn every_shield_bash_raises_its_own_guard_window() {
        for name in [
            "ShieldBash",
            "HarryingBash",
            "StaggeringBash",
            "ReflectingBash",
            "ShieldOfMania",
        ] {
            let u = uuid_of(name);
            let window = super::super::gamedata::ability_rank_clamped(u, 1)
                .and_then(|r| r.block_duration())
                .unwrap_or_else(|| panic!("{name} R1 must ship _blockDuration"));
            assert!(
                (window - 0.5).abs() < 1e-6,
                "{name} ships blockDuration 0.50, got {window}",
            );

            let now = Instant::now();
            let mut c = combat(now, 2);
            super::begin_ability_guard(&mut c, 0, u, 1, now);

            assert_eq!(
                c.fighters[0].actor_state(),
                ActorStateType::Blocking,
                "{name}: the caster's guard is up",
            );
            assert_eq!(
                c.fighters[0].block_phase(now),
                Some(BlockPhase::Optimal),
                "{name}: and it opens HIGH",
            );
            let inside = now + Duration::from_secs_f32(window * 0.5);
            assert_eq!(c.fighters[0].block_phase(inside), Some(BlockPhase::Optimal));
            let outside = now + Duration::from_secs_f32(window + 0.05);
            assert_eq!(
                c.fighters[0].block_phase(outside),
                None,
                "{name}: the window is only {window}s long",
            );
        }
    }

    /// An ability with no `_blockDuration` must not raise a guard.
    #[test]
    fn a_non_bash_ability_raises_no_guard() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        super::begin_ability_guard(&mut c, 0, uuid_of("Guardbreaker"), 1, now);
        assert_ne!(c.fighters[0].actor_state(), ActorStateType::Blocking);
        assert_eq!(c.fighters[0].block_phase(now), None);
    }

    /// The reporter's second complaint, end to end: bash, then the opponent's weapon
    /// swing lands inside the bash's own 0.5 s guard → the opponent is stunned.
    /// Harrying Bash ships NO `_damageToCauseStagger` and NO `_stunDuration`, so this
    /// (B)+(C) path — not the ability gate — is what satisfies the expectation.
    #[test]
    fn a_bash_guard_stuns_an_incoming_weapon_swing() {
        for name in ["HarryingBash", "StaggeringBash"] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            super::begin_ability_guard(&mut c, 0, uuid_of(name), 1, now);

            let (out, impact) = swing_and_land(&mut c, 1, 0, now);
            assert!(
                c.fighters[1].is_staggered(impact),
                "{name}: a swing into the bash's guard stuns the swinger",
            );
            assert_eq!(status_frames(&out, StatusEffectType::Staggered), c.fighters.len());
        }
    }

    /// NOT A BUG, asserted so nobody "fixes" it: Harrying Bash is not an ability-gate
    /// stunner at any of its 14 ranks. Its shipped text is *"adds {1} seconds to all of
    /// the target's skill cooldowns"*, and `damage_type` is `none`.
    #[test]
    fn harrying_bash_ships_no_stagger_fields_at_any_rank() {
        let u = uuid_of("HarryingBash");
        let a = super::super::gamedata::ability(u).expect("HarryingBash");
        assert_eq!(a.maximum_level, 14);
        for lvl in 1..=a.maximum_level {
            let r = super::super::gamedata::ability_rank_clamped(u, lvl).expect("rank");
            assert!(r.damage_to_cause_stagger().is_none(), "rank {lvl}");
            assert!(r.stun_duration().is_none(), "rank {lvl}");
        }
    }

    #[test]
    fn harrying_bash_extends_both_spell_and_maneuver_cooldowns() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let spell = "11111111-1111-4111-8111-111111111111";
        let maneuver = "22222222-2222-4222-8222-222222222222";
        let perk = "33333333-3333-4333-8333-333333333333";
        c.fighters[1].loadout.abilities = vec![
            EquippedAbility { instance_uuid: spell.into(), level: 1, tag: AbilityTag::Damage },
            EquippedAbility { instance_uuid: maneuver.into(), level: 1, tag: AbilityTag::Maneuver },
            EquippedAbility { instance_uuid: perk.into(), level: 1, tag: AbilityTag::Perk },
        ];
        c.fighters[1].cooldowns.insert(spell.into(), now + Duration::from_secs(7));

        let harrying = uuid_of("HarryingBash");
        let added = super::super::gamedata::ability_rank_clamped(harrying, 1)
            .and_then(|rank| {
                rank.get(super::super::gamedata::AbilityField::CooldownIncrease)
            })
            .expect("Harrying Bash R1 ships _cooldownIncrease");
        super::apply_shipped_effects(&mut c, 0, 1, harrying, 1, 500.0, false, false, now);

        assert_eq!(
            c.fighters[1].cooldowns.get(spell),
            Some(&(now + Duration::from_secs_f32(7.0 + added))),
            "an already-cooling spell is extended",
        );
        assert_eq!(
            c.fighters[1].cooldowns.get(maneuver),
            Some(&(now + Duration::from_secs_f32(added))),
            "a ready stamina maneuver starts a cooldown",
        );
        assert!(!c.fighters[1].cooldowns.contains_key(perk), "passive perks have no cooldown");
    }

    /// 03-D13: `AbilityDoHarryingBash$$ApplyAdditionalEffects@0x1e958b8` moves the
    /// target's cooldowns only when the target is not Blocking, not Absorbing, and
    /// took health damage > 0. A blocked, absorbed or zero-damage bash leaves them —
    /// and sends no op83. The control (an unblocked bash that dealt damage) still
    /// harries.
    #[test]
    fn harrying_bash_needs_an_unblocked_unabsorbed_damaging_hit() {
        let harrying = uuid_of("HarryingBash");
        let maneuver = "22222222-2222-4222-8222-222222222222";
        let added = super::super::gamedata::ability_rank_clamped(harrying, 1)
            .and_then(|rank| rank.get(super::super::gamedata::AbilityField::CooldownIncrease))
            .expect("Harrying Bash R1 ships _cooldownIncrease");
        for (label, dealt, blocked, absorbing, harries) in [
            ("unblocked hit (control)", 500.0, false, false, true),
            ("blocked", 500.0, true, false, false),
            ("absorbed", 500.0, false, true, false),
            ("no health damage", 0.0, false, false, false),
        ] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            c.fighters[1].loadout.abilities = vec![EquippedAbility {
                instance_uuid: maneuver.into(),
                level: 1,
                tag: AbilityTag::Maneuver,
            }];
            let out = super::apply_shipped_effects(&mut c, 0, 1, harrying, 1, dealt, blocked, absorbing, now);
            assert_eq!(
                c.fighters[1].cooldowns.contains_key(maneuver),
                harries,
                "Harrying Bash vs {label}: cooldown moved = {harries}",
            );
            let op83 = messages::modify_ability_cooldowns(c.fighters[1].net_object_id, added);
            assert_eq!(
                out.iter().any(|(_, f)| *f == op83),
                harries,
                "Harrying Bash vs {label}: op83 sent = {harries}",
            );
        }
    }

    // -- (D) Guardbreaker vs Staggering Bash: opposite block conditions ------

    /// `Ability.Maneuver.Guardbreaker.Description`: *"…stuns a target that **blocks**
    /// it."* PR #25 applied a uniform damage-threshold rule, so it stunned an
    /// unblocking target too.
    #[test]
    fn guardbreaker_stuns_only_a_target_that_blocks() {
        let u = uuid_of("Guardbreaker");
        let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.damage_to_cause_stagger())
            .expect("Guardbreaker R1 ships _damageToCauseStagger");

        // A LOW block is still a block — it just carries no wire flag (03-D5).
        for (label, blocked, want) in [("a block", true, true), ("no block", false, false)] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            let out = super::apply_shipped_effects(&mut c, 0, 1, u, 1, threshold + 1.0, blocked, false, now);
            assert_eq!(
                c.fighters[1].is_staggered(now),
                want,
                "Guardbreaker vs {label}: expected staggered = {want}",
            );
            assert_eq!(
                status_frames(&out, StatusEffectType::Staggered),
                if want { c.fighters.len() } else { 0 },
                "Guardbreaker vs {label}: op51 count",
            );
        }
    }

    /// `Ability.Maneuver.StaggeringBash.Description`: *"…stuns a target that **does
    /// not block** it."* The exact opposite, from the same `damageToCauseStagger: 1`.
    #[test]
    fn staggering_bash_stuns_only_a_target_that_does_not_block() {
        let u = uuid_of("StaggeringBash");
        let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.damage_to_cause_stagger())
            .expect("StaggeringBash R1 ships _damageToCauseStagger");

        for (label, blocked, want) in [("a block", true, false), ("no block", false, true)] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            let out = super::apply_shipped_effects(&mut c, 0, 1, u, 1, threshold + 1.0, blocked, false, now);
            assert_eq!(
                c.fighters[1].is_staggered(now),
                want,
                "StaggeringBash vs {label}: expected staggered = {want}",
            );
            assert_eq!(
                status_frames(&out, StatusEffectType::Staggered),
                if want { c.fighters.len() } else { 0 },
                "StaggeringBash vs {label}: op51 count",
            );
        }
    }

    /// The two maneuvers ship IDENTICAL stagger data — so nothing in the data could
    /// have told them apart, which is why the condition is keyed on the editor name
    /// (retail keys it on the C# class: `AbilityDoGuardbreaker` vs
    /// `AbilityDoStaggeringBash`).
    #[test]
    fn the_two_maneuvers_are_indistinguishable_in_the_shipped_data() {
        let gb = super::super::gamedata::ability(uuid_of("Guardbreaker")).unwrap();
        let sb = super::super::gamedata::ability(uuid_of("StaggeringBash")).unwrap();
        for lvl in 1..=13u16 {
            let g = super::super::gamedata::ability_rank_clamped(gb.uuid, lvl).unwrap();
            let s = super::super::gamedata::ability_rank_clamped(sb.uuid, lvl).unwrap();
            assert_eq!(g.damage_to_cause_stagger(), s.damage_to_cause_stagger(), "rank {lvl}");
            assert_eq!(g.stun_duration(), s.stun_duration(), "rank {lvl}");
        }
    }

    /// IceSpike carries the same field with NO block condition in its text — *"Enemies
    /// that suffer more than {1} damage are stunned."* It must be unaffected by (D).
    #[test]
    fn icespike_is_unconditional_on_blocking() {
        let u = uuid_of("IceSpike");
        let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.damage_to_cause_stagger())
            .expect("IceSpike R1 ships _damageToCauseStagger");
        for blocked in [false, true] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            super::apply_shipped_effects(&mut c, 0, 1, u, 1, threshold + 1.0, blocked, false, now);
            assert!(
                c.fighters[1].is_staggered(now),
                "IceSpike staggers on damage alone (blocked {blocked})",
            );
        }
    }
}

#[cfg(test)]
mod continuous_area_tests {
    //! Ebony Mail: "Does {0} poison damage per second." Before this, the property
    //! reached `apply_enchant` and fell through `_ => {}`, so the arena ignored it.
    use std::time::{Duration, Instant};

    use super::super::loadout::apply_template_properties;
    use super::super::state::{DamageSource, DamageType, FlowState, MatchCombat};
    use super::messages;

    const EBONY_MAIL: &str = "def810af-e9f5-4e23-9247-1edf391d82e1";
    const TICK: f32 = super::super::damage::CONTINUOUS_AREA_TICK_SECS;

    fn live() -> (MatchCombat, Instant) {
        let now = Instant::now();
        (super::tests::make_live_combat(now), now)
    }

    fn wear_ebony_mail(combat: &mut MatchCombat, slot: usize) {
        apply_template_properties(&mut combat.fighters[slot].loadout, EBONY_MAIL);
    }

    /// Step `on_tick` at the tick interval for `secs` seconds after `start`.
    fn run(combat: &mut MatchCombat, start: Instant, secs: f32) -> Vec<(usize, Vec<u8>)> {
        let steps = (secs / TICK).round() as u32;
        let interval = Duration::from_secs_f32(TICK);
        let mut out = Vec::new();
        for i in 1..=steps {
            out.extend(super::on_tick(combat, start + interval * i, false));
        }
        out
    }

    /// `(damaged obj, source, active side, total, components)` for every op50 sent
    /// to viewer 0.
    fn op50s(out: &[(usize, Vec<u8>)]) -> Vec<(i64, u8, u8, f32, Vec<(u8, f32)>)> {
        let float = |nd: &arena_proto::NetDataParse, k: u8| match nd.get(k) {
            Some(arena_proto::NetDataValue::Float(v)) => *v,
            _ => f32::NAN,
        };
        out.iter()
            .filter(|(v, f)| *v == 0 && messages::user_message_gmid(f) == Some(50))
            .map(|(_, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                let n = nd.int(12).unwrap_or(0) as u8;
                let comps = (0..n)
                    .map(|k| {
                        (
                            nd.int(13 + 2 * k).unwrap_or(0) as u8,
                            float(&nd, 14 + 2 * k),
                        )
                    })
                    .collect();
                (
                    nd.int(0).unwrap_or(-1),
                    nd.int(6).unwrap_or(0) as u8,
                    nd.int(10).unwrap_or(0) as u8,
                    float(&nd, 8),
                    comps,
                )
            })
            .collect()
    }

    fn area_effect(out: &[(usize, Vec<u8>)]) -> Vec<(i64, u8, u8, f32, Vec<(u8, f32)>)> {
        op50s(out)
            .into_iter()
            .filter(|m| m.1 == DamageSource::AreaEffect as u8)
            .collect()
    }

    /// THE REPORTED QUESTION. Five seconds in the mail costs the opponent 5 x 9.4 = 47
    /// health, in 25 ticks of 1.88 poison on the retail wire shape: AreaEffect (7),
    /// ActiveSide None (0), one Poison component.
    ///
    /// The wearer's own template also carries Fortify Poison t6, and the tick is still
    /// 1.88. Retail's path never runs the attacker's damage bonuses.
    #[test]
    fn ebony_mail_poisons_the_opponent_at_9_4_per_second() {
        let (mut combat, now) = live();
        wear_ebony_mail(&mut combat, 0);
        assert!(
            combat.fighters[0]
                .loadout
                .element_fortify
                .iter()
                .any(|(t, v)| *t == DamageType::Poison && *v > 0.0),
            "precondition: the real template carries Fortify Poison"
        );
        let target_obj = combat.fighters[1].net_object_id as i64;
        let hp0 = combat.fighters[1].health;

        // 26 steps: the first schedules, the next 25 each deliver one tick.
        let out = run(&mut combat, now, 26.0 * TICK);
        let ticks = area_effect(&out);
        assert_eq!(ticks.len(), 25, "one tick per {TICK}s over 5s");
        for (obj, _, side, total, comps) in &ticks {
            assert_eq!(*obj, target_obj, "the OPPONENT takes it, never the wearer");
            assert_eq!(*side, 0, "retail passes ActiveSide.None");
            assert_eq!(comps.len(), 1);
            assert_eq!(comps[0].0, DamageType::Poison as u8);
            assert!(
                (total - 9.4 * TICK).abs() < 1e-4,
                "tick = 9.4/s x {TICK}s, got {total}"
            );
        }
        let lost = hp0 - combat.fighters[1].health;
        assert!(
            (46..=47).contains(&lost),
            "5s at 9.4/s = 47 HP, lost {lost}"
        );
        assert_eq!(
            combat.fighters[0].health, combat.fighters[0].max_health,
            "the wearer is untouched"
        );
    }

    /// CONTROL: the same run without the mail emits no AreaEffect frame and costs
    /// nothing, so the damage above comes from the property alone.
    #[test]
    fn control_without_the_mail_nothing_ticks() {
        let (mut combat, now) = live();
        let hp0 = combat.fighters[1].health;
        let out = run(&mut combat, now, 26.0 * TICK);
        assert!(area_effect(&out).is_empty());
        assert_eq!(combat.fighters[1].health, hp0);
    }

    /// The effect keeps killing to the end, then ends the round and stops.
    #[test]
    fn it_can_kill_and_stops_once_the_opponent_is_dead() {
        let (mut combat, now) = live();
        wear_ebony_mail(&mut combat, 0);
        combat.fighters[1].health = 3;
        let out = run(&mut combat, now, 20.0 * TICK);
        let ticks = area_effect(&out);
        // 1.88 + 1.88 = 3.76 -> 3 whole HP after the second tick.
        assert_eq!(ticks.len(), 2, "no tick after the killing one");
        assert!(combat.fighters[1].is_dead());
        assert!(
            !matches!(combat.phase, FlowState::StateTimeout),
            "the death ended the round"
        );
    }

    /// A dead wearer's mail does nothing.
    #[test]
    fn a_dead_wearer_deals_nothing() {
        let (mut combat, now) = live();
        wear_ebony_mail(&mut combat, 0);
        combat.fighters[0].health = 0;
        let hp0 = combat.fighters[1].health;
        let out = run(&mut combat, now, 10.0 * TICK);
        assert!(area_effect(&out).is_empty());
        assert_eq!(combat.fighters[1].health, hp0);
    }

    /// Poison resistance is subtracted at the tick's share of a second, so a small
    /// rating trims the tick rather than zeroing it. A large one hits the shipped 95%
    /// cap, the same as any other hit.
    #[test]
    fn resistance_is_charged_per_second_and_capped() {
        for (rating, want) in [
            (2.0_f32, 9.4 * TICK - 2.0 * TICK),
            (500.0, 9.4 * TICK * 0.05),
        ] {
            let (mut combat, now) = live();
            wear_ebony_mail(&mut combat, 0);
            combat.fighters[1].loadout.resistances = vec![(DamageType::Poison, rating)];
            let out = run(&mut combat, now, 3.0 * TICK);
            let ticks = area_effect(&out);
            assert!(!ticks.is_empty());
            for (_, _, _, total, _) in ticks {
                assert!(
                    (total - want).abs() < 1e-3,
                    "rating {rating}: want {want}, got {total}"
                );
            }
        }
    }

    /// An AreaEffect (7) tick is NEVER blocked: `Damage$$IsBlockable@0x1bd4cc8` has no
    /// bit for source 7, whatever `unblockable` says (M-areaeffect-block / 01-D7). This
    /// test used to assert the opposite. A raised guard leaves the tick at 9.4 × 0.2;
    /// control: the same tick with the guard down, identical.
    #[test]
    fn a_raised_guard_does_not_block_it() {
        use super::super::damage::{flags, resolve_continuous_area_tick};
        use super::super::loadout::starter;
        use super::super::state::{ActorStateType, Fighter};
        let now = Instant::now();
        let mut d = Fighter::new(1, 2, starter(), now);
        let open = resolve_continuous_area_tick(&starter(), &d, DamageType::Poison, 9.4, now);
        assert!((open.total - 9.4 * TICK).abs() < 1e-4);
        assert_eq!(
            open.flags & (flags::WAS_LATE_BLOCKING | flags::WAS_OPTIMAL_BLOCKING),
            0
        );

        d.loadout.block_rating = 400.0;
        d.set_actor_state(ActorStateType::Blocking, now);
        d.last_block_dropped_at = Some(now);
        d.block_raised_at = Some(now);
        d.blocking_until = Some(now + Duration::from_secs(5));
        let guarded = resolve_continuous_area_tick(&starter(), &d, DamageType::Poison, 9.4, now);
        assert_eq!(guarded.total.to_bits(), open.total.to_bits(), "a guard does not touch it");
        assert!(!guarded.blocked);
        // A LOW guard: no bit 2 and no bit 3.
        assert_eq!(guarded.flags & (flags::WAS_LATE_BLOCKING | flags::WAS_OPTIMAL_BLOCKING), 0);
        assert_eq!(
            guarded.active_side as u8, 0,
            "the wire still says ActiveSide.None"
        );
    }

    /// A new round starts its schedule again, one interval after it goes live.
    #[test]
    fn a_round_reset_restarts_the_schedule() {
        let (mut combat, now) = live();
        wear_ebony_mail(&mut combat, 0);
        run(&mut combat, now, 5.0 * TICK);
        assert!(combat.fighters[0].continuous_next_tick_at.is_some());
        combat.reset_fighters_for_next_round(now);
        assert_eq!(combat.fighters[0].continuous_next_tick_at, None);
        assert_eq!(combat.fighters[0].continuous_carry, 0.0);
    }
}

/// CRE-SOAK: a round ends ONCE.
///
/// Found by the whole-match soak (`engine::soak_tests`, bot-vs-bot, seed
/// 0x305edec27c61db36): both fighters' maneuver impacts fell due on the same tick.
/// The first killed its target and ended the match; `land_due_impacts` had already
/// taken the second out of the queue and landed it anyway — from a DEAD caster, into
/// a finished round — which killed the survivor and ran `on_round_ended` twice more
/// (once per dead fighter in `emit_damage`). The result: `round_winners` [0,1,0,1,0]
/// (op48 announcing five rounds), three op29 death frames, and `combat.winner`
/// flipped 0 → 1 → 0, i.e. the match could be awarded to the player who died first.
/// `land_due_hits` and `land_due_echoes` already refuse to land outside the live
/// round; `land_due_impacts` did not.
#[cfg(test)]
mod round_ends_once_tests {
    use super::*;
    use super::super::loadout::starter;
    use super::super::state::{Fighter, FlowState, MatchCombat, PendingHit, PendingImpact};

    const POWER_ATTACK: &str = "ce6b63e9-9f18-49c4-aee0-51f7985f9892";

    fn live(now: Instant) -> MatchCombat {
        let mut c = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            c.fighters.push(Fighter::new(slot, obj, starter(), now));
        }
        c.match_net_object_id = c.alloc_net_object_id();
        c.phase = FlowState::StateTimeout;
        c.round = 1;
        c
    }

    fn impact(sender: usize, due: Instant) -> PendingImpact {
        PendingImpact {
            sender,
            target: 1 - sender,
            ability_uuid: POWER_ATTACK.to_string(),
            level: 1,
            tag: super::super::state::AbilityTag::Maneuver,
            magicka_full_at_cast: false,
            due,
        }
    }

    fn op48_count(out: &[(usize, Vec<u8>)]) -> usize {
        out.iter()
            .filter(|(v, b)| {
                *v == 0
                    && b.len() > 2
                    && b[1] == 0x36
                    && arena_proto::parse_netdata(&b[2..]).int(3) == Some(48)
            })
            .count()
    }

    /// Two lethal impacts due on one tick: the first ends the round, the second must
    /// not land. Round 1 (non-final): one winner recorded, one op48, survivor alive.
    #[test]
    fn a_second_impact_on_the_killing_tick_does_not_land() {
        let now = Instant::now();
        let mut c = live(now);
        c.fighters[0].health = 1;
        c.fighters[1].health = 1;
        c.pending_impacts.push(impact(0, now));
        c.pending_impacts.push(impact(1, now));

        let out = land_due_impacts(&mut c, now + Duration::from_millis(1));

        assert_eq!(c.round_winners, vec![0], "the round ended once, won by slot 0");
        assert_eq!(c.rounds_won, [1, 0]);
        assert_eq!(c.phase, FlowState::NextState);
        assert!(!c.fighters[0].is_dead(), "slot 1's impact landed after the round ended");
        assert_eq!(op48_count(&out), 1, "exactly one round result to each viewer");
    }

    /// The match-ending variant: the winner must stay the fighter who got the kill.
    #[test]
    fn the_match_winner_cannot_flip_on_a_trailing_impact() {
        let now = Instant::now();
        let mut c = live(now);
        c.round = 3;
        c.rounds_won = [1, 1];
        c.round_winners = vec![0, 1];
        c.fighters[0].health = 1;
        c.fighters[1].health = 1;
        c.pending_impacts.push(impact(0, now));
        c.pending_impacts.push(impact(1, now));

        let out = land_due_impacts(&mut c, now + Duration::from_millis(1));

        assert_eq!(c.winner, Some(0), "slot 0 killed first and won the match");
        assert_eq!(c.round_winners, vec![0, 1, 0], "three rounds, not five");
        assert_eq!(c.rounds_won, [2, 1]);
        assert_eq!(c.phase, FlowState::RoundEnd);
        assert_eq!(op48_count(&out), 1);
    }

    /// Control: impacts still land in a live round (the guard is not "never land").
    #[test]
    fn a_single_due_impact_still_lands_in_a_live_round() {
        let now = Instant::now();
        let mut c = live(now);
        let before = c.fighters[1].health;
        c.pending_impacts.push(impact(0, now));
        land_due_impacts(&mut c, now + Duration::from_millis(1));
        assert!(c.fighters[1].health < before, "the maneuver impact must land");
        assert_eq!(c.phase, FlowState::StateTimeout);
    }

    /// One hit that leaves BOTH fighters dead (the target dies, and Reflecting Bash
    /// sends enough back to kill the attacker) is ONE round end — a double KO —
    /// not two. `emit_damage` used to call `on_round_ending_death` once per corpse.
    #[test]
    fn a_hit_that_kills_both_fighters_ends_the_round_once() {
        let now = Instant::now();
        let mut c = live(now);
        c.fighters[0].health = 1;
        c.fighters[1].health = 1;
        c.fighters[1].reflect_until = Some(now + Duration::from_secs(5));
        c.fighters[1].reflect_remaining = 10_000.0;
        c.pending_hits.push(PendingHit {
            sender: 0,
            target: 1,
            side: super::super::state::ActiveSide::Right,
            swing_factor: 1.0,
            combo_count: 0,
            due: now,
        });

        let out = land_due_hits(&mut c, now + Duration::from_millis(1));

        assert!(c.fighters[0].is_dead() && c.fighters[1].is_dead(), "fixture: both must die");
        assert_eq!(c.rounds_won, [0, 0], "a double KO scores nothing");
        assert_eq!(
            c.round_winners.len(),
            1,
            "one round end, not one per corpse: {:?}",
            c.round_winners
        );
        assert_eq!(op48_count(&out), 1);
    }
}

/// PR-02 (gap register M-status-removes, M-absorb-gate, 08-D10, 03-D9, 03-D10).
///
/// A PvP client ends a status ONLY on the server's op51 remove
/// (`PvpPlayerActor$$RemoveStatusEffect@0x1a32aa8` removes only on `force`), so
/// every status we apply must get a remove at the end of its lifetime. Expected
/// lifetimes are the spec's shipped rank-1 values (combat-spec ch05/06/08), not
/// read back from the code under test.
#[cfg(test)]
mod status_removals_tests {
    use super::*;
    use super::super::loadout;
    use super::super::state::{ActorStateType, DamageType, Fighter, StatusEffectType};

    fn combat2(now: Instant) -> MatchCombat {
        let mut c = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            c.fighters.push(Fighter::new(slot, obj, loadout::starter(), now));
        }
        c.phase = FlowState::StateTimeout;
        c
    }

    fn uuid_of(editor: &str) -> &'static str {
        super::super::gamedata::ABILITIES
            .iter()
            .find(|a| a.editor_name == editor)
            .map(|a| a.uuid)
            .unwrap_or_else(|| panic!("{editor} missing from the shipped table"))
    }

    /// op51 frames in `out` for `status` with `apply` (true) or remove (false).
    fn op51(out: &[(usize, Vec<u8>)], status: StatusEffectType, apply: bool) -> usize {
        out.iter()
            .filter(|(_, f)| {
                if f.len() <= 2 || f[1] != 0x36 {
                    return false;
                }
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(3) == Some(51)
                    && nd.int(5) == Some(status as u16 as i64)
                    && nd.props.get(&4) == Some(&arena_proto::NetDataValue::Bool(apply))
            })
            .count()
    }

    fn at(now: Instant, secs: f32) -> Instant {
        now + Duration::from_secs_f32(secs)
    }

    /// Seed the diff while the status is live, then assert no remove just before
    /// `lifetime` and one remove per viewer just after it.
    fn assert_removed_at(
        c: &mut MatchCombat,
        now: Instant,
        status: StatusEffectType,
        lifetime: f32,
    ) {
        assert!(emit_status_removals(c, now).is_empty(), "{status:?}: fresh, nothing lapsed");
        let before = emit_status_removals(c, at(now, lifetime - 0.1));
        assert_eq!(op51(&before, status, false), 0, "{status:?}: still live at {lifetime}-0.1 s");
        let after = emit_status_removals(c, at(now, lifetime + 0.1));
        assert_eq!(
            op51(&after, status, false),
            c.fighters.len(),
            "{status:?}: one op51 remove per viewer after {lifetime} s"
        );
        let again = emit_status_removals(c, at(now, lifetime + 0.2));
        assert_eq!(op51(&again, status, false), 0, "{status:?}: removed once, not twice");
    }

    // ---- M-status-removes: every timed buff gets its remove -----------------

    /// Ward rank 1: `_wardDuration` 3 s (ch06 §4.1).
    #[test]
    fn ward_is_removed_when_its_duration_ends() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_ward(&mut c, 0, 1, now);
        assert_eq!(op51(&out, StatusEffectType::Ward, true), 2);
        assert_removed_at(&mut c, now, StatusEffectType::Ward, 3.0);
    }

    /// `AbilityDoWard$$Update@0x1e9aea0` also completes when the pool breaks, and
    /// `Cleanup` removes status 15 — ch06 §7's suggested test "op51 remove when a
    /// Ward breaks".
    #[test]
    fn ward_is_removed_when_its_pool_breaks() {
        let now = Instant::now();
        let mut c = combat2(now);
        apply_ward(&mut c, 0, 1, now);
        assert!(emit_status_removals(&mut c, now).is_empty());
        let mut hit = vec![(DamageType::Fire, 10_000.0)];
        c.fighters[0].apply_negation_pools(&mut hit);
        let out = emit_status_removals(&mut c, at(now, 0.5));
        assert_eq!(op51(&out, StatusEffectType::Ward, false), 2, "broken at 0.5 s, long before 3 s");
    }

    /// Absorb rank 1: `_duration` 1.5 s (ch06 §4.2). Retail: 26 applies, 21 removes.
    #[test]
    fn absorb_is_removed_when_its_duration_ends() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_absorb(&mut c, 0, 1, now);
        assert_eq!(op51(&out, StatusEffectType::Absorb, true), 2);
        assert_removed_at(&mut c, now, StatusEffectType::Absorb, 1.5);
    }

    /// Resist Elements rank 1: `_resistanceDuration` 10 s, one status per element
    /// 60-63 (ch06 §4.3). ch08's suggested test: four removes after the duration.
    #[test]
    fn resist_elements_removes_all_four_resistances() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_resist_elements(&mut c, 0, 1, now);
        let four = [
            StatusEffectType::FireResistance,
            StatusEffectType::FrostResistance,
            StatusEffectType::ShockResistance,
            StatusEffectType::PoisonResistance,
        ];
        for st in four {
            assert_eq!(op51(&out, st, true), 2, "{st:?} applied");
        }
        assert!(emit_status_removals(&mut c, now).is_empty());
        let before = emit_status_removals(&mut c, at(now, 9.9));
        let after = emit_status_removals(&mut c, at(now, 10.1));
        for st in four {
            assert_eq!(op51(&before, st, false), 0, "{st:?} still live at 9.9 s");
            assert_eq!(op51(&after, st, false), 2, "{st:?} removed after 10 s");
        }
    }

    /// Reckless Fury: `duration` 5 s (ch05 §3.9). Its remove is what lets the
    /// caster's client predict a guard again (`Actor$$get_CanBlock@0x1c5b9d4`).
    /// ch05 D6's suggested test: advance 5.1 s, expect op51(11, remove) to both.
    #[test]
    fn reckless_fury_is_removed_when_it_ends() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_reckless_fury(&mut c, 0, 1, now);
        assert_eq!(op51(&out, StatusEffectType::RecklessFury, true), 2);
        assert_removed_at(&mut c, now, StatusEffectType::RecklessFury, 5.0);
    }

    /// Wall of Fire: `_duration` 5 s (ch14 W5; `AbilityDoFirewall$$Cleanup
    /// @0x1e9463c` removes status 13).
    #[test]
    fn wall_of_fire_is_removed_when_it_ends() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("Firewall"), 1, 0.0, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::Firewall, true), 2);
        assert_removed_at(&mut c, now, StatusEffectType::Firewall, 5.0);
    }

    /// Storm armor has no timer; it ends when its shield breaks (`AbilityDoStormArmor
    /// $$Update@0x1e9a3ac`). Control: it must NOT be removed while the shield holds.
    #[test]
    fn storm_armor_is_removed_when_its_shield_breaks() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out =
            apply_shipped_effects(&mut c, 0, 1, uuid_of("BlizzardArmor"), 1, 0.0, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::ElementalStormArmor, true), 2);
        assert!(emit_status_removals(&mut c, now).is_empty());
        let held = emit_status_removals(&mut c, at(now, 60.0));
        assert_eq!(
            op51(&held, StatusEffectType::ElementalStormArmor, false),
            0,
            "no timer: an unbroken shield is still up after a minute"
        );
        for _ in 0..16 {
            let mut hit = vec![(DamageType::Slashing, 1_000.0)];
            c.fighters[0].apply_negation_pools(&mut hit);
        }
        let broken = emit_status_removals(&mut c, at(now, 61.0));
        assert_eq!(op51(&broken, StatusEffectType::ElementalStormArmor, false), 2);
    }

    /// FlashFreeze sends Frozen (5) with the paralysis; ch08 §8: it was never
    /// removed, leaving the client's local Slow and Stamina-regen block up.
    #[test]
    fn flash_freeze_frozen_is_removed_with_its_duration() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("FlashFreeze");
        let secs = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.freeze_duration().or_else(|| r.paralyze_duration()))
            .expect("FlashFreeze R1 ships a freeze duration");
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::Frozen, true), 2);
        assert_removed_at(&mut c, now, StatusEffectType::Frozen, secs);
    }

    /// Blocking (1): applied when the guard comes up, removed when it drops —
    /// retail 736 applies / 756 removes in s615+s616 (ch03 §7 suggested test).
    #[test]
    fn blocking_status_follows_the_guard() {
        let now = Instant::now();
        let mut c = combat2(now);
        let f = &mut c.fighters[0];
        f.set_actor_state(ActorStateType::Blocking, now);
        f.blocking_until = Some(now + Duration::from_secs(5));
        f.block_raised_at = Some(now);
        let up = drain_state_changes(&mut c, now);
        assert_eq!(op51(&up, StatusEffectType::Blocking, true), 2, "guard up → apply");
        assert_eq!(op51(&up, StatusEffectType::Blocking, false), 0);
        // State frame first, then the status (retail's order for a state + its status).
        let first_status = up.iter().position(|(_, f)| {
            arena_proto::parse_netdata(&f[2..]).int(3) == Some(51)
        });
        let last_state = up.iter().rposition(|(_, f)| {
            arena_proto::parse_netdata(&f[2..]).int(3) == Some(41)
        });
        assert!(last_state < first_status, "op41 before op51");

        // Control: nothing changed, nothing sent.
        let idle = drain_state_changes(&mut c, at(now, 0.5));
        assert_eq!(op51(&idle, StatusEffectType::Blocking, true), 0);
        assert_eq!(op51(&idle, StatusEffectType::Blocking, false), 0);
        // The OPPONENT never blocked and is never announced.
        assert!(!c.fighters[1].guard_up(now));

        c.fighters[0].blocking_until = None;
        c.fighters[0].reconcile_block(at(now, 1.0));
        let down = drain_state_changes(&mut c, at(now, 1.0));
        assert_eq!(op51(&down, StatusEffectType::Blocking, false), 2, "guard down → remove");
        assert_eq!(op51(&down, StatusEffectType::Blocking, true), 0);
    }

    /// A stagger drops the guard, so the Blocking remove goes out with it.
    #[test]
    fn a_stagger_that_drops_the_guard_removes_blocking() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[0].set_actor_state(ActorStateType::Blocking, now);
        c.fighters[0].blocking_until = Some(now + Duration::from_secs(5));
        drain_state_changes(&mut c, now);
        assert!(c.fighters[0].apply_stagger_for(at(now, 0.2), 2.5));
        let out = drain_state_changes(&mut c, at(now, 0.2));
        assert_eq!(op51(&out, StatusEffectType::Blocking, false), 2);
    }

    // ---- 03-D9 / 08-D10: a refused stagger is not announced ------------------

    /// ch03 §7: "Attacker under Reckless Fury is high-blocked. Expect no op51(3)
    /// and no op51(10)." `CanBeStaggered` is false under Fury
    /// (`Actor$$get_CanBeStaggered@0x1c5b9f4`), so `CausedStagger` never fires.
    #[test]
    fn high_blocking_a_fury_attacker_sends_no_stagger_and_no_weakness() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].loadout.powerful_block = 50.4;
        apply_reckless_fury(&mut c, 0, 1, now);
        let out = stun_the_blocked_attacker(&mut c, 0, 1, at(now, 1.0));
        assert_eq!(op51(&out, StatusEffectType::Staggered, true), 0);
        assert_eq!(op51(&out, StatusEffectType::StaggeredWeakness, true), 0);
        assert!(!c.fighters[0].is_staggered(at(now, 1.0)));
        assert_eq!(c.fighters[0].weakness_rating_against(DamageType::Slashing, at(now, 1.0)), 0.0);
    }

    /// The control: the same high block without Fury staggers and weakens.
    #[test]
    fn high_blocking_a_plain_attacker_still_staggers_and_weakens() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].loadout.powerful_block = 50.4;
        let out = stun_the_blocked_attacker(&mut c, 0, 1, now);
        assert_eq!(op51(&out, StatusEffectType::Staggered, true), 2);
        assert_eq!(op51(&out, StatusEffectType::StaggeredWeakness, true), 2);
        assert_eq!(c.fighters[0].weakness_rating_against(DamageType::Slashing, now), 50.4);
    }

    /// ch08 §4.2: "Paralyse, then high-block the victim's attack. The paralysis
    /// continues and no op51(3) is sent." (`ActorParalyzedState$$CanTransitionTo
    /// @0x1fd4c48` admits only Dead and EnemyNonLethal.)
    #[test]
    fn a_paralysed_attacker_cannot_be_staggered() {
        let now = Instant::now();
        let mut c = combat2(now);
        let par = try_paralyze(&mut c, 1, 0, 1, 1_000.0, false, now);
        assert_eq!(op51(&par, StatusEffectType::Paralyzed, true), 2);
        let out = stun_the_blocked_attacker(&mut c, 0, 1, at(now, 0.5));
        assert_eq!(op51(&out, StatusEffectType::Staggered, true), 0);
        assert!(c.fighters[0].is_paralyzed(), "the paralysis continues");
        assert!(!c.fighters[0].is_staggered(at(now, 0.5)));
    }

    /// The same rule on the ability path: an Ice Spike over its threshold does not
    /// stagger a paralysed target.
    #[test]
    fn an_ice_spike_does_not_stagger_a_paralysed_target() {
        let now = Instant::now();
        let mut c = combat2(now);
        try_paralyze(&mut c, 0, 1, 1, 1_000.0, false, now);
        let out =
            apply_shipped_effects(&mut c, 0, 1, uuid_of("IceSpike"), 1, 10_000.0, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::Staggered, true), 0);
        assert!(c.fighters[1].is_paralyzed());
    }

    // ---- 03-D10: StaggeredWeakness is not re-applied while held ---------------

    /// ch03 §7: "Two high blocks within one stagger, from blockers with different
    /// Powerful Block tiers. The magnitude must stay at the first."
    /// (`PowerfulBlockBonusInstance$$CausedStagger@0x1d51214` returns early.)
    #[test]
    fn a_second_powerful_block_does_not_overwrite_the_weakness() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].loadout.powerful_block = 50.4;
        stun_the_blocked_attacker(&mut c, 0, 1, now);
        c.fighters[1].loadout.powerful_block = 20.0;
        let second = stun_the_blocked_attacker(&mut c, 0, 1, at(now, 0.5));
        assert_eq!(op51(&second, StatusEffectType::StaggeredWeakness, true), 0, "no second apply");
        assert_eq!(
            c.fighters[0].weakness_rating_against(DamageType::Slashing, at(now, 0.5)),
            50.4,
            "the first magnitude sticks"
        );
    }

    /// The weakness is removed with the stagger that produced it, and a later
    /// plain stagger does not revive it.
    #[test]
    fn staggered_weakness_is_removed_with_the_stagger() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].loadout.powerful_block = 50.4;
        stun_the_blocked_attacker(&mut c, 0, 1, now);
        assert!(emit_status_removals(&mut c, now).is_empty());
        let out = emit_status_removals(&mut c, at(now, 2.6));
        assert_eq!(op51(&out, StatusEffectType::Staggered, false), 2);
        assert_eq!(op51(&out, StatusEffectType::StaggeredWeakness, false), 2);

        c.fighters[0].reconcile_stagger(at(now, 2.6));
        assert!(c.fighters[0].apply_stagger_for(at(now, 3.0), 2.5));
        assert!(
            !c.fighters[0].has_staggered_weakness(at(now, 3.0)),
            "a fresh plain stagger carries no weakness"
        );
    }

    // ---- M-absorb-gate: strict > and no proc through Absorb ------------------

    #[test]
    fn absorb_blocks_the_blind_proc() {
        let now = Instant::now();
        let u = uuid_of("Blind");
        let mut shielded = combat2(now);
        apply_absorb(&mut shielded, 1, 1, now);
        let absorbing = shielded.fighters[1].has_absorb(now);
        assert!(absorbing);
        let out = apply_shipped_effects(&mut shielded, 0, 1, u, 1, 9_999.0, false, absorbing, now);
        assert_eq!(op51(&out, StatusEffectType::Blind, true), 0, "no Blind through Absorb");

        let mut control = combat2(now);
        let out = apply_shipped_effects(&mut control, 0, 1, u, 1, 9_999.0, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::Blind, true), 2, "control: Blind lands");
    }

    #[test]
    fn absorb_blocks_the_paralyze_proc() {
        let now = Instant::now();
        let mut shielded = combat2(now);
        try_paralyze(&mut shielded, 0, 1, 1, 1_000.0, true, now);
        assert!(!shielded.fighters[1].is_paralyzed(), "no paralysis through Absorb");
        let mut control = combat2(now);
        try_paralyze(&mut control, 0, 1, 1, 1_000.0, false, now);
        assert!(control.fighters[1].is_paralyzed(), "control: paralysis lands");
    }

    #[test]
    fn absorb_blocks_ice_spike_and_staggering_bash_stuns() {
        for name in ["IceSpike", "StaggeringBash"] {
            let now = Instant::now();
            let u = uuid_of(name);
            let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
                .and_then(|r| r.damage_to_cause_stagger())
                .unwrap();
            let mut shielded = combat2(now);
            let out = apply_shipped_effects(&mut shielded, 0, 1, u, 1, threshold + 1.0, false, true, now);
            assert_eq!(op51(&out, StatusEffectType::Staggered, true), 0, "{name}: no stun through Absorb");
            let mut control = combat2(now);
            let out = apply_shipped_effects(&mut control, 0, 1, u, 1, threshold + 1.0, false, false, now);
            assert_eq!(op51(&out, StatusEffectType::Staggered, true), 2, "{name}: control stuns");
        }
    }

    /// Strictly greater (`fcmp s0, s1; b.le`): Poison exactly at the threshold
    /// does not paralyse. ch08 §4.1's suggested test.
    #[test]
    fn poison_exactly_at_the_paralyze_threshold_does_not_paralyse() {
        let now = Instant::now();
        let threshold = super::super::state::paralyze_damage_threshold(1);
        let mut c = combat2(now);
        try_paralyze(&mut c, 0, 1, 1, threshold, false, now);
        assert!(!c.fighters[1].is_paralyzed());
    }

    #[test]
    fn a_hit_exactly_at_the_blind_threshold_does_not_blind() {
        let now = Instant::now();
        let u = uuid_of("Blind");
        let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.damage_to_cause_blind())
            .expect("Blind R1 ships _damageToCauseBlind");
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, threshold, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::Blind, true), 0);
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, threshold + 0.1, false, false, now);
        assert_eq!(op51(&out, StatusEffectType::Blind, true), 2, "control: just over lands");
    }

    /// Blind while already Blind: no second apply (the PvP client would stack a
    /// second instance that our single remove never clears); the timer is
    /// refreshed so the one remove lands at the later end.
    #[test]
    fn a_second_blind_refreshes_instead_of_re_sending() {
        let now = Instant::now();
        let u = uuid_of("Blind");
        let secs = super::super::gamedata::ability_rank_clamped(u, 1)
            .and_then(|r| r.duration())
            .expect("Blind R1 ships a duration");
        let mut c = combat2(now);
        apply_shipped_effects(&mut c, 0, 1, u, 1, 9_999.0, false, false, now);
        assert!(emit_status_removals(&mut c, now).is_empty());
        let again = apply_shipped_effects(&mut c, 0, 1, u, 1, 9_999.0, false, false, at(now, 1.0));
        assert_eq!(op51(&again, StatusEffectType::Blind, true), 0, "no re-send while Blind");
        let mid = emit_status_removals(&mut c, at(now, secs + 0.1));
        assert_eq!(op51(&mid, StatusEffectType::Blind, false), 0, "refreshed: still Blind");
        let end = emit_status_removals(&mut c, at(now, secs + 1.1));
        assert_eq!(op51(&end, StatusEffectType::Blind, false), 2, "one remove at the refreshed end");
    }
}

/// PR-01 of the combat-spec gap register: the op53 / op83 wire fixes (12-D1, 12-D2,
/// M-harrying-op83, M-focusing-dodge-op83). Tracker #227 / #113. Expected values come
/// from the spec chapters and the shipped ability table, never from the code under test.
#[cfg(test)]
mod op53_op83_wire_tests {
    use super::*;
    use super::super::loadout::starter;
    use super::super::state::{
        ActiveSide, EquippedAbility, Fighter, FlowState, MatchCombat, PendingHit,
    };

    const FIREBALL: &str = "d07a8d30-9a1c-49b0-866d-97a8aa1534cf";

    fn live(now: Instant) -> MatchCombat {
        let mut c = MatchCombat::new(2, 2, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            c.fighters.push(Fighter::new(slot, obj, starter(), now));
        }
        c.match_net_object_id = c.alloc_net_object_id();
        c.phase = FlowState::StateTimeout;
        c.round = 1;
        c
    }

    fn uuid_of(editor: &str) -> &'static str {
        super::super::gamedata::ABILITIES
            .iter()
            .find(|a| a.editor_name == editor)
            .map(|a| a.uuid)
            .unwrap_or_else(|| panic!("{editor} missing from the shipped table"))
    }

    /// Cast `uuid` (rank 1) from slot 0 at slot 1 through the real op37 path.
    fn cast(c: &mut MatchCombat, uuid: &str, now: Instant) -> Vec<(usize, Vec<u8>)> {
        let tag = super::super::loadout::ability_tag_for_template(uuid);
        c.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: uuid.to_string(),
            level: 1,
            tag,
        });
        let frame = messages::request_execute_ability(c.fighters[0].net_object_id, uuid);
        let ea = input::parse_execute_ability(&frame).expect("synthesised op37 must parse");
        resolve_ability_cast(c, 0, 1, &frame, &ea, now)
    }

    fn nd(f: &[u8]) -> arena_proto::NetDataParse {
        arena_proto::parse_netdata(&f[2..])
    }

    /// `(viewer, propId 0, propId 4 float)` of every op83 in `out`.
    fn op83s(out: &[(usize, Vec<u8>)]) -> Vec<(usize, i64, f32)> {
        out.iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(83))
            .map(|(v, f)| {
                let n = nd(f);
                let secs = match n.props.get(&4) {
                    Some(arena_proto::NetDataValue::Float(x)) => *x,
                    other => panic!("op83 propId 4 must be a float, got {other:?}"),
                };
                (*v, n.int(0).unwrap_or(-1), secs)
            })
            .collect()
    }

    /// Viewers of every 39 Idle naming `obj`.
    fn idle_39_viewers(out: &[(usize, Vec<u8>)], obj: i32) -> Vec<usize> {
        out.iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(39))
            .filter(|(_, f)| {
                let n = nd(f);
                n.int(0) == Some(obj as i64) && n.int(6) == Some(ActorStateType::Idle as i64)
            })
            .map(|(v, _)| *v)
            .collect()
    }

    fn tick(c: &mut MatchCombat, t: Instant) -> Vec<(usize, Vec<u8>)> {
        let mut out = on_tick(c, t, false);
        out.extend(drain_state_changes(c, t));
        out
    }

    /// 12-D1: a Fireball's op53 goes to both viewers carrying propId 7, a history ring
    /// whose newest entry is Channeling (4) and which is the caster's live ring.
    #[test]
    fn a_spell_cast_op53_carries_the_casters_channeling_history() {
        let now = Instant::now();
        let mut c = live(now);
        let out = cast(&mut c, FIREBALL, now);
        let op53: Vec<_> = out
            .iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(53))
            .collect();
        assert_eq!(op53.len(), 2, "op53 to both viewers");
        for (_, f) in op53 {
            match nd(f).props.get(&7) {
                Some(arena_proto::NetDataValue::ByteArray(b)) => {
                    assert!(b.len() >= 3);
                    assert_eq!(b[0] as usize, b.len() - 3, "count matches the entries");
                    assert_eq!(b.last().copied(), Some(4), "tail = Channeling");
                    assert_eq!(b.as_slice(), c.fighters[0].packed_state_history().as_slice(), "the caster's own ring");
                }
                other => panic!("op53 propId 7 missing or wrong type: {other:?}"),
            }
        }
        // Control: the logical state is untouched (op58-style presentational record).
        assert_eq!(c.fighters[0].actor_state(), ActorStateType::Idle);
    }

    /// 12-D2: the opponent's client leaves Channeling only on a state message
    /// (`PvpOpponentActor$$TryChangeState@0x1964cc8` refuses local changes), so at the
    /// channel's end — Fireball's shipped `_channelDuration` 0.9 s (06 §1.2) — both
    /// viewers get a 39 Idle for the caster, and not before.
    #[test]
    fn a_channel_end_sends_idle_for_the_caster_to_both_viewers() {
        let r = super::super::gamedata::ability_rank_clamped(FIREBALL, 1).unwrap();
        assert_eq!(r.channel_duration(), Some(0.9), "spec 06: Fireball channels 0.9 s");
        let now = Instant::now();
        let mut c = live(now);
        let caster = c.fighters[0].net_object_id;
        let _ = cast(&mut c, FIREBALL, now);
        let _ = drain_state_changes(&mut c, now);

        let early = tick(&mut c, now + Duration::from_millis(850));
        assert!(idle_39_viewers(&early, caster).is_empty(), "no Idle before the channel ends");

        let end = tick(&mut c, now + Duration::from_millis(910));
        let mut viewers = idle_39_viewers(&end, caster);
        viewers.sort();
        assert_eq!(viewers, vec![0, 1], "one 39 Idle per viewer at the channel's end");
        let later = tick(&mut c, now + Duration::from_millis(1500));
        assert!(idle_39_viewers(&later, caster).is_empty(), "sent once");
    }

    /// 12-D2 for the two channels without a windup: Frostbite holds Channeling for its
    /// `_channelMaxLength` 3 s (06 §3.1).
    #[test]
    fn a_frostbite_channel_ends_at_its_channel_max_length() {
        let frostbite = uuid_of("Frostbite");
        let r = super::super::gamedata::ability_rank_clamped(frostbite, 1).unwrap();
        assert_eq!(
            r.get(super::super::gamedata::AbilityField::ChannelMaxLength),
            Some(3.0),
            "spec 06 §3.1: Frostbite channels for 3 s"
        );
        let now = Instant::now();
        let mut c = live(now);
        let caster = c.fighters[0].net_object_id;
        let _ = cast(&mut c, frostbite, now);
        let _ = drain_state_changes(&mut c, now);
        c.fighters[0].reconcile_scheduled_states(now + Duration::from_millis(2900));
        assert!(idle_39_viewers(&drain_state_changes(&mut c, now), caster).is_empty());
        c.fighters[0].reconcile_scheduled_states(now + Duration::from_millis(3010));
        assert_eq!(idle_39_viewers(&drain_state_changes(&mut c, now), caster).len(), 2);
    }

    /// Control for 12-D2: a later state change already took the client out of
    /// Channeling, so the channel's end must not add a spurious Idle on top.
    #[test]
    fn a_state_change_after_op53_cancels_the_channel_end_idle() {
        let now = Instant::now();
        let mut c = live(now);
        let caster = c.fighters[0].net_object_id;
        let _ = cast(&mut c, FIREBALL, now);
        let _ = drain_state_changes(&mut c, now);
        let t = now + Duration::from_millis(100);
        c.fighters[0].apply_stagger_for(t, 3.0);
        let _ = drain_state_changes(&mut c, t);
        c.fighters[0].reconcile_scheduled_states(now + Duration::from_millis(1000));
        let out = drain_state_changes(&mut c, now + Duration::from_millis(1000));
        assert!(idle_39_viewers(&out, caster).is_empty(), "stagger ended the pose: {out:?}");
        assert_eq!(c.fighters[0].actor_state(), ActorStateType::Staggered);
    }

    /// M-harrying-op83 (07-D1, 13-D3): the victim's client learns of the delay only
    /// from op83. Expected: exactly one op83, to the victim alone, naming the victim's
    /// avatar and carrying rank 1's shipped `_cooldownIncrease` 2.5 s — a value the
    /// retail captures carry.
    #[test]
    fn a_harrying_bash_sends_op83_to_the_victim() {
        let harrying = uuid_of("HarryingBash");
        let r = super::super::gamedata::ability_rank_clamped(harrying, 1).unwrap();
        assert_eq!(r.get(super::super::gamedata::AbilityField::CooldownIncrease), Some(2.5));
        let now = Instant::now();
        let mut c = live(now);
        let victim = c.fighters[1].net_object_id as i64;
        let mut out = cast(&mut c, harrying, now);
        for ms in (10..=2000).step_by(10) {
            out.extend(tick(&mut c, now + Duration::from_millis(ms)));
        }
        assert_eq!(op83s(&out), vec![(1, victim, 2.5)], "one op83, victim only, +2.5 s");
    }

    /// Control: a bash that does not harry sends no op83.
    #[test]
    fn a_shield_bash_sends_no_op83() {
        let now = Instant::now();
        let mut c = live(now);
        let mut out = cast(&mut c, uuid_of("ShieldBash"), now);
        for ms in (10..=2000).step_by(10) {
            out.extend(tick(&mut c, now + Duration::from_millis(ms)));
        }
        assert!(
            out.iter().any(|(_, f)| messages::user_message_gmid(f) == Some(58)),
            "fixture: the bash must be cast"
        );
        assert!(op83s(&out).is_empty());
    }

    fn swing_into(c: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
        c.pending_hits.push(PendingHit {
            sender: 1,
            target: 0,
            side: ActiveSide::Right,
            swing_factor: 1.0,
            combo_count: 0,
            due: now,
        });
        land_due_hits(c, now + Duration::from_millis(1))
    }

    /// M-focusing-dodge-op83 (04-DG8, 07-D8): a connected Focusing Dodge's refund
    /// reaches the dodger only as op83 with a NEGATIVE amount. Its magnitude here is the
    /// server's current flat payout, rank 1's shipped `_maximumCooldownReduction`.
    #[test]
    fn a_connected_focusing_dodge_sends_a_negative_op83_to_the_dodger() {
        let focusing = uuid_of("FocusingDodge");
        let cut = super::super::gamedata::ability_rank_clamped(focusing, 1)
            .and_then(|r| r.get(super::super::gamedata::AbilityField::MaximumCooldownReduction))
            .expect("Focusing Dodge ships a cooldown reduction");
        assert!(cut > 0.0);
        let now = Instant::now();
        let mut c = live(now);
        let dodger = c.fighters[0].net_object_id as i64;
        let _ = apply_shipped_effects(&mut c, 0, 1, focusing, 1, 0.0, false, false, now);
        let out = swing_into(&mut c, now);
        assert_eq!(op83s(&out), vec![(0, dodger, -cut)], "one op83 to the dodger, -cut");
    }

    /// Control: a plain Dodging Strike ships no cooldown reduction and sends no op83.
    #[test]
    fn a_connected_dodging_strike_sends_no_op83() {
        let now = Instant::now();
        let mut c = live(now);
        let _ = apply_shipped_effects(&mut c, 0, 1, uuid_of("DodgingStrike"), 1, 0.0, false, false, now);
        let out = swing_into(&mut c, now);
        assert!(
            out.iter().any(|(_, f)| matches!(messages::user_message_gmid(f), Some(50) | Some(66))),
            "fixture: the swing must resolve"
        );
        assert!(op83s(&out).is_empty());
    }
}
