//! Why the server refused a player's ability/spell cast, and the one log line that says
//! so (tracker #418).
//!
//! Every refusal used to log at `debug`, which production does not record, so a report
//! like "the Resist Elements button does nothing in the arena" could not be told apart
//! from a client-side problem. A HUMAN's refusal now emits exactly one structured
//! `info` line; a bot's stays at `debug` because the bot loop retries casts every tick.
//!
//! The classification is pure: [`input_gate_refusal`] mirrors the shared input pre-gates
//! in `resolve::on_c2s_input` (round not live, no opponent, target dead, paralysed, staggered) and
//! [`cast_gate_refusal`] the gates in `resolve::resolve_ability_cast` (cooldown,
//! `CanCast`, shield, stamina, magicka), each in the same order the resolver applies
//! them. Neither changes any state.

use std::fmt;
use std::time::Instant;

use log::{debug, info};

use super::interrupts::{self, CastRefusal};
use super::state::{AbilityTag, ActorStateType, FlowState, MatchCombat};
use super::tables;

/// The gate that refused a cast, with the numbers that decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastRefusalReason {
    /// The round is not live (round-start setup or between rounds); combat input is
    /// dropped until `StateTimeout`.
    RoundNotLive { phase: FlowState },
    /// Solo match: there is no opponent slot to cast at.
    NoOpponent,
    /// The target is already dead.
    TargetDead,
    /// The caster is paralysed; all inputs are locked.
    Paralyzed,
    /// The caster is staggered and the ability is not Quick-tagged.
    Staggered { remaining_ms: u64 },
    /// This ability instance is still on cooldown.
    Cooldown { remaining_ms: u64 },
    /// `Actor.CanCast` refused (dead, Reckless Fury, or a state that admits no cast).
    CanCast {
        refusal: CastRefusal,
        state: ActorStateType,
        time_in_state_ms: u64,
    },
    /// A shield bash with no shield equipped.
    NoShield,
    /// Not enough stamina for a maneuver.
    InsufficientStamina { have: u32, need: u32 },
    /// Not enough magicka for a spell.
    InsufficientMagicka { have: u32, need: u32 },
}

impl CastRefusalReason {
    /// The stable `reason=` token in the log line.
    pub fn code(&self) -> &'static str {
        match self {
            Self::RoundNotLive { .. } => "round_not_live",
            Self::NoOpponent => "no_opponent",
            Self::TargetDead => "target_dead",
            Self::Paralyzed => "paralyzed",
            Self::Staggered { .. } => "staggered",
            Self::Cooldown { .. } => "cooldown",
            Self::CanCast { .. } => "can_cast",
            Self::NoShield => "no_shield",
            Self::InsufficientStamina { .. } => "insufficient_stamina",
            Self::InsufficientMagicka { .. } => "insufficient_magicka",
        }
    }
}

impl fmt::Display for CastRefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reason={}", self.code())?;
        match self {
            Self::RoundNotLive { phase } => write!(f, " phase={phase:?}"),
            Self::Staggered { remaining_ms } | Self::Cooldown { remaining_ms } => {
                write!(f, " remaining_ms={remaining_ms}")
            }
            Self::CanCast {
                refusal,
                state,
                time_in_state_ms,
            } => write!(
                f,
                " refusal={refusal:?} state={state:?} in_state_ms={time_in_state_ms}"
            ),
            Self::InsufficientStamina { have, need } => {
                write!(f, " stamina={have} need={need}")
            }
            Self::InsufficientMagicka { have, need } => {
                write!(f, " magicka={have} need={need}")
            }
            Self::NoOpponent | Self::TargetDead | Self::Paralyzed | Self::NoShield => Ok(()),
        }
    }
}

fn ms_until(now: Instant, until: Instant) -> u64 {
    until.saturating_duration_since(now).as_millis() as u64
}

