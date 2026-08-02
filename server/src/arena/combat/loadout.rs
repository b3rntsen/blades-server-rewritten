//! Build a `Fighter`'s combat [`Loadout`] from the imported character.
//!
//! [`from_character`] is a **pure** parser (no DB) over the stored
//! `CompleteCharacter` + `CompleteInventory` (the matchmaker does the async query
//! and calls this).
//!
//! # Phase 3 — this now runs on real item data
//!
//! Every equipped item's `itemTemplateId` is looked up in [`gamedata`]:
//!
//! * **weapon** → `base_damage` / `damage_type` / `weapon_class` / `attack_delay` /
//!   `recovery_time` / `block_base` / `optimal_block_boost`, plus the item's own
//!   `tempering_level` via [`tables::tempering_bonus`];
//! * **armor** → summed `armor_rating` (Phase 3.3);
//! * **shield** → `block_base` + `optimal_block_boost` (Phase 3.5);
//! * **enchantments** → dispatched on the family's **logic class**
//!   (`WeaponDamageFirePropertyLogic`, `ResistFrostPropertyLogic`, …), with the
//!   magnitude from that family's own tier curve (Phase 3.6/3.7);
//! * **abilities** → the full 63-ability shipped table (Phase 3.11).
//!
//! What is *deleted*: `DEFAULT_WEAPON_WEIGHT` (the hardcoded `Light`), the
//! 8-char-prefix `defensive_enchant` matcher and its four invented per-tier
//! constants (`RESIST_PER_TIER` 8.0, `FORTIFY_CONDITION_PER_TIER` 0.02,
//! `ELEM_PIERCE_PER_TIER` 0.04, `STATUS_DUR_STEP` 0.03), and the single-prefix
//! `ability_tag_for_template`.

use blades_lib::user_data::{CompleteCharacter, CompleteInventory};
use serde_json::Value;
use uuid::Uuid;

use super::gamedata;
use super::state::{AbilityTag, DamageType, EquippedAbility, Loadout, StatusEffectType, WeaponProfile};
use super::tables;

/// A representative starter loadout, used when there is no character row / no DB
/// (bots, ghosts, tests). Built from **real shipped templates** rather than the
/// UESP fallback surface, so even the bot path exercises the real data model:
/// a tempering-4 **Glass Dagger** + **Chaurus Shield**, both L28-equippable, with a
/// tier-3 `Weapon Shock Damage` enchant (the historical starter flavour).
pub fn starter() -> Loadout {
    const STARTER_LEVEL: u16 = 30;
    let mut lo = Loadout {
        level: STARTER_LEVEL,
        status_dur_mult: 1.0,
        shield_optimal_block_boost: 1.0,
        weapon_optimal_block_boost: 1.0,
        ..Default::default()
    };
    match gamedata::weapon(STARTER_WEAPON) {
        Some(w) => install_weapon(&mut lo, w, STARTER_TEMPERING),
        None => lo.weapon = fallback_weapon_profile(STARTER_LEVEL),
    }
    if let Some(sh) = gamedata::shield(STARTER_SHIELD) {
        lo.has_shield = true;
        lo.block_rating += sh.block_base;
        lo.shield_optimal_block_boost = sh.optimal_block_boost.max(1.0);
    }
    lo.enchants = vec![(DamageType::Shock, STARTER_ENCHANT_TIER)];
    lo
}

/// `Glass Dagger` — Light / Slashing, `base_damage` 72.0, `block_base` 36.0, req L28.
const STARTER_WEAPON: &str = "82ed9c7a-bda4-446d-a83f-586d239e2fb9";
/// `Chaurus Shield` — `block_base` 240.0, req L28.
const STARTER_SHIELD: &str = "069654c7-32a6-4391-a944-3f1f97efa11c";
/// Tempering 4 → `QUALITY_BONUS[4] 15.0 × Light 0.60 = 9.0` → base 81.0.
const STARTER_TEMPERING: u64 = 4;
/// `Weapon Shock Damage` tier 3 (`value 1318`) → 23.84 damage + an equal Magicka drain.
const STARTER_ENCHANT_TIER: u8 = 3;

