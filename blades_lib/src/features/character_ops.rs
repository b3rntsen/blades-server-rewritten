//! Character & inventory management — level-up, ability learning, respec, inventory
//! upgrade, item destroy, loadout profiles, and equipment changes
//! (`POST /levelup`, `/abilities`, `/respec`, `/inventories/current/{upgrade,destroy}`,
//! `/loadouts/profiles/{n}`, `/loadouts/current`).
//!
//! Pure mutations over the character/inventory. Currency *costs* for level-up/respec/
//! inventory-upgrade are not present in captures (only the post-state is), so these
//! apply the progression effect but do not debit (documented leniency); the captured
//! currency sinks (global shop, vendors) charge for real elsewhere.

use std::collections::HashMap;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::user_data::{CompleteCharacter, CompleteInventory, InventoryChangeTracker, SingleEquippedItem};

/// Which attribute a level-up invests in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribute {
    Stamina,
    Magicka,
}

impl Attribute {
    pub fn parse(s: &str) -> Option<Attribute> {
        match s.to_ascii_uppercase().as_str() {
            "STAMINA" => Some(Attribute::Stamina),
            "MAGICKA" => Some(Attribute::Magicka),
            _ => None,
        }
    }
}

/// Spend a level: +1 level and +1 point in the chosen attribute. (The client only
/// calls this when the character has crossed an XP threshold, which it knows from its
/// bundles; we trust it and apply the effect.)
pub fn apply_levelup(ch: &mut CompleteCharacter, attribute: Attribute) {
    ch.level = ch.level.saturating_add(1);
    match attribute {
        Attribute::Stamina => ch.stamina_attribute_points = ch.stamina_attribute_points.saturating_add(1),
        Attribute::Magicka => ch.magicka_attribute_points = ch.magicka_attribute_points.saturating_add(1),
    }
    ch.version += 1;
}

/// Reallocate attribute points (respec): set the totals as requested.
pub fn apply_respec(ch: &mut CompleteCharacter, stamina: u32, magicka: u32) {
    ch.stamina_attribute_points = stamina;
    ch.magicka_attribute_points = magicka;
    ch.version += 1;
}

/// Raise the backpack capacity tier.
pub fn upgrade_inventory(ch: &mut CompleteCharacter) {
    ch.inventory_level = ch.inventory_level.saturating_add(1);
    ch.version += 1;
}

