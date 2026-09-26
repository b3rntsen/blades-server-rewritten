//! Interrupts, the cast state gate and maneuver timing (combat-spec PR-07:
//! 06-D1, 05-D5, 07-D2, 07-D7, and the read part of M-cooldown-start).
//!
//! * **Interrupt.** A stagger or paralysis ends a spell's `AbilityChannel` wind-up, a
//!   channelled spell and a weapon maneuver. No damage step runs, the cooldown starts
//!   at the interrupt, and in PvP the server says so with op59 `InterruptAbility`
//!   (`ChannelingState$$EndChanneling@0x1a52578`, `ActorManeuverState$$OnExit@0x1d556b0`,
//!   `AbilityExecutor$$Interrupt@0x1e9b94c`; 06 §1.8, 05 §2.8, 07 §5.5).
//! * **Cast state gate.** `Actor$$CanCast@0x1c58f54` (07 §2): Dead, Reckless Fury, and
//!   a state gate that the Quick tag skips unless the caster is paralysed.
//! * **Maneuver timing.** The retail server queued a maneuver's hits at the authored
//!   `OnManeuverApplyDamage` times and ended it at `OnManeuverEnded`
//!   (`ManeuverServerImplementation$$HandleExecutionBegin@0x1e9748c`, 05 §2.2).

use std::time::{Duration, Instant};

use log::info;

use super::gamedata::{self, WeaponType};
use super::messages_state;
use super::state::{ActorStateType, Execution, Fighter, MatchCombat};

const FROSTBITE_UUID: &str = "4be1d681-c35d-4540-b255-c2910ac80664";

// ---------------------------------------------------------------------------
// The Quick tag
// ---------------------------------------------------------------------------

/// Every ability whose `tags` carry 7 (`Quick`) in the shipped `LearnableAbilityList`
/// (`reference/game-defs/abilities_full.json`), by editor name. `gamedata.rs` does not
/// carry the tag list, so it is restated here and pinned by a test.
const QUICK_TAGGED: &[&str] = &[
    "Absorb",
    "AdrenalineDodge",
    "BlizzardArmor",
    "DodgingStrike",
    "EchoWeapon",
    "Firewall",
    "FirestormArmor",
    "FocusingDodge",
    "MagickaSurge",
    "RecoveryStrikes",
    "RenewingDodge",
    "Spellbreaker",
    "TempestArmor",
    "Thunderstorm",
    "Ward",
];

/// Does this ability carry the `Quick` tag? A Quick ability skips `CanCast`'s state
/// gate, so it may be cast from any actor state except while paralysed
/// (`Actor$$CanCast@0x1c58f54`, 07 §2.4).
pub fn is_quick(ability_uuid: &str) -> bool {
    gamedata::ability(ability_uuid).is_some_and(|a| QUICK_TAGGED.contains(&a.editor_name))
}

// ---------------------------------------------------------------------------
// Cast state gate (bots)
// ---------------------------------------------------------------------------

/// Why `Actor$$CanCast@0x1c58f54` refuses a cast (`AbilityReasonFailure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastRefusal {
    /// 3 `DeadActor`.
    Dead,
    /// 2 `RecklessFury`: the Fury status forbids skills.
    RecklessFury,
    /// 4 `NotAvailable`: the actor's state admits no cast.
    NotAvailable,
}

/// `Actor.CanCast`'s checks 1-4 (07 §2.1): Dead, Reckless Fury, then the state gate,
/// which a Quick ability skips unless the caster is paralysed. The state gate passes
/// in `Idle`, in `Recovery` once past the weapon's combo point
/// (`Actor$$get_CanCastInCurrentState@0x1c59178`), and while `Channeling`. Every other
/// state refuses: Charging, Blocking, Staggered, Maneuver, Paralyzed, the swing beats.
///
/// Cost, cooldown and `CanBeCast` are checked by the caller as before.
pub fn cast_refusal(f: &Fighter, ability_uuid: &str, now: Instant) -> Option<CastRefusal> {
    cast_refusal_in(f, f.actor_state(), ability_uuid, now)
}

/// [`cast_refusal`] for a bot that will lower its guard first (a released guard goes
/// Blocking → Idle), which is what the bot loop does before it casts.
pub fn cast_refusal_after_guard(f: &Fighter, ability_uuid: &str, now: Instant) -> Option<CastRefusal> {
    let state = match f.actor_state() {
        ActorStateType::Blocking => ActorStateType::Idle,
        s => s,
    };
    cast_refusal_in(f, state, ability_uuid, now)
}

fn cast_refusal_in(
    f: &Fighter,
    state: ActorStateType,
    ability_uuid: &str,
    now: Instant,
) -> Option<CastRefusal> {
    if f.is_dead() {
        return Some(CastRefusal::Dead);
    }
    if f.has_reckless_fury(now) {
        return Some(CastRefusal::RecklessFury);
    }
    if is_quick(ability_uuid) && !f.is_paralyzed() {
        return None;
    }
    // The resolver never moves the logical state into Maneuver or Channeling (op58 and
    // op53 record them presentationally), so read those two from their own fields.
    if f.maneuver_active(now) {
        return Some(CastRefusal::NotAvailable);
    }
    if f.in_channel_pose(now) {
        return None;
    }
    let state_ok = match state {
        ActorStateType::Idle => true,
        ActorStateType::PlayerRecovery => f.time_in_state(now) > recovery_to_combo_secs(f),
        _ => false,
    };
    (!state_ok).then_some(CastRefusal::NotAvailable)
}

/// The weapon's `recoveryToComboTime`: the recovery progress after which a new attack,
/// or a cast, may begin. Synthetic weapons (no template) use the class values
/// 0.10 / 0.15 / 0.20 s (02 §4).
fn recovery_to_combo_secs(f: &Fighter) -> f32 {
    use super::tables::Weight;
    match f.loadout.weapon_template {
        Some(w) => w.recovery_to_combo_time,
        None => match f.loadout.weapon.weight {
            Some(Weight::Heavy) => 0.20,
            Some(Weight::Versatile) => 0.15,
            _ => 0.10,
        },
    }
}

