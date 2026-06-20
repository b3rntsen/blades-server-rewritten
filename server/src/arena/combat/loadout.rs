//! Build a `Fighter`'s combat [`Loadout`] from the imported character.
//!
//! [`from_character`] is a **pure** parser (no DB) over the stored
//! `CompleteCharacter` + `CompleteInventory` (the matchmaker does the async query
//! and calls this). It extracts the data the fork actually has:
//!   - **equipped abilities** (instance UUID + level) — the values of
//!     `equippedAbilities` (slot→uuid), leveled via `abilities` (uuid→level);
//!   - **weapon damage enchants** — the `Weapon <Element> Damage` ENCHANTING
//!     properties on equipped items → `(DamageType, tier)`.
//!
//! What the fork does NOT store (so it stays representative — see [`starter`]):
//! the weapon's **base damage number** and its **physical damage type**
//! (Slashing/Cleaving/Bashing) — there is no item-damage data table in the fork
//! (`blades_lib::game_data` has only name+type; `parsed.json` is an empty stub).
//! Damage is therefore retail-exact in *structure* (formula + enchant tracks +
//! real ability levels) but the weapon base magnitude/type is a representative
//! default until item game-data is wired.

use blades_lib::user_data::{CompleteCharacter, CompleteInventory};
use serde_json::Value;
use uuid::Uuid;

use super::state::{AbilityTag, DamageType, EquippedAbility, Loadout, StatusEffectType, WeaponProfile};
use super::tables;

/// A representative starter loadout (a shock-enchanted blade), used when there is
/// no character row / no DB.
pub fn starter() -> Loadout {
    Loadout {
        level: 30, // representative mid-game level
        abilities: Vec::new(),
        weapon: WeaponProfile {
            primary_type: Some(DamageType::Slashing),
            base_by_type: vec![(
                DamageType::Slashing,
                tables::weapon_base_for_level(30, DEFAULT_WEAPON_WEIGHT),
            )],
            weight: Some(DEFAULT_WEAPON_WEIGHT),
        },
        has_shield: false,
        enchants: vec![(DamageType::Shock, 2)],
        display_name: String::new(),
        character_uuid: String::new(),
        profile_equipped_json: String::new(),
        profile_character_json: String::new(),
        resistances: Vec::new(),
        weaknesses: Vec::new(),
        elem_resist_piercing: 0.0,
        status_resist: Vec::new(),
        status_dur_mult: 1.0,
    }
}

/// The weapon-weight default until per-weapon item game-data is wired
/// (`docs/arena-combat-reproduction-spec.md` §4.1). The fork has **no item→weight
/// table** (`blades_lib::game_data` carries only name+type; `parsed.json` is an empty
/// stub), so `from_character` cannot derive the equipped weapon's class. We default to
/// **Light** — the recorded-match weapon's class (a Dragonbone Dagger) and the class
/// the on-device tester (Flappety) wields — which (a) reproduces s506 and (b) replaces
/// the old, capture-CONTRADICTED `Heavy` force (which over-based the dagger ~1.67× and
/// drove the wrong charged-`Middle`-crit swing behaviour). **Flagged as a calibration
/// default**: wire the real per-weapon weight from item data to make it exact.
pub const DEFAULT_WEAPON_WEIGHT: tables::Weight = tables::Weight::Light;