/// Build a [`WeaponProfile`] from a resolved shipped template + the instance's
/// `tempering_level`. [Phase 3.1/3.2]
pub fn weapon_profile(w: &'static gamedata::WeaponStats, tempering_level: u64) -> WeaponProfile {
    let weight = tables::Weight::from_class(w.weapon_class);
    let ty = map_damage_type(w.damage_type);
    let base = w.base_damage + tables::tempering_bonus(weight, tempering_level);
    WeaponProfile {
        primary_type: Some(ty),
        base_by_type: vec![(ty, base)],
        weight: Some(weight),
    }
}

/// Install a resolved weapon template onto `lo`: the damage profile, the template
/// (for cadence, Phase 3.12) and the weapon's Block Rating contribution.
fn install_weapon(lo: &mut Loadout, w: &'static gamedata::WeaponStats, tempering_level: u64) {
    lo.weapon = weapon_profile(w, tempering_level);
    lo.weapon_template = Some(w);
    lo.weapon_optimal_block_boost = w.optimal_block_boost.max(1.0);
    lo.block_rating += w.block_base;
}

/// The UESP fallback surface, for characters whose weapon does not resolve to a
/// shipped template (imported without inventory, bots).
fn fallback_weapon_profile(level: u16) -> WeaponProfile {
    let weight = tables::Weight::Light;
    WeaponProfile {
        primary_type: Some(DamageType::Slashing),
        base_by_type: vec![(
            DamageType::Slashing,
            tables::fallback::weapon_base_for_level(level, weight),
        )],
        weight: Some(weight),
    }
}

/// `gamedata::DamageType` → the combat model's `DamageType`. The two enums share
/// the client's raw values (`1 = Slashing, 2 = Cleaving, 3 = Bashing, 4..7 =
/// Fire/Frost/Shock/Poison`) — correction 3: the physical trio is a *swing shape*,
/// not a physical/elemental split.
pub fn map_damage_type(t: gamedata::DamageType) -> DamageType {
    match t {
        gamedata::DamageType::None => DamageType::None,
        gamedata::DamageType::Slashing => DamageType::Slashing,
        gamedata::DamageType::Cleaving => DamageType::Cleaving,
        gamedata::DamageType::Bashing => DamageType::Bashing,
        gamedata::DamageType::Fire => DamageType::Fire,
        gamedata::DamageType::Frost => DamageType::Frost,
        gamedata::DamageType::Shock => DamageType::Shock,
        gamedata::DamageType::Poison => DamageType::Poison,
    }
}

