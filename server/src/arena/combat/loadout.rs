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

use super::state::{DamageType, EquippedAbility, Loadout, WeaponProfile};
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
                tables::weapon_base_for_level(30, tables::Weight::Heavy),
            )],
        },
        has_shield: false,
        enchants: vec![(DamageType::Shock, 2)],
    }
}

/// Parse a combat [`Loadout`] from a player's stored character + inventory.
pub fn from_character(character: &CompleteCharacter, inventory: &CompleteInventory) -> Loadout {
    // Damage enchants from every equipped item (only weapons carry the
    // `Weapon <Element> Damage` family, so scanning all equipped items is safe).
    let mut enchants = Vec::new();
    for eq in inventory.loadout.equipped_items.0.values() {
        for prop in &eq.item.properties.enchanting {
            if let Some(dt) = enchant_damage_type(&prop.id) {
                enchants.push((dt, prop.tier.min(u8::MAX as u64) as u8));
            }
        }
    }

    let abilities = parse_equipped_abilities(&character.equipped_abilities, &character.abilities);

    // Weapon TYPE is not in the fork's data (defaults to Slashing); the base damage
    // is the UESP additive surface for a level-appropriate Heavy weapon
    // (`tables::weapon_base_for_level`) until equipped-item data is wired.
    let base = tables::weapon_base_for_level(character.level, tables::Weight::Heavy);
    Loadout {
        level: character.level,
        abilities,
        weapon: WeaponProfile {
            primary_type: Some(DamageType::Slashing),
            base_by_type: vec![(DamageType::Slashing, base)],
        },
        has_shield: false, // not derivable without item-name data
        enchants,
    }
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

/// `equippedAbilities` is `{slot: uuid}` — take the VALUES (the ability instance
/// UUIDs, NOT the slot keys); level each from `abilities` (`{uuid: level}`),
/// defaulting to 1. (The values-not-keys gotcha is documented project-wide.)
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
            out.push(EquippedAbility {
                instance_uuid: uuid.to_string(),
                level,
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