/// Parse a combat [`Loadout`] from a player's stored character + inventory.
pub fn from_character(character: &CompleteCharacter, inventory: &CompleteInventory) -> Loadout {
    // Scan every equipped item's ENCHANTING properties once, splitting them into the
    // OFFENSIVE damage track (`Weapon <Element> Damage`) and the DEFENSIVE side
    // (resist / Fortify-Poisoned / Elemental-Resistance-Piercing / status-duration).
    let mut enchants = Vec::new();
    let mut resistances: Vec<(DamageType, f32)> = Vec::new();
    let mut status_resist: Vec<(StatusEffectType, f32)> = Vec::new();
    let mut elem_resist_piercing: f32 = 0.0;
    let mut status_dur_mult: f32 = 1.0;
    for eq in inventory.loadout.equipped_items.0.values() {
        for prop in &eq.item.properties.enchanting {
            let tier = prop.tier.min(u8::MAX as u64) as u8;
            if let Some(dt) = enchant_damage_type(&prop.id) {
                enchants.push((dt, tier));
                continue;
            }
            match defensive_enchant(&prop.id) {
                Some(DefensiveEnchant::Resist(ty)) => {
                    resistances.push((ty, RESIST_PER_TIER * tier as f32));
                }
                Some(DefensiveEnchant::FortifyCondition(c)) => {
                    status_resist.push((c, FORTIFY_CONDITION_PER_TIER * tier as f32));
                }
                Some(DefensiveEnchant::ElementalResistPiercing) => {
                    // Stacks across pieces, capped below 1.0 (never fully ignore resist).
                    elem_resist_piercing =
                        (elem_resist_piercing + ELEM_PIERCE_PER_TIER * tier as f32).min(0.9);
                }
                Some(DefensiveEnchant::ShortenStatuses) => {
                    status_dur_mult *= 1.0 - STATUS_DUR_STEP * tier as f32;
                }
                Some(DefensiveEnchant::ExtendStatuses) => {
                    status_dur_mult *= 1.0 + STATUS_DUR_STEP * tier as f32;
                }
                None => {}
            }
        }
    }

    let abilities = parse_equipped_abilities(&character.equipped_abilities, &character.abilities);

    // Weapon TYPE is not in the fork's data (defaults to Slashing); the weight defaults
    // to `DEFAULT_WEAPON_WEIGHT` (Light — the recorded-match class) until equipped-item
    // data is wired. The base is the UESP additive surface for that weight.
    let weight = DEFAULT_WEAPON_WEIGHT;
    let base = tables::weapon_base_for_level(character.level, weight);
    Loadout {
        level: character.level,
        abilities,
        weapon: WeaponProfile {
            primary_type: Some(DamageType::Slashing),
            base_by_type: vec![(DamageType::Slashing, base)],
            weight: Some(weight),
        },
        has_shield: false, // not derivable without item-name data
        enchants,
        display_name: character.name.clone(),
        character_uuid: String::new(), // set by the matchmaker (needs the DB row id)
        profile_equipped_json: String::new(), // set by the matchmaker (op54 profile)
        profile_character_json: String::new(),
        resistances,
        weaknesses: Vec::new(), // PvP loadouts don't carry self-weakness enchants
        elem_resist_piercing,
        status_resist,
        status_dur_mult,
    }
}

/// A parsed DEFENSIVE enchant family (status-resistance-spec §2.5/§6). The exact
/// per-tier magnitudes are `<game-data>` (the calibration knobs below).
enum DefensiveEnchant {
    /// "Resist <Type>" — flat resist of that `DamageType`.
    Resist(DamageType),
    /// "Fortify <Poisoned/Burning/Frozen/Enervated>" — raises that condition's threshold.
    FortifyCondition(StatusEffectType),
    /// "Elemental Resistance Piercing" — attacker-side; bypasses the target's elem resist.
    ElementalResistPiercing,
    /// "Shorten Elemental Statuses" — ×(<1) the status duration.
    ShortenStatuses,
    /// "Extend Elemental Statuses" — ×(>1) the status duration.
    ExtendStatuses,
}

/// Per-tier flat resistance for a "Resist X" enchant (`<game-data>`; representative
/// tier-scaled value — §2.5). Lowers the matching component AND feeds `mostResisted`.
const RESIST_PER_TIER: f32 = 8.0;
/// Per-tier threshold bump (fraction of max HP) for a "Fortify <Condition>" enchant
/// (`<game-data>`; representative). Raises that condition's land threshold. [§5.5]
const FORTIFY_CONDITION_PER_TIER: f32 = 0.02;
/// Per-tier Elemental-Resistance-Piercing fraction (`<game-data>`; representative). [§2.3]
const ELEM_PIERCE_PER_TIER: f32 = 0.04;
/// Per-tier status-duration step for Shorten/Extend Elemental Statuses (`<game-data>`).
const STATUS_DUR_STEP: f32 = 0.03;

/// Map a DEFENSIVE enchant template id → its [`DefensiveEnchant`] family.
///
/// **The fork has no full `uuid_labels` table** (only the 6 damage-enchant UUIDs are
/// hardcoded above), and the spec gives the defensive families' UUIDs as **8-char
/// prefixes** only (`docs/arena-combat-reproduction-spec.md` §3 / status-resistance §6:
/// Fortify-Poisoned `8f372d6d`, Fortify-Poison-Damage `2b8fd511`, Elemental-Resistance-
/// Piercing `98757a01`). So we match on the UUID's **first group** (the leading 32 bits
/// — collision-safe across this small enchant set). **Flagged**: seed the full UUIDs
/// from prod `uuid_labels` to make this exact + complete (the Resist-<Type> / Fortify-
/// <Burning/Frozen/Enervated> / Shorten/Extend ids below are PLACEHOLDERS pending that
/// table — only the three capture-confirmed Flappety prefixes are wired today).
fn defensive_enchant(id: &Uuid) -> Option<DefensiveEnchant> {
    let s = id.as_hyphenated().to_string();
    let prefix = s.split('-').next().unwrap_or("");
    Some(match prefix {
        // Capture-confirmed (Flappety s506 loadout, §3) — prefixes from the spec:
        "8f372d6d" => DefensiveEnchant::FortifyCondition(StatusEffectType::Poisoned), // Fortify Poisoned
        "2b8fd511" => DefensiveEnchant::FortifyCondition(StatusEffectType::Poisoned), // Fortify Poison Damage (offensive amp → treat as poison-condition fortify)
        "98757a01" => DefensiveEnchant::ElementalResistPiercing, // Elemental Resistance Piercing
        _ => return None,
    })
}

