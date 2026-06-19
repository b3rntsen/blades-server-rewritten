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
use super::state::{ActiveSide, DamageSource, FlowState, MatchCombat, MatchState, NetObjectType};

/// Carrier MessageType (`user_data[1]`) of the combat-input family — `0x36` (54).
const CARRIER_USERMESSAGE: u8 = 0x36;

/// Minimum spacing between landed swings per attacker (stand-in for swipe-commit).
const SWING_COOLDOWN: Duration = Duration::from_millis(400);

/// Per-ability cooldown (representative; per-ability cooldowns come from game-data).
const ABILITY_COOLDOWN: Duration = Duration::from_millis(3000);

/// How long a `PlayerBlockingStateChange` (41) holds the guard up before it
/// auto-expires (a fresh op41 refreshes it). The dump's `PvpDefaultSettings`
/// `BLOCK_OPTIMAL_TIME` is 2.0s (docs/blades-combat-formulae.md §2); we use it as the
/// block window since the on/off flag isn't byte-pinned from a two-sided capture.
const BLOCK_WINDOW: Duration = Duration::from_secs(2);

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
    // Reconcile any lapsed block windows first (so a stale guard never keeps reducing
    // damage), using `now`. Cheap; both fighters.
    for f in combat.fighters.iter_mut() {
        f.reconcile_block(now);
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
            f.actor_state = super::state::ActorStateType::Blocking;
            f.blocking_side = side;
            f.blocking_until = Some(now + BLOCK_WINDOW);
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
    let max_hp = combat.fighters[target_slot].max_health;
    combat.fighters[target_slot].take_damage(resolved.total.round().max(0.0) as u32);
    let hp_after = combat.fighters[target_slot].health;
    // Log the per-hit-damage-vs-maxHP ratio (the anti-one-shot calibration knob): a
    // basic swing should take a CHUNK (≤25% by the clamp), never one-shot. info-level
    // so the ghost-verify on the box shows the before→after HP without RUST_LOG=debug.
    let pct = if max_hp > 0 { 100.0 * resolved.total / max_hp as f32 } else { 0.0 };
    let total = resolved.total;
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
        out.extend(on_round_ending_death(combat, attacker_slot));
    }
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
