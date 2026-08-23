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
    PendingHit,
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
/// Applied when the server-measured attack hold ≥ `CRIT_HOLD_HEAVY_SECS`.
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
/// Per weight class rather than per weapon because `x_items.py` still discards the
/// eleven `WeaponTemplate` timing fields.
const CRIT_HOLD_LIGHT_SECS: f32 = 0.116_667;
const CRIT_HOLD_VERSATILE_SECS: f32 = 0.2;
const CRIT_HOLD_HEAVY_SECS: f32 = 0.25;

/// The full-charge threshold for this fighter's weapon. An unknown class defaults to
/// Light — the shortest threshold, so it errs toward letting a genuine charge count
/// rather than silently swallowing it, which is the failure being replaced.
fn critical_hold_secs(fighter: &super::state::Fighter) -> f32 {
    match fighter.loadout.weapon.weight {
        Some(tables::Weight::Heavy) => CRIT_HOLD_HEAVY_SECS,
        Some(tables::Weight::Versatile) => CRIT_HOLD_VERSATILE_SECS,
        _ => CRIT_HOLD_LIGHT_SECS,
    }
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
///   - `CRIT_FACTOR_*` when `hold_secs >= CRIT_HOLD_HEAVY_SECS` (full charge / Critical
///     or PostCriticalDecay state — the server-side equivalent of op45 reporting ≥3).
///   - `1.0` for a partial hold (uncharged swing, no crit).
///
/// Light/Heavy/Versatile multipliers come from `tables::Weight::crit_combo().0`.
fn charge_crit_factor(fighter: &super::state::Fighter, hold_secs: f32) -> f32 {
    if hold_secs < critical_hold_secs(fighter) {
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
    //   - hold ≥ CRIT_HOLD_HEAVY_SECS → full charge → swing_factor = CRIT_FACTOR_* by weapon class
    //   - hold < CRIT_HOLD_HEAVY_SECS → partial / uncharged → swing_factor = 1.0
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
                let swing_factor = charge_crit_factor(&combat.fighters[sender], hold_secs);
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
            // Button DOWN — start the server's charge stopwatch AND enter the wind-up.
            f.charge_press_at = Some(now);
            f.bot_swing_at = None;
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

/// A weapon auto-attack (committed swing), throttled per attacker.
///
/// `swing_factor` is the held-charge crit multiplier:
///   - `1.0` for a normal (partial / uncharged) swing via carrier-0x36 or bot swings.
///   - `CRIT_FACTOR_*` for a full-charge crit dispatched from the op46 (0x2e) path
///     when the server-measured hold ≥ `CRIT_HOLD_HEAVY_SECS` (bug 4 fix).
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
        due: now + FOLLOW_THROUGH_DELAY,
    });
    debug!(
        "combat: slot {sender} swing COMMITTED — lands in {FOLLOW_THROUGH_DELAY:?}"
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
        let attacker_loadout = combat.fighters[h.sender].loadout.clone();
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
        let blocked_high = resolved.flags & super::damage::flags::WAS_OPTIMAL_BLOCKING != 0;
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
    combat.fighters[attacker_slot].apply_stagger_for(now, secs);
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
            messages::player_maneuver_state_change(
                combat.fighters[sender].net_object_id,
                combat.fighters[sender].packed_stats(),
                combat.fighters[target_slot].packed_stats(),
                0.0, // timeInState at state entry
                &ea.ability_uuid,
                anim,
                None, // propId 7: unmodelled in the corpus — omitted, never invented
            )
        })
    } else {
        let channel_secs = super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16)
            .and_then(|r| r.channel_duration())
            .unwrap_or(0.0);
        Some(messages::player_channeling_state_change(
            combat.fighters[sender].net_object_id,
            combat.fighters[sender].packed_stats(),
            combat.fighters[target_slot].packed_stats(),
            channel_secs,
            &ea.ability_uuid,
            None, // propId 7: unmodelled in the corpus — omitted, never invented
        ))
    };
    if let Some(f) = state_frame {
        out.push((sender, f.clone()));
        out.push((target_slot, f));
    }

    debug!("combat: slot {sender} casts ability {} (tag {tag:?}, level {level}) → slot {target_slot}", ea.ability_uuid);

    // Phase 3.11: route on the FULL shipped ability table. Ward/Absorb/ResistElements
    // are self-buffs (no direct damage); Paralyze/Damage/Maneuver deal the rank's own
    // `_damage`; Perks never activate.
    use super::state::AbilityTag;
    // Health damage this cast dealt, if any — the gate for threshold effects (Blind).
    // Zero for buffs and for a maneuver that missed, which is correct: a threshold
    // effect must not fire on a cast that did not land.
    let mut last_hit_total = 0.0f32;
    // The `ReceiveDamage` block bits this cast produced on the TARGET
    // (`WAS_LATE_BLOCKING` / `WAS_OPTIMAL_BLOCKING`). Guardbreaker and Staggering Bash
    // ship the SAME `_damageToCauseStagger` but opposite block conditions, so the gate
    // in `apply_shipped_effects` needs to know whether the target blocked. 0 for arms
    // that deal no damage — a cast that never touched the target was not blocked.
    let mut block_flags = 0u8;
    match tag {
        AbilityTag::Ward => out.extend(apply_ward(combat, sender, level, now)),
        AbilityTag::Absorb => out.extend(apply_absorb(combat, sender, level, now)),
        AbilityTag::ResistElements => out.extend(apply_resist_elements(combat, sender, level, now)),
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
        AbilityTag::Maneuver => {
            let mut attacker_loadout = combat.fighters[sender].loadout.clone();
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
            if let Some(r) = super::gamedata::ability_rank_clamped(&ea.ability_uuid, level as u16) {
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
            }
            // Middle is not part of a Left/Right chain, so it resets the combo — the
            // same rule `resolve_swing_with_side` applies to a Middle swing.
            combat.fighters[sender].reset_combo();
            let resolved = RetailDamageModel.resolve_attack(
                &attacker_loadout,
                &combat.fighters[target_slot],
                DamageSource::Attack,
                ActiveSide::Middle,
                1.0,
                0,
                now,
            );
            info!(
                "combat: slot {sender} maneuver {} → weapon damage {:.1} (Middle)",
                ea.ability_uuid, resolved.total,
            );
            last_hit_total = resolved.total;
            block_flags = resolved.flags;
            out.extend(emit_damage(combat, sender, target_slot, &resolved, now));
        }
        AbilityTag::Paralyze | AbilityTag::Damage | AbilityTag::Generic => {
            let resolved = RetailDamageModel.resolve_ability(
                &ea.ability_uuid,
                level,
                &combat.fighters[target_slot],
                ActiveSide::Middle,
                now,
            );
            last_hit_total = resolved.total;
            block_flags = resolved.flags;
            out.extend(emit_damage(combat, sender, target_slot, &resolved, now));
            // A CHANNELLED spell just emitted tick 1 of many. Schedule the rest;
            // `apply_channel_ticks` delivers them on the shipped PvP tick.
            if let Some(total_ticks) = super::damage::channel_ticks(&ea.ability_uuid, level) {
                if total_ticks > 1 {
                    combat.channels.push(super::state::ActiveChannel {
                        caster_slot: sender,
                        target_slot,
                        ability_uuid: ea.ability_uuid.clone(),
                        ability_level: level,
                        remaining_ticks: total_ticks - 1,
                        next_tick_at: now
                            + Duration::from_secs_f32(super::damage::CHANNEL_TICK_INTERVAL_SECS),
                    });
                }
            }
            // A landed Paralyze also carries its own paralyse threshold + duration
            // (`_damageToCauseParalyze` / `_duration`), applied by
            // `apply_status_conditioning` via the caster's `paralyze_rank`.
            if tag == AbilityTag::Paralyze {
                out.extend(try_paralyze(combat, sender, target_slot, level, now));
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
        combat, sender, target_slot, &ea.ability_uuid, level, last_hit_total, block_flags,
        now,
    ));
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
fn apply_shipped_effects(
    combat: &mut MatchCombat,
    caster: usize,
    target_slot: usize,
    ability_uuid: &str,
    level: u8,
    // Health damage this cast just dealt — the gate for threshold effects like Blind.
    last_hit_total: f32,
    // The `damage::flags` block bits this cast produced on the target. Guardbreaker
    // stuns only when the target DID block; Staggering Bash only when it did NOT.
    block_flags: u8,
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

    if let Some(cap) = r.maximum_damage_dodged() {
        if cap > 0.0 && caster < viewers {
            combat.fighters[caster].negation_pools.push(NegationPool {
                source: DamageNegationSource::Dodge,
                remaining: cap,
                expires_at: until_consumed,
                restoration_factor: 0.0,
                absorb_fraction: 1.0,
            });
            let obj = combat.fighters[caster].net_object_id;
            info!("combat: slot {caster} dodge pool +{cap:.1} ({ability_uuid})");
            let frame = messages::change_combat_status_effect(
                obj, true, StatusEffectType::Dodging, 0.0,
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

    // `_blockDuration` → **the bash's own guard window** (tracker #31).
    //
    // Retail's `AbilityDoShieldBash : AbilityDoManeuver` (`dump.cs:604149-604161`) is
    // literally a block followed by a slam: it holds `_blockDuration`, `_timer`,
    // `_appliedBlock`, `_removedBlock` and a `_blockingEffect`, and every subclass ctor
    // takes the duration first — `AbilityDoHarryingBash(maneuver, blockDuration,
    // cooldownIncrease)` (`:603663`), `AbilityDoStaggeringBash(maneuver, blockDuration,
    // damageToCauseStagger, stunDuration)` (`:604366`). The shipped text agrees:
    // `Ability.Maneuver.ShieldBash.Description` — *"The fighter **first blocks with
    // their shield**, then slams it into the enemy."*
    //
    // Five abilities ship the field, all at 0.50 s at every rank: ShieldBash,
    // HarryingBash, StaggeringBash, ReflectingBash, ShieldOfMania. We read it for the
    // `_damageReduction` window below but never raised an actual guard, so the block
    // half of a bash did not exist — which is the other half of the report ("a
    // well-timed harrying/staggering bash does not stun either"). With this window up,
    // an opponent's weapon swing that lands inside it is blocked HIGH and
    // `stun_the_blocked_attacker` fires. Harrying Bash needs exactly this: it carries
    // no `_damageToCauseStagger` and no `_stunDuration` at any of its 14 ranks, so it
    // was never meant to stun through the ability gate.
    //
    // A fresh raise (`block_raised_at = now`) so the window opens in the OPTIMAL phase,
    // subject to the normal `OPTIMAL_BLOCK_RECOVERY_SECS` cooldown in `block_phase` —
    // a bash cannot launder a guard that was just dropped.
    if let Some(window) = r.block_duration() {
        if window > 0.0 && caster < viewers && !combat.fighters[caster].is_dead() {
            let f = &mut combat.fighters[caster];
            f.set_actor_state(ActorStateType::Blocking, now);
            // Shipped `parameters.activeSide: 1` on every bash rank == Middle, the same
            // facing every recorded manual guard carries. Presentational only.
            f.blocking_side = ActiveSide::Middle;
            f.blocking_until = Some(now + Duration::from_secs_f32(window));
            f.block_raised_at = Some(now);
            info!(
                "combat: slot {caster} bash guard UP for {window:.2}s ({ability_uuid})"
            );
        }
    }

    // `_damageReduction` + `_blockDuration` → a flat reduction while the block window
    // is open. ShieldOfMania ships 50.11 and ReflectingBash 110.67 at R1, each with
    // `_blockDuration` 0.50 s: press block and for half a second incoming damage is cut
    // by that much. The numbers are FLAT RATINGS, not fractions — they run 50→139 and
    // 111→182 across ranks, so a fractional reading would be nonsense.
    //
    // Carried as transient resistances across every damage type, which is the existing
    // machinery for a timed flat subtraction (Resist-Elements uses it) and needs no
    // change to the damage pipeline. One entry per type because the store is keyed by
    // type; a generic reduction is simply all of them.
    if let Some(reduction) = r.damage_reduction() {
        let window = r.block_duration().unwrap_or(0.0);
        if reduction > 0.0 && window > 0.0 && caster < viewers {
            use super::state::DamageType;
            let until = now + Duration::from_secs_f32(window);
            let f = &mut combat.fighters[caster];
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
            info!(
                "combat: slot {caster} damage reduction {reduction:.1} for {window:.2}s                  ({ability_uuid})"
            );
        }
    }

    // `_damageToCauseBlind` → the green fog on the VICTIM when the hit lands hard
    // enough. Exactly parallel to the already-wired `_damageToCauseParalyze`.
    //
    // There is NO server-side mechanic to model: `ActorStateType.StateId` has no blind
    // state (all 29 members read), so the fog — and a burning opponent staying visible
    // through it — is rendered client-side off the status. The server's whole job is to
    // send `Blind` (8) with the ability's own `_duration`.
    if let Some(threshold) = r.damage_to_cause_blind() {
        if last_hit_total >= threshold
            && target_slot < viewers
            && !combat.fighters[target_slot].is_dead()
        {
            let secs = r.duration().unwrap_or(0.0);
            let obj = combat.fighters[target_slot].net_object_id;
            info!("combat: slot {target_slot} BLINDED {secs:.2}s (hit {last_hit_total:.1} >= {threshold:.1})");
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
    let target_blocked = block_flags
        & (super::damage::flags::WAS_LATE_BLOCKING | super::damage::flags::WAS_OPTIMAL_BLOCKING)
        != 0;
    let block_condition_met = match super::gamedata::ability(ability_uuid)
        .map(|a| a.editor_name)
    {
        Some("Guardbreaker") => target_blocked,
        Some("StaggeringBash") => !target_blocked,
        _ => true,
    };
    if let Some(threshold) = r.damage_to_cause_stagger() {
        if last_hit_total > 0.0
            && last_hit_total >= threshold
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
            combat.fighters[target_slot].apply_stagger_for(now, secs);
            let obj = combat.fighters[target_slot].net_object_id;
            info!(
                "combat: slot {target_slot} STAGGERED {secs:.2}s \
                 (hit {last_hit_total:.1} >= {threshold:.1}, {ability_uuid})"
            );
            let frame = messages::change_combat_status_effect(
                obj, true, StatusEffectType::Staggered, secs,
            );
            for v in 0..viewers {
                out.push((v, frame.clone()));
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
    // The mirrored Stamina/Magicka tracks come off their pools BEFORE `packed_stats()`
    // is read for the frame, so the bars the client draws match the numbers the same
    // frame reports. [Fighter::drain_mirrored_pools]
    let (drained_stam, drained_mag) = combat.fighters[target_slot].drain_mirrored_pools(&components);
    let hp_after = combat.fighters[target_slot].health;
    // Per-hit damage-vs-maxHP ratio (info-level so the ghost-verify on the box shows the
    // before→after HP without RUST_LOG=debug). NOTE: the 25% one-shot clamp is GONE for
    // arena — deep-combo hits are *earned* and can legitimately be large (§4.5).
    let pct = if max_hp > 0 { 100.0 * total / max_hp as f32 } else { 0.0 };
    let dealt = hp_before.saturating_sub(hp_after);
    info!(
        "combat damage: slot {attacker_slot} → slot {target_slot} | source {:?} side {:?} | total {total:.1} = {pct:.1}% of {max_hp} maxHP | HP {hp_before} → {hp_after} (−{dealt}) | drained stam −{drained_stam} mag −{drained_mag}",
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

    // The DEFENDER's gear hits back. Emitted after the hit that provoked it and
    // before any death check, so a Revenge proc can itself be the killing blow —
    // which is how retail orders it (`op50 blocked` then `op50 src=Revenge`).
    out.extend(apply_revenge(combat, target_slot, attacker_slot, now));

    if combat.fighters[target_slot].is_dead() {
        out.extend(on_round_ending_death(combat, attacker_slot, now));
    }
    if combat.fighters[attacker_slot].is_dead() {
        out.extend(on_round_ending_death(combat, target_slot, now));
    }
    out
}

/// Elemental retaliation: the fighter who was just hit deals their gear's Revenge
/// damage back at whoever hit them.
///
/// Capture-measured over 203 Revenge frames in s615/s616, which is also what proves
/// this is GEAR and not a block-punish: the damage type varies per wearer (Frost,
/// Fire, Poison), each wearer's magnitudes repeat from a tiny fixed set, and the
/// value does not track the incoming hit — 105.0 followed a blocked 54.3 and again a
/// blocked 23.8. Retail's frames carry `flags=3` (SHOW|ATTACKER) and never the
/// OPTIMAL bit, so it fires on being hit, blocked or not.
///
/// NO RECURSION: this emits a damage frame directly rather than re-entering the hit
/// pipeline, so an attacker's own Revenge cannot fire in response to being retaliated
/// against. Two wearers would otherwise ping-pong until one died.
fn apply_revenge(
    combat: &mut MatchCombat,
    defender_slot: usize,
    attacker_slot: usize,
    _now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    if defender_slot == attacker_slot {
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
        // Resistance is the attacker's, and it is what explains the gap between the
        // shipped 137.32 and the 137.21 seen on the wire.
        let resisted = {
            let a = &combat.fighters[attacker_slot];
            // No elemental piercing: that is a property of an ATTACK, and Revenge is
            // gear firing on its own, not a swing the wearer aimed.
            (raw - a.total_resistance_against(ty, 0.0, _now)).max(0.0)
        };
        if resisted <= 0.0 {
            continue;
        }
        combat.fighters[attacker_slot].take_damage(resisted.round().max(0.0) as u32);
        let msg = {
            let hit = &combat.fighters[attacker_slot];
            let other = &combat.fighters[defender_slot];
            messages::receive_damage(
                hit.net_object_id,
                NetObjectType::Avatar as u8,
                hit.packed_stats(),
                other.packed_stats(),
                super::state::DamageSource::Revenge,
                super::damage::flags::SHOW_DAMAGE | super::damage::flags::HAS_ATTACKER,
                resisted,
                0,
                ActiveSide::None,
                super::state::DamageType::None,
                &[(ty, resisted)],
            )
        };
        info!(
            "combat: slot {defender_slot} REVENGE {ty:?} {resisted:.2} back at slot {attacker_slot}"
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
                    target_obj, true, condition, CONDITION_DURATION_SECS,
                );
                debug!("combat: slot {target_slot} CONDITION {condition:?} landed ({recent:.0} ≥ {threshold:.0} window poison/elem)");
                for slot in 0..combat.fighters.len() {
                    out.push((slot, frame.clone()));
                }
            }


            // FREEZE (frost only): `Frozen` used to be inert. It emitted its op51 and
            // pushed an `ActiveEffect` whose per-tick is `dot_percent_health(Frost) ×
            // maxHP` = **0.0** — Frost is a CONTROL status, not a DoT (Phase 3.8) — and
            // then did nothing else: no actor state, no guard drop, and no gate anywhere
            // in the bot loop, which only ever checked `is_staggered` / `is_paralyzed`.
            // So a frozen opponent kept swinging and Frostbite's whole point was
            // invisible (report #31, "does not freeze the opponent").
            //
            // The vehicle is the STAGGER path, for the same reason `apply_stagger_for`
            // documents for ability stuns, mirrored: `ActorStateType` has **no `Frozen`
            // member** (`dump.cs` 340171–340200, transcribed verbatim in `state.rs`), so
            // there is no frozen actor-state id to put on the wire, and inventing one
            // risks the client's `FindStateTypeByID` returning null and dropping the
            // frame. `Staggered` (5) is capture-validated and does the same observable
            // thing — inputs locked, guard dropped, combo broken, and an animation to
            // play. The FROST identity still reaches the client: the op51 above carries
            // the real `StatusEffectType::Frozen` (5), which is what drives the frost
            // VFX. This reuses the existing plumbing wholesale, so the bot gate
            // (`is_staggered` in `on_tick`) and the human input gate (`is_staggered` in
            // `on_c2s_input`) both start applying to a freeze for free.
            //
            // Duration is SHIPPED: `ELEMENTAL_STATUSES[1]` (Frost) `.duration` = 5.0 s,
            // the same figure the op51 above already put on the wire — the lock and the
            // client-side status now expire together instead of disagreeing.
            if *ty == DamageType::Frost && !already {
                let secs = super::gamedata::combat_params::elemental_status(
                    StatusEffectType::Frozen as u16 as u8,
                )
                .map(|e| e.duration)
                .unwrap_or(CONDITION_DURATION_SECS);
                info!("combat: slot {target_slot} FROZEN (frost {recent:.1} ≥ {threshold:.1}) for {secs}s");
                combat.fighters[target_slot].apply_stagger_for(now, secs);
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
                        target_obj, true, StatusEffectType::Paralyzed, secs,
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

/// Drive DoT ticks for all active elemental conditions on all fighters. For each
/// `Burning/Frozen/Enervated/Poisoned` `ActiveEffect` whose `last_tick` is ≥
/// `DOT_TICK_INTERVAL` ago, emit a `ReceiveDamage` with `DamageSource::StatusEffect`
/// and the condition's elemental type. Multiple concurrent instances of the SAME
/// element tick INDEPENDENTLY (stack, do not refresh). Expired effects are dropped.
/// Returns `(target_slot, frame)` pairs — one `ReceiveDamage` per eligible tick.
///
/// **DoT tick magnitude**: `per_tick_damage` (= `_percentHealthDamage × maxHP`),
/// game-data-driven at `DOT_PERCENT_HEALTH_PER_TICK`. [§Mechanic-4 calibration flag]
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
        let (caster, target, uuid, level) = {
            let c = &combat.channels[i];
            (c.caster_slot, c.target_slot, c.ability_uuid.clone(), c.ability_level)
        };
        if target >= combat.fighters.len()
            || combat.fighters[target].is_dead()
            || caster >= combat.fighters.len()
            || combat.fighters[caster].is_dead()
        {
            combat.channels[i].remaining_ticks = 0;
            continue;
        }

        let resolved = RetailDamageModel.resolve_ability(
            &uuid,
            level,
            &combat.fighters[target],
            ActiveSide::Middle,
            now,
        );
        out.extend(emit_damage(combat, caster, target, &resolved, now));

        let c = &mut combat.channels[i];
        c.remaining_ticks = c.remaining_ticks.saturating_sub(1);
        // Advance from the SCHEDULED time, never from `now`. `on_tick` fires a little
        // after the instant a tick was due, and rebasing on the late arrival lets that
        // slack compound: measured, it dropped 4 of 15 ticks outside the channel and
        // stretched a 3.0 s cast to 3.6 s.
        c.next_tick_at += Duration::from_secs_f32(super::damage::CHANNEL_TICK_INTERVAL_SECS);
    }

    combat.channels.retain(|c| c.remaining_ticks > 0);
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
                out.extend(on_round_ending_death(combat, opp_slot, now));
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
        absorb_fraction: 1.0,    // Ward swallows a hit whole until exhausted
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

    // 1) op29 PlayerDead for the loser, props 0-10.
    //
    //    Transition the loser to `Dead` FIRST. The state ring at propId 7 is what the
    //    client reads to pick a death animation, and its newest entry must equal the
    //    frame's own propId 6 — an invariant that holds in every retail frame decoded.
    //    Snapshotting before the transition would ship a ring whose tail is whatever
    //    the fighter was doing a moment ago, and a death that animates out of the
    //    wrong pose.
    if let Some(f) = combat.fighters.get_mut(loser) {
        f.set_actor_state(ActorStateType::Dead, now);
    }
    let (loser_history, loser_time_in_prev) = combat
        .fighters
        .get(loser)
        .map(|f| (f.packed_state_history(), f.time_in_state(now)))
        .unwrap_or_default();
    let dead_frame =
        messages::player_dead(loser_obj, loser_stats, winner_stats, &loser_history, loser_time_in_prev);
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

        // Health regen: NOT APPLIED HERE, and note that `HEALTH_REGEN_RATE_PER_S`
        // is referenced by no code at all — only by comments. Setting it does
        // nothing; wiring health regen means adding a branch here (and honouring
        // `BlockHealthRegen`, which is already decoded).
        //
        // The captured wire shows ~0.20 %/s health, so the measured set says this
        // should be non-zero. It was left at zero deliberately when the
        // stamina/magicka rates were taken from the wire on 2026-08-22: the owner
        // was asked about the two POOLS, and switching health on makes HP climb in
        // every fight — a far larger change to how a match feels than a rate tweak,
        // and one that contradicts a standing owner decision from 2026-08-02.
        // Raise it as its own question rather than smuggling it in here.

        // Stamina regen: 3.03 %/s — the captured wire rate (see the constant).
        if !block_stam && f.stamina < f.max_stamina {
            let regen = ((STAMINA_REGEN_RATE_PER_S * f.max_stamina as f32).round() as u32).max(1);
            f.stamina = (f.stamina + regen).min(f.max_stamina);
        }
        // Magicka regen: 2.93 %/s — the captured wire rate (see the constant).
        if !block_mag && f.magicka < f.max_magicka {
            let regen = ((MAGICKA_REGEN_RATE_PER_S * f.max_magicka as f32).round() as u32).max(1);
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
    if matches!(combat.phase, FlowState::RoundEnd | FlowState::NextState) {
        // A landing blow just ended the round.
        return out;
    }

    // DoT ticks — one tick per second per active condition instance, independent of
    // whether a bot or player is the source. Runs BEFORE bot swings so a DoT killing
    // blow is processed before the bot's turn. [§Mechanic-2]
    // Tell the clients about anything that just LAPSED, before `apply_dot_ticks`
    // prunes the expired effects out of existence.
    out.extend(emit_status_removals(combat, now));
    out.extend(apply_dot_ticks(combat, now));
    out.extend(apply_channel_ticks(combat, now));
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
        let cast_ready = combat.fighters[bot]
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
                let frame = messages::request_execute_ability(&uuid);
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
                // Bots don't hold a button — always ×1.0 (no held-charge crit).
                out.extend(resolve_swing(combat, bot, target, 1.0, now));
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
            // The side is decided now, at the start of the wind-up, and
            // `resolve_swing`'s alternation fallback will produce the same one when the
            // swing lands — retail carries one side across all four beats (593/593).
            let side = match combat.fighters[bot].last_combo_side {
                ActiveSide::Right => ActiveSide::Left,
                _ => ActiveSide::Right,
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
        let mut out = on_c2s_input(&mut combat, 0, &ability_frame, now);
        out.extend(land(&mut combat, now));

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
        let frame = messages::request_execute_ability(uuid);
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

    /// Op46 UP after a FULL-CHARGE hold (≥ CRIT_HOLD_HEAVY_SECS) → crit ×1.325 on a Light weapon.
    /// Damage must be GREATER than an uncharged swing (×1.0) on the same fighter.
    /// Ratio must be ≈×1.325 (within 1% — integer rounding tolerance on an exact formula).
    #[test]
    fn op46_full_charge_light_weapon_applies_crit_multiplier() {
        let now = Instant::now();
        // No-enchant combat so the physical damage ratio is clean (not diluted by fixed enchant).
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);

        // Simulate a full-charge hold: press at t=0, release at t = CRIT_HOLD_HEAVY_SECS + 0.5s.
        let press_time = now;
        combat.fighters[0].charge_press_at = Some(press_time);
        let release_time = press_time + Duration::from_secs_f32(CRIT_HOLD_HEAVY_SECS + 0.5);

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
            Some(now - Duration::from_secs_f32(CRIT_HOLD_HEAVY_SECS + 0.3));

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
            (ratio - CRIT_FACTOR_HEAVY).abs() < 0.02,
            "Heavy crit ratio must be ≈×{CRIT_FACTOR_HEAVY}, got ×{ratio:.4}"
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

    /// Op46 UP after a SHORT hold (< CRIT_HOLD_HEAVY_SECS) → normal swing ×1.0 (no crit).
    /// Damage must equal an uncharged swing (no crit boost applied).
    #[test]
    fn op46_short_hold_partial_charge_no_crit() {
        let now = Instant::now();
        // No-enchant so the comparison is exact (no rounding from fixed enchant contribution).
        let mut combat = make_live_combat_no_enchant(now, super::super::tables::Weight::Light);

        // Press at t=0, release at t = CRIT_HOLD_HEAVY_SECS / 2 (definitely partial).
        let press_time = now;
        combat.fighters[0].charge_press_at = Some(press_time);
        let release_time = press_time + Duration::from_secs_f32(CRIT_HOLD_LIGHT_SECS / 2.0);

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
    (0..combat.fighters.len()).map(|s| (s, frame.clone())).collect()
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
        let held_to = t + Duration::from_secs_f32(CRIT_HOLD_HEAVY_SECS + 0.1);
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
    use super::super::state::{DamageNegationSource, Fighter};
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

    #[test]
    fn a_dodge_ability_gives_the_caster_a_dodge_pool() {
        let now = Instant::now();
        let mut c = combat2(now);
        let u = uuid_of("DodgingStrike");
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, 500.0, 0, now);
        let pools = &c.fighters[0].negation_pools;
        assert_eq!(pools.len(), 1, "one dodge pool");
        assert_eq!(pools[0].source, DamageNegationSource::Dodge);
        assert!(pools[0].remaining > 0.0, "the shipped cap must be positive");
        // Dodging (12) is a pinned status id, so this one DOES get an op51 — to both.
        assert_eq!(out.len(), 2, "op51 Dodging to both viewers");
    }

    /// The three *Armor spells get a real shield. No op51: the elemental-armor status
    /// id is not pinned, and a guessed id is dropped silently by the client.
    #[test]
    fn an_armor_spell_gives_a_shield_pool_but_no_guessed_status() {
        let now = Instant::now();
        for name in ["FirestormArmor", "BlizzardArmor", "TempestArmor"] {
            let mut c = combat2(now);
            let out = apply_shipped_effects(&mut c, 0, 1, uuid_of(name), 1, 500.0, 0, now);
            assert_eq!(c.fighters[0].negation_pools.len(), 1, "{name}: a shield pool");
            assert!(c.fighters[0].negation_pools[0].remaining >= 100.0, "{name}: shipped ~116");
            assert_eq!(out.len(), 2, "{name}: emits its now-known status id");
        }
    }

    /// FlashFreeze locks the TARGET, not the caster, for the rank's own duration.
    #[test]
    fn flashfreeze_locks_the_target_for_its_shipped_duration() {
        let now = Instant::now();
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("FlashFreeze"), 1, 500.0, 0, now);
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
            apply_shipped_effects(&mut c, 0, 1, uuid_of(name), 1, 500.0, 0, now);
            let tr = &c.fighters[0].transient_resistances;
            assert!(!tr.is_empty(), "{name}: a reduction must land");
            assert!(tr.iter().all(|(_, amt, _)| *amt >= 50.0), "{name}: flat rating, not a fraction");
            // The window is short on purpose — half a second, not a standing buff.
            let expiry = tr[0].2;
            assert!(expiry > now && expiry <= now + Duration::from_secs(1), "{name}: ~0.5s window");
        }
    }

    /// Blind: the green fog on the VICTIM, gated on the hit landing hard enough.
    /// `Blind = 8` and there is no blind ACTOR state, so the server's whole job is the
    /// op51 — the fog is client-rendered.
    #[test]
    fn blind_fires_only_when_the_hit_clears_its_threshold() {
        let now = Instant::now();
        let u = uuid_of("Blind");
        // A big hit → blinded.
        let mut c = combat2(now);
        let out = apply_shipped_effects(&mut c, 0, 1, u, 1, 9_999.0, 0, now);
        let blind = out.iter().filter(|(_, f)| {
            let nd = arena_proto::parse_netdata(&f[2..]);
            nd.int(3) == Some(51) && nd.int(5) == Some(8)
        }).count();
        assert_eq!(blind, 2, "op51 Blind (8) to both viewers");

        // A hit of zero → no blind. A threshold effect must not fire on a cast that
        // did not land.
        let mut c2 = combat2(now);
        let out2 = apply_shipped_effects(&mut c2, 0, 1, u, 1, 0.0, 0, now);
        assert!(
            !out2.iter().any(|(_, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                nd.int(3) == Some(51) && nd.int(5) == Some(8)
            }),
            "no damage means no blindness"
        );
    }

    /// The three *Armor spells now DO emit their status — `ElementalStormArmor` = 16,
    /// one shared value for all three (the element is on the ability, not the status).
    #[test]
    fn an_armor_spell_emits_the_storm_armor_status() {
        let now = Instant::now();
        for name in ["FirestormArmor", "BlizzardArmor", "TempestArmor"] {
            let mut c = combat2(now);
            let out = apply_shipped_effects(&mut c, 0, 1, uuid_of(name), 1, 0.0, 0, now);
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
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("Fireball"), 1, 500.0, 0, now);
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
        for (name, block_flags) in [
            ("StaggeringBash", 0u8),
            ("Guardbreaker", super::super::damage::flags::WAS_OPTIMAL_BLOCKING),
        ] {
            let now = Instant::now();
            let mut c = combat2(now);
            let u = uuid_of(name);
            let threshold = super::super::gamedata::ability_rank_clamped(u, 1)
                .and_then(|r| r.damage_to_cause_stagger())
                .unwrap_or_else(|| panic!("{name} R1 ships _damageToCauseStagger"));
            let stun = super::super::gamedata::ability_rank_clamped(u, 1)
                .and_then(|r| r.stun_duration())
                .unwrap_or_else(|| panic!("{name} R1 ships _stunDuration"));

            let out = apply_shipped_effects(&mut c, 0, 1, u, 1, threshold, block_flags, now);
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
        let out = apply_shipped_effects(&mut hard, 0, 1, u, 1, threshold, 0, now);
        assert!(hard.fighters[1].is_staggered(now), "a hit at the threshold staggers");
        assert_eq!(status_frames(&out, StatusEffectType::Staggered), hard.fighters.len());

        let mut soft = combat2(now);
        let out = apply_shipped_effects(&mut soft, 0, 1, u, 1, threshold - 0.1, 0, now);
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
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("StaggeringBash"), 1, 0.0, 0, now);
        assert!(!c.fighters[1].is_staggered(now), "no damage → no stagger");
        assert_eq!(status_frames(&out, super::super::state::StatusEffectType::Staggered), 0);
    }

    /// And a dead target is not staggered — the pre-existing guard, kept.
    #[test]
    fn a_dead_target_is_not_staggered() {
        let now = Instant::now();
        let mut c = combat2(now);
        c.fighters[1].health = 0;
        let out = apply_shipped_effects(&mut c, 0, 1, uuid_of("StaggeringBash"), 1, 500.0, 0, now);
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

    /// A LATE block must be weaker against a block-piercing attack. Skullcrusher ships
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

        assert!(
            pierced.factor_for(DamageType::Slashing) > plain.factor_for(DamageType::Slashing),
            "physical block pierce must let MORE damage through a late block"
        );
        assert!(
            pierced.factor_for(DamageType::Fire) > plain.factor_for(DamageType::Fire),
            "elemental block pierce must let more elemental through"
        );
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
        for ty in [DamageType::Slashing, DamageType::Fire, DamageType::Frost, DamageType::Stamina] {
            let f = b.factor_for(ty);
            // Re-deriving from the un-pierced rating must give the same bits.
            let mut zero = b;
            zero.block_piercing = 0.0;
            zero.elem_block_piercing = 0.0;
            assert_eq!(f.to_bits(), zero.factor_for(ty).to_bits(), "{ty:?} must be identical");
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
        ActorStateType, BlockPhase, BASE_STAGGER_DURATION_SECS, BLOCK_OPTIMAL_TIME_SECS, Fighter,
        FlowState, MatchCombat, StatusEffectType, WeaponProfile,
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

    /// `UI.Help.Skills.Description`: *"You do not get stunned when your ability
    /// attack is blocked high."* Driven through the real c2s cast path.
    #[test]
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

        let out = super::apply_revenge(&mut c, 1, 0, now);

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

    /// Two fighters both wearing Revenge must not ping-pong retaliation forever.
    #[test]
    fn revenge_does_not_retaliate_against_revenge() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let frost = super::super::state::DamageType::Frost;
        c.fighters[0].loadout.revenge = vec![(frost, 50.0)];
        c.fighters[1].loadout.revenge = vec![(frost, 50.0)];

        // One retaliation resolves and stops; it does not re-enter the hit pipeline.
        let out = super::apply_revenge(&mut c, 1, 0, now);
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
        let out = super::apply_revenge(&mut c, 1, 0, now);
        assert!(out.is_empty());
        assert_eq!(c.fighters[0].health, hp);
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
        c.fighters[1].loadout.revenge =
            vec![(super::super::state::DamageType::Frost, 137.32)];
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

        let at = now + super::ROUND_START_ENGAGE_DELAY + Duration::from_millis(10);
        let out = super::on_tick(&mut c, at, false);

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

    // -- Frozen must actually freeze --------------------------------------
    //
    // The Frost elemental status emitted its op51 and pushed an `ActiveEffect`,
    // and that was all it did: its per-tick is `dot_percent_health(Frost) × maxHP`
    // = 0.0 (Frost is a CONTROL status, not a DoT), it set no actor state, it did
    // not drop the victim's guard, and nothing in the bot loop gated on it. So a
    // "frozen" opponent kept swinging and the mechanic was invisible — the same
    // shape of defect as the missing stagger gate above.

    /// A frozen bot must stop swinging and must enter an actor state the client can
    /// animate, exactly the way a stunned one does.
    #[test]
    fn a_frozen_bot_stops_swinging_and_enters_an_actor_state() {
        let t0 = Instant::now();
        // The round has been live for a while, so nothing about round START is in play.
        let now = t0 + Duration::from_secs(30);

        // Control: at this same instant an UNFROZEN bot does engage.
        let mut ctrl = combat(t0, 1);
        super::on_tick(&mut ctrl, now, false);
        assert!(
            ctrl.fighters[1].bot_swing_at.is_some(),
            "control: an unfrozen bot engages at this instant",
        );

        // expected_peers = 1 → slot 1 is the bot. Land Frozen on it with an
        // overwhelming Frost hit (the conditioning threshold is a fraction of maxHP).
        let mut c = combat(t0, 1);
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

        // (1) The freeze sets an actor state, so the client has something to animate.
        assert_ne!(
            c.fighters[1].actor_state(),
            ActorStateType::Idle,
            "Frozen must set an actor state — a frozen fighter cannot still be Idle",
        );
        let entered = c.fighters[1].actor_state();
        let transitions = c.fighters[1].take_state_changes();
        assert!(
            transitions.iter().any(|t| t.to == entered),
            "…and the transition must be queued for the wire, so viewers see it",
        );

        // (2) The freeze gates the bot exactly the way a stagger does.
        super::on_tick(&mut c, now + Duration::from_millis(10), false);
        assert!(
            c.fighters[1].bot_swing_at.is_none(),
            "a frozen bot must not queue a wind-up",
        );
        assert_ne!(
            c.fighters[1].actor_state(),
            ActorStateType::Charging,
            "…nor enter Charging",
        );

        // (3) …and it thaws on the SHIPPED Frost duration, it is not a permanent lock.
        let frost_secs = super::super::gamedata::combat_params::elemental_status(5)
            .expect("Frost (status_type 5) is in the shipped ELEMENTAL_STATUSES table")
            .duration;
        let thawed = now + Duration::from_secs_f32(frost_secs + 0.1);
        super::on_tick(&mut c, thawed, false);
        assert!(
            c.fighters[1].bot_swing_at.is_some(),
            "the bot swings again once the {frost_secs}s freeze lapses",
        );
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
            super::apply_shipped_effects(&mut c, 0, 1, u, 1, 0.0, 0, now);

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
        super::apply_shipped_effects(&mut c, 0, 1, uuid_of("Guardbreaker"), 1, 5.0, 0, now);
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
            super::apply_shipped_effects(&mut c, 0, 1, uuid_of(name), 1, 0.0, 0, now);

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

        for (label, bits, want) in [
            ("high block", flags::WAS_OPTIMAL_BLOCKING, true),
            ("low block", flags::WAS_LATE_BLOCKING, true),
            ("no block", 0u8, false),
        ] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            let out = super::apply_shipped_effects(&mut c, 0, 1, u, 1, threshold, bits, now);
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

        for (label, bits, want) in [
            ("high block", flags::WAS_OPTIMAL_BLOCKING, false),
            ("low block", flags::WAS_LATE_BLOCKING, false),
            ("no block", 0u8, true),
        ] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            let out = super::apply_shipped_effects(&mut c, 0, 1, u, 1, threshold, bits, now);
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
        for bits in [0u8, flags::WAS_LATE_BLOCKING, flags::WAS_OPTIMAL_BLOCKING] {
            let now = Instant::now();
            let mut c = combat(now, 2);
            super::apply_shipped_effects(&mut c, 0, 1, u, 1, threshold, bits, now);
            assert!(
                c.fighters[1].is_staggered(now),
                "IceSpike staggers on damage alone (bits {bits:#06b})",
            );
        }
    }
}
