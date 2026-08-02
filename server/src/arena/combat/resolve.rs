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
//! corrected **asymmetric block** (optimal: phys ×0 / elem ×0.5; late: ÷1.6 / ÷1.23),
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
//! Still to wire: the `swingFactor` magnitude from the c2s body;
//! per-element DoT TICK damage (the
//! conditioning land + threshold is wired; the periodic StatusEffect-source ReceiveDamage
//! tick is the remaining piece); and routing real ability UUIDs to Ward/Absorb/Paralyze
//! casts (the casts push pools / run the threshold; the per-ability recognition is TODO).

use std::time::{Duration, Instant};

use log::{debug, info};

use super::damage::{DamageModel, ResolvedDamage, RetailDamageModel};
use super::input;
use super::messages;
use super::messages_state::{self, StateFrame};
use super::state::{
    ActiveSide, ActorStateType, DamageSource, FlowState, MatchCombat, MatchState, NetObjectType,
};
use super::tables;

/// Carrier MessageType (`user_data[1]`) of the combat-input family — `0x36` (54).
const CARRIER_USERMESSAGE: u8 = 0x36;
/// Carrier for `PlayerCombatInputActivate` (op46) — `0x2e` (46). Op46 uses its own
/// carrier byte (`GameMessageId` value), NOT the generic `0x36` UserMessage carrier.
const CARRIER_OP46: u8 = 0x2e;

/// The minimum spacing between committed swings for `fighter` — **Phase 3.12**: the
/// equipped weapon's own `attackDelay + recoveryTime`, floored at
/// `PlayerCombatParameters.globalMinimumAttackDelay` (0.1 s).
///
/// This replaces the guessed per-weight-class table (`Weight::swing_interval`, a flat
/// 400/650/900 ms). A Dragonbone Dagger now swings every 0.783 s, an Iron Warhammer
/// every 1.35 s — from the shipped `WeaponTemplateList`, per template, not per class.
fn swing_cooldown_for(fighter: &super::state::Fighter) -> Duration {
    fighter.loadout.swing_interval()
}

/// Held-charge crit swing multiplier for a **Light** weapon (dagger) — `×1.325`.
/// From `docs/arena-combat-actions.md` / `tables::Weight::Light.crit_combo().0`.
/// Applied when the server-measured attack hold ≥ `CRITICAL_HOLD_SECS`.
const CRIT_FACTOR_LIGHT: f32 = 1.325;

/// Held-charge crit swing multiplier for a **Heavy** weapon — `×1.987`.
/// From `docs/arena-combat-actions.md` / `tables::Weight::Heavy.crit_combo().0`.
const CRIT_FACTOR_HEAVY: f32 = 1.987;

/// Held-charge crit swing multiplier for a **Versatile** weapon — `×1.625`.
/// From `tables::Weight::Versatile.crit_combo().0`.
const CRIT_FACTOR_VERSATILE: f32 = 1.625;

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
/// **CALIBRATION FLAG** — set this const once `MinDamageTime`/`MaxDamageTime` are
/// captured from the CDN WeaponTemplate assets.
const CRITICAL_HOLD_SECS: f32 = 1.2;

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