/// Merge learned/upgraded abilities (`{abilityId: level}`) into `character.abilities`.
pub fn merge_abilities(ch: &mut CompleteCharacter, updates: &Value) {
    if !ch.abilities.is_object() {
        ch.abilities = json!({});
    }
    if let (Some(dst), Some(src)) = (ch.abilities.as_object_mut(), updates.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    ch.version += 1;
}

/// Set the equipped-ability slots (`{"0".."5": abilityId}`) on `character.equippedAbilities`.
pub fn set_equipped_abilities(ch: &mut CompleteCharacter, updates: &Value) {
    if !ch.equipped_abilities.is_object() {
        ch.equipped_abilities = json!({});
    }
    if let (Some(dst), Some(src)) = (ch.equipped_abilities.as_object_mut(), updates.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    ch.version += 1;
}

/// Store a named loadout profile at `index` in `character.loadoutProfiles` (an array).
pub fn set_loadout_profile(ch: &mut CompleteCharacter, index: usize, profile: Value) {
    if !ch.loadout_profiles.is_array() {
        ch.loadout_profiles = json!([]);
    }
    let arr = ch.loadout_profiles.as_array_mut().expect("just set to array");
    while arr.len() <= index {
        arr.push(Value::Null);
    }
    arr[index] = profile;
    ch.version += 1;
}

/// Destroy instanced backpack items by id (no-op for ids not present).
pub fn destroy_items(
    inv: &mut CompleteInventory,
    items: &[Uuid],
    tracker: &mut InventoryChangeTracker,
) {
    for id in items {
        if inv.backpack.items.0.remove(id).is_some() {
            tracker.modified_backpack.items.insert(*id);
        }
    }
}

/// Apply equipment changes (`{slotId: itemId | null}`): equip moves an item from the
/// backpack into the slot (returning any previously-equipped item to the backpack);
/// `null` unequips the slot back to the backpack.
pub fn apply_equipment_updates(
    inv: &mut CompleteInventory,
    updates: &HashMap<Uuid, Option<Uuid>>,
    tracker: &mut InventoryChangeTracker,
) {
    for (slot, target) in updates {
        // Return whatever currently occupies the slot to the backpack.
        if let Some(prev) = inv.loadout.equipped_items.0.remove(slot) {
            tracker.modified_loadout.modified_equipped_items.insert(*slot);
            inv.backpack.items.0.insert(prev.id, prev.item);
            tracker.modified_backpack.items.insert(prev.id);
        }
        if let Some(item_id) = target {
            // Equip from the backpack (skip silently if the id isn't there — stale client).
            if let Some(item) = inv.backpack.items.0.remove(item_id) {
                tracker.modified_backpack.items.insert(*item_id);
                inv.loadout.equipped_items.0.insert(
                    *slot,
                    SingleEquippedItem {
                        id: *item_id,
                        slot: *slot,
                        item,
                    },
                );
                tracker.modified_loadout.modified_equipped_items.insert(*slot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_data::{Backpack, Item, ItemPropertiesAll, Loadout, Treasury};

    fn inv() -> CompleteInventory {
        CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        }
    }

    fn item() -> Item {
        Item {
            item_template_id: Uuid::from_u128(9),
            tempering_level: 0,
            durability: 75.0,
            properties: ItemPropertiesAll::default(),
        }
    }

    #[test]
    fn levelup_bumps_level_and_chosen_attribute() {
        let mut ch = CompleteCharacter::default();
        let (lv, st, v) = (ch.level, ch.stamina_attribute_points, ch.version);
        apply_levelup(&mut ch, Attribute::Stamina);
        assert_eq!(ch.level, lv + 1);
        assert_eq!(ch.stamina_attribute_points, st + 1);
        assert_eq!(ch.magicka_attribute_points, 0);
        assert_eq!(ch.version, v + 1);
        apply_levelup(&mut ch, Attribute::Magicka);
        assert_eq!(ch.magicka_attribute_points, 1);
    }

    #[test]
    fn attribute_parse() {
        assert_eq!(Attribute::parse("STAMINA"), Some(Attribute::Stamina));
        assert_eq!(Attribute::parse("magicka"), Some(Attribute::Magicka));
        assert_eq!(Attribute::parse("luck"), None);
    }

    #[test]
    fn abilities_merge_into_opaque_value() {
        let mut ch = CompleteCharacter::default();
        let a = Uuid::from_u128(1).to_string();
        merge_abilities(&mut ch, &json!({ &a: 3 }));
        assert_eq!(ch.abilities[&a], 3);
        merge_abilities(&mut ch, &json!({ &a: 5 }));
        assert_eq!(ch.abilities[&a], 5, "later upgrade overwrites");
    }

    #[test]
    fn loadout_profile_stored_at_index() {
        let mut ch = CompleteCharacter::default();
        set_loadout_profile(&mut ch, 2, json!({ "name": "clutch" }));
        assert_eq!(ch.loadout_profiles[2]["name"], "clutch");
        assert!(ch.loadout_profiles[0].is_null(), "gaps padded with null");
    }

    #[test]
    fn destroy_removes_backpack_items() {
        let mut i = inv();
        let id = Uuid::from_u128(7);
        i.backpack.items.0.insert(id, item());
        let mut t = InventoryChangeTracker::default();
        destroy_items(&mut i, &[id], &mut t);
        assert!(!i.backpack.items.0.contains_key(&id));
        assert!(t.modified_backpack.items.contains(&id));
    }

    #[test]
    fn equip_moves_item_into_slot_and_back() {
        let mut i = inv();
        let item_id = Uuid::from_u128(7);
        let slot = Uuid::from_u128(100);
        i.backpack.items.0.insert(item_id, item());
        let mut t = InventoryChangeTracker::default();

        // Equip.
        apply_equipment_updates(&mut i, &HashMap::from([(slot, Some(item_id))]), &mut t);
        assert!(i.loadout.equipped_items.0.contains_key(&slot));
        assert!(!i.backpack.items.0.contains_key(&item_id), "left the backpack");

        // Unequip.
        let mut t2 = InventoryChangeTracker::default();
        apply_equipment_updates(&mut i, &HashMap::from([(slot, None)]), &mut t2);
        assert!(!i.loadout.equipped_items.0.contains_key(&slot));
        assert!(i.backpack.items.0.contains_key(&item_id), "returned to backpack");
    }
}
