//! Combat resolution: turn inbound c2s inputs (swipe / ability / block) and tick
//! events into authoritative s2c messages.
//!
//! A carrier-54 c2s input is either a **weapon swing** (auto-attack, throttled) or
//! a **`RequestExecuteAbility`** (spell/ability cast, on cooldown). Both resolve via
//! the RE-derived [`RetailDamageModel`] from the attacker's loadout → `ReceiveDamage`
//! to both players (+ a `PerformExecuteAbility` echo for casts); a fighter reaching
//! 0 HP ends the match (`PlayerDeadStateChange` + `MatchEndMatchMsg`).
//!
//! Still to wire (model + builders are ready): decoding the real swipe-input
//! geometry (`activeSide`/`swingFactor` — defaults to a Middle swing for now), the
//! `PlayerChannelingStateChange` (53) cast animation (positional framing is
//! build-specific), block as a c2s *input*, and status-effect/DoT ticks.

use std::time::{Duration, Instant};

use log::{debug, info};

use super::damage::{DamageModel, ResolvedDamage, RetailDamageModel};
use super::input;
use super::messages;
use super::state::{ActiveSide, DamageSource, FlowState, MatchCombat, NetObjectType};

/// Carrier MessageType (`user_data[1]`) of the combat-input family — `0x36` (54).
const CARRIER_USERMESSAGE: u8 = 0x36;

/// Minimum spacing between landed swings per attacker (stand-in for swipe-commit).
const SWING_COOLDOWN: Duration = Duration::from_millis(400);

/// Per-ability cooldown (representative; per-ability cooldowns come from game-data).
const ABILITY_COOLDOWN: Duration = Duration::from_millis(3000);

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
    if user_data.get(1) != Some(&CARRIER_USERMESSAGE) {
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

    // A RequestExecuteAbility (spell/ability) vs a weapon swing.
    if let Some(ea) = input::parse_execute_ability(user_data) {
        resolve_ability_cast(combat, sender, target_slot, user_data, &ea, now)
    } else {
        resolve_swing(combat, sender, target_slot, now)
    }
}

/// A weapon auto-attack (committed swing), throttled per attacker.
fn resolve_swing(
    combat: &mut MatchCombat,
    sender: usize,
    target_slot: usize,
    now: Instant,
) -> Vec<(usize, Vec<u8>)> {
    if let Some(last) = combat.fighters[sender].last_swing {
        if now.duration_since(last) < SWING_COOLDOWN {
            debug!("combat: slot {sender} swing throttled (< {SWING_COOLDOWN:?} since last)");
            return Vec::new();
        }
    }
    combat.fighters[sender].last_swing = Some(now);

    // (Next refinement: decode the real swipe-input activeSide/swingFactor; default
    // to a committed Middle swing.)
    let attacker_loadout = combat.fighters[sender].loadout.clone();
    let resolved = RetailDamageModel.resolve_attack(
        &attacker_loadout,
        &combat.fighters[target_slot],
        DamageSource::Attack,
        ActiveSide::Middle,
        1.0,
    );
    emit_damage(combat, sender, target_slot, &resolved)
}

/// A spell/ability cast: cooldown-gated, echoes `PerformExecuteAbility`, applies
/// Spell-source damage.
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
    combat
        .fighters[sender]
        .cooldowns
        .insert(ea.ability_uuid.clone(), now + ABILITY_COOLDOWN);

    let mut out = Vec::new();
    // PerformExecuteAbility (38) echo to both — the cast confirmation/visual.
    let perform = messages::perform_execute_ability(user_data, ea.sep_offset);
    out.push((sender, perform.clone()));
    out.push((target_slot, perform));

    // Spell damage (level from the equipped ability if we know it; else 1).
    let level = combat.fighters[sender]
        .loadout
        .abilities
        .iter()
        .find(|a| a.instance_uuid == ea.ability_uuid)
        .map(|a| a.level)
        .unwrap_or(1);
    debug!("combat: slot {sender} casts ability {} (level {level}) → slot {target_slot}", ea.ability_uuid);
    let resolved = RetailDamageModel.resolve_ability(level, &combat.fighters[target_slot], ActiveSide::Middle);
    out.extend(emit_damage(combat, sender, target_slot, &resolved));
    out
}