/// One decoded `PlayerCombatInputPosition` (gmid 47) pointer sample.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PointerSample {
    /// Normalised screen X (propId 4).
    x: f32,
    /// Normalised screen Y (propId 5).
    y: f32,
    /// Client-reported charge seconds latched at propId 7 (telemetry only).
    client_charge: Option<f32>,
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
///   - `CRIT_FACTOR_*` when `hold_secs >= CRITICAL_HOLD_SECS` (full charge / Critical
///     or PostCriticalDecay state — the server-side equivalent of op45 reporting ≥3).
///   - `1.0` for a partial hold (uncharged swing, no crit).
///
/// Light/Heavy/Versatile multipliers come from `tables::Weight::crit_combo().0`.
fn charge_crit_factor(fighter: &super::state::Fighter, hold_secs: f32) -> f32 {
    if hold_secs < CRITICAL_HOLD_SECS {
        return 1.0;
    }
    // Full charge: pick multiplier by weapon class.
    match fighter.loadout.weapon.weight {
        Some(tables::Weight::Light) => CRIT_FACTOR_LIGHT,
        Some(tables::Weight::Heavy) => CRIT_FACTOR_HEAVY,
        Some(tables::Weight::Versatile) => CRIT_FACTOR_VERSATILE,
        // Default to Light if weight not set (the calibration target's class).
        None => CRIT_FACTOR_LIGHT,
    }
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
    //   - hold ≥ CRITICAL_HOLD_SECS → full charge → swing_factor = CRIT_FACTOR_* by weapon class
    //   - hold < CRITICAL_HOLD_SECS → partial / uncharged → swing_factor = 1.0
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
                // Broadcast op45 PlayerChargingStateChange — THE CHARGE/COMBO CIRCLE.
                // Retail sends this on every charge (13,060 captured frames); we sent
                // it zero times, so a plain swing showed no circle at all while an
                // ability incidentally produced one. No damage on press — this is
                // purely the actor-state broadcast.
                let side = classified_side_for(&combat.fighters[sender], now)
                    .unwrap_or(ActiveSide::Right);
                let own = combat.fighters[sender].packed_stats();
                let opp = combat
                    .opponent_of(sender)
                    .and_then(|o| combat.fighters.get(o))
                    .map(|f| f.packed_stats())
                    .unwrap_or(0);
                let obj = combat.fighters[sender].net_object_id;
                let charging = messages::player_charging_state_change(obj, own, opp, side);
                // To BOTH viewers: the charging player needs its own circle, and the
                // opponent needs to see the wind-up.
                let mut out = vec![(sender, charging.clone())];
                if let Some(o) = combat.opponent_of(sender) {
                    out.push((o, charging));
                }
                return out;
            }
            Some(false) => {
                // Button-UP (commit): compute hold duration, apply crit.
                let hold_secs = combat.fighters[sender]
                    .charge_press_at
                    .map(|t| now.duration_since(t).as_secs_f32())
                    .unwrap_or(0.0);
                // Reset press timestamp — this charge is consumed.
                combat.fighters[sender].charge_press_at = None;
                let swing_factor = charge_crit_factor(&combat.fighters[sender], hold_secs);
                let is_crit = swing_factor > 1.0;
                if is_crit {
                    info!(
                        "combat: slot {sender} op46 UP — hold {hold_secs:.3}s ≥ {CRITICAL_HOLD_SECS}s threshold \
                         → CRIT ×{swing_factor:.3} (weapon {:?})",
                        combat.fighters[sender].loadout.weapon.weight,
                    );
                } else {
                    debug!(
                        "combat: slot {sender} op46 UP — hold {hold_secs:.3}s < {CRITICAL_HOLD_SECS}s \
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
                    debug!("combat: slot {sender} op46 UP ignored — paralysed");
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
        debug!("combat: slot {sender} input ignored — target slot {target_slot} already dead");
        return Vec::new();
    }
    // A PARALYZED sender can't act — its inputs are locked for the paralyse duration
    // (`ActorParalyzedState`, §5.4). Handshake/block frames were already handled above;
    // this drops only the combat swing/ability of a paralysed attacker.
    if combat.fighters[sender].is_paralyzed() {
        debug!("combat: slot {sender} input ignored — paralysed (inputs locked)");
        return Vec::new();
    }
    // A STAGGERED sender can't act either, for `baseStaggerDuration` (1.5 s). [Phase 3.13]
    if combat.fighters[sender].is_staggered(now) {
        debug!("combat: slot {sender} input ignored — staggered");
        return Vec::new();
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
            // Button DOWN — start the server's charge stopwatch. No damage.
            f.charge_press_at = Some(now);
            debug!(
                "combat: slot {sender} op46 DOWN (carrier 0x36) — charge press recorded \
                 (blockZone={:?})",
                act.block_zone
            );
            return Vec::new();
        }
        // Button UP — commit the swing.
        let hold_secs = combat.fighters[sender]
            .charge_press_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0);
        combat.fighters[sender].charge_press_at = None;
        // Server measurement is authoritative; the client's number is only compared.
        charge_cross_check(sender, hold_secs, act.client_charge);
        let swing_factor = charge_crit_factor(&combat.fighters[sender], hold_secs);
        if combat.fighters[target_slot].is_dead() {
            return Vec::new();
        }
        let side = classified_side_for(&combat.fighters[sender], now);
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

/// A weapon auto-attack (committed swing), throttled per attacker.
///
/// `swing_factor` is the held-charge crit multiplier:
///   - `1.0` for a normal (partial / uncharged) swing via carrier-0x36 or bot swings.
///   - `CRIT_FACTOR_*` for a full-charge crit dispatched from the op46 (0x2e) path
///     when the server-measured hold ≥ `CRITICAL_HOLD_SECS` (bug 4 fix).
fn resolve_swing_with_side(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    swing_factor: f32,
    decoded_side: Option<ActiveSide>,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    let cooldown = swing_cooldown_for(&combat.fighters[sender]);
    if let Some(last) = combat.fighters[sender].last_swing {
        if now.duration_since(last) < cooldown {
            debug!("combat: slot {sender} swing throttled (< {cooldown:?} since last, weapon cadence)");
            return Vec::new();
        }
    }
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
    // AutoAttack → FollowThrough → Recovery → Idle so BOTH clients animate it. Runs
    // after `register_combo_swing` so `last_combo_side` is this swing's side, and
    // before damage so the wind-up precedes the hit on the wire, as in retail.
    begin_swing_animation(combat, sender, now);

    let attacker_loadout = combat.fighters[sender].loadout.clone();
    let resolved = RetailDamageModel.resolve_attack(
        &attacker_loadout,
        &combat.fighters[target_slot],
        DamageSource::Attack,
        next_side,
        swing_factor,
        combo_count,
        now,
    );
    // A connected OPTIMAL block on the target RESETS the attacker's combo (§4.2: a block
    // breaks the chain — the next swing starts fresh at ×1.0).
    if resolved.flags & super::damage::flags::WAS_OPTIMAL_BLOCKING != 0 {
        combat.fighters[sender].reset_combo();
    }
    emit_damage(combat, sender, target_slot, &resolved, now)
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
fn resolve_ability_cast(
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
        .unwrap_or((1, super::state::AbilityTag::Generic));

    // Resource gate (spec §1, bug 2): reject the cast (no effect, no cooldown set,
    // no damage) if the caster lacks the required stamina (maneuvers) or magicka
    // (spells).  `ability_cost` returns APK-authoritative costs; unknown UUIDs
    // return (0,0) — no gate applies (backward-compatible: unrecognised spells still
    // fire). The rank (1-based, from the equipped level) drives the linear cost ramp.
    let (stam_cost, mag_cost) = tables::ability_cost(&ea.ability_uuid, level);
    if stam_cost > 0 && combat.fighters[sender].stamina < stam_cost {
        debug!(
            "combat: slot {sender} ability {} REJECTED — insufficient stamina ({} < {} required)",
            ea.ability_uuid, combat.fighters[sender].stamina, stam_cost,
        );
        return Vec::new(); // no cooldown set; client retries when stamina is up
    }
    if mag_cost > 0 && combat.fighters[sender].magicka < mag_cost {
        debug!(
            "combat: slot {sender} ability {} REJECTED — insufficient magicka ({} < {} required)",
            ea.ability_uuid, combat.fighters[sender].magicka, mag_cost,
        );
        return Vec::new();
    }

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
        let frame = messages::player_stats_update(obj_id, packed);
        (0..combat.fighters.len()).map(|s| (s, frame.clone())).collect()
    } else {
        Vec::new()
    };

    let mut out = Vec::new();
    // PerformExecuteAbility (38) echo to both — the cast confirmation/visual.
    let perform = messages::perform_execute_ability(user_data, ea.sep_offset);
    out.push((sender, perform.clone()));
    out.push((target_slot, perform));

    // Emit the stat update (after the cast echo so the client sees the visual before
    // the bar drop — matches retail ordering).
    out.extend(stat_frames);

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
    // `_channelDuration`), and the unmodelled propId-7 blob is deliberately omitted
    // rather than fabricated.
    let channel_secs = super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16)
        .and_then(|r| r.channel_duration())
        .unwrap_or(0.0);
    let channeling = messages::player_channeling_state_change(
        combat.fighters[sender].net_object_id,
        combat.fighters[sender].packed_stats(),
        combat.fighters[target_slot].packed_stats(),
        channel_secs,
        &ea.ability_uuid,
        None, // propId 7: unmodelled in the corpus — omitted, never invented
    );
    out.push((sender, channeling.clone()));
    out.push((target_slot, channeling));

    debug!("combat: slot {sender} casts ability {} (tag {tag:?}, level {level}) → slot {target_slot}", ea.ability_uuid);

    // Phase 3.11: route on the FULL shipped ability table. Ward/Absorb/ResistElements
    // are self-buffs (no direct damage); Paralyze/Damage/Maneuver deal the rank's own
    // `_damage`; Perks never activate.
    use super::state::AbilityTag;
    match tag {
        AbilityTag::Ward => out.extend(apply_ward(combat, sender, level, now)),
        AbilityTag::Absorb => out.extend(apply_absorb(combat, sender, level, now)),
        AbilityTag::ResistElements => out.extend(apply_resist_elements(combat, sender, level, now)),
        AbilityTag::Perk => {}
        AbilityTag::Paralyze | AbilityTag::Damage | AbilityTag::Maneuver | AbilityTag::Generic => {
            let resolved = RetailDamageModel.resolve_ability(
                &ea.ability_uuid,
                level,
                &combat.fighters[target_slot],
                ActiveSide::Middle,
                now,
            );
            out.extend(emit_damage(combat, sender, target_slot, &resolved, now));
            // A landed Paralyze also carries its own paralyse threshold + duration
            // (`_damageToCauseParalyze` / `_duration`), applied by
            // `apply_status_conditioning` via the caster's `paralyze_rank`.
            if tag == AbilityTag::Paralyze {
                out.extend(try_paralyze(combat, sender, target_slot, level, now));
            }
            // A maneuver rank that defines `_damageToCauseStagger` staggers the
            // target when the hit lands hard enough. [Phase 3.13]
            if let Some(threshold) = super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16)
                .and_then(|r| r.damage_to_cause_stagger())
            {
                if resolved.total >= threshold && !combat.fighters[target_slot].is_dead() {
                    combat.fighters[target_slot].apply_stagger(now);
                    let obj = combat.fighters[target_slot].net_object_id;
                    let frame = messages::change_combat_status_effect(
                        obj,
                        true,
                        super::state::StatusEffectType::Staggered,
                        super::state::BASE_STAGGER_DURATION_SECS,
                        0,
                    );
                    for slot in 0..combat.fighters.len() {
                        out.push((slot, frame.clone()));
                    }
                }
            }
        }
    }
    out
}

