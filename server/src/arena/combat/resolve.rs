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
//! Still to wire: decoding the real swipe-input `activeSide`/`swingFactor` from the c2s
//! body (auto-swings ALTERNATE Left/Right as a faithful stand-in); the
//! `PlayerChannelingStateChange` (53) cast animation; per-element DoT TICK damage (the
//! conditioning land + threshold is wired; the periodic StatusEffect-source ReceiveDamage
//! tick is the remaining piece); and routing real ability UUIDs to Ward/Absorb/Paralyze
//! casts (the casts push pools / run the threshold; the per-ability recognition is TODO).

use std::time::{Duration, Instant};

use log::{debug, info};

use super::damage::{DamageModel, ResolvedDamage, RetailDamageModel};
use super::input;
use super::messages;
use super::state::{ActiveSide, DamageSource, FlowState, MatchCombat, MatchState, NetObjectType};
use super::tables;

/// Carrier MessageType (`user_data[1]`) of the combat-input family — `0x36` (54).
const CARRIER_USERMESSAGE: u8 = 0x36;
/// Carrier for `PlayerCombatInputActivate` (op46) — `0x2e` (46). Op46 uses its own
/// carrier byte (`GameMessageId` value), NOT the generic `0x36` UserMessage carrier.
const CARRIER_OP46: u8 = 0x2e;

/// Minimum spacing between landed swings per attacker (stand-in for swipe-commit).
const SWING_COOLDOWN: Duration = Duration::from_millis(400);

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

/// Authoritative per-ability cooldown, keyed by the ability *definition* UUID
/// carried in the cast. Values are `_cooldown` (seconds → ms) read from the APK's
/// `ActiveAbility` ScriptableObjects (rank-independent; extracted via UnityPy —
/// see docs/arena-cooldowns-authoritative.md). Unknown UUIDs fall back to
/// `ABILITY_COOLDOWN`. NOTE: Lightning Bolt's 0.5s is the channeled re-fire
/// interval (not a between-cast gate); `_initialCooldown` (round-start delay) and
/// the 10 never-captured abilities are not yet applied.
fn ability_cooldown(ability_uuid: &str) -> Duration {
    let ms: u64 = match ability_uuid {
        "d07a8d30-9a1c-49b0-866d-97a8aa1534cf" => 3540, // Fireball
        "7fc15804-1637-40a9-8dcc-3ea1eb0f778d" => 500,  // Lightning Bolt (channeled re-fire)
        "cfee0b02-6d91-4d34-869c-a7e54329060d" => 5230, // Ice Spike
        "4be1d681-c35d-4540-b255-c2910ac80664" => 8090, // Frostbite
        "e07f9b1a-64db-44ef-ba25-0e4378789ddc" => 8090, // Consuming Inferno
        "dfb8d247-1333-42eb-9730-a1c16d10584f" => 6580, // Delayed Lightning Bolt
        "66bdc017-30c5-4b5e-9753-215c45056f6a" => 6580, // Poison Cloud
        "9fdc4d52-ce90-44f8-9b5d-21f31e27dbda" => 8090, // Paralyze
        "4e760726-b012-4b25-bc92-0cd6312d6601" => 6000, // Absorb
        "c4b48518-e847-4f3d-81a2-2856bdb4ed98" => 7500, // Blizzard Armor
        "91078132-ef5c-492a-97f2-ac69be5140a8" => 8000, // Resist Elements
        "65ede044-d68a-4b2b-8f0c-02075ad133cc" => 7500, // Ward
        "eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13" => 5830, // Quick Strikes
        "cdab44fb-6ff6-4701-a4ec-d19cce79e49f" => 5830, // Piercing Strikes
        "ce6b63e9-9f18-49c4-aee0-51f7985f9892" => 8090, // Power Attack
        "69ffa3fd-deb7-4824-bab6-ac6450f19676" => 6700, // Harrying Bash
        "9b915ec3-c63b-4b62-b417-4c5436d45fc1" => 6700, // Staggering Bash
        "f9a2373b-a84f-4716-90ce-165baa2dd6ed" => 6700, // Shield Bash
        "ba61ce46-163f-4a61-8ede-f5b7ae365e40" => 6700, // Reflecting Bash
        "1e7f0dd6-6015-4f65-b811-3246e407e330" => 8650, // Dodging Strike
        "be56c560-a4ba-47ad-8513-f24c342ca594" => 8650, // Adrenaline Dodge
        "e08f95de-85bb-4829-ba7e-cf45bc6fb422" => 8750, // Recovery Strikes
        "cc768bae-a063-4885-8207-f39c6542fb36" => 8090, // Guardbreaker
        _ => return ABILITY_COOLDOWN,
    };
    Duration::from_millis(ms)
}

