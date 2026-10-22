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

use crate::user_data::{
    CompleteCharacter, CompleteInventory, InventoryChangeTracker, SingleEquippedItem,
};

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
            Attribute::Stamina => {
                ch.stamina_attribute_points = ch.stamina_attribute_points.saturating_add(1)
            }
            Attribute::Magicka => {
                ch.magicka_attribute_points = ch.magicka_attribute_points.saturating_add(1)
            }
        }
    }
    ch.version += 1;
}

/// Reallocate attribute points (respec): set the totals as requested and clear
/// learned/equipped abilities so the client recomputes all skill points as free.
pub fn apply_respec(ch: &mut CompleteCharacter, stamina: u32, magicka: u32) {
    ch.stamina_attribute_points = stamina;
    ch.magicka_attribute_points = magicka;
    ch.abilities = Value::Null;
    ch.equipped_abilities = Value::Null;
    ch.version += 1;
}

/// Raise the backpack capacity tier.
pub fn upgrade_inventory(ch: &mut CompleteCharacter) {
    ch.inventory_level = ch.inventory_level.saturating_add(1);
    ch.version += 1;
}

/// Store the full learned/upgraded ability map (`{abilityId: level}`).
pub fn replace_abilities(ch: &mut CompleteCharacter, updates: &Value) {
    ch.abilities = if updates.is_object() {
        updates.clone()
    } else {
        json!({})
    };
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
    let profile = normalize_loadout_profile(profile);
    if !ch.loadout_profiles.is_array() {
        ch.loadout_profiles = json!([]);
    }
    let arr = ch
        .loadout_profiles
        .as_array_mut()
        .expect("just set to array");
    while arr.len() <= index {
        arr.push(Value::Null);
    }
    arr[index] = profile;
    ch.version += 1;
}

/// Retail saved loadout profiles never carry equipped consumables. Across 35,779
/// captured profile objects, the keysets are:
///
/// * `name`, `iconIndex`, `colorIndex`, `keepShield`, `equippedItems`
/// * the same plus `equippedAbilities`
///
/// The live loadout endpoint has an `equippedConsumables` field, but persisting that
/// field into `character.loadoutProfiles` creates a profile JSON shape the Loadouts UI
/// never saw in retail. Drop only that non-retail profile member; leave null gaps,
/// empty slots, unknown future fields, and every retail member untouched.
pub fn normalize_loadout_profile(mut profile: Value) -> Value {
    normalize_profile_object(&mut profile);
    profile
}

/// One saved profile, reshaped to what the retail server served back.
///
/// Besides dropping `equippedConsumables`, an EMPTY item slot loses its `itemId`
/// key. The client saves an empty slot as `{"slot": …, "itemId": ""}` (252 of 594
/// item entries in captured `POST …/loadouts/profiles/{n}` bodies), but retail never
/// served that form back: across the capture snapshot's `GET /characters/{id}`
/// responses, 4,617 empty slots are `{"slot": …}` with NO `itemId`, and zero carry
/// `""` or `null`. We stored the client's `""` verbatim and served it; the Loadouts
/// screen of a player whose build had four empty slots then shuddered and froze
/// (tracker #274/#278). Filled slots and every other field are untouched.
fn normalize_profile_object(profile: &mut Value) {
    let Some(obj) = profile.as_object_mut() else {
        return;
    };
    obj.remove("equippedConsumables");
    if let Some(items) = obj.get_mut("equippedItems").and_then(Value::as_array_mut) {
        for entry in items {
            if let Some(e) = entry.as_object_mut() {
                let empty = match e.get("itemId") {
                    Some(Value::Null) => true,
                    Some(Value::String(s)) => s.trim().is_empty(),
                    _ => false,
                };
                if empty {
                    e.remove("itemId");
                }
            }
        }
    }
}