/// Apply a resolved hit: decrement the target, build the `ReceiveDamage` for both
/// players, and end the match if the target died.
fn emit_damage(
    combat: &mut MatchCombat,
    attacker_slot: usize,
    target_slot: usize,
    resolved: &ResolvedDamage,
) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let hp_before = combat.fighters[target_slot].health;
    combat.fighters[target_slot].take_damage(resolved.total.round().max(0.0) as u32);
    let hp_after = combat.fighters[target_slot].health;
    debug!(
        "combat damage: slot {attacker_slot} → slot {target_slot} | source {:?} | total {:.1} | HP {hp_before} → {hp_after} (−{})",
        resolved.source,
        resolved.total,
        hp_before.saturating_sub(hp_after),
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
            resolved.total,
            0,
            resolved.active_side,
            resolved.most_resisted,
            &resolved.components,
        )
    };
    out.push((target_slot, msg.clone()));
    out.push((attacker_slot, msg));

    if combat.fighters[target_slot].is_dead() {
        out.extend(end_match(combat, attacker_slot));
    }
    out
}

/// `winner` defeated its opponent: emit `PlayerDeadStateChange` for the loser +
/// `MatchEndMatchMsg` to everyone, and finish the match.
fn end_match(combat: &mut MatchCombat, winner: usize) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let loser = combat.opponent_of(winner).unwrap_or(winner);
    if winner < combat.rounds_won.len() {
        combat.rounds_won[winner] += 1;
    }
    combat.phase = FlowState::Finished;
    let loser_obj = combat.fighters.get(loser).map(|f| f.net_object_id).unwrap_or(0);
    let winner_obj = combat.fighters.get(winner).map(|f| f.net_object_id).unwrap_or(0);

    // op29 PlayerDead + op49 MatchEnd both have UNVERIFIED wire layouts (never
    // captured — see messages.rs). Build each once and log the exact emitted bytes:
    // the builders are infallible, so the failure mode we CAN'T otherwise see is the
    // client rejecting a malformed frame and hanging at match-end. With these two
    // lines, the next on-device capture validates the real format against what we
    // sent, instead of match-end being a silent mystery.
    let dead_frame = messages::player_dead(loser_obj);
    let end_frame = messages::match_end(winner_obj);
    info!(
        "combat: match end → winner slot {winner} (obj {winner_obj}), loser slot {loser} (obj {loser_obj}); \
         emitting PlayerDead + MatchEnd to {} player(s)",
        combat.fighters.len(),
    );
    info!("combat op29 PlayerDead [UNVERIFIED layout] {} bytes: {}", dead_frame.len(), hex(&dead_frame));
    info!("combat op49 MatchEnd  [UNVERIFIED layout] {} bytes: {}", end_frame.len(), hex(&end_frame));

    for slot in 0..combat.fighters.len() {
        out.push((slot, dead_frame.clone()));
        out.push((slot, end_frame.clone()));
    }
    out
}

/// Lowercase hex of an emitted frame, for logging the UNVERIFIED s2c layouts
/// (op29/op49) so the next capture can validate the exact bytes the server sent.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A bot fighter's auto-swing cadence. Slower than a human's `SWING_COOLDOWN` so the
/// player wins comfortably but sees real incoming damage — a fight, not a static dummy.
const BOT_SWING_COOLDOWN: Duration = Duration::from_millis(1800);

/// Tick-driven combat. Drives any BOT fighters (slots at/after `expected_peers`,
/// which have no real ENet peer — a solo-vs-bot match's 2nd fighter) to auto-swing
/// at their opponent on `BOT_SWING_COOLDOWN`. Real players are input-driven
/// (`on_c2s_input`); only bots act on the tick. (DoT/status-effect ticks will also
/// plug in here once that path is wired.)
pub fn on_tick(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    if !matches!(combat.phase, FlowState::StateTimeout) {
        return Vec::new();
    }
    let mut out = Vec::new();
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
            out.extend(resolve_swing(combat, bot, target, now));
        }
    }
    out
}