// ---------------------------------------------------------------------------
// Maneuver timing
// ---------------------------------------------------------------------------

/// A maneuver's authored hit times and end, in seconds from the cast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManeuverTiming {
    /// `OnManeuverApplyDamage` times, in order.
    pub impacts: &'static [f32],
    /// `OnManeuverEnded`.
    pub end: f32,
}

/// `FirstPersonReactionsCombatController.ControllerType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerType {
    OneHanded,
    SmallWeapons,
    TwoHanded,
}

/// `FirstPersonReactionsCombatController$$DetermineControllerType@0x1b146f0`: a table
/// indexed by `weaponType - 1`, 0 (OneHanded) outside 1-9 (05 §2.2).
fn controller_type(weapon: Option<WeaponType>) -> ControllerType {
    use WeaponType::*;
    match weapon {
        Some(Dagger | HandAxe | LightHammer | Mace) => ControllerType::SmallWeapons,
        Some(Greatsword | Battleaxe | Warhammer) => ControllerType::TwoHanded,
        Some(Longsword | WarAxe) | None => ControllerType::OneHanded,
    }
}

// The per-family times, from `reference/game-defs/maneuver_anim_events.json`
// (extracted in blades-capture cf60ac3). Every member of a family shares one Animator
// state, so one clip and one event list. `MockAnimationData` itself is not shipped;
// these are the client clips it was authored from (05 §2.2, 05 §8).
//
// Checked against retail (22 sessions, ENet sentTime from the op38 echo, 2026-09-26):
// first hits Quick 0.183 / 0.200 (2H) s, second 0.701 s, Power 0.766 / 0.750 (2H),
// Dodge 1.117 / 1.133 (2H), one tick (~17 ms) ahead of the authored times because the
// echo leaves a tick after the maneuver starts. The caster's closing op39 carried
// time-in-state Quick 1.201 / 1.333, Power 0.933 / 0.916, Dodge 2H 1.217: the authored
// ends. The one exception is the 1H/SmallWeapons dodge, which retail ended at
// 1.234-1.250 s (70 casts) against the clip's 1.713 s; the retail figure is used,
// because an end that is too late would swallow the player's next inputs.
const POWER_1H: ManeuverTiming = ManeuverTiming { impacts: &[0.779_136], end: 0.917_049 };
const POWER_2H: ManeuverTiming = ManeuverTiming { impacts: &[0.751_629], end: 0.904_180 };
const QUICK_1H: ManeuverTiming = ManeuverTiming { impacts: &[0.195_024, 0.703_207], end: 1.196_565 };
const QUICK_2H: ManeuverTiming = ManeuverTiming { impacts: &[0.215_827, 0.714_207], end: 1.328_459 };
const DODGE_1H: ManeuverTiming = ManeuverTiming { impacts: &[1.133_333], end: 1.25 };
const DODGE_2H: ManeuverTiming = ManeuverTiming { impacts: &[1.137_825], end: 1.187_631 };
const BASH: ManeuverTiming = ManeuverTiming { impacts: &[0.331_469], end: 1.006_707 };
const FURY: ManeuverTiming = ManeuverTiming { impacts: &[], end: 0.871_108 };

/// The weapon type that picks a maneuver's controller. A synthetic weapon with no
/// template (bots, test fixtures) falls back on its weight: Heavy plays the 2H clips.
pub fn timing_weapon(f: &Fighter) -> Option<WeaponType> {
    use super::tables::Weight;
    f.loadout.weapon_template.map(|w| w.weapon_type).or_else(|| {
        matches!(f.loadout.weapon.weight, Some(Weight::Heavy)).then_some(WeaponType::Greatsword)
    })
}