/// Normalize every saved loadout profile on a character before serving or rewriting it.
pub fn normalize_loadout_profiles_for_retail(ch: &mut CompleteCharacter) {
    let Some(profiles) = ch.loadout_profiles.as_array_mut() else {
        return;
    };
    for profile in profiles {
        normalize_profile_object(profile);
    }
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

/// The instanced gear item with this id, wherever the player holds it: in the backpack
/// or already worn in some equipment slot.
///
/// An equip request can name either. Switching a saved build (loadout profile) sends
/// the build's full slot map, and a ring that is worn in ring slot A but saved in ring
/// slot B in the build arrives as "slot B ← an item that is not in the backpack".
pub fn owned_gear_item<'a>(
    inv: &'a CompleteInventory,
    item_id: &Uuid,
) -> Option<&'a crate::user_data::Item> {
    inv.backpack.items.0.get(item_id).or_else(|| {
        inv.loadout
            .equipped_items
            .0
            .values()
            .find(|e| e.id == *item_id)
            .map(|e| &e.item)
    })
}

/// Report #395: switching builds unequipped a ring and dropped it in the backpack
/// marked "new", every time.
///
/// The request is a whole slot map, `{slotId: itemId | null}`, but the old code
/// applied it one slot at a time in `HashMap` order: unequip what is in this slot,
/// then equip the target FROM THE BACKPACK. A ring that is already worn in the
/// other ring slot is not in the backpack, so it was skipped as a "stale id" —
/// and when its own slot came up later it was unequipped into the backpack. Two
/// rings that trade places (the same ring saved in ring slot 1 in one build and
/// ring slot 2 in another) therefore always ended with one ring slot empty; a
/// one-way move broke or not depending on hash order. Retail landed all 27
/// cross-slot ring moves (10 of them full swaps) in the captured
/// `/loadouts/current` traffic, and never returned the moved ring to the backpack.
///
/// So the map is applied as a whole, in two phases:
///   1. every slot the request writes gives its current item back to the backpack;
///   2. every target is equipped — from the backpack, or, when it is worn in a slot
///      this request does not write, moved out of that slot.
/// The outcome no longer depends on iteration order.
pub fn apply_equipment_updates(
    inv: &mut CompleteInventory,
    updates: &HashMap<Uuid, Option<Uuid>>,
    tracker: &mut InventoryChangeTracker,
    game_data: Option<&crate::game_data::GameData>,
) {
    // Refuse a nonsense equip BEFORE unequipping what is already there.
    // Order matters: rejecting after the removal would strip the slot and
    // leave it empty, which is a different bug with the same cause.
    let accepted: Vec<(Uuid, Option<Uuid>)> = updates
        .iter()
        .filter(|(slot, target)| {
            let (Some(gd), Some(item_id)) = (game_data, target.as_ref()) else {
                return true;
            };
            match owned_gear_item(inv, item_id) {
                Some(item) => item_allowed_in_slot(gd, **slot, item.item_template_id),
                None => true,
            }
        })
        .map(|(slot, target)| (*slot, *target))
        .collect();
    let written: std::collections::HashSet<Uuid> = accepted.iter().map(|(s, _)| *s).collect();

    // Report #356: a slot whose target is the item ALREADY worn there is kept, not
    // round-tripped through the backpack. The client fills every empty slot of a saved
    // build with what is currently worn, so switching to an armour-only build sends the
    // worn weapon, shield and jewellery back unchanged. Round-tripping them listed each
    // one in `backpack.removedItems` — "this item left your backpack" for an item that
    // was never in it. Retail did not: across the capture snapshot's 1,236 gear
    // switches, 6,977 of 6,987 kept slots (368 of them the weapon) were echoed in
    // `equippedItems` only, never in the backpack diff.
    let kept: std::collections::HashSet<Uuid> = accepted
        .iter()
        .filter(|(slot, target)| {
            target.is_some() && inv.loadout.equipped_items.0.get(slot).map(|e| e.id) == *target
        })
        .map(|(s, _)| *s)
        .collect();
    for slot in &kept {
        tracker
            .modified_loadout
            .modified_equipped_items
            .insert(*slot);
    }

    // Phase 1: return whatever occupies each written slot to the backpack.
    for (slot, _) in accepted.iter().filter(|(s, _)| !kept.contains(s)) {
        if let Some(prev) = inv.loadout.equipped_items.0.remove(slot) {
            tracker
                .modified_loadout
                .modified_equipped_items
                .insert(*slot);
            inv.backpack.items.0.insert(prev.id, prev.item);
            tracker.modified_backpack.items.insert(prev.id);
        }
    }

    // Phase 2: equip every target.
    for (slot, target) in accepted.iter().filter(|(s, _)| !kept.contains(s)) {
        let Some(item_id) = target else { continue };
        if let Some(item) = inv.backpack.items.0.remove(item_id) {
            // Instanced gear from the backpack (including an item phase 1 just
            // returned there from another written slot).
            tracker.modified_backpack.items.insert(*item_id);
            equip_into(inv, tracker, *slot, *item_id, item);
        } else if let Some(from) = inv
            .loadout
            .equipped_items
            .0
            .iter()
            .find(|(s, e)| e.id == *item_id && !written.contains(*s))
            .map(|(s, _)| *s)
        {
            // Worn in a slot this request leaves alone: move it across. (A slot the
            // request DOES write was emptied in phase 1, so finding the item there
            // means two slots named the same item — the first one keeps it.)
            let moved = inv
                .loadout
                .equipped_items
                .0
                .remove(&from)
                .expect("slot was just found");
            tracker
                .modified_loadout
                .modified_equipped_items
                .insert(from);
            equip_into(inv, tracker, *slot, *item_id, moved.item);
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

fn equip_into(
    inv: &mut CompleteInventory,
    tracker: &mut InventoryChangeTracker,
    slot: Uuid,
    item_id: Uuid,
    item: crate::user_data::Item,
) {
    inv.loadout.equipped_items.0.insert(
        slot,
        SingleEquippedItem {
            id: item_id,
            slot,
            item,
        },
    );
    tracker
        .modified_loadout
        .modified_equipped_items
        .insert(slot);
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
    #[test]
    fn an_empty_profile_slot_is_served_without_an_itemid_like_retail() {
        let mut ch = CompleteCharacter::default();
        set_loadout_profile(
            &mut ch,
            0,
            json!({
                "name": "Physical armour",
                "iconIndex": 0,
                "colorIndex": 0,
                "keepShield": true,
                "equippedItems": [
                    {"slot": "897a600c-91d6-4449-af09-173da88a907e", "itemId": "6d8a9f14-7d79-4b5a-8a2a-7f59d2083836"},
                    {"slot": "417e79de-c810-42f8-8273-f9759df6ae25", "itemId": ""},
                    {"slot": "36d141e4-7783-466c-9565-6f90f09de428", "itemId": null}
                ],
                "equippedAbilities": [{"slot": "abilitySlot0", "itemId": "91078132-ef5c-492a-97f2-ac69be5140a8"}]
            }),
        );
        let items = ch.loadout_profiles[0]["equippedItems"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            items.len(),
            3,
            "every slot entry stays, as retail served 9 of 9"
        );
        assert_eq!(
            items[0]["itemId"], "6d8a9f14-7d79-4b5a-8a2a-7f59d2083836",
            "a filled slot is untouched"
        );
        assert!(
            items[1].get("itemId").is_none(),
            "an empty \"\" slot is served with no itemId key"
        );
        assert!(
            items[2].get("itemId").is_none(),
            "a null slot is served with no itemId key"
        );
        assert_eq!(
            ch.loadout_profiles[0]["equippedAbilities"][0]["itemId"],
            "91078132-ef5c-492a-97f2-ac69be5140a8",
            "abilities are not touched"
        );
    }

    #[test]
    fn stored_profiles_with_empty_slots_are_repaired_on_serve() {
        let mut ch = CompleteCharacter::default();
        ch.loadout_profiles = json!([{
            "name": "old",
            "equippedItems": [{"slot": "417e79de-c810-42f8-8273-f9759df6ae25", "itemId": ""}]
        }]);
        normalize_loadout_profiles_for_retail(&mut ch);
        assert!(
            ch.loadout_profiles[0]["equippedItems"][0]
                .get("itemId")
                .is_none()
        );
    }

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
        apply_equipment_updates(
            &mut inv,
            &HashMap::from([(off_hand, Some(shield_id))]),
            &mut t,
            Some(&gd),
        );
        assert!(
            inv.loadout.equipped_items.0.contains_key(&off_hand),
            "shield equips"
        );

        let mut t2 = InventoryChangeTracker::default();
        apply_equipment_updates(
            &mut inv,
            &HashMap::from([(off_hand, Some(junk_id))]),
            &mut t2,
            Some(&gd),
        );
        let still = inv
            .loadout
            .equipped_items
            .0
            .get(&off_hand)
            .expect("slot must not be emptied");
        assert_eq!(still.id, shield_id, "the shield must still be equipped");
        assert!(
            inv.backpack.items.0.contains_key(&junk_id),
            "the junk stays in the backpack"
        );
    }

    const RING_SLOT_1: &str = "0d8f2023-4701-41e8-8bd5-92381d787456";
    const RING_SLOT_2: &str = "959c1931-bf85-4587-92ec-8ecaa58b06d5";

    fn wear(inv: &mut CompleteInventory, slot: Uuid, id: Uuid, template: Uuid) {
        inv.loadout.equipped_items.0.insert(
            slot,
            SingleEquippedItem {
                id,
                slot,
                item: item_of(template),
            },
        );
    }

    fn worn(inv: &CompleteInventory, slot: Uuid) -> Option<Uuid> {
        inv.loadout.equipped_items.0.get(&slot).map(|e| e.id)
    }

    /// Report #395 (Sephoris): "When switching saved equipment, a ring is unequipped
    /// and then marked as new. I have to re-equip it every time."
    ///
    /// Her builds save the SAME two rings in opposite ring slots ("Feuer Gift": slot 1
    /// = Grand Inquisitor's Ring, slot 2 = the other; "Frost": the reverse). Switching
    /// between them is a swap: each target is worn in the other ring slot, not in
    /// the backpack. The old one-slot-at-a-time apply skipped the first target as an
    /// unknown id and then unequipped it into the backpack, so one ring slot always
    /// came back empty and the ring reappeared in the backpack diff (= "new").
    #[test]
    fn switching_builds_that_swap_two_worn_rings_keeps_both_rings_on() {
        let gd = slot_test_game_data();
        let (s1, s2): (Uuid, Uuid) = (RING_SLOT_1.parse().unwrap(), RING_SLOT_2.parse().unwrap());
        let inquisitor = Uuid::from_u128(0x90d1);
        let other = Uuid::from_u128(0x73a0);
        let weapon_slot: Uuid = "417e79de-c810-42f8-8273-f9759df6ae25".parse().unwrap();
        let dagger = Uuid::from_u128(0xDA);

        let mut inv = inv();
        wear(&mut inv, s1, inquisitor, RING);
        wear(&mut inv, s2, other, RING);
        wear(&mut inv, weapon_slot, dagger, WEAPON);

        // The build switch sends the build's whole slot map, unchanged slots included.
        let mut t = InventoryChangeTracker::default();
        apply_equipment_updates(
            &mut inv,
            &HashMap::from([
                (s1, Some(other)),
                (s2, Some(inquisitor)),
                (weapon_slot, Some(dagger)),
            ]),
            &mut t,
            Some(&gd),
        );

        assert_eq!(
            worn(&inv, s1),
            Some(other),
            "ring slot 1 holds the other ring"
        );
        assert_eq!(
            worn(&inv, s2),
            Some(inquisitor),
            "ring slot 2 holds the Inquisitor ring"
        );
        assert_eq!(worn(&inv, weapon_slot), Some(dagger));
        assert!(
            inv.backpack.items.0.is_empty(),
            "no ring falls into the backpack"
        );

        let update = inv.generate_client_update(&t);
        assert!(
            update.loadout.unequipped_item_slots.is_empty(),
            "no slot is reported empty"
        );
        assert_eq!(update.loadout.equipped_items.0[&s1].id, other);
        assert_eq!(update.loadout.equipped_items.0[&s2].id, inquisitor);
        assert!(
            update.backpack.items.0.is_empty(),
            "a ring served as a backpack item is what the client flags as new"
        );
    }

    /// Report #356 (SpaceMunk): "switching to a loadout containing only armour freezes
    /// the game".
    ///
    /// The client fills a build's empty slots with what is worn now, so the switch to
    /// an armour-only build names the worn weapon, shield and ring again and only the
    /// armour changes. Retail's reply (capture 53184, a build saved with an empty
    /// weapon and shield) listed only the CHANGED items in `backpack.removedItems`. We
    /// round-tripped every slot through the backpack and so told the client that the
    /// weapon in its hand had just left its backpack.
    #[test]
    fn an_armour_only_switch_reports_only_the_armour_in_the_backpack_diff() {
        let gd = slot_test_game_data();
        let slot = |s: &str| -> Uuid { s.parse().unwrap() };
        let (weapon_slot, shield_slot, body_slot, ring_slot) = (
            slot("417e79de-c810-42f8-8273-f9759df6ae25"),
            slot("862605de-c67f-4bce-b527-4e5fb6f25162"),
            slot("897a600c-91d6-4449-af09-173da88a907e"),
            slot(RING_SLOT_1),
        );
        let (sword, shield, ring) = (Uuid::from_u128(0x5a), Uuid::from_u128(0x5b), Uuid::from_u128(0x5c));
        let (old_cuirass, heavy_cuirass) = (Uuid::from_u128(0xc1), Uuid::from_u128(0xc2));

        let mut inv = inv();
        wear(&mut inv, weapon_slot, sword, WEAPON);
        wear(&mut inv, shield_slot, shield, SHIELD);
        wear(&mut inv, ring_slot, ring, RING);
        wear(&mut inv, body_slot, old_cuirass, ARMOUR);
        inv.backpack.items.0.insert(heavy_cuirass, item_of(ARMOUR));

        let mut t = InventoryChangeTracker::default();
        apply_equipment_updates(
            &mut inv,
            &HashMap::from([
                (weapon_slot, Some(sword)),
                (shield_slot, Some(shield)),
                (ring_slot, Some(ring)),
                (body_slot, Some(heavy_cuirass)),
            ]),
            &mut t,
            Some(&gd),
        );

        assert_eq!(worn(&inv, weapon_slot), Some(sword));
        assert_eq!(worn(&inv, shield_slot), Some(shield));
        assert_eq!(worn(&inv, ring_slot), Some(ring));
        assert_eq!(worn(&inv, body_slot), Some(heavy_cuirass));

        let update = inv.generate_client_update(&t);
        assert_eq!(
            update.backpack.removed_items,
            std::collections::HashSet::from([heavy_cuirass]),
            "only the cuirass that left the backpack is reported removed"
        );
        assert_eq!(
            update.backpack.items.0.keys().copied().collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from([old_cuirass]),
            "only the cuirass taken off arrives in the backpack"
        );
        // Retail still echoes every requested slot in equippedItems (1,236 of 1,236).
        assert_eq!(update.loadout.equipped_items.0.len(), 4);
        assert_eq!(update.loadout.equipped_items.0[&weapon_slot].id, sword);
        assert!(update.loadout.unequipped_item_slots.is_empty());
    }

    /// A one-way move (worn ring goes to the other ring slot, a backpack ring takes
    /// its place) used to depend on `HashMap` order. Fresh maps reseed the hasher, so
    /// repeating the call covers both orders.
    #[test]
    fn moving_a_worn_ring_to_the_other_ring_slot_does_not_depend_on_order() {
        let gd = slot_test_game_data();
        let (s1, s2): (Uuid, Uuid) = (RING_SLOT_1.parse().unwrap(), RING_SLOT_2.parse().unwrap());
        let (a, b, c) = (
            Uuid::from_u128(0xA),
            Uuid::from_u128(0xB),
            Uuid::from_u128(0xC),
        );
        for _ in 0..64 {
            let mut inv = inv();
            wear(&mut inv, s1, a, RING);
            wear(&mut inv, s2, b, RING);
            inv.backpack.items.0.insert(c, item_of(RING));
            let mut t = InventoryChangeTracker::default();
            apply_equipment_updates(
                &mut inv,
                &HashMap::from([(s1, Some(c)), (s2, Some(a))]),
                &mut t,
                Some(&gd),
            );
            assert_eq!(worn(&inv, s1), Some(c));
            assert_eq!(worn(&inv, s2), Some(a));
            assert_eq!(
                inv.backpack.items.0.keys().copied().collect::<Vec<_>>(),
                vec![b],
                "only the ring that left the build goes to the backpack"
            );
        }
    }

    /// Naming a worn item for a slot the request does not otherwise touch moves it
    /// across and empties its old slot — it is never in two slots at once.
    #[test]
    fn a_worn_ring_named_for_the_other_slot_moves_rather_than_duplicates() {
        let gd = slot_test_game_data();
        let (s1, s2): (Uuid, Uuid) = (RING_SLOT_1.parse().unwrap(), RING_SLOT_2.parse().unwrap());
        let a = Uuid::from_u128(0xA);
        let mut inv = inv();
        wear(&mut inv, s1, a, RING);
        let mut t = InventoryChangeTracker::default();
        apply_equipment_updates(&mut inv, &HashMap::from([(s2, Some(a))]), &mut t, Some(&gd));
        assert_eq!(worn(&inv, s2), Some(a));
        assert_eq!(worn(&inv, s1), None);
        assert!(inv.backpack.items.0.is_empty());
        let update = inv.generate_client_update(&t);
        assert!(update.loadout.unequipped_item_slots.contains(&s1));
    }

    /// One item named for two slots ends up in exactly one of them.
    #[test]
    fn one_item_named_for_both_ring_slots_is_worn_once() {
        let gd = slot_test_game_data();
        let (s1, s2): (Uuid, Uuid) = (RING_SLOT_1.parse().unwrap(), RING_SLOT_2.parse().unwrap());
        let a = Uuid::from_u128(0xA);
        for _ in 0..16 {
            let mut inv = inv();
            inv.backpack.items.0.insert(a, item_of(RING));
            let mut t = InventoryChangeTracker::default();
            apply_equipment_updates(
                &mut inv,
                &HashMap::from([(s1, Some(a)), (s2, Some(a))]),
                &mut t,
                Some(&gd),
            );
            let copies = [worn(&inv, s1), worn(&inv, s2)]
                .iter()
                .filter(|w| **w == Some(a))
                .count()
                + inv.backpack.items.0.contains_key(&a) as usize;
            assert_eq!(copies, 1, "the ring exists exactly once");
        }
    }

    /// The slot-type check still applies to a worn item being moved: a ring cannot
    /// be moved out of its ring slot into the weapon slot.
    #[test]
    fn a_worn_ring_cannot_be_moved_into_the_weapon_slot() {
        let gd = slot_test_game_data();
        let s1: Uuid = RING_SLOT_1.parse().unwrap();
        let weapon_slot: Uuid = "417e79de-c810-42f8-8273-f9759df6ae25".parse().unwrap();
        let (ring, dagger) = (Uuid::from_u128(0xA), Uuid::from_u128(0xDA));
        let mut inv = inv();
        wear(&mut inv, s1, ring, RING);
        wear(&mut inv, weapon_slot, dagger, WEAPON);
        let mut t = InventoryChangeTracker::default();
        apply_equipment_updates(
            &mut inv,
            &HashMap::from([(weapon_slot, Some(ring))]),
            &mut t,
            Some(&gd),
        );
        assert_eq!(worn(&inv, weapon_slot), Some(dagger));
        assert_eq!(worn(&inv, s1), Some(ring));
    }

    const WEAPON: Uuid = Uuid::from_u128(0x11);
    const ARMOUR: Uuid = Uuid::from_u128(0x33);
    const SHIELD: Uuid = Uuid::from_u128(0x99);
    const RING: Uuid = Uuid::from_u128(0xA0);
    const NECKLACE: Uuid = Uuid::from_u128(0xB0);
    const DECORATION: Uuid = Uuid::from_u128(0xC0);
    const POTION: Uuid = Uuid::from_u128(0xD0);

    fn slot_test_game_data() -> crate::game_data::GameData {
        let mk = |t: u64| crate::game_data::GameDataItem {
            name: String::new(),
            r#type: t,
        };
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
    fn abilities_save_replaces_the_full_opaque_value() {
        let mut ch = CompleteCharacter::default();
        let a = Uuid::from_u128(1).to_string();
        let stale = Uuid::from_u128(2).to_string();
        ch.abilities = json!({ &stale: 4 });

        replace_abilities(&mut ch, &json!({ &a: 3 }));
        assert_eq!(ch.abilities[&a], 3);
        assert!(
            ch.abilities.get(&stale).is_none(),
            "retail sends the full post-reset map; omitted stale abilities must be removed"
        );
    }

    #[test]
    fn respec_clears_learned_and_equipped_abilities() {
        let ability = Uuid::from_u128(1).to_string();
        let mut ch = CompleteCharacter {
            stamina_attribute_points: 8,
            magicka_attribute_points: 4,
            abilities: json!({ &ability: 3 }),
            equipped_abilities: json!({ "0": &ability }),
            ..CompleteCharacter::default()
        };
        let version = ch.version;

        apply_respec(&mut ch, 12, 0);

        assert_eq!(ch.stamina_attribute_points, 12);
        assert_eq!(ch.magicka_attribute_points, 0);
        assert!(
            ch.abilities.is_null(),
            "respec response must omit abilities"
        );
        assert!(
            ch.equipped_abilities.is_null(),
            "resetting learned abilities also clears invalid equipped slots"
        );
        assert_eq!(ch.version, version + 1);
    }

    #[test]
    fn loadout_profile_stored_at_index() {
        let mut ch = CompleteCharacter::default();
        set_loadout_profile(&mut ch, 2, json!({ "name": "clutch" }));
        assert_eq!(ch.loadout_profiles[2]["name"], "clutch");
        assert!(ch.loadout_profiles[0].is_null(), "gaps padded with null");
    }

    #[test]
    fn loadout_profile_save_strips_nonretail_consumables_field() {
        let mut ch = CompleteCharacter::default();
        set_loadout_profile(
            &mut ch,
            0,
            json!({
                "name": "clutch",
                "iconIndex": 1,
                "colorIndex": 2,
                "keepShield": true,
                "equippedItems": [
                    { "slot": "417e79de-c810-42f8-8273-f9759df6ae25", "itemId": "" }
                ],
                "equippedConsumables": []
            }),
        );

        let profile = &ch.loadout_profiles[0];
        assert!(profile.get("equippedConsumables").is_none());
        assert!(
            profile["equippedItems"][0].get("itemId").is_none(),
            "an empty slot is served without itemId, as retail did"
        );
        assert_eq!(profile["keepShield"], true);
    }

    #[test]
    fn loadout_profile_normalization_preserves_retail_shape_and_gaps() {
        let mut ch = CompleteCharacter::default();
        ch.loadout_profiles = json!([
            null,
            {
                "name": "retail",
                "iconIndex": 0,
                "colorIndex": 0,
                "keepShield": false,
                "equippedItems": [
                    { "slot": "862605de-c67f-4bce-b527-4e5fb6f25162" }
                ],
                "equippedAbilities": [
                    { "slot": "abilitySlot0", "itemId": "91078132-ef5c-492a-97f2-ac69be5140a8" }
                ],
                "note": "leave unknown profile fields alone"
            },
            {
                "name": "dirty",
                "equippedConsumables": [
                    "00000000-0000-0000-0000-000000000001"
                ]
            }
        ]);

        normalize_loadout_profiles_for_retail(&mut ch);

        assert!(
            ch.loadout_profiles[0].is_null(),
            "null profile gaps are retail"
        );
        assert_eq!(
            ch.loadout_profiles[1]["equippedItems"][0]["slot"],
            "862605de-c67f-4bce-b527-4e5fb6f25162"
        );
        assert_eq!(
            ch.loadout_profiles[1]["equippedAbilities"][0]["slot"],
            "abilitySlot0"
        );
        assert_eq!(
            ch.loadout_profiles[1]["note"],
            "leave unknown profile fields alone"
        );
        assert!(ch.loadout_profiles[2].get("equippedConsumables").is_none());
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
        apply_equipment_updates(
            &mut i,
            &HashMap::from([(slot, Some(item_id))]),
            &mut t,
            None,
        );
        assert!(i.loadout.equipped_items.0.contains_key(&slot));
        assert!(
            !i.backpack.items.0.contains_key(&item_id),
            "left the backpack"
        );

        // Unequip.
        let mut t2 = InventoryChangeTracker::default();
        apply_equipment_updates(&mut i, &HashMap::from([(slot, None)]), &mut t2, None);
        assert!(!i.loadout.equipped_items.0.contains_key(&slot));
        assert!(
            i.backpack.items.0.contains_key(&item_id),
            "returned to backpack"
        );
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
        assert_eq!(
            i.loadout.equipped_consumables,
            vec![potion],
            "potion is equipped"
        );
        assert_eq!(
            i.backpack.stackable_items.count(potion),
            5,
            "equipping does not consume the stack"
        );
        assert!(
            t.modified_loadout.consumables_changed,
            "tracker flags the change"
        );

        // The loadout diff echoes the equipped-consumable list so the client sees it.
        let diff = i.loadout.generate_client_update(&t.modified_loadout);
        assert_eq!(
            diff.equipped_consumables,
            Some(vec![potion]),
            "diff carries the equipped consumable"
        );
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
        assert_eq!(
            i.loadout.equipped_consumables,
            vec![owned],
            "unowned dropped, duplicate collapsed"
        );

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
        assert!(
            i.loadout.equipped_items.0.contains_key(&gear_slot),
            "gear equipped normally"
        );
        assert_eq!(
            i.loadout.equipped_consumables,
            vec![potion],
            "potion routed to consumables, not dropped"
        );
        assert!(
            t.modified_loadout.consumables_changed,
            "consumable change tracked for the diff"
        );
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

        assert!(
            !granting.is_empty() && !withholding.is_empty(),
            "both bands must exist"
        );
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
        assert_eq!(
            ch.stamina_attribute_points, 1,
            "the level that reaches the cap still pays"
        );

        // 50 -> 51 does not.
        let mut ch = at_level(MAX_ATTRIBUTE_POINT_LEVEL);
        apply_levelup(&mut ch, Attribute::Stamina);
        assert_eq!(ch.level, MAX_ATTRIBUTE_POINT_LEVEL + 1);
        assert_eq!(
            ch.stamina_attribute_points, 0,
            "the level past the cap pays nothing"
        );
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
