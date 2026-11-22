//! **Gear calculator** — one spell against a table of defender builds and
//! defender states, answered by the live combat code.
//!
//! Nothing here computes damage. A request is turned into the same
//! `CompleteCharacter` + `CompleteInventory` a stored character is, parsed by the
//! production [`loadout::from_character`], put into a throwaway [`MatchCombat`],
//! and the spell is landed with [`resolve::apply_ability_impact`] — the function
//! a real cast reaches. The answer is then read back off the two fighters: health
//! lost, `Paralyzed`, the elemental condition, the stagger. So a change to block,
//! resistance, Ward, the perks or a threshold shows up here with no second copy
//! to keep in step.
//!
//! It is pure: no database, no match registry, no network frame leaves this
//! module (the frames the engine builds are dropped), and the combat log lines it
//! would otherwise write are silenced by the caller (`arena::gear_calc`).
//!
//! # What a cell means
//!
//! * `hit` is the FIRST impact, exactly as resolved: the spell's components after
//!   the attacker's bonuses, after the block, after resistance, and after
//!   Ward/Absorb pools. For a one-hit spell that is the whole spell.
//! * `total_health` is the health the defender lost over the WHOLE spell — every
//!   tick of a channel (Frostbite, Consuming Inferno) and every Thunderstorm bolt.
//!   The defender's block phase is held for the whole spell; buffs (Resist
//!   Elements, Ward, a potion) run from the instant of the cast and lapse on their
//!   own shipped durations.
//! * `paralysed` / `conditioned` / `stunned` are read off the defender afterwards.
//!   `conditioned` is the elemental condition (Burning / Frozen / Enervated /
//!   Poisoned): the server lands it when the element's damage in the last 5 s
//!   reaches 25 % of the defender's un-tripled max health, so one cast only
//!   conditions if it clears that alone — `prior_element_damage` is the damage of
//!   that element the defender has already taken inside the window.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use blades_lib::user_data::{
    Backpack, CompleteCharacter, CompleteInventory, EquippedItems, Loadout as InvLoadout, Treasury,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::damage::{self, DamageModel, RetailDamageModel};
use super::gamedata::{self, AbilityKind};
use super::perks::CasterPerks;
use super::state::{
    ActiveSide, ActorStateType, DamageSource, DamageType, Fighter, Loadout, MatchCombat,
    StatusEffectType, condition_for_element,
};
use super::{loadout, resolve};

