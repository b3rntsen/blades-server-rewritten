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
///
/// GEAR is instanced (`backpack.items`, one row per item), so a normal equip moves that
/// instance into the slot. POTIONS/consumables are STACKABLE (`backpack.stackableItems`,
/// template id + count) and carry no per-instance id — the client normally equips them
/// via the separate `equippedConsumables` field (see [`set_equipped_consumables`]). But
/// if a consumable's TEMPLATE id ever arrives here (in `equipmentUpdates`) it must NOT
/// be silently dropped: the old code only checked `backpack.items`, so the potion was
/// never equipped, never appeared in the diff, and the client surfaced "Unable to
/// connect". We now route such an id to the consumable list so the equip lands and is
/// reflected in the loadout diff. Instanced gear equips are unchanged.
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
            // Equip an instanced gear item from the backpack.
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
            } else if inv.backpack.stackable_items.count(*item_id) > 0 {
                // Not instanced gear, but the id IS a stackable consumable the player
                // owns → treat it as a consumable equip rather than silently skipping.
                // (A client can route a potion through equipmentUpdates; without this it
                // fell through and the client saw "Unable to connect".)
                add_equipped_consumable(&mut inv.loadout.equipped_consumables, *item_id);
                tracker.modified_loadout.consumables_changed = true;
            }
            // else: unknown id (stale client) — skip silently, as before.
        }
    }
}

/// Insert a consumable template id into the equipped-consumable list (idempotent: no
/// duplicates). Extracted so both the `equippedConsumables` request path and the
/// `equipmentUpdates` fallback share one definition.
fn add_equipped_consumable(equipped: &mut Vec<Uuid>, template: Uuid) {
    if !equipped.contains(&template) {
        equipped.push(template);
    }
}

/// Set the equipped consumables to exactly `templates` (the `equippedConsumables` field
/// of `POST /loadouts/current` — a full replacement, matching how the client sends the
/// current equipped-consumable list). Only templates the player actually OWNS in
/// `backpack.stackableItems` are accepted (an unowned id is dropped, never equipped);
/// duplicates are collapsed. Marks the tracker so the loadout diff echoes the result.
/// Returns true iff the equipped-consumable list changed.
pub fn set_equipped_consumables(
    inv: &mut CompleteInventory,
    templates: &[Uuid],
    tracker: &mut InventoryChangeTracker,
) -> bool {
    let mut next: Vec<Uuid> = Vec::with_capacity(templates.len());
    for t in templates {
        if inv.backpack.stackable_items.count(*t) > 0 {
            add_equipped_consumable(&mut next, *t);
        }
    }
    if next != inv.loadout.equipped_consumables {
        inv.loadout.equipped_consumables = next;
        tracker.modified_loadout.consumables_changed = true;
        true
    } else {
        false
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

    /// Equipping a STACKABLE consumable (potion) via the `equippedConsumables` field
    /// must land in the loadout, be reflected in the loadout diff, and NOT touch the
    /// stackable count (equipping a potion doesn't consume it). Before the fix a potion
    /// equip was silently skipped and the client showed "Unable to connect".
    #[test]
    fn equip_stackable_consumable_updates_loadout_and_diff() {
        let mut i = inv();
        let potion = Uuid::from_u128(42);
        i.backpack.stackable_items.add(potion, 5);
        let mut t = InventoryChangeTracker::default();

        let changed = set_equipped_consumables(&mut i, &[potion], &mut t);
        assert!(changed, "equipping a potion changes the loadout");
        assert_eq!(i.loadout.equipped_consumables, vec![potion], "potion is equipped");
        assert_eq!(i.backpack.stackable_items.count(potion), 5, "equipping does not consume the stack");
        assert!(t.modified_loadout.consumables_changed, "tracker flags the change");

        // The loadout diff echoes the equipped-consumable list so the client sees it.
        let diff = i.loadout.generate_client_update(&t.modified_loadout);
        assert_eq!(diff.equipped_consumables, Some(vec![potion]), "diff carries the equipped consumable");
    }

    /// A consumable that the player does NOT own is dropped (never equipped), and an
    /// idempotent re-equip of the same list reports no change (no spurious diff).
    #[test]
    fn equip_consumable_ignores_unowned_and_is_idempotent() {
        let mut i = inv();
        let owned = Uuid::from_u128(1);
        let unowned = Uuid::from_u128(2);
        i.backpack.stackable_items.add(owned, 3);
        let mut t = InventoryChangeTracker::default();

        set_equipped_consumables(&mut i, &[owned, unowned, owned], &mut t);
        assert_eq!(i.loadout.equipped_consumables, vec![owned], "unowned dropped, duplicate collapsed");

        // Re-applying the same effective list → no change.
        let mut t2 = InventoryChangeTracker::default();
        let changed = set_equipped_consumables(&mut i, &[owned], &mut t2);
        assert!(!changed, "re-equipping the same list is a no-op");
        assert!(!t2.modified_loadout.consumables_changed, "no spurious diff");
    }

    /// A potion TEMPLATE id routed through `equipmentUpdates` (the exact reported path)
    /// must NOT be silently skipped — it is treated as a consumable equip. Instanced
    /// gear in the same batch still equips normally.
    #[test]
    fn equipment_updates_routes_a_potion_id_to_consumables() {
        let mut i = inv();
        let potion = Uuid::from_u128(99);
        i.backpack.stackable_items.add(potion, 2);
        let gear_id = Uuid::from_u128(7);
        let gear_slot = Uuid::from_u128(100);
        i.backpack.items.0.insert(gear_id, item());
        let mut t = InventoryChangeTracker::default();

        // The client sends both a gear equip and a potion (template id) in equipmentUpdates.
        apply_equipment_updates(
            &mut i,
            &HashMap::from([
                (gear_slot, Some(gear_id)),
                (Uuid::from_u128(200), Some(potion)),
            ]),
            &mut t,
        );
        assert!(i.loadout.equipped_items.0.contains_key(&gear_slot), "gear equipped normally");
        assert_eq!(i.loadout.equipped_consumables, vec![potion], "potion routed to consumables, not dropped");
        assert!(t.modified_loadout.consumables_changed, "consumable change tracked for the diff");
    }
}