/// The shared input pre-gates of `resolve::on_c2s_input`, as they apply to an ability
/// cast from `sender`, in the resolver's order: live round, opponent, target alive,
/// not paralysed, not staggered (Quick abilities excepted). `None` when the cast gets past them (or
/// when a slot is out of range, which the resolver drops without a reason).
pub fn input_gate_refusal(
    combat: &MatchCombat,
    sender: usize,
    ability_uuid: &str,
    now: Instant,
) -> Option<CastRefusalReason> {
    if combat.phase != FlowState::StateTimeout {
        return Some(CastRefusalReason::RoundNotLive {
            phase: combat.phase,
        });
    }
    let caster = combat.fighters.get(sender)?;
    let Some(target_slot) = combat.opponent_of(sender) else {
        return Some(CastRefusalReason::NoOpponent);
    };
    let target = combat.fighters.get(target_slot)?;
    if target.is_dead() {
        return Some(CastRefusalReason::TargetDead);
    }
    if caster.is_paralyzed() {
        return Some(CastRefusalReason::Paralyzed);
    }
    if caster.is_staggered(now) && !interrupts::is_quick(ability_uuid) {
        let remaining_ms = caster.staggered_until.map_or(0, |t| ms_until(now, t));
        return Some(CastRefusalReason::Staggered { remaining_ms });
    }
    None
}

/// The gates of `resolve::resolve_ability_cast`, in the resolver's order: cooldown,
/// `CanCast` (human maneuvers excepted, as there), then shield, stamina and magicka.
/// `level` and `tag` are what the resolver looked up for the cast.
pub fn cast_gate_refusal(
    combat: &MatchCombat,
    sender: usize,
    ability_uuid: &str,
    level: u8,
    tag: AbilityTag,
    now: Instant,
) -> Option<CastRefusalReason> {
    let caster = combat.fighters.get(sender)?;
    if let Some(&until) = caster.cooldowns.get(ability_uuid) {
        if now < until {
            return Some(CastRefusalReason::Cooldown {
                remaining_ms: ms_until(now, until),
            });
        }
    }
    if sender >= combat.expected_peers || tag != AbilityTag::Maneuver {
        if let Some(refusal) = interrupts::cast_refusal(caster, ability_uuid, now) {
            return Some(CastRefusalReason::CanCast {
                refusal,
                state: caster.actor_state(),
                time_in_state_ms: (caster.time_in_state(now) * 1000.0).max(0.0) as u64,
            });
        }
    }
    let (stam_cost, mag_cost) = tables::ability_cost(ability_uuid, level);
    if super::resolve::is_shield_bash(ability_uuid, level) && !caster.loadout.has_shield {
        return Some(CastRefusalReason::NoShield);
    }
    if stam_cost > 0 && caster.stamina < stam_cost {
        return Some(CastRefusalReason::InsufficientStamina {
            have: caster.stamina,
            need: stam_cost,
        });
    }
    if mag_cost > 0 && caster.magicka < mag_cost {
        return Some(CastRefusalReason::InsufficientMagicka {
            have: caster.magicka,
            need: mag_cost,
        });
    }
    None
}

/// The log line for a refused cast. `level`/`tag` are `None` before the resolver has
/// looked the ability up (the input pre-gates).
pub fn refusal_line(
    combat: &MatchCombat,
    sender: usize,
    ability_uuid: &str,
    rank_tag: Option<(u8, AbilityTag)>,
    reason: &CastRefusalReason,
) -> String {
    let (actor, character) = combat
        .fighters
        .get(sender)
        .map(|f| {
            (
                f.loadout.display_name.as_str(),
                f.loadout.character_uuid.as_str(),
            )
        })
        .unwrap_or(("", ""));
    let name = super::gamedata::ability(ability_uuid).map_or("?", |a| a.editor_name);
    let (rank, tag) = match rank_tag {
        Some((level, tag)) => (level.to_string(), format!("{tag:?}")),
        None => {
            // Pre-gates run before the resolver's lookup; report the equipped entry.
            combat
                .fighters
                .get(sender)
                .and_then(|f| {
                    f.loadout
                        .abilities
                        .iter()
                        .find(|a| a.instance_uuid == ability_uuid)
                })
                .map_or(("?".to_string(), "?".to_string()), |a| {
                    (a.level.to_string(), format!("{:?}", a.tag))
                })
        }
    };
    format!(
        "combat cast_refused: gsid={} slot={sender} character={character} actor={actor} \
         ability={ability_uuid} name={name} tag={tag} rank={rank} {reason}",
        combat.game_session_id,
    )
}