/// Request limits. The table is recomputed on every edit in the browser and the
/// production box has two cores and live matches, so the work is bounded here
/// rather than trusted to the caller.
pub const MAX_DEFENDERS: usize = 24;
pub const MAX_STATES: usize = 16;
pub const MAX_ITEMS: usize = 12;
pub const MAX_PROPERTIES_PER_ITEM: usize = 8;
pub const MAX_ABILITIES: usize = 80;
/// Ticks one spell may run for. The longest shipped spell is a 3 s channel at the
/// 0.2 s PvP tick (15); Thunderstorm is 3 bolts.
const MAX_SPELL_TICKS: usize = 32;

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Property {
    pub id: Uuid,
    pub tier: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildItem {
    /// Item template. `None` is a plain ring or necklace: jewellery has no stat
    /// block, so only its enchantments and grade properties matter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<Uuid>,
    #[serde(default)]
    pub tempering_level: u8,
    #[serde(default)]
    pub enchants: Vec<Property>,
    /// Jewellery grade properties (`<Ability>BonusRanks`).
    #[serde(default)]
    pub grades: Vec<Property>,
    /// Which slot the item sits in, and what it is called — carried for the
    /// caller, never read here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One character build: what `from_character` reads, and nothing else.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Build {
    pub level: u16,
    /// `customization.CharacterUID` — race, for the racial innates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub race: Option<String>,
    #[serde(default)]
    pub stamina_points: u16,
    #[serde(default)]
    pub magicka_points: u16,
    #[serde(default)]
    pub items: Vec<BuildItem>,
    /// Learned abilities, `uuid -> rank`, BEFORE jewellery bonus ranks: perks,
    /// and the spells the calculator may cast for the build (Resist Elements,
    /// Ward, the attacking spell).
    #[serde(default)]
    pub abilities: HashMap<String, u8>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Block {
    None,
    Low,
    Optimal,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefenderState {
    pub id: String,
    pub block: Block,
    /// The defender cast Resist Elements (at the build's own rank) before the hit.
    #[serde(default)]
    pub resist_elements: bool,
    /// The defender has a fresh Ward up (at the build's own rank).
    #[serde(default)]
    pub ward: bool,
    /// The defender drank the top-tier resistance potion for the spell's element.
    #[serde(default)]
    pub potion: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefenderRow {
    pub id: String,
    pub build: Build,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Spell {
    pub uuid: String,
    /// Learned rank, before the attacker's jewellery bonus ranks.
    pub rank: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub spell: Spell,
    pub attacker: Build,
    /// Maximum Power's condition: the caster's magicka was full at the cast.
    #[serde(default)]
    pub magicka_full: bool,
    /// Damage of the spell's element the defender already took in the last 5 s
    /// (the conditioning window). 0 = this cast alone.
    #[serde(default)]
    pub prior_element_damage: f32,
    pub defenders: Vec<DefenderRow>,
    pub states: Vec<DefenderState>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub value: f32,
}

/// The first impact, step by step. Health-affecting components only in the sums.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    /// After the attacker's bonuses (perks, Fortify gear, racial), before the
    /// defender does anything.
    pub attack: f32,
    pub after_block: f32,
    pub after_resistance: f32,
    /// After Ward / Absorb pools: what reaches the health bar.
    pub landed: f32,
    /// The landed value of the spell's own element — what Paralyze compares.
    pub element_landed: f32,
    pub components: Vec<Component>,
    /// Defender's block rating against the spell's element for this hit (0 when
    /// not blocking), and the resistance rating charged against it.
    pub block_rating: f32,
    pub resistance_rating: f32,
    pub blocked: bool,
    pub optimal: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Flag {
    pub secs: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    pub state: String,
    pub hit: Hit,
    pub total_health: f32,
    pub ticks: u32,
    /// How far the spell's element is toward its condition after this cast:
    /// damage inside the 5 s window over the threshold, 1.0 once it has landed.
    pub condition_progress: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paralysed: Option<Flag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditioned: Option<Flag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stunned: Option<Flag>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefenderSummary {
    pub level: u16,
    pub max_health: u32,
    /// Elemental-condition threshold: 25 % of the un-tripled pool.
    pub condition_threshold: f32,
    pub armor_rating: f32,
    pub has_shield: bool,
    pub block_rating: f32,
    pub block_rating_optimal: f32,
    /// Block rating against the spell's element, low and optimal, with the typed
    /// material bonus and Elemental Protection.
    pub element_block_rating: f32,
    pub element_block_rating_optimal: f32,
    /// Permanent (gear) resistance rating against this spell.
    pub resistance_rating: f32,
    pub resist_elements_rank: u8,
    pub ward_rank: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: String,
    pub defender: DefenderSummary,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttackSummary {
    pub spell: &'static str,
    pub element: &'static str,
    pub learned_rank: u8,
    /// After jewellery bonus ranks, clamped to the ability's maximum.
    pub effective_rank: u8,
    pub source: &'static str,
    /// The rank's shipped number: `_damage`, or one tick of `_damagePerSecond`.
    pub base: f32,
    pub ticks: u32,
    /// After Maximum Power, Augmented <element>, Fortify gear and racial bonuses.
    pub attack: f32,
    pub resistance_piercing: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paralyze_threshold: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paralyze_secs: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stun_threshold: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stun_secs: Option<f32>,
    pub condition: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub attack: AttackSummary,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalcError {
    UnknownSpell,
    NotACastableSpell,
    TooLarge(&'static str),
    BadBuild(String),
}

impl std::fmt::Display for CalcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalcError::UnknownSpell => write!(f, "unknown spell"),
            CalcError::NotACastableSpell => {
                write!(
                    f,
                    "that ability is not a damaging spell the calculator casts"
                )
            }
            CalcError::TooLarge(what) => write!(f, "too many {what}"),
            CalcError::BadBuild(why) => write!(f, "bad build: {why}"),
        }
    }
}

pub fn damage_type_name(t: DamageType) -> &'static str {
    match t {
        DamageType::None => "none",
        DamageType::Slashing => "slashing",
        DamageType::Cleaving => "cleaving",
        DamageType::Bashing => "bashing",
        DamageType::Fire => "fire",
        DamageType::Frost => "frost",
        DamageType::Shock => "shock",
        DamageType::Poison => "poison",
        DamageType::Stamina => "stamina",
        DamageType::Magicka => "magicka",
        DamageType::Health => "health",
    }
}

pub fn condition_name(s: StatusEffectType) -> Option<&'static str> {
    Some(match s {
        StatusEffectType::Burning => "burning",
        StatusEffectType::Frozen => "frozen",
        StatusEffectType::Enervated => "enervated",
        StatusEffectType::Poisoned => "poisoned",
        _ => return None,
    })
}

/// The top-tier resistance potion for `element`, from the shipped consumables.
pub fn resist_potion(element: DamageType) -> Option<gamedata::AlchemyConsumable> {
    let effect: u16 = match element {
        DamageType::Fire => 60,
        DamageType::Frost => 61,
        DamageType::Shock => 62,
        DamageType::Poison => 63,
        _ => return None,
    };
    gamedata::alchemy_consumables()
        .filter(|c| c.effect_type == effect && c.charge_count == 0)
        .max_by(|a, b| a.value.total_cmp(&b.value))
}

/// Is this an ability the calculator can land: a player spell that ships a
/// damage number and hits the opponent?
///
/// Out: Wall of Fire (arms a trap, no hit of its own) and the three storm armors
/// (a shield whose stream lasts as long as the ice does — there is no "one cast").
pub fn is_castable_spell(a: &gamedata::Ability) -> bool {
    a.kind == AbilityKind::Spell
        && !a.enemy_only
        && a.damage_type.is_some()
        && damage::ships_damage(a.uuid, 1)
        && !resolve::is_firewall_ability(a.uuid)
        && !resolve::is_storm_armor(a.uuid, 1)
}

/// The production loadout for a calculator build.
pub fn loadout_for(build: &Build, name: &str) -> Result<Loadout, CalcError> {
    if build.items.len() > MAX_ITEMS {
        return Err(CalcError::TooLarge("items in a build"));
    }
    if build.abilities.len() > MAX_ABILITIES {
        return Err(CalcError::TooLarge("abilities in a build"));
    }
    let mut equipped = serde_json::Map::new();
    for (i, item) in build.items.iter().enumerate() {
        if item.enchants.len() > MAX_PROPERTIES_PER_ITEM
            || item.grades.len() > MAX_PROPERTIES_PER_ITEM
        {
            return Err(CalcError::TooLarge("properties on an item"));
        }
        // Slot and instance ids only have to be distinct: `from_character` walks
        // the VALUES and reads the template, the tempering level and the two
        // property lists.
        let slot = Uuid::from_u128(0x6ea2_ca1c_0000_4000_8000_0000_0000_0000 + i as u128);
        let props = |ps: &[Property]| -> Vec<Value> {
            ps.iter()
                .map(|p| json!({ "id": p.id, "tier": p.tier }))
                .collect()
        };
        equipped.insert(
            slot.to_string(),
            json!({
                "id": Uuid::from_u128(0x6ea2_ca1c_1000_4000_8000_0000_0000_0000 + i as u128),
                "slot": slot,
                "itemTemplateId": item.template_id.unwrap_or_else(Uuid::nil),
                "temperingLevel": item.tempering_level,
                "durability": 0,
                "properties": {
                    "ENCHANTING": props(&item.enchants),
                    "GRADING": props(&item.grades),
                },
            }),
        );
    }
    let equipped_items: EquippedItems = serde_json::from_value(Value::Object(equipped))
        .map_err(|e| CalcError::BadBuild(e.to_string()))?;
    let inventory = CompleteInventory {
        backpack: Backpack::default(),
        loadout: InvLoadout {
            equipped_items,
            ..Default::default()
        },
        treasury: Treasury::default(),
        overflow_treasury: Treasury::default(),
        backpack_version: 1,
        treasury_version: 0,
    };

    // Every learned non-perk ability is "equipped": the calculator decides what is
    // cast, and an ability that is never cast changes nothing by sitting in a slot.
    let mut slots = serde_json::Map::new();
    let mut levels = serde_json::Map::new();
    let mut uuids: Vec<&String> = build.abilities.keys().collect();
    uuids.sort();
    for uuid in uuids {
        let rank = build.abilities[uuid];
        let Some(a) = gamedata::ability(uuid) else {
            return Err(CalcError::BadBuild(format!("unknown ability {uuid}")));
        };
        if rank == 0 || a.enemy_only {
            continue;
        }
        levels.insert(uuid.clone(), json!(rank));
        if a.kind != AbilityKind::Perk {
            slots.insert(slots.len().to_string(), json!(uuid));
        }
    }
    let character = CompleteCharacter {
        name: name.to_string(),
        level: build.level,
        stamina_attribute_points: u32::from(build.stamina_points),
        magicka_attribute_points: u32::from(build.magicka_points),
        equipped_abilities: Value::Object(slots),
        abilities: Value::Object(levels),
        ..Default::default()
    };

    let mut lo = loadout::from_character(&character, &inventory);
    if let Some(race) = build.race.as_deref().filter(|r| !r.is_empty()) {
        loadout::apply_racial_innates_from_customization(&mut lo, &json!({ "CharacterUID": race }));
    }
    lo.display_name = name.to_string();
    Ok(lo)
}

fn rank_of(lo: &Loadout, ability_uuid: &str) -> u8 {
    lo.abilities
        .iter()
        .find(|a| a.instance_uuid == ability_uuid)
        .map(|a| a.level)
        .unwrap_or(0)
}

fn health_sum(cs: &[(DamageType, f32)]) -> f32 {
    cs.iter()
        .filter(|(t, _)| damage::is_health_type(*t))
        .map(|(_, v)| *v)
        .sum()
}

fn of_type(cs: &[(DamageType, f32)], ty: DamageType) -> f32 {
    cs.iter().filter(|(t, _)| *t == ty).map(|(_, v)| *v).sum()
}

/// Hold the defender's guard in the requested phase at `now`.
///
/// A guard is optimal for its first 2 s and low afterwards, so the phase is set by
/// when it went up. It is re-set before every tick: the question a column asks is
/// "what if they are low-blocking", not "what if they started blocking 2.5 s
/// before a 3 s channel". A paralysed or staggered defender's guard stays down,
/// as the engine left it.
fn hold_guard(f: &mut Fighter, block: Block, now: Instant) {
    if block == Block::None
        || matches!(
            f.actor_state(),
            ActorStateType::Paralyzed | ActorStateType::Staggered
        )
    {
        return;
    }
    if f.actor_state() != ActorStateType::Blocking {
        f.raise_guard(now);
    }
    let held = match block {
        Block::Optimal => Duration::ZERO,
        _ => Duration::from_secs_f32(super::state::BLOCK_OPTIMAL_TIME_SECS + 1.0),
    };
    f.block_raised_at = Some(now - held);
    f.blocking_until = Some(now + super::state::BLOCK_LEAK_GUARD);
}

struct Cast<'a> {
    spell: &'static gamedata::Ability,
    element: DamageType,
    level: u8,
    tag: super::state::AbilityTag,
    attacker: &'a Loadout,
    magicka_full: bool,
    prior_element_damage: f32,
}

fn run_cell(cast: &Cast<'_>, defender: &Loadout, state: &DefenderState) -> Cell {
    // A fixed distance from "now" so a low block's raise instant (in the past)
    // cannot underflow the monotonic clock.
    let now = Instant::now() + Duration::from_secs(60);
    let mut combat = MatchCombat::new(2, 2, now);
    combat.game_session_id = "gear-calc".to_string();
    combat
        .fighters
        .push(Fighter::new(0, 564, cast.attacker.clone(), now));
    combat
        .fighters
        .push(Fighter::new(1, 565, defender.clone(), now));

    // The defender's buffs, through the functions a real cast of each reaches.
    if state.resist_elements {
        let rank = rank_of(defender, gamedata::ids::RESIST_ELEMENTS);
        if rank > 0 {
            resolve::apply_resist_elements(&mut combat, 1, rank, now);
        }
    }
    if state.ward {
        let rank = rank_of(defender, gamedata::ids::WARD);
        if rank > 0 {
            resolve::apply_ward(&mut combat, 1, rank, now);
        }
    }
    if state.potion {
        if let (Some(potion), Some(status)) = (
            resist_potion(cast.element),
            resist_potion(cast.element).and_then(|p| resolve::alchemy_status(p.effect_type)),
        ) {
            resolve::apply_alchemy_resistance(&mut combat, 1, &potion, status, now);
        }
    }
    if cast.prior_element_damage > 0.0 {
        combat.fighters[1].record_element_damage(cast.element, cast.prior_element_damage, now);
    }
    hold_guard(&mut combat.fighters[1], state.block, now);

    // The first impact, step by step — the same calls the impact below makes, on
    // the same state, kept so the cell can show where the damage went.
    let caster = CasterPerks {
        magicka_full: cast.magicka_full,
        ..CasterPerks::of(&combat.fighters[0])
    };
    let resolved = RetailDamageModel.resolve_ability(
        cast.spell.uuid,
        cast.level,
        &caster,
        &combat.fighters[1],
        ActiveSide::Middle,
        now,
    );
    let block = if damage::source_is_blockable(resolved.source) {
        damage::block_outcome(&combat.fighters[1], cast.attacker, ActiveSide::Middle, now)
    } else {
        damage::block_outcome(&combat.fighters[1], cast.attacker, ActiveSide::None, now)
    };
    let mut after_block = resolved.pre_mitigation_components.clone();
    block.apply(
        &mut after_block,
        matches!(
            resolved.source,
            DamageSource::ContinuousSpell | DamageSource::ContinuousAttack
        ),
    );
    let mut landed = resolved.components.clone();
    {
        let mut probe = combat.fighters[1].clone();
        probe.apply_non_dodge_negation_pools(resolved.source, &mut landed, now);
    }
    let resistance_rating = combat.fighters[1].resistance_rating_against_source(
        cast.element,
        resolved.source,
        cast.attacker.elem_resist_piercing,
        cast.attacker.elem_resist_piercing_rating,
    ) + combat.fighters[1].transient_resistance_against(cast.element, now);
    let hit = Hit {
        attack: health_sum(&resolved.pre_mitigation_components),
        after_block: health_sum(&after_block),
        after_resistance: health_sum(&resolved.components),
        landed: health_sum(&landed),
        element_landed: of_type(&landed, cast.element),
        components: landed
            .iter()
            .filter(|(_, v)| *v > 0.0)
            .map(|(t, v)| Component {
                ty: damage_type_name(*t),
                value: *v,
            })
            .collect(),
        block_rating: if block.blocking {
            block.rating_for_type(cast.element)
        } else {
            0.0
        },
        resistance_rating,
        blocked: block.blocking,
        optimal: block.optimal,
    };

    // The cast itself.
    let hp_before = combat.fighters[1].health as f32 - combat.fighters[1].health_damage_carry;
    resolve::apply_ability_impact(
        &mut combat,
        0,
        1,
        cast.spell.uuid,
        cast.level,
        cast.tag,
        cast.magicka_full,
        now,
        false,
    );
    let mut ticks = 1u32;
    let mut t = now;
    while !combat.channels.is_empty() && (ticks as usize) < MAX_SPELL_TICKS {
        let Some(next) = combat.channels.iter().map(|c| c.next_tick_at).min() else {
            break;
        };
        t = next;
        hold_guard(&mut combat.fighters[1], state.block, t);
        let before = combat.fighters[1].health as f32 - combat.fighters[1].health_damage_carry;
        resolve::apply_channel_ticks(&mut combat, t);
        let after = combat.fighters[1].health as f32 - combat.fighters[1].health_damage_carry;
        if after < before || combat.channels.is_empty() {
            ticks += 1;
        }
    }
    let d = &combat.fighters[1];
    let hp_after = d.health as f32 - d.health_damage_carry;

    let paralysed = (d.actor_state() == ActorStateType::Paralyzed).then(|| Flag {
        secs: d.paralyze_secs,
        status: None,
    });
    let conditioned = condition_for_element(cast.element).and_then(|condition| {
        d.effects
            .iter()
            .find(|e| e.effect == condition && t < e.expires_at)
            .map(|e| Flag {
                secs: e
                    .expires_at
                    .saturating_duration_since(e.last_tick)
                    .as_secs_f32(),
                status: condition_name(condition),
            })
            .or_else(|| {
                // FlashFreeze freezes outright, through a timer and not the
                // conditioning window.
                d.status_timers
                    .iter()
                    .find(|(st, until)| *st == condition && t < *until)
                    .map(|(_, until)| Flag {
                        secs: until.saturating_duration_since(now).as_secs_f32(),
                        status: condition_name(condition),
                    })
            })
    });
    let stunned = d
        .staggered_until
        .filter(|until| t < *until)
        .map(|until| Flag {
            secs: until.saturating_duration_since(now).as_secs_f32(),
            status: None,
        });

    let condition_progress = match condition_for_element(cast.element) {
        Some(_) if conditioned.is_some() => 1.0,
        Some(c) if d.condition_threshold(c) > 0.0 => {
            (d.recent_element_damage(cast.element) / d.condition_threshold(c)).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };

    Cell {
        state: state.id.clone(),
        hit,
        total_health: (hp_before - hp_after).max(0.0),
        ticks,
        condition_progress,
        paralysed,
        conditioned,
        stunned,
    }
}

fn defender_summary(lo: &Loadout, cast: &Cast<'_>) -> DefenderSummary {
    let now = Instant::now() + Duration::from_secs(60);
    let mut f = Fighter::new(1, 565, lo.clone(), now);
    let elem_rating = |f: &mut Fighter, b: Block| -> f32 {
        hold_guard(f, b, now);
        damage::block_outcome(f, cast.attacker, ActiveSide::Middle, now)
            .rating_for_type(cast.element)
    };
    let low = elem_rating(&mut f, Block::Low);
    let optimal = elem_rating(&mut f, Block::Optimal);
    let source = if gamedata::ability_rank_clamped(cast.spell.uuid, u16::from(cast.level))
        .is_some_and(|r| r.damage().is_none())
    {
        DamageSource::ContinuousSpell
    } else {
        DamageSource::Spell
    };
    DefenderSummary {
        level: lo.level,
        max_health: f.max_health,
        condition_threshold: condition_for_element(cast.element)
            .map(|c| f.condition_threshold(c))
            .unwrap_or(0.0),
        armor_rating: lo.armor_rating_with_innates(),
        has_shield: lo.has_shield,
        block_rating: f.block_rating(false),
        block_rating_optimal: f.block_rating(true),
        element_block_rating: low,
        element_block_rating_optimal: optimal,
        resistance_rating: f.resistance_rating_against_source(
            cast.element,
            source,
            cast.attacker.elem_resist_piercing,
            cast.attacker.elem_resist_piercing_rating,
        ),
        resist_elements_rank: rank_of(lo, gamedata::ids::RESIST_ELEMENTS),
        ward_rank: rank_of(lo, gamedata::ids::WARD),
    }
}

/// Run the whole table.
pub fn calculate(req: &Request) -> Result<Response, CalcError> {
    if req.defenders.len() > MAX_DEFENDERS {
        return Err(CalcError::TooLarge("defender rows"));
    }
    if req.states.len() > MAX_STATES {
        return Err(CalcError::TooLarge("defender states"));
    }
    let spell = gamedata::ability(&req.spell.uuid).ok_or(CalcError::UnknownSpell)?;
    if !is_castable_spell(spell) {
        return Err(CalcError::NotACastableSpell);
    }
    let element = spell
        .damage_type
        .map(loadout::map_damage_type)
        .ok_or(CalcError::NotACastableSpell)?;

    // The attacker knows the spell at the requested rank; its jewellery then
    // raises it, exactly as for a stored character.
    let mut attacker_build = req.attacker.clone();
    attacker_build
        .abilities
        .insert(spell.uuid.to_string(), req.spell.rank.max(1));
    let attacker = loadout_for(&attacker_build, "calc attacker")?;
    let level = rank_of(&attacker, spell.uuid).max(1);
    let cast = Cast {
        spell,
        element,
        level,
        tag: loadout::ability_tag_for_template(spell.uuid),
        attacker: &attacker,
        magicka_full: req.magicka_full,
        prior_element_damage: req.prior_element_damage.clamp(0.0, 100_000.0),
    };

    let mut rows = Vec::with_capacity(req.defenders.len());
    let attack_summary: Option<(f32, u32, &'static str)>;
    for row in &req.defenders {
        let defender = loadout_for(&row.build, "calc defender")?;
        let cells: Vec<Cell> = req
            .states
            .iter()
            .map(|s| run_cell(&cast, &defender, s))
            .collect();
        rows.push(Row {
            id: row.id.clone(),
            defender: defender_summary(&defender, &cast),
            cells,
        });
    }

    // The attack on its own, against a bare target: nothing of the defender's
    // touches `pre_mitigation_components`, so any target gives the same number.
    {
        let now = Instant::now() + Duration::from_secs(60);
        let bare = Fighter::new(
            1,
            565,
            Loadout {
                level: attacker.level.max(1),
                ..Default::default()
            },
            now,
        );
        let me = Fighter::new(0, 564, attacker.clone(), now);
        let caster = CasterPerks {
            magicka_full: req.magicka_full,
            ..CasterPerks::of(&me)
        };
        let r = RetailDamageModel.resolve_ability(
            spell.uuid,
            level,
            &caster,
            &bare,
            ActiveSide::Middle,
            now,
        );
        let source = match r.source {
            DamageSource::ContinuousSpell => "channel",
            _ => "spell",
        };
        attack_summary = Some((
            health_sum(&r.pre_mitigation_components),
            damage::channel_ticks(spell.uuid, level).unwrap_or(1).max(1),
            source,
        ));
    }
    let (attack, ticks, source) = attack_summary.unwrap_or((0.0, 1, "spell"));
    let rank = gamedata::ability_rank_clamped(spell.uuid, u16::from(level));
    let base = rank
        .and_then(|r| {
            r.damage().or_else(|| {
                r.damage_per_second()
                    .map(|d| d * damage::CHANNEL_TICK_INTERVAL_SECS)
            })
        })
        .unwrap_or(0.0);
    let is_paralyze = cast.tag == super::state::AbilityTag::Paralyze;
    Ok(Response {
        attack: AttackSummary {
            spell: spell.name,
            element: damage_type_name(element),
            learned_rank: req.spell.rank.max(1),
            effective_rank: level,
            source,
            base,
            ticks: resolve::thunderstorm_bolts(spell.uuid, level)
                .map(|(n, _)| n)
                .unwrap_or(ticks),
            attack,
            resistance_piercing: attacker.elem_resist_piercing_rating
                + rank
                    .and_then(|r| r.elemental_resistance_piercing())
                    .unwrap_or(0.0),
            paralyze_threshold: is_paralyze.then(|| super::state::paralyze_damage_threshold(level)),
            paralyze_secs: is_paralyze.then(|| super::state::paralyze_duration_secs(level)),
            stun_threshold: rank.and_then(|r| r.damage_to_cause_stagger()),
            stun_secs: rank
                .filter(|r| r.damage_to_cause_stagger().is_some())
                .and_then(|r| r.stun_duration()),
            condition: condition_for_element(element).and_then(condition_name),
        },
        rows,
    })
}

#[cfg(test)]
pub(crate) mod tests;