/// Parse a combat [`Loadout`] from a player's stored character + inventory.
pub fn from_character(character: &CompleteCharacter, inventory: &CompleteInventory) -> Loadout {
    let mut lo = Loadout {
        level: character.level,
        display_name: character.name.clone(),
        status_dur_mult: 1.0,
        shield_optimal_block_boost: 1.0,
        ..Default::default()
    };

    let mut weapon: Option<(&'static gamedata::WeaponStats, u64)> = None;

    for eq in inventory.loadout.equipped_items.0.values() {
        let template = eq.item.item_template_id.as_hyphenated().to_string();

        // --- item stats (Phase 3.1/3.2/3.3/3.5) ---
        if let Some(w) = gamedata::weapon(&template) {
            // Prefer the highest-damage resolvable weapon if several are equipped
            // (the client only ever equips one, but be deterministic).
            let better = weapon.map(|(p, _)| w.base_damage > p.base_damage).unwrap_or(true);
            if better {
                weapon = Some((w, eq.item.tempering_level));
            }
        } else if let Some(a) = gamedata::armor(&template) {
            lo.armor_rating += a.armor_rating;
        } else if let Some(s) = gamedata::shield(&template) {
            lo.has_shield = true;
            lo.block_rating += s.block_base;
            lo.shield_optimal_block_boost = lo.shield_optimal_block_boost.max(s.optimal_block_boost);
        }

        // --- enchantments, dispatched on the family's LOGIC CLASS (Phase 3.6/3.7) ---
        for prop in &eq.item.properties.enchanting {
            let tier = prop.tier.min(u8::MAX as u64) as u8;
            apply_enchant(&mut lo, &prop.id, tier);
        }
    }

    match weapon {
        Some((w, tempering)) => install_weapon(&mut lo, w, tempering),
        None => lo.weapon = fallback_weapon_profile(character.level),
    }

    lo.abilities = parse_equipped_abilities(&character.equipped_abilities, &character.abilities);
    lo.paralyze_rank = lo
        .abilities
        .iter()
        .find(|a| a.tag == AbilityTag::Paralyze)
        .map(|a| a.level)
        .unwrap_or(0);

    lo
}

fn profile_base(p: &WeaponProfile) -> f32 {
    p.base_by_type.iter().map(|(_, v)| *v).sum()
}

// ---------------------------------------------------------------------------
// Enchantments — dispatched on the shipped `ItemPropertyLogic` subclass
// ---------------------------------------------------------------------------

/// Apply one `{id, tier}` ENCHANTING property to `lo`, dispatching on the shipped
/// family's **logic class** (correction 2: the `_value` curve is *shared* — 32 of
/// 116 families use the identical `268 … 7591` ramp — so the logic class, not the
/// magnitude, is what an enchantment *is*).
///
/// Unknown families are ignored (they are cosmetic / out-of-combat / economy
/// logics such as `SelfRepairOnFrostDamagePropertyLogic`).
fn apply_enchant(lo: &mut Loadout, id: &Uuid, tier: u8) {
    let uuid = id.as_hyphenated().to_string();
    let Some(family) = gamedata::enchant_family(&uuid) else {
        return;
    };
    let value = family.value(tier).unwrap_or(0.0);
    let magnitude = value * tables::ENCHANT_DAMAGE_PER_VALUE;

    match family.logic {
        // ---- offensive weapon damage tracks -------------------------------
        "WeaponDamageFirePropertyLogic" => push_enchant(lo, DamageType::Fire, tier),
        "WeaponDamageFrostPropertyLogic" => push_enchant(lo, DamageType::Frost, tier),
        "WeaponDamageShockPropertyLogic" => push_enchant(lo, DamageType::Shock, tier),
        "WeaponDamagePoisonPropertyLogic" => push_enchant(lo, DamageType::Poison, tier),
        "WeaponDamageStaminaPropertyLogic" => push_enchant(lo, DamageType::Stamina, tier),
        "WeaponDamageMagickaPropertyLogic" => push_enchant(lo, DamageType::Magicka, tier),

        // ---- resistance ratings (Phase 3.4) --------------------------------
        "ResistFirePropertyLogic" | "ResistFireMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Fire, magnitude)
        }
        "ResistFrostPropertyLogic" | "ResistFrostMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Frost, magnitude)
        }
        "ResistShockPropertyLogic" | "ResistShockMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Shock, magnitude)
        }
        "ResistPoisonPropertyLogic" | "ResistPoisonMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Poison, magnitude)
        }
        "ResistSlashingPropertyLogic" | "ResistSlashingMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Slashing, magnitude)
        }
        "ResistCleavingPropertyLogic" | "ResistCleavingMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Cleaving, magnitude)
        }
        "ResistBashingPropertyLogic" | "ResistBashingMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Bashing, magnitude)
        }

        // ---- block rating -------------------------------------------------
        "BlockReductionFirePropertyLogic"
        | "BlockReductionFrostPropertyLogic"
        | "BlockReductionShockPropertyLogic"
        | "BlockReductionPoisonPropertyLogic"
        | "BlockReductionSlashingPropertyLogic"
        | "BlockReductionCleavingPropertyLogic"
        | "BlockReductionBashingPropertyLogic"
        | "BlockReductionTemplarPropertyLogic"
        | "PowerfulBlockPropertyLogic" => lo.block_rating += value,

        // ---- piercing ------------------------------------------------------
        "ResistancePiercingElementalPropertyLogic" => lo.elem_resist_piercing_rating += magnitude,
        "ArmorPiercingPhysicalPropertyLogic" => lo.armor_piercing_rating += magnitude,

        // ---- status-threshold fortifies (Phase 3.8) ------------------------
        "FortifyPoisonedPropertyLogic" => push_status_resist(lo, StatusEffectType::Poisoned, magnitude),
        "FortifyBurningPropertyLogic" => push_status_resist(lo, StatusEffectType::Burning, magnitude),
        "FortifyFrozenPropertyLogic" => push_status_resist(lo, StatusEffectType::Frozen, magnitude),
        "FortifyEnervatedPropertyLogic" => push_status_resist(lo, StatusEffectType::Enervated, magnitude),

        // ---- status duration ------------------------------------------------
        // The shared curve is a magnitude, not a percentage; express it as a
        // fraction of the family's own tier-10 ceiling so the multiplier stays in
        // a sane band. [Class 3: shape authored, family + curve real]
        "ShortenElementalStatusPropertyLogic" => {
            lo.status_dur_mult *= (1.0 - curve_fraction(family, tier) * 0.5).max(0.1)
        }
        "ExtendElementalStatusesPropertyLogic" => {
            lo.status_dur_mult *= 1.0 + curve_fraction(family, tier) * 0.5
        }

        // ---- offensive element amplification --------------------------------
        // `Fortify <Element> Damage` raises the attacker's own element track.
        "FortifyFirePropertyLogic" => push_fortify(lo, DamageType::Fire, curve_fraction(family, tier)),
        "FortifyFrostPropertyLogic" => push_fortify(lo, DamageType::Frost, curve_fraction(family, tier)),
        "FortifyShockPropertyLogic" => push_fortify(lo, DamageType::Shock, curve_fraction(family, tier)),
        "FortifyPoisonPropertyLogic" => push_fortify(lo, DamageType::Poison, curve_fraction(family, tier)),

        _ => {}
    }
}