/// Is `slot` a human player (a real peer) rather than a bot?
pub fn is_human(combat: &MatchCombat, slot: usize) -> bool {
    slot < combat.expected_peers
}

/// Log one refusal: `info` for a human player, `debug` for a bot.
pub fn log_refusal(
    combat: &MatchCombat,
    sender: usize,
    ability_uuid: &str,
    rank_tag: Option<(u8, AbilityTag)>,
    reason: &CastRefusalReason,
) {
    let line = refusal_line(combat, sender, ability_uuid, rank_tag, reason);
    if is_human(combat, sender) {
        info!("{line}");
    } else {
        debug!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::gamedata;
    use super::super::loadout::starter;
    use super::super::state::{EquippedAbility, Fighter, FlowState};
    use super::*;

    fn uuid_of(editor: &str) -> &'static str {
        gamedata::ABILITIES
            .iter()
            .find(|a| a.editor_name == editor)
            .map(|a| a.uuid)
            .unwrap_or_else(|| panic!("{editor} missing from the shipped table"))
    }

    /// Two fighters with deep pools, live round. Slots below `expected_peers` are human.
    fn combat(now: Instant, expected_peers: usize) -> MatchCombat {
        let mut c = MatchCombat::new(2, expected_peers, now);
        for slot in 0..2 {
            let obj = c.alloc_net_object_id();
            let mut f = Fighter::new(slot, obj, starter(), now);
            f.stamina = 100_000;
            f.magicka = 100_000;
            c.fighters.push(f);
        }
        c.match_net_object_id = c.alloc_net_object_id();
        c.phase = FlowState::StateTimeout;
        c.phase_entered = now;
        c.game_session_id = "gsid-418".into();
        c
    }

    fn equip(c: &mut MatchCombat, uuid: &str, level: u8, tag: AbilityTag) {
        c.fighters[0].loadout.abilities.push(EquippedAbility {
            instance_uuid: uuid.to_string(),
            level,
            tag,
        });
    }

    #[test]
    fn a_castable_spell_passes_every_gate() {
        let now = Instant::now();
        let c = combat(now, 2);
        let re = uuid_of("ResistElements");
        assert_eq!(input_gate_refusal(&c, 0, re, now), None);
        assert_eq!(
            cast_gate_refusal(&c, 0, re, 1, AbilityTag::ResistElements, now),
            None
        );
    }

    #[test]
    fn input_gates_classify_in_resolver_order() {
        let now = Instant::now();
        let re = uuid_of("ResistElements");

        let mut setup = combat(now, 2);
        setup.phase = FlowState::Spawning;
        setup.fighters[0].set_actor_state(ActorStateType::Paralyzed, now);
        assert_eq!(
            input_gate_refusal(&setup, 0, re, now),
            Some(CastRefusalReason::RoundNotLive {
                phase: FlowState::Spawning
            })
        );

        let mut solo = MatchCombat::new(1, 1, now);
        let obj = solo.alloc_net_object_id();
        solo.fighters.push(Fighter::new(0, obj, starter(), now));
        solo.phase = FlowState::StateTimeout;
        assert_eq!(
            input_gate_refusal(&solo, 0, re, now),
            Some(CastRefusalReason::NoOpponent)
        );

        let mut c = combat(now, 2);
        c.fighters[1].health = 0;
        // Also paralysed: target-dead is checked first, as in the resolver.
        c.fighters[0].set_actor_state(ActorStateType::Paralyzed, now);
        assert_eq!(
            input_gate_refusal(&c, 0, re, now),
            Some(CastRefusalReason::TargetDead)
        );

        let mut c = combat(now, 2);
        c.fighters[0].set_actor_state(ActorStateType::Paralyzed, now);
        assert_eq!(
            input_gate_refusal(&c, 0, re, now),
            Some(CastRefusalReason::Paralyzed)
        );

        let mut c = combat(now, 2);
        c.fighters[0].staggered_until = Some(now + Duration::from_millis(1200));
        assert_eq!(
            input_gate_refusal(&c, 0, re, now),
            Some(CastRefusalReason::Staggered { remaining_ms: 1200 })
        );
        // A Quick ability acts through the stagger, as the resolver allows.
        assert_eq!(
            input_gate_refusal(&c, 0, uuid_of("RecoveryStrikes"), now),
            None
        );
    }

    #[test]
    fn cooldown_is_classified_with_its_remaining_time() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let re = uuid_of("ResistElements");
        c.fighters[0]
            .cooldowns
            .insert(re.to_string(), now + Duration::from_millis(4500));
        assert_eq!(
            cast_gate_refusal(&c, 0, re, 1, AbilityTag::ResistElements, now),
            Some(CastRefusalReason::Cooldown { remaining_ms: 4500 })
        );
        // Expired cooldown no longer refuses.
        assert_eq!(
            cast_gate_refusal(
                &c,
                0,
                re,
                1,
                AbilityTag::ResistElements,
                now + Duration::from_millis(4500)
            ),
            None
        );
    }

    #[test]
    fn can_cast_state_gate_is_classified_with_the_state() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let re = uuid_of("ResistElements");
        c.fighters[0].set_actor_state(ActorStateType::Charging, now);
        assert_eq!(
            cast_gate_refusal(&c, 0, re, 1, AbilityTag::ResistElements, now),
            Some(CastRefusalReason::CanCast {
                refusal: CastRefusal::NotAvailable,
                state: ActorStateType::Charging,
                time_in_state_ms: 0,
            })
        );
        // A human maneuver is not state-gated here (the resolver's exception).
        let pa = uuid_of("PowerAttack");
        assert!(!matches!(
            cast_gate_refusal(&c, 0, pa, 1, AbilityTag::Maneuver, now),
            Some(CastRefusalReason::CanCast { .. })
        ));
    }

    #[test]
    fn resource_and_shield_gates_carry_their_numbers() {
        let now = Instant::now();
        let fireball = uuid_of("Fireball");
        let (_, mag_cost) = tables::ability_cost(fireball, 1);
        assert!(mag_cost > 0, "Fireball ships a magicka cost");
        let mut c = combat(now, 2);
        c.fighters[0].magicka = mag_cost - 1;
        assert_eq!(
            cast_gate_refusal(&c, 0, fireball, 1, AbilityTag::Generic, now),
            Some(CastRefusalReason::InsufficientMagicka {
                have: mag_cost - 1,
                need: mag_cost,
            })
        );

        let pa = uuid_of("PowerAttack");
        let (stam_cost, _) = tables::ability_cost(pa, 1);
        assert!(stam_cost > 0, "Power Attack ships a stamina cost");
        let mut c = combat(now, 2);
        c.fighters[0].stamina = stam_cost - 1;
        assert_eq!(
            cast_gate_refusal(&c, 0, pa, 1, AbilityTag::Maneuver, now),
            Some(CastRefusalReason::InsufficientStamina {
                have: stam_cost - 1,
                need: stam_cost,
            })
        );

        let bash = uuid_of("ShieldBash");
        let mut c = combat(now, 2);
        c.fighters[0].loadout.has_shield = false;
        c.fighters[0].stamina = 0; // the shield gate is reported first
        assert_eq!(
            cast_gate_refusal(&c, 0, bash, 1, AbilityTag::Maneuver, now),
            Some(CastRefusalReason::NoShield)
        );
    }

    /// The classifier mirrors the resolver: every state it calls refused, the real
    /// op37 path refuses; the passing state is cast.
    #[test]
    fn classification_agrees_with_the_real_cast_path() {
        use super::super::{input, messages, resolve};
        let now = Instant::now();
        let re = uuid_of("ResistElements");
        let cast = |c: &mut MatchCombat| {
            let frame = messages::request_execute_ability(c.fighters[0].net_object_id, re);
            assert!(input::parse_execute_ability(&frame).is_some());
            resolve::on_c2s_input(c, 0, &frame, now)
        };
        let setups: Vec<(&str, Box<dyn Fn(&mut MatchCombat)>)> = vec![
            (
                "round_not_live",
                Box::new(|c| c.phase = FlowState::Spawning),
            ),
            ("target_dead", Box::new(|c| c.fighters[1].health = 0)),
            (
                "paralyzed",
                Box::new(|c| c.fighters[0].set_actor_state(ActorStateType::Paralyzed, now)),
            ),
            (
                "staggered",
                Box::new(|c| c.fighters[0].staggered_until = Some(now + Duration::from_secs(1))),
            ),
            (
                "cooldown",
                Box::new(|c| {
                    c.fighters[0]
                        .cooldowns
                        .insert(re.to_string(), now + Duration::from_secs(5));
                }),
            ),
            (
                "can_cast",
                Box::new(|c| c.fighters[0].set_actor_state(ActorStateType::Charging, now)),
            ),
        ];
        for (code, setup) in setups {
            let mut c = combat(now, 2);
            equip(&mut c, re, 1, AbilityTag::ResistElements);
            setup(&mut c);
            let reason = input_gate_refusal(&c, 0, re, now)
                .or_else(|| cast_gate_refusal(&c, 0, re, 1, AbilityTag::ResistElements, now))
                .unwrap_or_else(|| panic!("{code}: classifier passed the cast"));
            assert_eq!(reason.code(), code);
            assert!(cast(&mut c).is_empty(), "{code}: the resolver must refuse");
        }

        // Short of magicka: refused, but the caster's pools are re-stated (#109).
        let mut c = combat(now, 2);
        equip(&mut c, re, 1, AbilityTag::ResistElements);
        let (_, need) = tables::ability_cost(re, 1);
        assert!(need > 0, "Resist Elements ships a magicka cost");
        c.fighters[0].magicka = need - 1;
        assert_eq!(
            cast_gate_refusal(&c, 0, re, 1, AbilityTag::ResistElements, now).map(|r| r.code()),
            Some("insufficient_magicka")
        );
        let out = cast(&mut c);
        assert!(!out.is_empty());
        assert!(out
            .iter()
            .all(|(_, f)| messages::user_message_gmid(f) == Some(65)));
        assert_eq!(c.fighters[0].magicka, need - 1, "a refusal costs nothing");

        // Control: the passing state casts.
        let mut c = combat(now, 2);
        equip(&mut c, re, 1, AbilityTag::ResistElements);
        let out = cast(&mut c);
        assert!(out
            .iter()
            .any(|(_, f)| messages::user_message_gmid(f) != Some(65)));
        assert!(c.fighters[0].magicka < 100_000, "the cast was paid for");
    }

    #[test]
    fn humans_are_peers_and_bots_are_not() {
        let now = Instant::now();
        let vs_bot = combat(now, 1);
        assert!(is_human(&vs_bot, 0));
        assert!(!is_human(&vs_bot, 1));
        let pvp = combat(now, 2);
        assert!(is_human(&pvp, 0) && is_human(&pvp, 1));
    }

    #[test]
    fn the_line_names_match_character_ability_rank_and_reason() {
        let now = Instant::now();
        let mut c = combat(now, 2);
        let re = uuid_of("ResistElements");
        equip(&mut c, re, 3, AbilityTag::ResistElements);
        c.fighters[0].loadout.display_name = "SpaceMunk".into();
        c.fighters[0].loadout.character_uuid = "char-1".into();
        let reason = CastRefusalReason::InsufficientMagicka { have: 10, need: 42 };
        let line = refusal_line(&c, 0, re, None, &reason);
        assert_eq!(
            line,
            format!(
                "combat cast_refused: gsid=gsid-418 slot=0 character=char-1 actor=SpaceMunk \
                 ability={re} name=ResistElements tag=ResistElements rank=3 \
                 reason=insufficient_magicka magicka=10 need=42"
            )
        );
    }
}