/// Land `Paralyzed` on `target_slot` when the caster's Paralyze rank says the hit is
/// strong enough. The threshold is the **absolute** shipped `_damageToCauseParalyze`
/// (32.7 @ R1) checked against the target's accumulated poison in the sliding window,
/// and the lock lasts the rank's own `_duration` (2.0 s @ R1). [Phase 3.9]
fn try_paralyze(
    combat: &mut MatchCombat,
    _caster: usize,
    target_slot: usize,
    rank: u8,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    use super::state::{ActorStateType, DamageType, StatusEffectType};
    let mut out = Vec::new();
    if !combat.fighters[target_slot].can_be_paralyzed
        || combat.fighters[target_slot].actor_state() == ActorStateType::Paralyzed
    {
        return out;
    }
    let recent = combat.fighters[target_slot].recent_element_damage(DamageType::Poison);
    let threshold = super::state::paralyze_damage_threshold(rank);
    if recent < threshold {
        return out;
    }
    let secs = super::state::paralyze_duration_secs(rank);
    let f = &mut combat.fighters[target_slot];
    f.set_actor_state(ActorStateType::Paralyzed, now);
    f.clear_scheduled_states();
    f.blocking_until = None;
    let obj = f.net_object_id;
    info!("combat: slot {target_slot} PARALYZED (poison {recent:.1} ≥ {threshold:.1}) for {secs}s");
    let frame = messages::change_combat_status_effect(obj, true, StatusEffectType::Paralyzed, secs, 0);
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
    if neg.negated {
        let defender_obj = combat.fighters[target_slot].net_object_id;
        if neg.heal > 0.0 {
            let f = &mut combat.fighters[target_slot];
            f.health = (f.health + neg.heal.round() as u32).min(f.max_health);
        }
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
    combat.fighters[target_slot].take_damage(total.round().max(0.0) as u32);
    let hp_after = combat.fighters[target_slot].health;
    // Per-hit damage-vs-maxHP ratio (info-level so the ghost-verify on the box shows the
    // before→after HP without RUST_LOG=debug). NOTE: the 25% one-shot clamp is GONE for
    // arena — deep-combo hits are *earned* and can legitimately be large (§4.5).
    let pct = if max_hp > 0 { 100.0 * total / max_hp as f32 } else { 0.0 };
    let dealt = hp_before.saturating_sub(hp_after);
    info!(
        "combat damage: slot {attacker_slot} → slot {target_slot} | source {:?} side {:?} | total {total:.1} = {pct:.1}% of {max_hp} maxHP | HP {hp_before} → {hp_after} (−{dealt})",
        resolved.source,
        resolved.active_side,
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
            0,
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

    if combat.fighters[target_slot].is_dead() {
        out.extend(on_round_ending_death(combat, attacker_slot));
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
/// mirrored Stamina/Magicka drain, not a damage-over-time. The old flat
/// `DOT_PERCENT_HEALTH_PER_TICK = 0.003` was 6.7× too small for Fire/Poison **and**
/// wrongly nonzero for Frost/Shock.
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

/// Regen tick cadence. We regen once per second and apply the video-ground-truth per-
/// second rates. A fractional tick (e.g. regen ~31 stamina/s from a 625 pool at L86)
/// is rounded to nearest integer to avoid float drift.
const REGEN_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// In-combat stamina/magicka regen rate as a fraction of the pool per second.
/// **Video ground-truth (s293)**: stamina and magicka both recover at ~5 %/s during
/// passive recovery phases (t=50..52 clean window: 5%→10%→15% over 2s).  The earlier
/// UESP figure of 4 %/s was slightly low; 5 %/s matches the observed HUD data.
/// Rates are CDN `[ExcelVariable]` (`PlayerStats._staminaRegenRate` / `_magickaRegenRate`);
/// 5 %/s is the video-pinned value and supersedes the UESP 4 %/s estimate.
/// [ground-truth: /tmp/arena-video-groundtruth.md §1; calibration flag]
const STAMINA_REGEN_RATE_PER_S: f32 = 0.05;
const MAGICKA_REGEN_RATE_PER_S: f32 = 0.05;

/// In-combat health regen: **ZERO** — video ground-truth (s293) shows NO passive HP
/// recovery during a fight; health only changes on hits.  Between rounds the server
/// already calls `reset_fighters_for_next_round` (full HP reset), so no in-round regen
/// is needed.  The old UESP-derived 0.5 %/s figure was wrong for arena PvP.
/// `BlockHealthRegen` status suppression is kept (still correct to gate any future
/// out-of-arena regen path).
/// [ground-truth: §1 "Health regen: 0 in-round; full reset between rounds"]
const HEALTH_REGEN_RATE_PER_S: f32 = 0.0; // NO in-round health regen (video-proven)

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
    use super::state::{condition_for_element, ActorStateType, DamageType, StatusEffectType};

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
                let max_hp = combat.fighters[target_slot].max_health;
                // Phase 3.8: the PER-ELEMENT shipped `_percentHealthDamage`
                // (Fire/Poison 0.02, Frost/Shock 0.0 — they are control statuses).
                let per_tick = dot_percent_health(*ty) * max_hp as f32;
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
                    target_obj, true, condition, CONDITION_DURATION_SECS, 0,
                );
                debug!("combat: slot {target_slot} CONDITION {condition:?} landed ({recent:.0} ≥ {threshold:.0} window poison/elem)");
                for slot in 0..combat.fighters.len() {
                    out.push((slot, frame.clone()));
                }
            }

            // PARALYSE (poison only): the absolute poison threshold layered on top —
            // gated by can_be_paralyzed (player) + the defender's poison resist /
            // Fortify-Poisoned / Ward (all already folded into `recent` via mitigation
            // + into `threshold` via Fortify; Ward eats poison so it never accumulates).
            // **Phase 3.9:** the threshold is the shipped, ABSOLUTE
            // `ParalyzeAbility._damageToCauseParalyze` (32.7 @ R1) — not a fraction of
            // max HP — and the lock lasts the rank's own `_duration` (2.0 s @ R1).
            if *ty == DamageType::Poison && combat.fighters[target_slot].can_be_paralyzed {
                // The ATTACKER's Paralyze rank selects the row (0 → the R1 default).
                let rank = combat
                    .opponent_of(target_slot)
                    .and_then(|s| combat.fighters.get(s))
                    .map(|f| f.loadout.paralyze_rank)
                    .unwrap_or(0);
                let paralyze_threshold = super::state::paralyze_damage_threshold(rank);
                let secs = super::state::paralyze_duration_secs(rank);
                let not_already_paralyzed =
                    combat.fighters[target_slot].actor_state() != ActorStateType::Paralyzed;
                if recent >= paralyze_threshold && not_already_paralyzed {
                    let f = &mut combat.fighters[target_slot];
                    f.set_actor_state(ActorStateType::Paralyzed, now); // locks inputs (is_paralyzed)
                    f.clear_scheduled_states();
                    f.blocking_until = None; // paralysed → guard drops
                    f.paralyze_secs = secs;
                    let frame = messages::change_combat_status_effect(
                        target_obj, true, StatusEffectType::Paralyzed, secs, 0,
                    );
                    info!("combat: slot {target_slot} PARALYZED (poison {recent:.1} ≥ {paralyze_threshold:.1}) for {secs}s");
                    for slot in 0..combat.fighters.len() {
                        out.push((slot, frame.clone()));
                    }
                }
            }
        }
    }
    out
}

