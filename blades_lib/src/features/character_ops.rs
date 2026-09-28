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

/// The last level that grants an attribute point.
///
/// `level_rewards.json` is explicit and unanimous: `attribute_points` is **1 for
/// levels 1-50 and 0 for 51-100** — 50 entries each way, so the lifetime total
/// caps at 50 however high the character goes.
///
/// This is a constant rather than a lookup because `level_rewards.json` lives in
/// `deploy/static/`, which `.dockerignore` excludes from the image build — an
/// `include_str!` reaching in there compiles locally and then fails only in the
/// release build, which has already cost one broken deploy. The test
/// `the_attribute_point_cap_matches_level_rewards_json` asserts the constant
/// against the shipped file, so the data stays the authority without the trap.
pub const MAX_ATTRIBUTE_POINT_LEVEL: u16 = 50;

/// Spend a level: +1 level, and +1 point in the chosen attribute **only while the
/// new level actually grants one**.
///
/// The point used to be unconditional, so a character kept earning one at every
/// level for ever: at level 86 they would hold 85 where retail gives 50. Identity
/// check against captured retail characters — levels 20, 36, 37, 38 and 41 all
/// satisfy `stamina + magicka == level - 1` exactly, while the one captured
/// level-56 character holds **49**, i.e. still capped at level 50's total.
///
/// **This is the effect, not the decision.** It does not check that the level was
/// earned and it does not spend the experience — that is
/// [`crate::features::level_up::apply_level_up`], which gates on the captured XP
/// table and then calls this. Nothing outside that function should call it: doing
/// so hands out a free level.
pub fn apply_levelup(ch: &mut CompleteCharacter, attribute: Attribute) {
    ch.level = ch.level.saturating_add(1);
    if ch.level <= MAX_ATTRIBUTE_POINT_LEVEL {
        match attribute {
            Attribute::Stamina => ch.stamina_attribute_points = ch.stamina_attribute_points.saturating_add(1),
            Attribute::Magicka => ch.magicka_attribute_points = ch.magicka_attribute_points.saturating_add(1),
        }
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
/// Which item TYPE belongs in each equipment slot.
///
/// Derived from live data, not guessed: across 151 characters and 772 equipped
/// items on production, **every slot carried exactly one type, with no
/// exceptions**. The types are semantically obvious too — 2 weapon, 3 armour,
/// 9 shield, 10 ring, 11 necklace.
///
/// ```text
///   417e79de  type 2   weapon        151/151
///   862605de  type 9   shield/off-hand 140/140
///   897a600c  type 3   armour (body)
///   e273a4d7  type 3   armour (boots)
///   48021ab1  type 3   armour (helmet)
///   58b6d121  type 3   armour (gauntlets)
///   36d141e4  type 11  necklace
///   959c1931  type 10  ring
///   0d8f2023  type 10  ring
/// ```
///
/// An unknown slot maps to `None` and is left permissive — this table refuses
/// what it KNOWS is wrong rather than allowing only what it has seen, so a slot
/// nobody has filled yet cannot become unequippable.
fn expected_type_for_slot(slot: Uuid) -> Option<u64> {
    const MAP: [(&str, u64); 9] = [
        ("417e79de-c810-42f8-8273-f9759df6ae25", 2),
        ("862605de-c67f-4bce-b527-4e5fb6f25162", 9),
        ("897a600c-91d6-4449-af09-173da88a907e", 3),
        ("e273a4d7-fb87-4f7e-8f1e-398be59afbcb", 3),
        ("48021ab1-a1a6-487b-80a4-ca472a4d0c77", 3),
        ("58b6d121-2e23-4fa4-b892-c92ae2e2c4c5", 3),
        ("36d141e4-7783-466c-9565-6f90f09de428", 11),
        ("959c1931-bf85-4587-92ec-8ecaa58b06d5", 10),
        ("0d8f2023-4701-41e8-8bd5-92381d787456", 10),
    ];
    let s = slot.as_hyphenated().to_string();
    MAP.iter().find(|(k, _)| *k == s).map(|(_, t)| *t)
}

/// Is this item allowed in this slot?
///
/// WHY THIS EXISTS. The equip path took whatever the client named and put it
/// where the client said, with no check of any kind. Report #164: with a
/// two-handed weapon equipped, the off-hand filled with junk — "I switched
/// loadouts and it gave me a hiscarp, then I switched loadouts and it was a fire
/// aversion, then a large decoration" — a different arbitrary item each time.
/// That corrupt loadout is then what the arena renders, which is why the match
/// screen showed the wrong fighters.
///
/// It is also a soundness hole independent of that bug: a server that equips
/// whatever it is told is one where a crafted request can equip anything
/// anywhere.
///
/// Unknown slot or unknown template → allowed. This refuses what it knows is
/// wrong; it is not a whitelist.
pub fn item_allowed_in_slot(
    game_data: &crate::game_data::GameData,
    slot: Uuid,
    item_template_id: Uuid,
) -> bool {
    let Some(expected) = expected_type_for_slot(slot) else {
        return true;
    };
    match game_data.items_template.get(&item_template_id) {
        Some(t) => t.r#type == expected,
        None => true,
    }
}

pub fn apply_equipment_updates(
    inv: &mut CompleteInventory,
    updates: &HashMap<Uuid, Option<Uuid>>,
    tracker: &mut InventoryChangeTracker,
    game_data: Option<&crate::game_data::GameData>,
) {
    for (slot, target) in updates {
        // Refuse a nonsense equip BEFORE unequipping what is already there.
        // Order matters: rejecting after the removal would strip the slot and
        // leave it empty, which is a different bug with the same cause.
        if let (Some(gd), Some(item_id)) = (game_data, target.as_ref()) {
            if let Some(item) = inv.backpack.items.0.get(item_id) {
                if !item_allowed_in_slot(gd, *slot, item.item_template_id) {
                    continue;
                }
            }
        }
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

    /// Report #164: the equip path put ANY item in ANY slot.
    ///
    /// "If I have two handed weapons equipped. My off hand becomes equipped with
    /// a random item such as a gold bar, a decoration or a potion… I switched
    /// loadouts and it gave me a hiscarp, then I switched loadouts and it was a
    /// fire aversion, then a large decoration." A different arbitrary item each
    /// time, and that corrupt loadout is what the arena then renders.
    ///
    /// The slot→type table is derived from live data rather than guessed: across
    /// 151 characters and 772 equipped items on production, every slot carried
    /// exactly one item type with no exceptions.
    #[test]
    fn a_decoration_cannot_go_in_the_off_hand() {
        let gd = slot_test_game_data();
        let off_hand: Uuid = "862605de-c67f-4bce-b527-4e5fb6f25162".parse().unwrap();
        assert!(
            !item_allowed_in_slot(&gd, off_hand, DECORATION),
            "a decoration must not be equippable as a shield"
        );
        assert!(
            !item_allowed_in_slot(&gd, off_hand, POTION),
            "a potion must not be equippable as a shield"
        );
        assert!(
            item_allowed_in_slot(&gd, off_hand, SHIELD),
            "a shield must still go in the off-hand"
        );
    }

    /// The control that stops this becoming a lockout: every REAL pairing the
    /// live data shows must still be allowed. A check that refuses too much would
    /// leave players unable to equip their own gear, which is worse than the bug.
    #[test]
    fn every_real_slot_and_type_pairing_is_still_allowed() {
        let gd = slot_test_game_data();
        for (slot, item) in [
            ("417e79de-c810-42f8-8273-f9759df6ae25", WEAPON),
            ("862605de-c67f-4bce-b527-4e5fb6f25162", SHIELD),
            ("897a600c-91d6-4449-af09-173da88a907e", ARMOUR),
            ("e273a4d7-fb87-4f7e-8f1e-398be59afbcb", ARMOUR),
            ("48021ab1-a1a6-487b-80a4-ca472a4d0c77", ARMOUR),
            ("58b6d121-2e23-4fa4-b892-c92ae2e2c4c5", ARMOUR),
            ("36d141e4-7783-466c-9565-6f90f09de428", NECKLACE),
            ("959c1931-bf85-4587-92ec-8ecaa58b06d5", RING),
            ("0d8f2023-4701-41e8-8bd5-92381d787456", RING),
        ] {
            let s: Uuid = slot.parse().unwrap();
            assert!(
                item_allowed_in_slot(&gd, s, item),
                "slot {slot} must still accept its own item type"
            );
        }
    }

    /// Refuse what is known wrong, do not allow only what is known right. An
    /// unknown slot or an item missing from game data stays permissive, so a slot
    /// nobody has filled yet cannot silently become unequippable.
    #[test]
    fn unknown_slots_and_unknown_items_stay_permissive() {
        let gd = slot_test_game_data();
        let unknown_slot = Uuid::from_u128(0xDEAD);
        assert!(item_allowed_in_slot(&gd, unknown_slot, DECORATION));
        let off_hand: Uuid = "862605de-c67f-4bce-b527-4e5fb6f25162".parse().unwrap();
        assert!(item_allowed_in_slot(&gd, off_hand, Uuid::from_u128(0xBEEF)));
    }

    /// A refused equip must leave the slot exactly as it was — it must not strip
    /// what is already equipped. The check therefore runs BEFORE the unequip;
    /// rejecting afterwards would empty the slot, a different bug with the same
    /// cause.
    #[test]
    fn a_refused_equip_does_not_disturb_the_existing_item() {
        let gd = slot_test_game_data();
        let off_hand: Uuid = "862605de-c67f-4bce-b527-4e5fb6f25162".parse().unwrap();
        let shield_id = Uuid::from_u128(1);
        let junk_id = Uuid::from_u128(2);

        let mut inv = inv();
        inv.backpack.items.0.insert(shield_id, item_of(SHIELD));
        inv.backpack.items.0.insert(junk_id, item_of(DECORATION));

        let mut t = InventoryChangeTracker::default();
        apply_equipment_updates(&mut inv, &HashMap::from([(off_hand, Some(shield_id))]), &mut t, Some(&gd));
        assert!(inv.loadout.equipped_items.0.contains_key(&off_hand), "shield equips");

        let mut t2 = InventoryChangeTracker::default();
        apply_equipment_updates(&mut inv, &HashMap::from([(off_hand, Some(junk_id))]), &mut t2, Some(&gd));
        let still = inv.loadout.equipped_items.0.get(&off_hand).expect("slot must not be emptied");
        assert_eq!(still.id, shield_id, "the shield must still be equipped");
        assert!(inv.backpack.items.0.contains_key(&junk_id), "the junk stays in the backpack");
    }

    const WEAPON: Uuid = Uuid::from_u128(0x11);
    const ARMOUR: Uuid = Uuid::from_u128(0x33);
    const SHIELD: Uuid = Uuid::from_u128(0x99);
    const RING: Uuid = Uuid::from_u128(0xA0);
    const NECKLACE: Uuid = Uuid::from_u128(0xB0);
    const DECORATION: Uuid = Uuid::from_u128(0xC0);
    const POTION: Uuid = Uuid::from_u128(0xD0);

    fn slot_test_game_data() -> crate::game_data::GameData {
        let mk = |t: u64| crate::game_data::GameDataItem { name: String::new(), r#type: t };
        let mut gd = crate::game_data::GameData {
            items_template: std::collections::HashMap::new(),
            interactables: std::collections::HashMap::new(),
            quests: std::collections::HashMap::new(),
            dungeons: std::collections::HashMap::new(),
            events: std::collections::HashMap::new(),
        };
        gd.items_template.insert(WEAPON, mk(2));
        gd.items_template.insert(ARMOUR, mk(3));
        gd.items_template.insert(SHIELD, mk(9));
        gd.items_template.insert(RING, mk(10));
        gd.items_template.insert(NECKLACE, mk(11));
        gd.items_template.insert(DECORATION, mk(14));
        gd.items_template.insert(POTION, mk(8));
        gd
    }

    fn item_of(template: Uuid) -> crate::user_data::Item {
        let mut it = item();
        it.item_template_id = template;
        it
    }

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
            grade: None,
            arcane_tier: None,
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
        apply_equipment_updates(&mut i, &HashMap::from([(slot, Some(item_id))]), &mut t, None);
        assert!(i.loadout.equipped_items.0.contains_key(&slot));
        assert!(!i.backpack.items.0.contains_key(&item_id), "left the backpack");

        // Unequip.
        let mut t2 = InventoryChangeTracker::default();
        apply_equipment_updates(&mut i, &HashMap::from([(slot, None)]), &mut t2, None);
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
            None,
        );
        assert!(i.loadout.equipped_items.0.contains_key(&gear_slot), "gear equipped normally");
        assert_eq!(i.loadout.equipped_consumables, vec![potion], "potion routed to consumables, not dropped");
        assert!(t.modified_loadout.consumables_changed, "consumable change tracked for the diff");
    }
}

#[cfg(test)]
mod attribute_point_cap_tests {
    use super::*;

    fn at_level(level: u16) -> CompleteCharacter {
        let mut ch = CompleteCharacter::default();
        ch.level = level;
        ch
    }

    /// THE CONSTANT MUST MATCH THE SHIPPED TABLE.
    ///
    /// `MAX_ATTRIBUTE_POINT_LEVEL` is hardcoded because `deploy/static/` is excluded
    /// from the image build context, so it cannot be `include_str!`d. That is only
    /// safe while something checks it against the file — otherwise the number drifts
    /// silently and nothing ever notices.
    #[test]
    fn the_attribute_point_cap_matches_level_rewards_json() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/level_rewards.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let table: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_str(&raw).unwrap();

        let granting: Vec<u16> = table
            .iter()
            .filter(|(_, v)| v["attribute_points"].as_i64() == Some(1))
            .filter_map(|(k, _)| k.parse::<u16>().ok())
            .collect();
        let withholding: Vec<u16> = table
            .iter()
            .filter(|(_, v)| v["attribute_points"].as_i64() == Some(0))
            .filter_map(|(k, _)| k.parse::<u16>().ok())
            .collect();

        assert!(!granting.is_empty() && !withholding.is_empty(), "both bands must exist");
        assert_eq!(
            granting.iter().copied().max().unwrap(),
            MAX_ATTRIBUTE_POINT_LEVEL,
            "the highest level granting a point must be the constant"
        );
        assert_eq!(
            withholding.iter().copied().min().unwrap(),
            MAX_ATTRIBUTE_POINT_LEVEL + 1,
            "the first level withholding a point must be one above the constant"
        );
    }

    /// Levelling INTO the cap still pays; levelling past it does not.
    #[test]
    fn the_point_stops_at_the_cap() {
        // 49 -> 50 pays.
        let mut ch = at_level(MAX_ATTRIBUTE_POINT_LEVEL - 1);
        apply_levelup(&mut ch, Attribute::Stamina);
        assert_eq!(ch.level, MAX_ATTRIBUTE_POINT_LEVEL);
        assert_eq!(ch.stamina_attribute_points, 1, "the level that reaches the cap still pays");

        // 50 -> 51 does not.
        let mut ch = at_level(MAX_ATTRIBUTE_POINT_LEVEL);
        apply_levelup(&mut ch, Attribute::Stamina);
        assert_eq!(ch.level, MAX_ATTRIBUTE_POINT_LEVEL + 1);
        assert_eq!(ch.stamina_attribute_points, 0, "the level past the cap pays nothing");
    }

    /// THE CONTROL: the level itself must keep rising, and the character must still
    /// be written back. A fix that simply refused the level-up past 50 would satisfy
    /// "no extra point" while silently capping progression — a worse bug.
    #[test]
    fn levelling_past_the_cap_still_levels_and_still_bumps_version() {
        let mut ch = at_level(80);
        let before = ch.version;
        apply_levelup(&mut ch, Attribute::Magicka);
        assert_eq!(ch.level, 81, "the level must still advance past the cap");
        assert_eq!(ch.magicka_attribute_points, 0, "but no point is granted");
        assert!(ch.version > before, "the character must still be persisted");
    }

    /// The retail identity this reproduces: total points == level - 1 below the cap,
    /// and frozen at the cap above it. Measured on captured characters — levels 20,
    /// 36, 37, 38, 41 match exactly; the one captured level-56 holds 49.
    #[test]
    fn total_points_track_retails_measured_shape() {
        let mut ch = at_level(1);
        for _ in 0..60 {
            apply_levelup(&mut ch, Attribute::Stamina);
        }
        assert_eq!(ch.level, 61);
        assert_eq!(
            ch.stamina_attribute_points, 49,
            "a character past the cap holds exactly what level 50 granted"
        );
    }
}