/// This tier's position on its family's own curve, 0..1 (tier value ÷ the family's
/// maximum tier value). Used where a family's magnitude must become a *fraction*.
fn curve_fraction(family: &'static gamedata::EnchantFamily, tier: u8) -> f32 {
    let max = family
        .tiers()
        .iter()
        .map(|t| t.value)
        .fold(0.0_f32, f32::max);
    if max <= 0.0 {
        return 0.0;
    }
    (family.value(tier).unwrap_or(0.0) / max).clamp(0.0, 1.0)
}

fn push_enchant(lo: &mut Loadout, ty: DamageType, tier: u8) {
    lo.enchants.push((ty, tier));
}

fn push_resist(lo: &mut Loadout, ty: DamageType, rating: f32) {
    lo.resistances.push((ty, rating));
}

fn push_status_resist(lo: &mut Loadout, cond: StatusEffectType, magnitude: f32) {
    // The threshold bump is expressed as a fraction of max HP by
    // `Fighter::condition_threshold`; the shipped magnitude is a damage figure, so
    // scale it against the base 25 %-of-maxHP trigger at L86 arena HP.
    lo.status_resist.push((cond, magnitude / STATUS_THRESHOLD_REFERENCE_HP));
}

fn push_fortify(lo: &mut Loadout, ty: DamageType, frac: f32) {
    lo.element_fortify.push((ty, frac));
}

/// Reference max-HP used to turn a `Fortify <Condition>` damage magnitude into the
/// fraction-of-max-HP threshold bump `Fighter::condition_threshold` expects
/// (L86 arena HP ≈ 3150). [Class 3: bridge]
const STATUS_THRESHOLD_REFERENCE_HP: f32 = 3150.0;

// ---------------------------------------------------------------------------
// Abilities (Phase 3.11)
// ---------------------------------------------------------------------------

/// Classify an ability by its shipped `editor_name` / `kind` — the **full 63-row
/// table**, replacing the single `"91078132" => ResistElements` prefix match.
pub fn ability_tag_for_template(uuid_str: &str) -> AbilityTag {
    let Some(a) = gamedata::ability(uuid_str) else {
        return AbilityTag::Generic;
    };
    match a.editor_name {
        "Ward" | "Spellbreaker" => AbilityTag::Ward,
        "Absorb" | "SiphonLife" => AbilityTag::Absorb,
        "ResistElements" => AbilityTag::ResistElements,
        "Paralyze" => AbilityTag::Paralyze,
        _ => match a.kind {
            gamedata::AbilityKind::Perk => AbilityTag::Perk,
            gamedata::AbilityKind::Maneuver => AbilityTag::Maneuver,
            gamedata::AbilityKind::Spell => {
                if a.damage_type.is_some() {
                    AbilityTag::Damage
                } else {
                    AbilityTag::Generic
                }
            }
        },
    }
}

