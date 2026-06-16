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
    // Combat only resolves once the match exists and isn't over.
    if matches!(combat.phase, FlowState::Connecting | FlowState::Finished) {
        return Vec::new();
    }
    if user_data.get(1) != Some(&CARRIER_USERMESSAGE) {
        return Vec::new();
    }
    let Some(target_slot) = combat.opponent_of(sender) else {
        return Vec::new(); // solo / bot match: no opponent
    };
    if sender >= combat.fighters.len() || target_slot >= combat.fighters.len() {
        return Vec::new();
    }
    if combat.fighters[target_slot].is_dead() {
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
    combat.fighters[target_slot].take_damage(resolved.total.round().max(0.0) as u16);

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
    for slot in 0..combat.fighters.len() {
        out.push((slot, messages::player_dead(loser_obj)));
        out.push((slot, messages::match_end(winner_obj)));
    }
    out
}

/// Tick-driven combat (DoT ticks, cooldown expiry, channel completion). No-op for
/// now — DoT/status ticks plug in here once the status-effect path is wired.
pub fn on_tick(_combat: &mut MatchCombat, _now: Instant) -> Vec<(usize, Vec<u8>)> {
    Vec::new()
}