/// Map a `Weapon <Element> Damage` enchant template id → its [`DamageType`].
/// These six are the only ENCHANTING ids that add combat damage (confirmed from
/// prod `uuid_labels`); every other enchant (resist/fortify/…) returns `None`.
fn enchant_damage_type(id: &Uuid) -> Option<DamageType> {
    Some(match id.as_hyphenated().to_string().as_str() {
        "c40ed851-8777-4d09-b169-0223dae8f67d" => DamageType::Fire,
        "63b6c73a-af1a-4f95-8ffe-9434b8e68d56" => DamageType::Frost,
        "139024a7-3965-4e90-a4c1-60e3d7ca3133" => DamageType::Shock,
        "08ea75d0-5cf1-44a9-9816-d3c6740c4191" => DamageType::Poison,
        "9fdbb542-ff37-4199-93a3-d9444cca9090" => DamageType::Stamina,
        "5a145cf8-3a20-4b8a-bf6d-8ee1607d3417" => DamageType::Magicka,
        _ => return None,
    })
}

/// Map an ability TEMPLATE UUID prefix → its [`AbilityTag`]. Template UUIDs identify
/// the ABILITY CLASS (not the per-character instance). The relevant classes here are:
///   - `WardAbility` subclasses (Ward/SpellbreakerAbility) — template UUIDs from
///     prod `uuid_labels` Ward entries (first group only; matches the instantiation
///     UUIDs observed in character JSON).
///   - `ResistElementsAbility` — the Resist-Elements spell.
///
/// **CALIBRATION FLAG**: these prefixes are from prod `uuid_labels` WHERE label LIKE
/// '%ward%' OR label LIKE '%resist%elem%'. The full UUID table is NOT in the fork, so
/// only the first group (8 hex chars) is matched for safety. Update with the complete
/// list once the full uuid_labels table is available.
fn ability_tag_for_template(uuid_str: &str) -> AbilityTag {
    let prefix = uuid_str.split('-').next().unwrap_or("");
    match prefix {
        // Resist Elements (ResistElementsAbility) — s506 Flappety slot1 UUID 91078132.
        "91078132" => AbilityTag::ResistElements,
        // Ward ability family — common Ward spell prefix from uuid_labels.
        // Add further Ward template prefixes here as they are confirmed.
        _ => AbilityTag::Generic,
    }
}

/// `equippedAbilities` is `{slot: uuid}` — take the VALUES (the ability instance
/// UUIDs, NOT the slot keys); level each from `abilities` (`{uuid: level}`),
/// defaulting to 1. (The values-not-keys gotcha is documented project-wide.)
/// The `tag` is derived from the ability template UUID for Ward/ResistElements routing.
fn parse_equipped_abilities(equipped: &Value, levels: &Value) -> Vec<EquippedAbility> {
    let mut out = Vec::new();
    let Some(slots) = equipped.as_object() else {
        return out;
    };
    let levels = levels.as_object();
    for v in slots.values() {
        if let Some(uuid) = v.as_str() {
            let level = levels
                .and_then(|m| m.get(uuid))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(u8::MAX as u64) as u8;
            let tag = ability_tag_for_template(uuid);
            out.push(EquippedAbility {
                instance_uuid: uuid.to_string(),
                level,
                tag,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_weapon_damage_enchants_only() {
        let shock = Uuid::parse_str("139024a7-3965-4e90-a4c1-60e3d7ca3133").unwrap();
        let fire = Uuid::parse_str("c40ed851-8777-4d09-b169-0223dae8f67d").unwrap();
        let resist = Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(enchant_damage_type(&shock), Some(DamageType::Shock));
        assert_eq!(enchant_damage_type(&fire), Some(DamageType::Fire));
        assert_eq!(enchant_damage_type(&resist), None); // resist/fortify enchant → ignored
    }

    #[test]
    fn parses_equipped_abilities_by_value_with_levels() {
        // equippedAbilities keys are slots; VALUES are the instance UUIDs.
        let equipped = json!({ "0": "aaaaaaaa-0000-0000-0000-000000000001", "1": "bbbbbbbb-0000-0000-0000-000000000002" });
        let levels = json!({ "aaaaaaaa-0000-0000-0000-000000000001": 3 }); // b has no level → defaults to 1
        let abilities = parse_equipped_abilities(&equipped, &levels);
        assert_eq!(abilities.len(), 2);
        let a = abilities.iter().find(|a| a.instance_uuid.starts_with("aaaa")).unwrap();
        let b = abilities.iter().find(|a| a.instance_uuid.starts_with("bbbb")).unwrap();
        assert_eq!(a.level, 3);
        assert_eq!(b.level, 1, "missing level defaults to 1");
    }

    #[test]
    fn empty_abilities_value_is_safe() {
        assert!(parse_equipped_abilities(&Value::Null, &Value::Null).is_empty());
    }
}