/// The authored timing of a player maneuver for the caster's weapon, or `None` for
/// anything that is not one (spells, perks, enemy-only abilities).
///
/// A shield bash's clip starts after its `_blockDuration` guard phase: retail ended
/// bashes at time-in-state 1.500 s = 0.5 + 1.007 (160 casts). Its hit keeps the
/// resolver's own guard-then-strike timing (`ability_impact_delay`); retail landed it
/// at 0.80 s, which is recorded for the bash work rather than changed here.
pub fn maneuver_timing(
    ability_uuid: &str,
    level: u8,
    weapon: Option<WeaponType>,
) -> Option<ManeuverTiming> {
    let a = gamedata::ability(ability_uuid)?;
    let two_handed = controller_type(weapon) == ControllerType::TwoHanded;
    let pick = |one: ManeuverTiming, two: ManeuverTiming| if two_handed { two } else { one };
    Some(match a.editor_name {
        "PowerAttack" | "Skullcrusher" | "Guardbreaker" | "IndomitableSmash" => pick(POWER_1H, POWER_2H),
        "QuickStrikes" | "PiercingStrikes" | "VenomStrikes" | "RecoveryStrikes" => pick(QUICK_1H, QUICK_2H),
        "DodgingStrike" | "AdrenalineDodge" | "FocusingDodge" | "RenewingDodge" => pick(DODGE_1H, DODGE_2H),
        "ShieldBash" | "HarryingBash" | "StaggeringBash" | "ReflectingBash" => {
            let guard = gamedata::ability_rank_clamped(ability_uuid, u16::from(level.max(1)))
                .and_then(|r| r.block_duration())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(0.0);
            ManeuverTiming { end: guard + BASH.end, ..BASH }
        }
        "RecklessFury" => FURY,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// When the cooldown starts (M-cooldown-start, the read cases)
// ---------------------------------------------------------------------------

/// Seconds from the cast to the start of the ability's cooldown
/// (`Actor$$BeginAbilityExecution@0x1c59930` loads the cooldown with its timer
/// stopped; 07 §5.4):
///
/// * the six spells with an `AbilityBeginCooldown` step right after the channel
///   (Paralyze, Blind, Ice Spike, Lightning Bolt, Fireball, Resist Elements): the
///   channel;
/// * the two channelled spells (Frostbite, Consuming Inferno): the channel, whose end
///   is the end of execution;
/// * Magicka Surge: its duration (`AbilityDoMagickaSurge$$Update@0x1e95e94` runs for it);
/// * a weapon maneuver: `OnManeuverEnded` (`maneuver_end`).
///
/// Everything else keeps the cooldown at the cast, as before: when its execution ends
/// has not been read.
pub fn cooldown_start_offset(ability_uuid: &str, level: u8, maneuver_end: Option<f32>) -> f32 {
    if let Some(end) = maneuver_end {
        return end;
    }
    let (Some(a), Some(r)) = (
        gamedata::ability(ability_uuid),
        gamedata::ability_rank_clamped(ability_uuid, u16::from(level.max(1))),
    ) else {
        return 0.0;
    };
    let secs = match a.editor_name {
        "Paralyze" | "Blind" | "IceSpike" | "LightningBolt" | "Fireball" | "ResistElements" => {
            r.channel_duration().unwrap_or(0.0)
        }
        "MagickaSurge" => r.duration().unwrap_or(0.0),
        _ => channelled_secs(r),
    };
    if secs.is_finite() { secs.max(0.0) } else { 0.0 }
}

/// A channelled spell's channel: `_channelMaxLength` (Frostbite, Consuming Inferno).
/// Thunderstorm and Poison Cloud's cloud are not channels (06 §1.1, §1.9).
pub fn channelled_secs(r: &gamedata::AbilityRank) -> f32 {
    r.get(gamedata::AbilityField::ChannelMaxLength).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.0)
}

/// The interruptible phase of a spell: its `AbilityChannel` wind-up
/// (`_channelDuration`), or the whole channel of a channelled spell. Zero for a spell
/// that never enters `Channeling` (Ward, Absorb, Thunderstorm, the armors, Surge,
/// Wall of Fire; 06 §1.9), which a stagger therefore cannot interrupt.
pub fn spell_interruptible_secs(ability_uuid: &str, level: u8) -> f32 {
    gamedata::ability_rank_clamped(ability_uuid, u16::from(level.max(1)))
        .map(|r| r.channel_duration().filter(|v| v.is_finite() && *v > 0.0).unwrap_or_else(|| channelled_secs(r)))
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Interrupt processing
// ---------------------------------------------------------------------------

/// Record a cast's interruptible phase. `until <= started_at` records nothing.
pub fn begin_execution(f: &mut Fighter, execution: Execution) {
    f.executions.retain(|e| e.until > execution.started_at);
    if execution.until > execution.started_at {
        f.executions.push(execution);
    }
}

/// Cancel what every pending stagger or paralysis interrupted, and put op59 on the
/// wire for each (`selfInterrupt` false: the state entered is Staggered or Paralyzed,
/// `ChannelingState$$EndChanneling@0x1a52578`).
///
/// Called before an impact lands, before a channel ticks, before a new cast is
/// accepted and after the actor-state drain, so a stagger always wins against work
/// that is due in the same tick.
pub fn process_interrupts(combat: &mut MatchCombat, now: Instant) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    for slot in 0..combat.fighters.len() {
        let Some(at) = combat.fighters[slot].interrupt_pending.take() else {
            continue;
        };
        let hit: Vec<Execution> = {
            let f = &mut combat.fighters[slot];
            let (hit, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut f.executions)
                .into_iter()
                .filter(|e| e.until > now.min(at))
                .partition(|e| e.started_at <= at && at < e.until);
            f.executions = keep;
            hit
        };
        out.extend(end_executions(combat, slot, &hit, at, false));
    }
    out
}

/// A maneuver cast while another runs replaces it: `ChangeManeuverInProgress@0x1d55a54`
/// self-interrupts the old one, so its queued hits are lost (05 §2.8).
pub fn replace_running_maneuver(combat: &mut MatchCombat, slot: usize, now: Instant) -> Vec<(usize, Vec<u8>)> {
    let hit: Vec<Execution> = {
        let f = &mut combat.fighters[slot];
        let (hit, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut f.executions)
            .into_iter()
            .filter(|e| e.until > now)
            .partition(|e| e.is_maneuver);
        f.executions = keep;
        hit
    };
    end_executions(combat, slot, &hit, now, true)
}

fn end_executions(
    combat: &mut MatchCombat,
    slot: usize,
    ended: &[Execution],
    at: Instant,
    self_interrupt: bool,
) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let viewers = combat.fighters.len();
    for e in ended {
        let same = |sender: usize, uuid: &str, cast_at: Instant| {
            sender == slot && uuid == e.ability_uuid && cast_at == e.started_at
        };
        // Only what was still to come at `at`: an impact already due then lands.
        let before = combat.pending_impacts.len();
        combat
            .pending_impacts
            .retain(|p| !(same(p.sender, &p.ability_uuid, p.cast_at) && p.due > at));
        let dropped_impacts = before - combat.pending_impacts.len();
        let mut stopped_channel = false;
        let mut stopped_frostbite_targets = Vec::new();
        for c in combat.channels.iter_mut() {
            if same(c.caster_slot, &c.ability_uuid, c.cast_at) && c.remaining_ticks > 0 {
                if c.ability_uuid == FROSTBITE_UUID {
                    stopped_frostbite_targets.push(c.target_slot);
                }
                c.remaining_ticks = 0;
                stopped_channel = true;
            }
        }
        for target in stopped_frostbite_targets {
            if let Some(f) = combat.fighters.get_mut(target) {
                f.frostbite_slow_until = None;
            }
        }
        // A spell whose impact already landed, or whose channel already ended (its
        // target left), has nothing left to interrupt. A maneuver always does: the
        // caster is in `Maneuver` until `OnManeuverEnded`, even after its hit.
        if !e.is_maneuver && dropped_impacts == 0 && !stopped_channel {
            continue;
        }
        let f = &mut combat.fighters[slot];
        if e.is_maneuver {
            // `ForceStop` runs every step's cleanup (06 §1.8): the maneuver's own
            // windows end with it — Combat Focus's Maneuver state, and a bash's guard
            // `_damageReduction` and Reflecting Bash's redirect.
            f.maneuver_state_until = f.maneuver_state_until.map(|t| t.min(at));
            f.reflect_until = f.reflect_until.map(|t| t.min(at));
            if let Some(guard) = gamedata::ability_rank_clamped(&e.ability_uuid, 1)
                .and_then(|r| r.block_duration())
                .filter(|v| v.is_finite() && *v > 0.0)
            {
                let guard_end = e.started_at + Duration::from_secs_f32(guard);
                for entry in f.transient_resistances.iter_mut() {
                    if entry.2 == guard_end {
                        entry.2 = at;
                    }
                }
            }
        }
        if stopped_channel {
            // `AbilityContinuousDamage`'s cleanup unregisters Frostbite's resistance
            // (06 §1.8): cut the channel-long entries it granted at the cast.
            end_channel_resistance(f, &e.ability_uuid, e.started_at, at);
        }
        // `EndExecution` → `EquippedAbility$$BeginCooldown@0x1b0ef60`: the full
        // cooldown runs from the interrupt, and the cost is not refunded (07 §5.5).
        f.cooldowns.insert(e.ability_uuid.clone(), at + e.cooldown);
        info!(
            "combat: slot {slot} {} INTERRUPTED {:.2}s in ({}, {dropped_impacts} impact(s) dropped{})",
            e.ability_uuid,
            at.saturating_duration_since(e.started_at).as_secs_f32(),
            if self_interrupt { "replaced" } else { "stagger/paralysis" },
            if stopped_channel { ", channel stopped" } else { "" },
        );
        let frame = messages_state::interrupt_ability(f.net_object_id, &e.ability_uuid, self_interrupt);
        for v in 0..viewers {
            out.push((v, frame.clone()));
        }
    }
    out
}

/// Frostbite grants `_resistanceBonus` on `_resistTypes` for the channel
/// (`resolve::apply_shipped_effects`); an interrupted channel ends it at `at`.
fn end_channel_resistance(f: &mut Fighter, ability_uuid: &str, started_at: Instant, at: Instant) {
    let Some(r) = gamedata::ability_rank_clamped(ability_uuid, 1) else {
        return;
    };
    if r.get(gamedata::AbilityField::ResistanceBonus).is_none() {
        return;
    }
    let secs = channelled_secs(r);
    if secs <= 0.0 {
        return;
    }
    let granted_until = started_at + Duration::from_secs_f32(secs);
    for entry in f.transient_resistances.iter_mut() {
        if entry.2 == granted_until {
            entry.2 = at;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests. Expected values come from the shipped rank data (abilities_full.json /
// gamedata.rs), maneuver_anim_events.json, retail captures and combat-spec
// 05/06/07, never from the code under test.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use arena_proto::NetDataValue;

    use super::super::gamedata;
    use super::super::input;
    use super::super::loadout;
    use super::super::messages;
    use super::super::resolve::{self, drain_state_changes, on_c2s_input, on_tick};
    use super::super::state::{
        AbilityTag, ActorStateType, BotObservedState, BotOpponentSnapshot, DamageNegationSource,
        EquippedAbility, Fighter, FlowState, MatchCombat,
    };
    use super::*;

    fn uuid_of(editor: &str) -> &'static str {
        gamedata::ABILITIES
            .iter()
            .find(|a| a.editor_name == editor)
            .map(|a| a.uuid)
            .unwrap_or_else(|| panic!("{editor} missing from the shipped table"))
    }

    /// Two fighters, live round. `bots` = how many trailing slots are bots.
    fn combat(now: Instant, bots: usize) -> MatchCombat {
        let mut c = MatchCombat::new(2, 2 - bots, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            let mut f = Fighter::new(slot, obj, loadout::starter(), now);
            // Room for any cast in these fixtures (Magicka Surge costs 425).
            f.max_magicka = 5_000;
            f.magicka = 5_000;
            f.max_stamina = 5_000;
            f.stamina = 5_000;
            c.fighters.push(f);
        }
        c.phase = FlowState::StateTimeout;
        c.phase_entered = now;
        c
    }

    /// Cast `editor` from `slot` at the other slot through the real cast path.
    fn cast(
        c: &mut MatchCombat,
        slot: usize,
        editor: &str,
        tag: AbilityTag,
        at: Instant,
    ) -> Vec<(usize, Vec<u8>)> {
        let uuid = uuid_of(editor);
        if !c.fighters[slot].loadout.abilities.iter().any(|a| a.instance_uuid == uuid) {
            c.fighters[slot].loadout.abilities.push(EquippedAbility {
                instance_uuid: uuid.to_string(),
                level: 1,
                tag,
            });
        }
        let frame = messages::request_execute_ability(c.fighters[slot].net_object_id, uuid);
        let ea = input::parse_execute_ability(&frame).expect("op37 parses");
        resolve::resolve_ability_cast(c, slot, 1 - slot, &frame, &ea, at)
    }

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    fn count(out: &[(usize, Vec<u8>)], gmid: u8) -> usize {
        out.iter().filter(|(_, f)| messages::user_message_gmid(f) == Some(gmid)).count()
    }

    /// Every op59 as `(dest, ability id, selfInterrupt)`.
    fn op59s(out: &[(usize, Vec<u8>)]) -> Vec<(usize, String, bool)> {
        out.iter()
            .filter(|(_, f)| messages::user_message_gmid(f) == Some(59))
            .map(|(d, f)| {
                let nd = arena_proto::parse_netdata(&f[2..]);
                let flag = matches!(nd.get(5), Some(NetDataValue::Bool(true)));
                (*d, nd.string(4).unwrap_or_default().to_string(), flag)
            })
            .collect()
    }

    /// op50s addressed to `target`'s avatar, counted once (viewer 0's copy).
    fn hits_on(c: &MatchCombat, out: &[(usize, Vec<u8>)], target: usize) -> usize {
        let obj = i64::from(c.fighters[target].net_object_id);
        out.iter()
            .filter(|(d, f)| *d == 0 && messages::user_message_gmid(f) == Some(50))
            .filter(|(_, f)| arena_proto::parse_netdata(&f[2..]).int(0) == Some(obj))
            .count()
    }

    fn assert_op59(out: &[(usize, Vec<u8>)], uuid: &str, self_interrupt: bool) {
        let got = op59s(out);
        assert_eq!(got.len(), 2, "one op59 to each viewer, got {got:?}");
        let mut dests: Vec<usize> = got.iter().map(|g| g.0).collect();
        dests.sort_unstable();
        assert_eq!(dests, vec![0, 1]);
        for (_, id, flag) in &got {
            assert_eq!(id, uuid, "op59 must name the interrupted ability");
            assert_eq!(*flag, self_interrupt, "selfInterrupt");
        }
    }

    // --- the Quick tag -----------------------------------------------------

    /// The list restates abilities_full.json's tag-7 set (15 abilities), hand-copied
    /// from the shipped data; every name must still resolve in gamedata.rs.
    #[test]
    fn the_quick_tag_set_is_the_shipped_one() {
        for name in QUICK_TAGGED {
            assert!(is_quick(uuid_of(name)), "{name} is tag-7 in abilities_full.json");
        }
        assert_eq!(QUICK_TAGGED.len(), 15);
        // Controls: abilities whose shipped tags carry no 7.
        for name in [
            "Fireball", "LightningBolt", "IceSpike", "QuickStrikes", "PowerAttack",
            "HarryingBash", "RecklessFury", "ResistElements", "Frostbite",
        ] {
            assert!(!is_quick(uuid_of(name)), "{name} carries no tag 7");
        }
    }

    // --- 06-D1: spell wind-up and channel ------------------------------------

    /// Register PR-07 test 1: stagger the caster 0.3 s into Fireball (0.9 s channel).
    /// No op50, an op59 (selfInterrupt false) to both viewers, and the cooldown runs
    /// from the interrupt: 0.3 + 3.54 s.
    #[test]
    fn a_stagger_during_the_fireball_windup_cancels_it() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        let hp = c.fighters[1].health;
        cast(&mut c, 0, "Fireball", AbilityTag::Damage, t0);
        assert_eq!(c.pending_impacts.len(), 1, "Fireball waits for its 0.9 s channel");

        let hit_at = t0 + secs(0.3);
        assert!(c.fighters[0].apply_stagger_for(hit_at, 1.5));
        let out = drain_state_changes(&mut c, hit_at);
        assert_op59(&out, uuid_of("Fireball"), false);
        // Retail order: op59 before the op39 that enters Staggered (83/83 captured).
        let first59 = out.iter().position(|(_, f)| messages::user_message_gmid(f) == Some(59));
        let first39 = out.iter().position(|(_, f)| messages::user_message_gmid(f) == Some(39));
        assert!(first39.is_some() && first59 < first39, "op59 must precede the Staggered op39");

        let late = resolve::land_due_impacts(&mut c, t0 + secs(1.5));
        assert_eq!(count(&late, 50), 0, "an interrupted Fireball deals no damage");
        assert_eq!(c.fighters[1].health, hp);
        assert_eq!(
            c.fighters[0].cooldowns.get(uuid_of("Fireball")).copied(),
            Some(hit_at + secs(3.54)),
            "the cooldown restarts at the interrupt (EndExecution → BeginCooldown)",
        );
    }

    /// The control: no stagger, and Fireball lands after its channel.
    #[test]
    fn an_uninterrupted_fireball_still_lands() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "Fireball", AbilityTag::Damage, t0);
        assert_eq!(count(&resolve::land_due_impacts(&mut c, t0 + secs(0.89)), 50), 0);
        let out = resolve::land_due_impacts(&mut c, t0 + secs(0.91));
        assert_eq!(hits_on(&c, &out, 1), 1, "Fireball lands after its 0.9 s channel");
        assert!(op59s(&out).is_empty());
    }

    /// A stagger of the TARGET is not an interrupt of the caster.
    #[test]
    fn staggering_the_target_does_not_cancel_the_casters_spell() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "Fireball", AbilityTag::Damage, t0);
        c.fighters[1].apply_stagger_for(t0 + secs(0.3), 1.5);
        let out = drain_state_changes(&mut c, t0 + secs(0.3));
        assert!(op59s(&out).is_empty());
        let out = resolve::land_due_impacts(&mut c, t0 + secs(0.95));
        assert_eq!(hits_on(&c, &out, 1), 1);
    }

    /// Paralysis interrupts too: Paralyze's own 1.5 s channel, paralysed at 1.0 s.
    #[test]
    fn a_paralysis_during_a_windup_cancels_it() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "Paralyze", AbilityTag::Paralyze, t0);
        let at = t0 + secs(1.0);
        c.fighters[0].paralyze_secs = 2.0;
        c.fighters[0].set_actor_state(ActorStateType::Paralyzed, at);
        let out = drain_state_changes(&mut c, at);
        assert_op59(&out, uuid_of("Paralyze"), false);
        let out = resolve::land_due_impacts(&mut c, t0 + secs(2.0));
        assert_eq!(count(&out, 50), 0);
    }

    /// Delayed Lightning Bolt: channel 1.3 s, then a 4.0 s delay. Only the channel is
    /// interruptible (the delay is not a `Channeling` state), but death clears every
    /// executor (`ActorDeadState$$OnEnter@0x1fd3810`).
    #[test]
    fn delayed_lightning_bolt_is_interruptible_only_in_its_channel() {
        let t0 = Instant::now();
        // Staggered in the channel: cancelled.
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "DelayedLightningBolt", AbilityTag::Damage, t0);
        c.fighters[0].apply_stagger_for(t0 + secs(0.5), 1.5);
        let out = drain_state_changes(&mut c, t0 + secs(0.5));
        assert_op59(&out, uuid_of("DelayedLightningBolt"), false);
        assert_eq!(count(&resolve::land_due_impacts(&mut c, t0 + secs(5.4)), 50), 0);

        // Staggered in the delay: the bolt still lands at 1.3 + 4.0 s.
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "DelayedLightningBolt", AbilityTag::Damage, t0);
        c.fighters[0].apply_stagger_for(t0 + secs(2.0), 1.5);
        let out = drain_state_changes(&mut c, t0 + secs(2.0));
        assert!(op59s(&out).is_empty(), "nothing is channelling at 2.0 s");
        let out = resolve::land_due_impacts(&mut c, t0 + secs(5.35));
        assert_eq!(hits_on(&c, &out, 1), 1);

        // Killed in the delay: no bolt.
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "DelayedLightningBolt", AbilityTag::Damage, t0);
        c.fighters[0].health = 0;
        assert_eq!(count(&resolve::land_due_impacts(&mut c, t0 + secs(5.4)), 50), 0);
    }

    /// Frostbite channels for `_channelMaxLength` 3 s at 0.2 s ticks. Staggered at
    /// 1.0 s, the ticks stop and its channel-long physical resistance ends with them.
    #[test]
    fn a_stagger_stops_a_frostbite_channel() {
        let t0 = Instant::now();
        let run = |stagger: bool| {
            let mut c = combat(t0, 0);
            let mut out = cast(&mut c, 0, "Frostbite", AbilityTag::Damage, t0);
            let mut resist_left = 0;
            for ms in (10..=3500).step_by(10) {
                let t = t0 + Duration::from_millis(ms);
                if stagger && ms == 1000 {
                    c.fighters[0].apply_stagger_for(t, 1.5);
                }
                out.extend(on_tick(&mut c, t, false));
                out.extend(drain_state_changes(&mut c, t));
                if ms == 1000 {
                    // Resistance still running past the stagger instant.
                    resist_left = c.fighters[0]
                        .transient_resistances
                        .iter()
                        .filter(|(_, _, until)| *until > t)
                        .count();
                }
            }
            (hits_on(&c, &out, 1), op59s(&out), resist_left)
        };
        let (full, none, resist_full) = run(false);
        assert!(none.is_empty());
        assert!(full >= 15, "the control channel runs its 3 s ({full} hits)");
        assert!(resist_full > 0, "Frostbite grants its resistance for the channel");

        let (cut, op59, resist_cut) = run(true);
        assert_eq!(op59.len(), 2, "one op59 per viewer");
        assert!(op59.iter().all(|(_, id, flag)| id == uuid_of("Frostbite") && !flag));
        // Tick 1 at the cast, then one every 0.2 s up to the stagger at 1.0 s.
        assert!(cut <= 6, "no tick after the stagger ({cut} hits)");
        assert_eq!(resist_cut, 0, "the interrupted channel's resistance ends with it");
    }

    /// Thunderstorm never enters `Channeling` (06 §1.9), so a stagger does not stop it.
    #[test]
    fn a_stagger_does_not_stop_thunderstorm() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        let mut out = cast(&mut c, 0, "Thunderstorm", AbilityTag::Damage, t0);
        c.fighters[0].apply_stagger_for(t0 + secs(1.0), 1.5);
        out.extend(drain_state_changes(&mut c, t0 + secs(1.0)));
        for ms in (20..=7000).step_by(20) {
            out.extend(on_tick(&mut c, t0 + Duration::from_millis(ms), false));
        }
        assert!(op59s(&out).is_empty());
        assert_eq!(hits_on(&c, &out, 1), 3, "all three bolts still fall");
    }

    // --- 05-D5: maneuvers ----------------------------------------------------

    /// Power Attack's hit lands at its authored `OnManeuverApplyDamage`, 0.779 s in
    /// with a one-handed (or no) weapon — not at the cast. Retail: 0.766 s (26 casts).
    #[test]
    fn a_power_attack_lands_at_its_authored_time() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        let out = cast(&mut c, 0, "PowerAttack", AbilityTag::Maneuver, t0);
        assert_eq!(count(&out, 38), 2);
        assert_eq!(count(&out, 50), 0, "no damage at the cast");
        assert_eq!(count(&resolve::land_due_impacts(&mut c, t0 + secs(0.77)), 50), 0);
        let out = resolve::land_due_impacts(&mut c, t0 + secs(0.79));
        assert_eq!(hits_on(&c, &out, 1), 1);
    }

    /// Register PR-07 test 2: stagger a maneuver 0.2 s in; its queued hit is lost.
    #[test]
    fn a_stagger_during_a_maneuver_drops_its_hit() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        let hp = c.fighters[1].health;
        cast(&mut c, 0, "PowerAttack", AbilityTag::Maneuver, t0);
        let at = t0 + secs(0.2);
        c.fighters[0].apply_stagger_for(at, 1.5);
        let out = drain_state_changes(&mut c, at);
        assert_op59(&out, uuid_of("PowerAttack"), false);
        assert_eq!(count(&resolve::land_due_impacts(&mut c, t0 + secs(1.0)), 50), 0);
        assert_eq!(c.fighters[1].health, hp);
        assert!(!c.fighters[0].maneuver_active(at), "the interrupt ends the maneuver");
        // PowerAttackRank1._cooldown 8.09 s, from the interrupt.
        assert_eq!(
            c.fighters[0].cooldowns.get(uuid_of("PowerAttack")).copied(),
            Some(at + secs(8.09)),
        );
    }

    /// Until `OnManeuverEnded` (0.917 s for Power Attack) a guard press starts no
    /// block; after it, the same press raises the guard.
    #[test]
    fn a_maneuver_locks_out_block_and_swing_until_it_ends() {
        let block_press = |obj: i32| {
            let mut w = arena_proto::NetDataWriter::new();
            w.int(0, obj)
                .byte(1, 56)
                .byte(2, 3)
                .byte(3, 46)
                .bool(4, true)
                .float(5, 0.0)
                .bool(6, true);
            let mut f = messages::frame_for_test(w.finish());
            f[0] = 0x84;
            f
        };
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "PowerAttack", AbilityTag::Maneuver, t0);
        let obj = c.fighters[0].net_object_id;
        on_c2s_input(&mut c, 0, &block_press(obj), t0 + secs(0.3));
        assert_ne!(c.fighters[0].actor_state(), ActorStateType::Blocking, "no guard mid-maneuver");
        // A plain swing body is dropped as well.
        assert!(on_c2s_input(&mut c, 0, &[0xBE, 0x36], t0 + secs(0.4)).is_empty());

        on_c2s_input(&mut c, 0, &block_press(obj), t0 + secs(0.95));
        assert_eq!(
            c.fighters[0].actor_state(),
            ActorStateType::Blocking,
            "the guard rises once the maneuver has ended",
        );
    }

    /// A Quick dodge cast from a raised guard ends the guard (Blocking → Maneuver), and
    /// a guard release that arrives mid-maneuver is not swallowed by the lock.
    #[test]
    fn a_dodge_from_a_guard_drops_the_guard() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        let f = &mut c.fighters[0];
        f.set_actor_state(ActorStateType::Blocking, t0);
        f.blocking_until = Some(t0 + secs(8.0));
        f.block_raised_at = Some(t0);
        cast(&mut c, 0, "DodgingStrike", AbilityTag::Maneuver, t0 + secs(0.1));
        assert_ne!(c.fighters[0].actor_state(), ActorStateType::Blocking);
        assert!(c.fighters[0].blocking_until.is_none());
    }

    /// A stagger during a Reflecting Bash's guard ends its redirect with it.
    #[test]
    fn an_interrupted_bash_loses_its_guard_effects() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        c.fighters[0].loadout.has_shield = true;
        cast(&mut c, 0, "ReflectingBash", AbilityTag::Maneuver, t0);
        assert!(c.fighters[0].reflect_until.is_some_and(|t| t > t0 + secs(0.2)));
        let at = t0 + secs(0.2);
        c.fighters[0].apply_stagger_for(at, 1.5);
        let out = drain_state_changes(&mut c, at);
        assert_op59(&out, uuid_of("ReflectingBash"), false);
        assert!(c.fighters[0].reflect_until.is_some_and(|t| t <= at));
        assert!(c.fighters[0].transient_resistances.iter().all(|(_, _, until)| *until <= at));
    }

    /// A bot skips an ability that is still cooling down and casts a ready one.
    #[test]
    fn a_bot_casts_a_ready_ability_rather_than_waiting_on_one() {
        let t0 = Instant::now();
        let mut c = combat(t0, 1);
        for (name, tag) in [("MagickaSurge", AbilityTag::Generic), ("Fireball", AbilityTag::Damage)] {
            c.fighters[1].loadout.abilities.push(EquippedAbility {
                instance_uuid: uuid_of(name).to_string(),
                level: 1,
                tag,
            });
        }
        c.fighters[1].cooldowns.insert(uuid_of("MagickaSurge").into(), t0 + secs(15.0));
        let snapshot = BotOpponentSnapshot {
            state: BotObservedState::Idle,
            ability_uuid: None,
        };
        assert_eq!(
            resolve::bot_next_ready_ability(&mut c.fighters[1], "interrupt-test", 1, snapshot, t0)
                .as_deref(),
            Some(uuid_of("Fireball")),
        );
        // A raised guard does not hide a ready ability: the bot lowers it to cast.
        c.fighters[1].set_actor_state(ActorStateType::Blocking, t0);
        assert_eq!(
            resolve::bot_next_ready_ability(&mut c.fighters[1], "interrupt-test", 1, snapshot, t0)
                .as_deref(),
            Some(uuid_of("Fireball")),
        );
    }

    /// `ChangeManeuverInProgress@0x1d55a54`: a dodge cast during Power Attack replaces
    /// it. The Power Attack's hit is lost (op59, selfInterrupt true: the state entered
    /// is not Staggered/Paralyzed) and the dodge's own hit lands 1.133 s after it.
    #[test]
    fn a_new_maneuver_replaces_a_running_one() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "PowerAttack", AbilityTag::Maneuver, t0);
        let at = t0 + secs(0.3);
        let out = cast(&mut c, 0, "DodgingStrike", AbilityTag::Maneuver, at);
        assert_op59(&out, uuid_of("PowerAttack"), true);
        assert_eq!(count(&resolve::land_due_impacts(&mut c, t0 + secs(1.0)), 50), 0);
        let out = resolve::land_due_impacts(&mut c, at + secs(1.14));
        assert_eq!(hits_on(&c, &out, 1), 1, "the dodge's own hit lands");
    }

    /// The dodge window starts with the execution (04-DG1), not with the delayed hit.
    #[test]
    fn a_dodge_pool_is_granted_at_the_cast() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        let out = cast(&mut c, 0, "DodgingStrike", AbilityTag::Maneuver, t0);
        let dodges = |c: &MatchCombat| {
            c.fighters[0]
                .negation_pools
                .iter()
                .filter(|p| p.source == DamageNegationSource::Dodge)
                .count()
        };
        assert_eq!(dodges(&c), 1, "the dodge pool must be up at the cast");
        assert_eq!(count(&out, 50), 0, "the dodge's strike is still to come");
        // …and the later hit does not grant a second pool.
        resolve::land_due_impacts(&mut c, t0 + secs(1.2));
        assert!(dodges(&c) <= 1);
    }

    /// Timing table: controller type from the weapon (05 §2.2).
    #[test]
    fn maneuver_timing_follows_the_controller_type() {
        use gamedata::WeaponType::*;
        let qs = uuid_of("QuickStrikes");
        assert_eq!(maneuver_timing(qs, 1, Some(Dagger)).unwrap().impacts, &[0.195_024, 0.703_207]);
        assert_eq!(
            maneuver_timing(qs, 1, Some(Greatsword)).unwrap().impacts,
            &[0.215_827, 0.714_207]
        );
        assert_eq!(maneuver_timing(qs, 1, None).unwrap().end, 1.196_565);
        // A bash's clip follows its 0.5 s guard: retail ended bashes at 1.500 s.
        let end = maneuver_timing(uuid_of("HarryingBash"), 1, None).unwrap().end;
        assert!((end - 1.506_707).abs() < 1e-4, "{end}");
        assert!(maneuver_timing(uuid_of("Fireball"), 1, None).is_none());
    }

    // --- M-cooldown-start ----------------------------------------------------

    /// Magicka Surge's cooldown runs after its 10 s surge: a 20 s cycle, not 10 s.
    /// Frostbite's after its 3 s channel. Ward (no read execution length) at the cast.
    #[test]
    fn the_cooldown_starts_when_the_execution_ends() {
        let t0 = Instant::now();
        let mut c = combat(t0, 0);
        cast(&mut c, 0, "MagickaSurge", AbilityTag::Generic, t0);
        cast(&mut c, 0, "Frostbite", AbilityTag::Damage, t0);
        cast(&mut c, 0, "Ward", AbilityTag::Ward, t0);
        let cd = |c: &MatchCombat, n: &str| c.fighters[0].cooldowns.get(uuid_of(n)).copied();
        assert_eq!(cd(&c, "MagickaSurge"), Some(t0 + secs(10.0) + secs(10.0)));
        assert_eq!(cd(&c, "Frostbite"), Some(t0 + secs(3.0) + secs(8.09)));
        assert_eq!(cd(&c, "Ward"), Some(t0 + secs(7.5)));
    }

    // --- 07-D7: bot cast gates -------------------------------------------------

    #[test]
    fn a_bot_cannot_cast_under_reckless_fury_or_dead() {
        let t0 = Instant::now();
        let mut c = combat(t0, 1);
        c.fighters[1].reckless_fury_until = Some(t0 + secs(5.0));
        assert!(cast(&mut c, 1, "Fireball", AbilityTag::Damage, t0 + secs(1.0)).is_empty());
        // A Quick ability is refused too: Fury is checked before the state gate.
        assert!(cast(&mut c, 1, "Ward", AbilityTag::Ward, t0 + secs(1.0)).is_empty());
        // After the Fury: the cast goes through.
        assert_eq!(count(&cast(&mut c, 1, "Fireball", AbilityTag::Damage, t0 + secs(5.1)), 38), 2);

        let mut c = combat(t0, 1);
        c.fighters[1].health = 0;
        assert_eq!(cast_refusal(&c.fighters[1], uuid_of("Ward"), t0), Some(CastRefusal::Dead));
    }

    /// A human is not state-gated by the server: the same Fury cast is answered.
    #[test]
    fn a_human_cast_is_not_state_gated_server_side() {
        let t0 = Instant::now();
        let mut c = combat(t0, 1);
        c.fighters[0].reckless_fury_until = Some(t0 + secs(5.0));
        assert_eq!(count(&cast(&mut c, 0, "Fireball", AbilityTag::Damage, t0 + secs(1.0)), 38), 2);
    }

    /// Mid-maneuver a bot may cast only a Quick ability.
    #[test]
    fn a_bot_mid_maneuver_casts_only_quick_abilities() {
        let t0 = Instant::now();
        let mut c = combat(t0, 1);
        assert_eq!(count(&cast(&mut c, 1, "PowerAttack", AbilityTag::Maneuver, t0), 38), 2);
        let at = t0 + secs(0.3);
        assert!(cast(&mut c, 1, "Fireball", AbilityTag::Damage, at).is_empty());
        assert_eq!(count(&cast(&mut c, 1, "Ward", AbilityTag::Ward, at), 38), 2);
        // After OnManeuverEnded (0.917 s) the non-Quick cast is fine.
        assert_eq!(count(&cast(&mut c, 1, "Fireball", AbilityTag::Damage, t0 + secs(0.95)), 38), 2);
    }

    /// In a swing's recovery the cast waits for the combo point
    /// (`CurrentRecoveryProgress > RecoveryToComboTime`); Charging, Blocking and the
    /// swing beats refuse; Idle passes.
    #[test]
    fn a_bot_waits_for_the_combo_point_and_never_casts_from_a_charge_or_guard() {
        let t0 = Instant::now();
        let mut c = combat(t0, 1);
        let fb = uuid_of("Fireball");
        let combo = recovery_to_combo_secs(&c.fighters[1]);
        c.fighters[1].set_actor_state(ActorStateType::PlayerRecovery, t0);
        assert_eq!(
            cast_refusal(&c.fighters[1], fb, t0 + secs(combo * 0.5)),
            Some(CastRefusal::NotAvailable)
        );
        assert_eq!(cast_refusal(&c.fighters[1], fb, t0 + secs(combo + 0.01)), None);
        for state in [
            ActorStateType::Charging,
            ActorStateType::Blocking,
            ActorStateType::PlayerAutoAttack,
        ] {
            c.fighters[1].set_actor_state(state, t0);
            assert_eq!(
                cast_refusal(&c.fighters[1], fb, t0),
                Some(CastRefusal::NotAvailable),
                "{state:?}"
            );
            assert_eq!(cast_refusal(&c.fighters[1], uuid_of("Ward"), t0), None, "Quick passes {state:?}");
        }
        c.fighters[1].set_actor_state(ActorStateType::Idle, t0);
        assert_eq!(cast_refusal(&c.fighters[1], fb, t0), None);
    }
}