/// `equippedAbilities` is `{slot: uuid}` — take the VALUES (the ability instance
/// UUIDs, NOT the slot keys); level each from `abilities` (`{uuid: level}`),
/// defaulting to 1. The level is clamped to the ability's shipped `maximum_level`.
fn parse_equipped_abilities(equipped: &Value, levels: &Value) -> Vec<EquippedAbility> {
    let mut out = Vec::new();
    let Some(slots) = equipped.as_object() else {
        return out;
    };
    let levels = levels.as_object();
    for v in slots.values() {
        if let Some(uuid) = v.as_str() {
            let mut level = levels
                .and_then(|m| m.get(uuid))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(u8::MAX as u64) as u8;
            if let Some(a) = gamedata::ability(uuid) {
                level = level.clamp(1, a.maximum_level.min(u8::MAX as u16) as u8);
            }
            out.push(EquippedAbility {
                instance_uuid: uuid.to_string(),
                level,
                tag: ability_tag_for_template(uuid),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WEAPON_POISON_DAMAGE: &str = "08ea75d0-5cf1-44a9-9816-d3c6740c4191";
    const RESIST_FIRE: &str = "464bedb7-a631-43b6-a2df-f65f089d39da";
    const ELEM_PIERCE: &str = "98757a01-33b8-40ea-bb45-6acd89811ae3";

    fn lo() -> Loadout {
        Loadout { status_dur_mult: 1.0, shield_optimal_block_boost: 1.0, ..Default::default() }
    }

    /// Enchants are routed by the family's LOGIC CLASS, and the magnitude comes
    /// from that family's own tier curve (correction 2).
    #[test]
    fn enchants_dispatch_on_logic_class_not_uuid_prefix() {
        let mut l = lo();
        apply_enchant(&mut l, &Uuid::parse_str(WEAPON_POISON_DAMAGE).unwrap(), 10);
        assert_eq!(l.enchants, vec![(DamageType::Poison, 10)]);
        let dmg = tables::enchant_damage(WEAPON_POISON_DAMAGE, 10).unwrap();
        assert!((dmg - 137.32).abs() < 0.5, "s506 poison enchant base {dmg}");

        // A RESIST family with the SAME shared curve becomes a resistance RATING,
        // not a damage track — the value alone cannot tell them apart.
        let mut r = lo();
        let fire_family = gamedata::enchant_family(RESIST_FIRE).unwrap();
        let a_real_tier = fire_family.tiers().last().unwrap().tier;
        apply_enchant(&mut r, &Uuid::parse_str(RESIST_FIRE).unwrap(), a_real_tier);
        assert!(r.enchants.is_empty(), "a resist enchant is not a damage track");
        assert!(
            r.resistances.iter().any(|(t, v)| *t == DamageType::Fire && *v > 0.0),
            "Resist Fire becomes a Fire Resistance RATING, got {:?}",
            r.resistances
        );

        // Elemental Resistance Piercing is a RATING now, not a 0.04/tier fraction.
        let mut p = lo();
        apply_enchant(&mut p, &Uuid::parse_str(ELEM_PIERCE).unwrap(), 10);
        assert!(p.elem_resist_piercing_rating > 0.0);
        assert_eq!(p.elem_resist_piercing, 0.0, "the fractional field is ability-side only");
    }

    /// The full ability table drives routing — not one hardcoded prefix.
    #[test]
    fn ability_routing_covers_the_shipped_table() {
        assert_eq!(ability_tag_for_template(gamedata::ids::WARD), AbilityTag::Ward);
        assert_eq!(ability_tag_for_template(gamedata::ids::PARALYZE), AbilityTag::Paralyze);
        assert_eq!(
            ability_tag_for_template(gamedata::ids::RESIST_ELEMENTS),
            AbilityTag::ResistElements
        );
        assert_eq!(ability_tag_for_template(gamedata::ids::FIREBALL), AbilityTag::Damage);
        // Absorb (a spell with no damage_type) is its own negation class.
        assert_eq!(
            ability_tag_for_template("4e760726-b012-4b25-bc92-0cd6312d6601"),
            AbilityTag::Absorb
        );
        // Maneuvers and perks are distinguished.
        assert_eq!(
            ability_tag_for_template("ce6b63e9-9f18-49c4-aee0-51f7985f9892"),
            AbilityTag::Maneuver
        );
        assert_eq!(
            ability_tag_for_template("09aa3390-8f42-4cd5-a88c-5c94d5e1dd29"),
            AbilityTag::Perk
        );
        assert_eq!(ability_tag_for_template("not-a-uuid"), AbilityTag::Generic);
    }

    /// A resolved weapon carries the shipped cadence + block stats and the
    /// tempering bonus — the old `DEFAULT_WEAPON_WEIGHT` is gone.
    #[test]
    fn weapon_profile_uses_real_template_and_tempering() {
        let w = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).unwrap();
        let p = weapon_profile(w, 10);
        assert_eq!(p.weight, Some(tables::Weight::Light));
        assert_eq!(p.primary_type, Some(DamageType::Slashing));
        assert!((profile_base(&p) - 144.0).abs() < 1e-3, "99 + 45 tempering = 144");
        assert!((w.block_base - 49.5).abs() < 1e-3);
        let mut lo = Loadout::default();
        install_weapon(&mut lo, w, 10);
        assert!((lo.swing_interval().as_secs_f32() - 0.783333).abs() < 1e-4);
        assert!((lo.block_rating - 49.5).abs() < 1e-3);
        // Untempered = the shipped quality-0 cell exactly.
        assert!((profile_base(&weapon_profile(w, 0)) - 99.0).abs() < 1e-3);
    }

    #[test]
    fn starter_resolves_a_real_item() {
        let s = starter();
        assert!(s.weapon_template.is_some(), "starter uses a shipped template");
        assert_eq!(s.weapon.weight, Some(tables::Weight::Light));
        // Glass Dagger 72.0 + tempering-4 bonus 9.0.
        let base: f32 = s.weapon.base_by_type.iter().map(|(_, v)| *v).sum();
        assert!((base - 81.0).abs() < 1e-3, "starter base {base}");
        // Chaurus Shield 240 + the dagger's own 36.
        assert!((s.block_rating - 276.0).abs() < 1e-3, "starter block rating {}", s.block_rating);
        assert!(s.has_shield);
        assert_eq!(s.enchants, vec![(DamageType::Shock, 3)]);
    }

    #[test]
    fn parses_equipped_abilities_by_value_with_levels() {
        let equipped = json!({ "0": "aaaaaaaa-0000-0000-0000-000000000001", "1": "bbbbbbbb-0000-0000-0000-000000000002" });
        let levels = json!({ "aaaaaaaa-0000-0000-0000-000000000001": 3 });
        let abilities = parse_equipped_abilities(&equipped, &levels);
        assert_eq!(abilities.len(), 2);
        let a = abilities.iter().find(|a| a.instance_uuid.starts_with("aaaa")).unwrap();
        let b = abilities.iter().find(|a| a.instance_uuid.starts_with("bbbb")).unwrap();
        assert_eq!(a.level, 3);
        assert_eq!(b.level, 1, "missing level defaults to 1");
    }

    /// A rank above the ability's shipped `maximum_level` is clamped.
    #[test]
    fn ability_level_is_clamped_to_the_shipped_maximum() {
        let equipped = json!({ "0": gamedata::ids::FIREBALL });
        let levels = json!({ gamedata::ids::FIREBALL: 250 });
        let a = parse_equipped_abilities(&equipped, &levels);
        let max = gamedata::ability(gamedata::ids::FIREBALL).unwrap().maximum_level as u8;
        assert_eq!(a[0].level, max);
    }

    #[test]
    fn empty_abilities_value_is_safe() {
        assert!(parse_equipped_abilities(&Value::Null, &Value::Null).is_empty());
    }
}