/// Clear a lapsed `Paralyzed` actor-state back to Idle once the paralyse duration
/// (`PARALYZE_DURATION_SECS`) has elapsed since it was applied (`state_entered`) — so a
/// paralysed fighter regains its inputs. (The client also times the status out via the
/// op51 duration; the un-paralyse op51 *remove* is a cosmetic nicety not emitted here —
/// the apply carried the duration.) No-op for a non-paralysed fighter.
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

/// Drive DoT ticks for all active elemental conditions on all fighters. For each
/// `Burning/Frozen/Enervated/Poisoned` `ActiveEffect` whose `last_tick` is ≥
/// `DOT_TICK_INTERVAL` ago, emit a `ReceiveDamage` with `DamageSource::StatusEffect`
/// and the condition's elemental type. Multiple concurrent instances of the SAME
/// element tick INDEPENDENTLY (stack, do not refresh). Expired effects are dropped.
/// Returns `(target_slot, frame)` pairs — one `ReceiveDamage` per eligible tick.
///
/// **DoT tick magnitude**: `per_tick_damage` (= `_percentHealthDamage × maxHP`),
/// game-data-driven at `DOT_PERCENT_HEALTH_PER_TICK`. [§Mechanic-4 calibration flag]
fn apply_dot_ticks(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageSource as DS, StatusEffectType};
    let mut out = Vec::new();

    for slot in 0..combat.fighters.len() {
        // Prune expired effects.
        combat.fighters[slot].effects.retain(|e| now < e.expires_at);
        // Prune expired transient resistances.
        combat.fighters[slot].prune_transient_resistances(now);

        let opp_slot = combat.fighters[slot].arena_target;
        if combat.fighters[slot].is_dead() {
            continue;
        }

        // Collect ticking DoT indices + their damage so we can split the borrow.
        let ticking: Vec<(usize, f32, super::state::DamageType)> = combat.fighters[slot]
            .effects
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.effect,
                    StatusEffectType::Burning
                        | StatusEffectType::Frozen
                        | StatusEffectType::Enervated
                        | StatusEffectType::Poisoned
                ) && now.duration_since(e.last_tick) >= DOT_TICK_INTERVAL
            })
            .map(|(i, e)| (i, e.per_tick_damage, e.damage_type))
            .collect();

        for (idx, tick_dmg, dmg_type) in ticking {
            // Update last_tick on the effect.
            combat.fighters[slot].effects[idx].last_tick = now;

            if tick_dmg <= 0.0 {
                continue;
            }

            let hp_before = combat.fighters[slot].health;
            let max_hp = combat.fighters[slot].max_health;
            combat.fighters[slot].take_damage(tick_dmg.round().max(0.0) as u32);
            let hp_after = combat.fighters[slot].health;
            let pct = if max_hp > 0 { 100.0 * tick_dmg / max_hp as f32 } else { 0.0 };
            debug!(
                "combat DoT: slot {slot} {dmg_type:?} tick {tick_dmg:.2} ({pct:.2}% maxHP) | HP {hp_before}→{hp_after}"
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
                super::damage::flags::SHOW_DAMAGE, // no HAS_ATTACKER for DoT
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
                out.extend(on_round_ending_death(combat, opp_slot));
                break;
            }
        }
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
        messages::change_combat_status_effect(target_obj, true, StatusEffectType::Ward, ward_duration, 0);
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
    });
    let obj = f.net_object_id;
    info!("combat: slot {caster_slot} ABSORB r{rank} applied (pool {amount:.2}, heal ×{restoration}, {duration}s)");
    let frame =
        messages::change_combat_status_effect(obj, true, StatusEffectType::Absorb, duration, 0);
    for slot in 0..combat.fighters.len() {
        out.push((slot, frame.clone()));
    }
    out
}