/// How long a `PlayerBlockingStateChange` (41) holds the guard up before it
/// auto-expires (a fresh op41 refreshes it). The dump's `PvpDefaultSettings`
/// `BLOCK_OPTIMAL_TIME` is 2.0s (docs/blades-combat-formulae.md §2); we use it as the
/// block window since the on/off flag isn't byte-pinned from a two-sided capture.
const BLOCK_WINDOW: Duration = Duration::from_secs(2);

/// True iff `user_data` is a `PlayerCombatInputActivate` (op46) frame.
/// These have carrier `0x2e` (46) — NOT the generic `0x36` UserMessage carrier.
fn is_op46(user_data: &[u8]) -> bool {
    user_data.get(1) == Some(&CARRIER_OP46)
}

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
                return Vec::new(); // no damage on press
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
                return resolve_swing(combat, sender, target_slot, swing_factor, now);
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
        reconcile_paralysis(f, now);
        f.prune_negation_pools(now);
    }
    // op41 PlayerBlockingStateChange (c2s) — the client raised/refreshed its guard.
    // Apply a BLOCK state on the sender: incoming hits within the block window are
    // reduced/negated per `damage::block_outcome` (optimal on the matching side,
    // late/half otherwise). This is the block-as-input wiring (was a resolve.rs TODO).
    // Bounded by `BLOCK_WINDOW` (the dump's `BLOCK_OPTIMAL_TIME` 2.0s) and auto-expired
    // by `reconcile_block`, since the on/off flag isn't byte-pinned from a two-sided
    // capture — a fresh op41 simply refreshes the window. No damage, no s2c (the client
    // animates its own guard; the opponent learns of the block via the reduced
    // ReceiveDamage flags when it lands a hit). Handled BEFORE the swing fallback so a
    // block frame is never mis-resolved as an attack.
    if messages::is_player_blocking_state_change(user_data) {
        if sender < combat.fighters.len() {
            let side = messages::blocking_active_side(user_data).unwrap_or(ActiveSide::Middle);
            let f = &mut combat.fighters[sender];
            // Record block-raise instant for OPTIMAL→LATE timeout logic.
            // If the fighter re-raises within the recovery window (`last_block_dropped_at`
            // + OPTIMAL_BLOCK_RECOVERY_SECS), the new block starts as LATE (not OPTIMAL).
            // `block_phase()` in damage::block_outcome handles this via `block_raised_at` +
            // `last_block_dropped_at`. [PvpDefaultSettings dump.cs 427014-427015]
            f.actor_state = super::state::ActorStateType::Blocking;
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
    if messages::is_noncombat_user_message(user_data) {
        debug!("combat: slot {sender} carrier-54 handshake/flow frame (not a swing) — ignored");
        return Vec::new();
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

    // A RequestExecuteAbility (spell/ability) vs a weapon swing.
    if let Some(ea) = input::parse_execute_ability(user_data) {
        resolve_ability_cast(combat, sender, target_slot, user_data, &ea, now)
    } else {
        // Carrier-0x36 swing (no `_held` parse — no charge info). Use ×1.0 (no crit).
        // Full charge crits arrive via op46 (carrier 0x2e, handled above).
        resolve_swing(combat, sender, target_slot, 1.0, now)
    }
}

/// A weapon auto-attack (committed swing), throttled per attacker.
///
/// `swing_factor` is the held-charge crit multiplier:
///   - `1.0` for a normal (partial / uncharged) swing via carrier-0x36 or bot swings.
///   - `CRIT_FACTOR_*` for a full-charge crit dispatched from the op46 (0x2e) path
///     when the server-measured hold ≥ `CRITICAL_HOLD_SECS` (bug 4 fix).
fn resolve_swing(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    swing_factor: f32,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    if let Some(last) = combat.fighters[sender].last_swing {
        if now.duration_since(last) < SWING_COOLDOWN {
            debug!("combat: slot {sender} swing throttled (< {SWING_COOLDOWN:?} since last)");
            return Vec::new();
        }
    }
    combat.fighters[sender].last_swing = Some(now);

    // Swipe-input geometry: the raw c2s swipe body doesn't carry a decodable activeSide
    // here, so a normal auto-attack ALTERNATES Left/Right (a dagger combos via chained
    // alternating side-swings, §4.2) — this drives the combo ramp. The first swing of a
    // chain is Right (the s506 combo-0 reference). `register_combo_swing` increments the
    // chain on an alternating side and returns the depth for `combo_factor`.
    let next_side = match combat.fighters[sender].last_combo_side {
        ActiveSide::Right => ActiveSide::Left,
        _ => ActiveSide::Right, // None / Left / Middle → start (or restart) on Right
    };
    let combo_count = combat.fighters[sender].register_combo_swing(next_side);

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
        .insert(ea.ability_uuid.clone(), now + ability_cooldown(&ea.ability_uuid));

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

    debug!("combat: slot {sender} casts ability {} (tag {tag:?}, level {level}) → slot {target_slot}", ea.ability_uuid);

    // Route Ward and ResistElements to their specific handlers (no direct damage).
    // Generic abilities go through the standard spell-damage path.
    match tag {
        super::state::AbilityTag::Ward => {
            out.extend(apply_ward(combat, sender, now));
        }
        super::state::AbilityTag::ResistElements => {
            out.extend(apply_resist_elements(combat, sender, now));
        }
        super::state::AbilityTag::Generic => {
            let resolved = RetailDamageModel.resolve_ability(level, &combat.fighters[target_slot], ActiveSide::Middle, now);
            out.extend(emit_damage(combat, sender, target_slot, &resolved, now));
        }
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

/// `BURNING/FROZEN/ENERVATED/POISONED` DoT durations once they land (s506 op51: 5 s for
/// the elemental four; 4.89 s for the Paralyze-spell poison). [§5.3]
const CONDITION_DURATION_SECS: f32 = 5.0;
/// `Paralyzed` status duration = `ParalyzeAbility._duration` (s506 op51 = 3.1 s). [§5.4]
const PARALYZE_DURATION_SECS: f32 = 3.1;

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

/// `_percentHealthDamage` default for elemental DoT effects (game-data-driven; flagged
/// as a calibration guess). Derived from s506's dominant Poison DoT value of 3.87/tick
/// at ~L86 arena×3 maxHP ≈ 1290 HP → 3.87/1290 ≈ 0.003. Range observed: 1.25–7.73/tick.
/// **CALIBRATION FLAG**: the exact per-ability `_percentHealthDamage` requires the game's
/// Excel data. [docs/arena-combat-fidelity-iteration.md §Mechanic-4]
const DOT_PERCENT_HEALTH_PER_TICK: f32 = 0.003;

/// Resist-Elements status duration (11.5s), measured from op51 apply events across
/// sessions s127, s167, s293, s385 (multi-session analysis). Applies to all four
/// resistance types simultaneously. [docs/arena-combat-fidelity-iteration.md §Mechanic-3]
const RESIST_ELEMENTS_DURATION_SECS: f32 = 11.5;

/// Flat resistance value per element for Resist-Elements. Game-data-driven
/// (`ResistElementsAbility._resistanceAmount`). **CALIBRATION FLAG**: no direct
/// measurement from captures (no Resist-Elements hits in s506). Representative value
/// chosen so that a typical Poison hit (137 base) is partially reduced without full
/// negation. Calibrate against a session that has op51 ResistElements + matching
/// ReceiveDamage hits.
const RESIST_ELEMENTS_FLAT_AMOUNT: f32 = 50.0;

/// Ward `_wardArmor` default physical reduction (flat armor, subtracted from
/// incoming physical damage). Game-data from `WardAbility._wardArmor`. **CALIBRATION
/// FLAG**: the exact value requires game-data extraction. Representative: ~20 flat
/// physical armor (reduces a 113-base Slashing hit by ~18%).
const WARD_ARMOR_FLAT: f32 = 20.0;

/// Ward `_wardHealth` default negation pool size (HP-equivalent). Game-data from
/// `WardAbility._wardHealth`. **CALIBRATION FLAG**. Representative: 300 HP pool
/// (absorbs ~2 average hits before draining). [arena-status-resistance-spec §4.2]
const WARD_HEALTH_POOL: f32 = 300.0;

/// Ward duration — pool-managed (not time-managed); op51 events have duration=0 in
/// captures. We give it a generously long hard cap so the pool can drain naturally
/// before it expires. Retail: pool hits 0 → DamageNegated emitted and Ward removed.
const WARD_DURATION_SECS: f32 = 60.0;

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
    use super::state::{condition_for_element, ActorStateType, DamageType, StatusEffectType, PARALYZE_POISON_THRESHOLD_FRACTION};

    let mut out = Vec::new();
    let target_obj = combat.fighters[target_slot].net_object_id;
    let max_hp = combat.fighters[target_slot].max_health;

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
                let per_tick = DOT_PERCENT_HEALTH_PER_TICK * max_hp as f32;
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
            if *ty == DamageType::Poison && combat.fighters[target_slot].can_be_paralyzed {
                let paralyze_threshold = PARALYZE_POISON_THRESHOLD_FRACTION * max_hp as f32;
                let not_already_paralyzed =
                    combat.fighters[target_slot].actor_state != ActorStateType::Paralyzed;
                if recent >= paralyze_threshold && not_already_paralyzed {
                    let f = &mut combat.fighters[target_slot];
                    f.actor_state = ActorStateType::Paralyzed; // locks inputs (is_paralyzed)
                    f.state_entered = now;
                    f.blocking_until = None; // paralysed → guard drops
                    let frame = messages::change_combat_status_effect(
                        target_obj, true, StatusEffectType::Paralyzed, PARALYZE_DURATION_SECS, 0,
                    );
                    info!("combat: slot {target_slot} PARALYZED (poison {recent:.0} ≥ {paralyze_threshold:.0}) for {PARALYZE_DURATION_SECS}s");
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
    if f.actor_state == ActorStateType::Paralyzed
        && now.duration_since(f.state_entered) >= Duration::from_secs_f32(PARALYZE_DURATION_SECS)
    {
        f.actor_state = ActorStateType::Idle;
    }
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
fn apply_ward(combat: &mut MatchCombat, caster_slot: usize, now: Instant) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageNegationSource, NegationPool, StatusEffectType};
    let mut out = Vec::new();
    if caster_slot >= combat.fighters.len() {
        return out;
    }
    let f = &mut combat.fighters[caster_slot];
    let ward_expires = now + Duration::from_secs_f32(WARD_DURATION_SECS);
    // Add the negation pool.
    f.negation_pools.push(NegationPool {
        source: DamageNegationSource::Ward,
        remaining: WARD_HEALTH_POOL,
        expires_at: ward_expires,
        restoration_factor: 0.0, // Ward: pure negation, no heal-back
    });
    // Add transient flat physical armor (subtracted from incoming physical as a
    // transient resistance on the caster — `DamageType::Health` is NOT physical;
    // Slashing/Cleaving/Bashing are. We model ward armor as flat resist on physical
    // types using the transient_resistances mechanism).
    use super::state::DamageType;
    for ty in [DamageType::Slashing, DamageType::Cleaving, DamageType::Bashing] {
        f.transient_resistances.push((ty, WARD_ARMOR_FLAT, ward_expires));
    }
    let target_obj = f.net_object_id;
    info!("combat: slot {caster_slot} WARD applied (pool {WARD_HEALTH_POOL}, armor {WARD_ARMOR_FLAT}, duration {WARD_DURATION_SECS}s)");
    // op51 apply Ward=15, duration=0 (pool-managed, not time-managed per captures).
    let frame = messages::change_combat_status_effect(target_obj, true, StatusEffectType::Ward, 0.0, 0);
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
fn apply_resist_elements(combat: &mut MatchCombat, caster_slot: usize, now: Instant) -> Vec<(usize, Vec<u8>)> {
    use super::state::{DamageType, StatusEffectType};
    let mut out = Vec::new();
    if caster_slot >= combat.fighters.len() {
        return out;
    }
    let expires = now + Duration::from_secs_f32(RESIST_ELEMENTS_DURATION_SECS);
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
            .push((dmg_ty, RESIST_ELEMENTS_FLAT_AMOUNT, expires));
        let frame = messages::change_combat_status_effect(
            target_obj,
            true,
            effect_ty,
            RESIST_ELEMENTS_DURATION_SECS,
            0,
        );
        for slot in 0..combat.fighters.len() {
            out.push((slot, frame.clone()));
        }
    }
    info!(
        "combat: slot {caster_slot} RESIST ELEMENTS applied (flat {RESIST_ELEMENTS_FLAT_AMOUNT}/elem, {RESIST_ELEMENTS_DURATION_SECS}s)"
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
    if winner < combat.rounds_won.len() {
        combat.rounds_won[winner] += 1;
    }
    let match_won = combat.match_is_won();
    let loser_obj = combat.fighters.get(loser).map(|f| f.net_object_id).unwrap_or(0);
    let winner_obj = combat.fighters.get(winner).map(|f| f.net_object_id).unwrap_or(0);
    let loser_stats = combat.fighters.get(loser).map(|f| f.packed_stats()).unwrap_or(0);
    let winner_stats = combat.fighters.get(winner).map(|f| f.packed_stats()).unwrap_or(0);
    let winner_uuid = combat.fighters.get(winner).map(|f| f.loadout.character_uuid.clone()).unwrap_or_default();
    let loser_uuid = combat.fighters.get(loser).map(|f| f.loadout.character_uuid.clone()).unwrap_or_default();

    // 1) op29 PlayerDead for the loser. Carrier 0x36, props 0-6 (NetObjectInfo + the
    //    two packed-stats ULongs + a cause byte). Cause = WeaponManeuver(3), the s506
    //    final-blow value. [capture-proven layout — supersedes the old bare guess.]
    let dead_frame = messages::player_dead(loser_obj, loser_stats, winner_stats, DamageSource::WeaponManeuver as u8);
    // 3) op48 MatchPostRoundInfoMsg — the result (winner/loser char UUIDs + match id).
    //    matchId = the gameSessionId (the Match net-object's propId9). result_code 3 (s506).
    let result_frame = messages::match_post_round_info(
        combat.match_net_object_id,
        &winner_uuid,
        &loser_uuid,
        &combat.game_session_id,
        3,
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

        let out = on_c2s_input(&mut combat, 0, &block_frame, now);

        // A block MUST produce zero outbound frames (no damage, no error, no phantom swing).
        assert!(
            out.is_empty(),
            "block input must emit zero s2c frames (no damage), got {} frame(s)",
            out.len()
        );
        // And the fighter should now be in the Blocking state.
        assert_eq!(
            combat.fighters[0].actor_state,
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

    /// Op46 DOWN (button press): records `charge_press_at`; emits ZERO damage frames.
    #[test]
    fn op46_down_records_press_no_damage() {
        let now = Instant::now();
        let mut combat = make_live_combat(now);

        let down_frame = make_op46_frame(0x1234_5678, true);
        let out = on_c2s_input(&mut combat, 0, &down_frame, now);

        assert!(out.is_empty(), "op46 DOWN must not emit damage, got {} frame(s)", out.len());
        assert!(
            combat.fighters[0].charge_press_at.is_some(),
            "op46 DOWN must record charge_press_at"
        );
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
}

#[cfg(test)]
mod cooldown_data_tests {
    use super::*;

    #[test]
    fn authoritative_per_ability_cooldowns() {
        assert_eq!(ability_cooldown("d07a8d30-9a1c-49b0-866d-97a8aa1534cf"), Duration::from_millis(3540)); // Fireball
        assert_eq!(ability_cooldown("7fc15804-1637-40a9-8dcc-3ea1eb0f778d"), Duration::from_millis(500)); // Lightning Bolt (channel)
        assert_eq!(ability_cooldown("ce6b63e9-9f18-49c4-aee0-51f7985f9892"), Duration::from_millis(8090)); // Power Attack
        assert_eq!(ability_cooldown("65ede044-d68a-4b2b-8f0c-02075ad133cc"), Duration::from_millis(7500)); // Ward
        assert_eq!(ability_cooldown("not-a-real-uuid"), ABILITY_COOLDOWN); // fallback
    }
}