/// Apply Resist-Elements to `caster_slot`: push four transient elemental resistances
/// (Fire/Frost/Shock/Poison) with 11.5s duration and emit four op51
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
        let frame =
            messages::change_combat_status_effect(target_obj, true, effect_ty, resist_duration, 0);
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
fn on_round_ending_death(combat: &mut MatchCombat, winner: usize) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let loser = combat.opponent_of(winner).unwrap_or(winner);
    // **Phase 3.14 — DOUBLE-KO.** Both fighters at 0 HP in the same resolution step:
    // nobody scores, the round is replayed. AUTHORED, not capture-derived — no
    // recorded match ends this way, so this is a designed rule.
    let double_ko = combat.round_outcome() == super::state::RoundOutcome::DoubleKo;
    if double_ko {
        info!("combat: DOUBLE-KO — both fighters at 0 HP, round is replayed (no score)");
    } else if winner < combat.rounds_won.len() {
        combat.rounds_won[winner] += 1;
    }
    let match_won = combat.match_is_won();
    let loser_obj = combat.fighters.get(loser).map(|f| f.net_object_id).unwrap_or(0);
    let winner_obj = combat.fighters.get(winner).map(|f| f.net_object_id).unwrap_or(0);
    let loser_stats = combat.fighters.get(loser).map(|f| f.packed_stats()).unwrap_or(0);
    let winner_stats = combat.fighters.get(winner).map(|f| f.packed_stats()).unwrap_or(0);

    // 1) op29 PlayerDead for the loser. Carrier 0x36, props 0-6 (NetObjectInfo + the
    //    two packed-stats ULongs + a cause byte). Cause = WeaponManeuver(3), the s506
    //    final-blow value. [capture-proven layout — supersedes the old bare guess.]
    let dead_frame = messages::player_dead(loser_obj, loser_stats, winner_stats, DamageSource::WeaponManeuver as u8);
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

    if match_won {
        // Final round → walk the terminal match-end states next.
        combat.winner = Some(winner);
        combat.matchend_step = 0;
        combat.phase = FlowState::RoundEnd;
        info!(
            "combat: MATCH-ending death → winner slot {winner} (obj {winner_obj}) won the match \
             (score {:?}); emitting op29 + op79 RoundEnd + op48 + MatchState→PostRound(14) to {} player(s); \
             engine tick now walks PostRound→BackendMatchEnd→PostMatch→Disconnecting",
            combat.rounds_won,
            combat.fighters.len(),
        );
    } else {
        // Non-final round → loop to the next round (best-of-3). The engine's NextState
        // branch walks ChooseLoadout(8)→…→InRound(13) + resets HP + re-enters the round.
        combat.interround_step = 0;
        combat.phase = FlowState::NextState;
        info!(
            "combat: round-ending death (round {}) → winner slot {winner} (obj {winner_obj}), loser slot {loser} \
             (obj {loser_obj}); score {:?} (no fighter at {} wins yet) — LOOPING to the next round; \
             emitting op29 + op79 RoundEnd + op48 + MatchState→PostRound(14), then the engine walks \
             ChooseLoadout(8)→…→InRound(13) and resets both fighters to full HP",
            combat.round,
            combat.rounds_won,
            super::state::ROUND_WINS_TO_WIN_MATCH,
        );
    }
    debug!("combat op29 PlayerDead {} bytes: {}", dead_frame.len(), hex(&dead_frame));
    debug!("combat op48 result {} bytes: {}", result_frame.len(), hex(&result_frame));

    for slot in 0..combat.fighters.len() {
        out.push((slot, dead_frame.clone()));
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
fn apply_regen_tick(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    use super::state::StatusEffectType;

    let mut out = Vec::new();

    for slot in 0..combat.fighters.len() {
        let f = &mut combat.fighters[slot];
        if f.is_dead() {
            continue;
        }

        // Check which regen channels are suppressed by active status effects.
        // BlockHealthRegen(50) is kept for future use but has no effect (rate = 0.0).
        let block_stam = f.effects.iter().any(|e| {
            e.effect == StatusEffectType::BlockStaminaRegen && now < e.expires_at
        });
        let block_mag = f.effects.iter().any(|e| {
            e.effect == StatusEffectType::BlockMagickaRegen && now < e.expires_at
        });

        let before_s = f.stamina;
        let before_m = f.magicka;

        // Health regen: NONE in-round (HEALTH_REGEN_RATE_PER_S = 0.0).
        // Video ground-truth: HP only changes on hits; full reset happens between rounds.

        // Stamina regen: 5% of pool per second (video-pinned, s293 §1).
        if !block_stam && f.stamina < f.max_stamina {
            let regen = ((STAMINA_REGEN_RATE_PER_S * f.max_stamina as f32).round() as u32).max(1);
            f.stamina = (f.stamina + regen).min(f.max_stamina);
        }
        // Magicka regen: 5% of pool per second (video-pinned, s293 §1).
        if !block_mag && f.magicka < f.max_magicka {
            let regen = ((MAGICKA_REGEN_RATE_PER_S * f.max_magicka as f32).round() as u32).max(1);
            f.magicka = (f.magicka + regen).min(f.max_magicka);
        }

        let changed = f.stamina != before_s || f.magicka != before_m;
        if changed {
            f.stats_seq = f.stats_seq.wrapping_add(1);
            let packed = f.packed_stats();
            let obj_id = f.net_object_id;
            let frame = messages::player_stats_update(obj_id, packed);
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

/// Delay from `PlayerAutoAttackStateChange` (52) to `PlayerFollowThroughStateChange`
/// (43). **Capture-pinned**: the measured 52→43 gaps in retail are 49, 49, 49, 53 and
/// 65 ms, and the 43 frame's own `_timeInPreviousState` is 0.050 — the message states
/// its own delay, and the two agree.
const FOLLOW_THROUGH_DELAY: Duration = Duration::from_millis(50);

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
/// * `PlayerAutoAttack` / `PlayerFollowThrough` / `PlayerRecovery` → 52 / 43 / 44, the
///   three beats of a swing;
/// * `PlayerDraining` → 42;
/// * everything else → 39, the generic member. That includes `Idle`, which is how a
///   block **ends**: there is no shield-down variant of 41 (all 248 decoded 41 frames
///   carry prop6 = Blocking), so the guard comes down with a 39 carrying stateId 0.
///
/// `Charging` is absent on purpose: gmid 45 is emitted directly on the op46 button-DOWN
/// path, which is capture-pinned, and nothing sets the `Charging` actor state.
pub fn drain_state_changes(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    let viewers = combat.fighters.len();
    let mut out = Vec::new();
    for slot in 0..viewers {
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
    }
    let _ = now;
    out
}

/// Walk the attacker through the three beats of a swing.
///
/// `PlayerAutoAttack` now, then `PlayerFollowThrough` and `PlayerRecovery` on the
/// capture-measured delays, then back to `Idle` when the weapon's own cadence is up
/// (which is exactly when the next swing becomes legal). The transitions land on the
/// outbox; [`drain_state_changes`] puts them on the wire.
///
/// Retail's per-session counts corroborate one of each per swing: s503 sent 330 × gmid
/// 52, 325 × 43 and 291 × 44 — near-1:1, with 44 slightly lower because a swing that
/// is interrupted never reaches recovery.
fn begin_swing_animation(combat: &mut MatchCombat, slot: usize, now: Instant) {
    let cadence = swing_cooldown_for(&combat.fighters[slot]);
    let f = &mut combat.fighters[slot];
    f.set_actor_state(ActorStateType::PlayerAutoAttack, now);
    f.schedule_state(now + FOLLOW_THROUGH_DELAY, ActorStateType::PlayerFollowThrough);
    f.schedule_state(
        now + FOLLOW_THROUGH_DELAY + RECOVERY_DELAY,
        ActorStateType::PlayerRecovery,
    );
    // Idle at the end of the weapon's cadence — never earlier than the recovery beat
    // it must follow, so a very fast weapon still walks the states in order.
    let idle_at = (now + cadence).max(now + FOLLOW_THROUGH_DELAY + RECOVERY_DELAY * 2);
    f.schedule_state(idle_at, ActorStateType::Idle);
}

/// A bot fighter's auto-swing cadence. Slower than a human's `SWING_COOLDOWN` so the
/// player wins comfortably but sees real incoming damage — a fight, not a static dummy.
const BOT_SWING_COOLDOWN: Duration = Duration::from_millis(1800);

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
        // Advance any in-flight swing: AutoAttack → FollowThrough → Recovery → Idle.
        // The tick is the ONLY thing that moves it for a player who stops sending
        // input mid-swing, so this must run here as well as on the input path.
        f.reconcile_scheduled_states(now);
    }
    let mut out = Vec::new();

    // DoT ticks — one tick per second per active condition instance, independent of
    // whether a bot or player is the source. Runs BEFORE bot swings so a DoT killing
    // blow is processed before the bot's turn. [§Mechanic-2]
    out.extend(apply_dot_ticks(combat, now));
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
        let ready = combat.fighters[bot]
            .last_swing
            .map(|t| now.duration_since(t) >= BOT_SWING_COOLDOWN)
            .unwrap_or(true);
        if ready {
            // Bots don't charge — always ×1.0 (no held-charge crit for bot swings).
            out.extend(resolve_swing(combat, bot, target, 1.0, now));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Unit tests (spec §IMPLEMENT: focused tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::messages::{self, frame_for_test};
    use super::super::state::{EquippedAbility, AbilityTag, Fighter, FlowState, MatchCombat, DamageType};
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
        let out = drain_state_changes(&mut combat, now);
        assert_eq!(
            out.len(),
            combat.fighters.len(),
            "block must relay PlayerBlockingStateChange to every viewer, got {} frame(s)",
            out.len()
        );
        for (_, f) in &out {
            let nd = arena_proto::parse_netdata(&f[2..]);
            assert_eq!(
                nd.int(3),
                Some(41),
                "the only frame a block emits is the gmid-41 relay — never damage"
            );
            assert_eq!(nd.int(6), Some(1), "prop6 = ActorStateType::Blocking");
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

    /// An ability cast when the caster has LESS stamina than required must be silently
    /// rejected — no cooldown set, no damage emitted. [spec bug 2 / §1 cost gate]
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
        let out = on_c2s_input(&mut combat, 0, &ability_frame, now);

        assert!(
            out.is_empty(),
            "underfunded cast must emit zero frames (rejected), got {} frame(s)",
            out.len()
        );
        // Cooldown must NOT be set — the cast was rejected before the commit point.
        assert!(
            combat.fighters[0].cooldowns.get(qs_uuid).is_none(),
            "rejected cast must not set the ability cooldown"
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
        let out = on_c2s_input(&mut combat, 0, &ability_frame, now);

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
        let out = on_c2s_input(&mut combat, 0, &down_frame, now);

        assert!(
            combat.fighters[0].charge_press_at.is_some(),
            "op46 DOWN must record charge_press_at"
        );

        // Both viewers get it: the charging player (own circle) and the opponent
        // (sees the wind-up).
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

    /// Op46 UP after a FULL-CHARGE hold (≥ CRITICAL_HOLD_SECS) → crit ×1.325 on a Light weapon.
    /// Damage must be GREATER than an uncharged swing (×1.0) on the same fighter.
    /// Ratio must be ≈×1.325 (within 1% — integer rounding tolerance on an exact formula).
    #[test]
    fn op46_full_charge_light_weapon_applies_crit_multiplier() {
        let now = Instant::now();
        // No-enchant combat so the physical damage ratio is clean (not diluted by fixed enchant).
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);

        // Simulate a full-charge hold: press at t=0, release at t = CRITICAL_HOLD_SECS + 0.5s.
        let press_time = now;
        combat.fighters[0].charge_press_at = Some(press_time);
        let release_time = press_time + Duration::from_secs_f32(CRITICAL_HOLD_SECS + 0.5);

        let up_frame = make_op46_frame(0x1234_5678, false);
        let out = on_c2s_input(&mut combat, 0, &up_frame, release_time);

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
        let _uncharged_out = resolve_swing(&mut uncharged_combat, 0, 1, 1.0, now);

        // The charged combat emitted frames → the target (slot 1) received some HP reduction.
        let crit_hp_after = combat.fighters[1].health;
        let norm_hp_after = uncharged_combat.fighters[1].health;
        let crit_dealt = combat.fighters[1].max_health.saturating_sub(crit_hp_after);
        let norm_dealt = uncharged_combat.fighters[1].max_health.saturating_sub(norm_hp_after);

        assert!(
            crit_dealt > norm_dealt,
            "full-charge crit (×{CRIT_FACTOR_LIGHT}) must deal MORE damage than an uncharged swing: \
             crit dealt {crit_dealt}, uncharged dealt {norm_dealt}"
        );

        // The ratio must be approximately CRIT_FACTOR_LIGHT (1.325), within 2% (rounding tolerance).
        // No enchants → ratio is pure physical = swing_factor (1.325 crit / 1.0 normal).
        let ratio = crit_dealt as f32 / norm_dealt as f32;
        let _ = out; // suppress unused warning
        assert!(
            (ratio - CRIT_FACTOR_LIGHT).abs() < 0.02,
            "damage ratio must be ≈×{CRIT_FACTOR_LIGHT} (Light crit), got ×{ratio:.4} \
             (crit={crit_dealt}, normal={norm_dealt})"
        );
    }

    /// Op46 UP after a FULL-CHARGE hold with a Heavy weapon → crit ×1.987.
    #[test]
    fn op46_full_charge_heavy_weapon_applies_crit_multiplier() {
        let now = Instant::now();
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Heavy);

        combat.fighters[0].charge_press_at =
            Some(now - Duration::from_secs_f32(CRITICAL_HOLD_SECS + 0.3));

        let up_frame = make_op46_frame(0x1234_5678, false);
        let out = on_c2s_input(&mut combat, 0, &up_frame, now);

        assert!(!out.is_empty(), "full-charge Heavy op46 UP must emit damage");

        // Compare against uncharged heavy.
        let mut uncharged = make_live_combat_no_enchant(now, super::super::tables::Weight::Heavy);
        let _ = resolve_swing(&mut uncharged, 0, 1, 1.0, now);

        let crit_dealt = combat.fighters[1].max_health.saturating_sub(combat.fighters[1].health);
        let norm_dealt = uncharged.fighters[1].max_health.saturating_sub(uncharged.fighters[1].health);

        let ratio = crit_dealt as f32 / norm_dealt as f32;
        assert!(
            (ratio - CRIT_FACTOR_HEAVY).abs() < 0.02,
            "Heavy crit ratio must be ≈×{CRIT_FACTOR_HEAVY}, got ×{ratio:.4}"
        );
    }

    /// Op46 UP after a SHORT hold (< CRITICAL_HOLD_SECS) → normal swing ×1.0 (no crit).
    /// Damage must equal an uncharged swing (no crit boost applied).
    #[test]
    fn op46_short_hold_partial_charge_no_crit() {
        let now = Instant::now();
        // No-enchant so the comparison is exact (no rounding from fixed enchant contribution).
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);

        // Press at t=0, release at t = CRITICAL_HOLD_SECS / 2 (definitely partial).
        let press_time = now;
        combat.fighters[0].charge_press_at = Some(press_time);
        let release_time = press_time + Duration::from_secs_f32(CRITICAL_HOLD_SECS / 2.0);

        let up_frame = make_op46_frame(0x1234_5678, false);
        let _ = on_c2s_input(&mut combat, 0, &up_frame, release_time);

        // Resolve an uncharged swing on a fresh combat at the same `release_time`.
        let mut uncharged = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);
        let _ = resolve_swing(&mut uncharged, 0, 1, 1.0, release_time);

        let partial_dealt = combat.fighters[1].max_health.saturating_sub(combat.fighters[1].health);
        let normal_dealt = uncharged.fighters[1].max_health.saturating_sub(uncharged.fighters[1].health);

        // Partial charge must be equal to uncharged (×1.0, no crit boost).
        assert_eq!(
            partial_dealt, normal_dealt,
            "partial hold (< {CRITICAL_HOLD_SECS}s) must NOT crit: partial dealt {partial_dealt}, \
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
    fn make_live_combat(now: Instant) -> MatchCombat {
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
    /// ability `uuid`. Matches the exact binary layout that `input::parse_execute_ability`
    /// scans: `marker(0x84) + carrier(0x36) + [prefix NetObjectInfo bytes] + 02 00 00 +
    /// [type_nibble] + [role_byte=3] + [gmid_byte=37] + [u16-LE len=36] + [UUID ASCII]`.
    /// Derived from the `op37_frame` worked example in input.rs tests.
    fn make_ability_frame(_obj_id: i32, uuid: &str) -> Vec<u8> {
        assert_eq!(uuid.len(), 36, "UUID must be 36 chars for this builder");
        let mut frame = Vec::new();
        // marker + carrier
        frame.push(0x84u8); // c2s marker
        frame.push(0x36u8); // UserMessage carrier
        // A minimal NetObjectInfo prefix (6 bytes from the op37 worked example).
        frame.extend_from_slice(&[0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35]);
        // Separator + encoding
        frame.extend_from_slice(&[
            0x02, 0x00, 0x00, // separator @ offset (frame.len()-2 from carrier)
            0x38,             // type nibble byte
            0x03,             // role = Autonomous
            0x25,             // gmid = 37 (RequestExecuteAbility)
            0x24, 0x00,       // u16-LE length = 36
        ]);
        frame.extend_from_slice(uuid.as_bytes());
        frame
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
        let out1 = resolve_swing(&mut combat, 0, 1, 1.0, now);
        assert!(!out1.is_empty(), "first swing lands (emits ReceiveDamage)");
        let hp_after_first = combat.fighters[1].health;

        // Second swing HALF an interval later → rejected, no additional damage.
        let too_soon = now + interval / 2;
        let out2 = resolve_swing(&mut combat, 0, 1, 1.0, too_soon);
        assert!(out2.is_empty(), "a swing before the weapon cadence elapses is rejected");
        assert_eq!(combat.fighters[1].health, hp_after_first, "rejected swing deals no damage");

        // A swing just past the interval lands again.
        let ok_time = now + interval + Duration::from_millis(1);
        let out3 = resolve_swing(&mut combat, 0, 1, 1.0, ok_time);
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
            if !resolve_swing(&mut combat, 0, 1, 1.0, t).is_empty() {
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
        assert!(!resolve_swing(&mut light, 0, 1, 1.0, now).is_empty());
        assert!(!resolve_swing(&mut heavy, 0, 1, 1.0, now).is_empty());

        // A time past the Light interval but before the Heavy interval.
        let t = now + tables::fallback_swing_interval(Weight::Light) + Duration::from_millis(1);
        assert!(t < now + tables::fallback_swing_interval(Weight::Heavy), "test time is inside the Heavy cadence");
        assert!(!resolve_swing(&mut light, 0, 1, 1.0, t).is_empty(), "Light can swing again");
        assert!(resolve_swing(&mut heavy, 0, 1, 1.0, t).is_empty(), "Heavy is still on cadence — rejected");
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
    info!("combat: slot {sender} consumed {uuid} (op63 → op64)");
    let frame = messages::perform_consume_consumable(obj, &uuid);
    (0..combat.fighters.len()).map(|s| (s, frame.clone())).collect()
}

#[cfg(test)]
mod phase4_tests {
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
    /// 5:Float y · 6:Float frameDelta · 7:Float charge · 8:Int seq}`.
    fn make_pos_frame(x: f32, y: f32, charge: f32) -> Vec<u8> {
        let mut w = arena_proto::NetDataWriter::new();
        w.int(0, 565)
            .byte(1, 56)
            .byte(2, 3)
            .byte(3, 47)
            .float(4, x)
            .float(5, y)
            .float(6, 0.033_334)
            .float(7, charge)
            .int(8, 410);
        let mut f = messages::frame_for_test(w.finish());
        f[0] = 0x84; // c2s marker
        f
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
            let out = on_c2s_input(&mut combat, 0, &[0x84, 0x36], now + step * i);
            assert!(!out.is_empty(), "the fallback must still land a swing");
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
        let honest_dmg = honest.fighters[1].health;

        // Cheating: identical timing, but the client CLAIMS a 2.8 s charge.
        let mut liar = live_combat(now);
        on_c2s_input(&mut liar, 0, &make_pos_frame(0.814, 0.5, 0.0), t);
        on_c2s_input(&mut liar, 0, &make_act_frame(true, 2.817, false), t);
        on_c2s_input(&mut liar, 0, &make_act_frame(false, 2.817, false), t);

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
        let held_to = t + Duration::from_secs_f32(CRITICAL_HOLD_SECS + 0.1);
        on_c2s_input(&mut real, 0, &make_act_frame(false, 1.3, false), held_to);
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
        assert_eq!(out.len(), 2, "op64 goes to both players");
        let expect = messages::perform_consume_consumable(obj, POTION);
        assert_eq!(out[0].1, expect, "byte-identical to the op64 builder");
        assert_eq!(out[1].1, expect);
        assert_eq!(messages::user_message_gmid(&out[0].1), Some(64));
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
        assert_eq!(out2.len(), 2, "the budget resets between rounds");
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
        assert!((BASE_STAGGER_DURATION_SECS - 1.5).abs() < 1e-6);
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, super::super::loadout::starter(), now);
        f.apply_stagger(now);
        assert!(f.is_staggered(now));
        assert_eq!(f.actor_state(), super::super::state::ActorStateType::Staggered);
        assert!(f.blocking_until.is_none(), "a stagger drops the guard");
        // Still locked just before the duration, recovered just after.
        assert!(f.is_staggered(now + Duration::from_millis(1400)));
        assert!(!f.is_staggered(now + Duration::from_millis(1600)));
        assert!(f.reconcile_stagger(now + Duration::from_millis(1600)));
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
        let _ = on_round_ending_death(&mut combat, 0);
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
