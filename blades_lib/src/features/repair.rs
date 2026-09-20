//! Item repair — the pure half of `POST /…/characters/{id}/repairs`.
//!
//! # What retail did
//!
//! Gear in Blades loses durability and eventually breaks; the player repairs it
//! at the blacksmith. Two numbers govern the screen, and both are shipped inside
//! the client's own asset bundles — which is why the client can price a repair
//! with no server round-trip, and why the server MUST agree with it exactly or
//! the player sees a cost the repair does not clear.
//!
//! **Max durability** is an absolute per-`(itemTemplateId, temperingLevel)`
//! value. From the il2cpp dump:
//!
//! ```text
//! ItemTemplate (dump.cs:559428)
//!     public const float DEFAULT_DURABILITY = 100;
//!     [SerializeField] protected float _maxDurability;              // temper 0
//!     [SerializeField] private ItemTemperProperties[] _temperProperties;
//! ItemTemperProperties (dump.cs:559379)
//!     _damage, _twoHandedDamage, _protection, _value, _maxDurability
//! ```
//!
//! so temper 0 reads `ItemTemplate._maxDurability` and temper 1..10 read
//! `_temperProperties[level - 1]._maxDurability`.
//!
//! **Repair price** comes from `RepairRecipe` (dump.cs:553248), whose `_inputs`
//! are pure Gold and whose `AddInputs(map, inputs, float itemCondition)` scales
//! that gold by the item's condition. So the recipe's gold is the price of
//! repairing from BROKEN to full, and a partly-worn item costs the same
//! fraction. All 626 shipped recipes charge only Gold; the per-temper and
//! per-enchantment surcharge arrays exist but are zero throughout.
//!
//! # The bug this module fixes (tracker #30)
//!
//! The previous implementation looked `(template, temper)` up in a table built
//! from CAPTURED traffic by taking the largest durability ever observed for a
//! pair. On prod that table covered 218 of 1113 templates at an average of 1.44
//! of the 11 temper levels, and a miss made the server leave the item at its
//! damaged value — so "Repair all" repaired almost nothing and the next visit
//! still quoted a price. The table is now generated from the APK
//! (`script/extract_item_repair_data.py`), covering all 672 breakable templates
//! across all 11 temper levels, and a miss is no longer silent.
//!
//! Everything here is pure: no DB, no clock, no HTTP.

use std::collections::HashMap;

use uuid::Uuid;

use crate::economy::GOLD;
use crate::user_data::{CompleteInventory, CompleteWallet, InventoryChangeTracker, Item};

/// `ItemTemplate.DEFAULT_DURABILITY` (dump.cs:559431). Last-resort value for a
/// breakable item whose template is absent from the table — chosen so an unknown
/// item is still restored to *something* full rather than silently left broken,
/// which was the whole defect.
pub const DEFAULT_DURABILITY: f64 = 100.0;

/// Highest temper level the client models (`0` = untempered).
pub const MAX_TEMPER_LEVEL: u64 = 10;

/// The APK-derived repair tables (`deploy/static/item_durability.json` +
/// `repair_costs.json`), parsed once at startup.
#[derive(Debug, Clone, Default)]
pub struct RepairData {
    /// `itemTemplateId` -> temper level (0..=10) -> max durability.
    max_durability: HashMap<Uuid, [f64; (MAX_TEMPER_LEVEL as usize) + 1]>,
    /// `itemTemplateId` -> gold to repair from zero condition to full.
    repair_gold: HashMap<Uuid, u64>,
}

/// One deserialized row of `item_durability.json`: temper level (as a string
/// key, matching the JSON) -> max durability.
type DurabilityRow = HashMap<String, f64>;

impl RepairData {
    /// Build from the two parsed JSON documents. Both files carry a `_meta`
    /// provenance object which is skipped, and any row that does not parse as
    /// the expected shape is skipped rather than failing the whole load — a
    /// partially readable table still repairs most items, whereas a hard error
    /// at startup would take the server down.
    pub fn from_json(durability: &serde_json::Value, costs: &serde_json::Value) -> Self {
        let mut max_durability = HashMap::new();
        if let Some(map) = durability.as_object() {
            for (key, row) in map {
                let Ok(template) = Uuid::parse_str(key) else {
                    continue; // `_meta` and anything else non-UUID
                };
                let Ok(row) = serde_json::from_value::<DurabilityRow>(row.clone()) else {
                    continue;
                };
                let mut levels = [0.0f64; (MAX_TEMPER_LEVEL as usize) + 1];
                let mut ok = true;
                for i in 0..levels.len() {
                    match row.get(&i.to_string()) {
                        Some(v) if *v > 0.0 => levels[i] = *v,
                        // A gap in the ladder carries the previous level
                        // forward; level 0 missing means the row is unusable.
                        _ if i > 0 => levels[i] = levels[i - 1],
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    max_durability.insert(template, levels);
                }
            }
        }

        let mut repair_gold = HashMap::new();
        if let Some(map) = costs.as_object() {
            for (key, v) in map {
                let Ok(template) = Uuid::parse_str(key) else {
                    continue;
                };
                if let Some(g) = v.as_u64() {
                    repair_gold.insert(template, g);
                }
            }
        }

        RepairData {
            max_durability,
            repair_gold,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.max_durability.is_empty()
    }

    pub fn template_count(&self) -> usize {
        self.max_durability.len()
    }

    pub fn cost_template_count(&self) -> usize {
        self.repair_gold.len()
    }

    /// Max durability for an item, i.e. "max condition for its level and type".
    ///
    /// Returns `None` only for a template the table does not know at all; every
    /// known template carries all 11 temper levels, and a temper level above
    /// [`MAX_TEMPER_LEVEL`] clamps to the top of the ladder rather than missing.
    pub fn max_durability(&self, template: Uuid, temper_level: u64) -> Option<f64> {
        let levels = self.max_durability.get(&template)?;
        let idx = temper_level.min(MAX_TEMPER_LEVEL) as usize;
        Some(levels[idx])
    }

    /// Max durability with the last-resort default applied. Used by the repair
    /// path so an unknown template is restored to [`DEFAULT_DURABILITY`] instead
    /// of being left damaged. `None` when the item is not breakable at all.
    pub fn max_durability_or_default(&self, item: &Item) -> Option<f64> {
        match self.max_durability(item.item_template_id, item.tempering_level) {
            Some(max) => Some(max),
            // An item the table has never heard of. If it is carrying a
            // durability at all it is breakable, so give it the game's own
            // default; a genuinely unbreakable item (durability 0) is left be.
            None if item.durability > 0.0 => Some(DEFAULT_DURABILITY),
            None => None,
        }
    }

    /// Gold to fully repair one item at its current condition.
    ///
    /// `recipe_gold * (1 - durability / maxDurability)`, mirroring
    /// `RepairRecipe.AddInputs(_, _, itemCondition)`. Rounded half-up to an
    /// integer, and never rounded down to 0 for an item that IS damaged (the
    /// client will not offer a free repair for a worn item).
    pub fn repair_cost(&self, item: &Item) -> u64 {
        let Some(max) = self.max_durability_or_default(item) else {
            return 0;
        };
        if max <= 0.0 || item.durability >= max {
            return 0;
        }
        let Some(full) = self.repair_gold.get(&item.item_template_id) else {
            // No recipe (NPC-only / unequippable gear): retail had no repair
            // entry for it either, so it is free.
            return 0;
        };
        let deficit = ((max - item.durability) / max).clamp(0.0, 1.0);
        let cost = (*full as f64 * deficit).round() as u64;
        cost.max(1)
    }

    /// Is this item below max condition for its level and type?
    pub fn needs_repair(&self, item: &Item) -> bool {
        match self.max_durability_or_default(item) {
            Some(max) => max > 0.0 && item.durability < max,
            None => false,
        }
    }

    /// Restore one item to max condition. Returns `true` if it changed.
    pub fn restore(&self, item: &mut Item) -> bool {
        match self.max_durability_or_default(item) {
            Some(max) if item.durability < max => {
                item.durability = max;
                true
            }
            _ => false,
        }
    }
}

/// Every repairable item the character owns, equipped or in the backpack, as
/// `(item id, is_equipped)`. This is what a faithful "Repair all" covers.
pub fn repairable_items(
    data: &RepairData,
    inventory: &CompleteInventory,
) -> Vec<(Uuid, bool)> {
    let mut out: Vec<(Uuid, bool)> = Vec::new();
    for equipped in inventory.loadout.equipped_items.0.values() {
        if data.needs_repair(&equipped.item) {
            out.push((equipped.id, true));
        }
    }
    for (id, item) in inventory.backpack.items.0.iter() {
        if data.needs_repair(item) {
            out.push((*id, false));
        }
    }
    // Stable order so a repair is deterministic (and the affordability cut is
    // reproducible) regardless of HashMap iteration order.
    out.sort_by_key(|(id, _)| *id);
    out
}

/// Total gold to bring every repairable item to max condition. This is the
/// number the blacksmith screen shows for "Repair all" — and the number tracker
/// #30 requires to be **zero** on a second visit with no fighting in between.
pub fn repair_all_cost(data: &RepairData, inventory: &CompleteInventory) -> u64 {
    let mut total: u64 = 0;
    for equipped in inventory.loadout.equipped_items.0.values() {
        total = total.saturating_add(data.repair_cost(&equipped.item));
    }
    for item in inventory.backpack.items.0.values() {
        total = total.saturating_add(data.repair_cost(item));
    }
    total
}

/// Outcome of a repair request.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepairOutcome {
    /// Item ids actually restored to max condition.
    pub repaired: Vec<Uuid>,
    /// Item ids the request named that we could not find (stale client ids).
    pub unknown: Vec<Uuid>,
    /// Item ids skipped because the character could not afford them.
    pub unaffordable: Vec<Uuid>,
    /// Gold debited.
    pub gold_spent: u64,
}

/// Repair the requested items, charging gold, and mark them in the change
/// tracker so the response carries the inventory diff.
///
/// Items are processed cheapest-first and each is charged only if the character
/// can afford it, so the wallet and the inventory can never disagree: nothing is
/// ever charged without being repaired, and nothing is ever repaired for free.
/// An item already at max condition costs nothing and is not reported as
/// repaired.
pub fn apply_repairs(
    data: &RepairData,
    requested: &[Uuid],
    inventory: &mut CompleteInventory,
    wallet: &mut CompleteWallet,
    tracker: &mut InventoryChangeTracker,
) -> RepairOutcome {
    let mut outcome = RepairOutcome::default();

    // Resolve each requested id to where it lives, and price it. Unknown ids
    // (the client can send stale ones) are reported, not fatal.
    let mut priced: Vec<(Uuid, bool, u64)> = Vec::new();
    for id in requested {
        let equipped_slot = inventory
            .loadout
            .equipped_items
            .0
            .values()
            .find(|e| e.id == *id)
            .map(|e| data.repair_cost(&e.item));
        if let Some(cost) = equipped_slot {
            priced.push((*id, true, cost));
            continue;
        }
        match inventory.backpack.items.0.get(id) {
            Some(item) => priced.push((*id, false, data.repair_cost(item))),
            None => outcome.unknown.push(*id),
        }
    }

    // Cheapest first: with a partial budget the player gets the most items
    // fixed, and the order is deterministic.
    priced.sort_by_key(|(id, _, cost)| (*cost, *id));

    for (id, is_equipped, cost) in priced {
        if cost > 0 && wallet.debit(GOLD, cost).is_err() {
            outcome.unaffordable.push(id);
            continue;
        }
        let changed = if is_equipped {
            let Some(equipped) = inventory
                .loadout
                .equipped_items
                .0
                .values_mut()
                .find(|e| e.id == id)
            else {
                continue;
            };
            let slot = equipped.slot;
            let changed = data.restore(&mut equipped.item);
            if changed {
                tracker
                    .modified_loadout
                    .modified_equipped_items
                    .insert(slot);
            }
            changed
        } else {
            let Some(item) = inventory.backpack.items.0.get_mut(&id) else {
                continue;
            };
            let changed = data.restore(item);
            if changed {
                tracker.modified_backpack.items.insert(id);
            }
            changed
        };
        if changed {
            outcome.repaired.push(id);
            outcome.gold_spent = outcome.gold_spent.saturating_add(cost);
        } else if cost > 0 {
            // Priced but nothing to restore — refund rather than pocket it.
            wallet.credit(GOLD, cost);
        }
    }

    outcome.repaired.sort();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_data::{
        Backpack, ItemPropertiesAll, Loadout, SingleEquippedItem, Treasury,
    };
    use serde_json::json;

    /// A template with a full 11-level ladder plus a repair price.
    const TPL: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
    /// A breakable template the tables do NOT know — the shape that used to make
    /// repair silently skip an item.
    const TPL_UNKNOWN: Uuid = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);

    fn data() -> RepairData {
        // 325 at temper 0 climbing to 675 at temper 10 — the real Dragonscale
        // Helmet ladder (APK, cross-checked against retail captures).
        let ladder = json!({
            "0": 325.0, "1": 330.775, "2": 344.9375, "3": 360.4, "4": 385.0,
            "5": 416.875, "6": 450.0, "7": 484.375, "8": 520.0, "9": 595.0,
            "10": 675.0
        });
        let durability = json!({
            "_meta": { "_source": "test" },
            TPL.to_string(): ladder,
        });
        let costs = json!({
            "_meta": { "_source": "test" },
            TPL.to_string(): 1000u64,
        });
        RepairData::from_json(&durability, &costs)
    }

    fn item(template: Uuid, temper: u64, durability: f64) -> Item {
        Item {
            item_template_id: template,
            tempering_level: temper,
            durability,
            grade: None,
            arcane_tier: None,
            properties: ItemPropertiesAll::default(),
        }
    }

    fn inventory() -> CompleteInventory {
        CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        }
    }

    fn equip(inv: &mut CompleteInventory, id: Uuid, slot: Uuid, it: Item) {
        inv.loadout.equipped_items.0.insert(
            slot,
            SingleEquippedItem { id, slot, item: it },
        );
    }

    fn wallet(gold: u64) -> CompleteWallet {
        let mut w = CompleteWallet::default();
        w.credit(GOLD, gold);
        w
    }

    #[test]
    fn meta_key_does_not_break_the_table() {
        let d = data();
        assert_eq!(d.template_count(), 1, "_meta must not become a template row");
        assert_eq!(d.cost_template_count(), 1);
    }

    #[test]
    fn max_durability_reads_the_whole_temper_ladder() {
        let d = data();
        assert_eq!(d.max_durability(TPL, 0), Some(325.0));
        assert_eq!(d.max_durability(TPL, 6), Some(450.0));
        assert_eq!(d.max_durability(TPL, 10), Some(675.0));
        // Above the modelled ceiling clamps to the top instead of missing.
        assert_eq!(d.max_durability(TPL, 99), Some(675.0));
    }

    /// The bug: an item whose `(template, temper)` the table lacks used to be
    /// left at its damaged value, so "Repair all" silently skipped it.
    #[test]
    fn unknown_template_is_still_restored_not_skipped() {
        let d = data();
        let mut it = item(TPL_UNKNOWN, 4, 12.0);
        assert!(d.needs_repair(&it));
        assert!(d.restore(&mut it));
        assert_eq!(it.durability, DEFAULT_DURABILITY);
        assert!(!d.needs_repair(&it));
    }

    #[test]
    fn unbreakable_item_is_left_alone() {
        let d = data();
        let mut it = item(TPL_UNKNOWN, 0, 0.0);
        assert!(!d.needs_repair(&it));
        assert!(!d.restore(&mut it));
        assert_eq!(d.repair_cost(&it), 0);
    }

    #[test]
    fn cost_scales_with_the_condition_deficit() {
        let d = data();
        // temper 6 -> max 450. Half worn costs half of the 1000-gold recipe.
        assert_eq!(d.repair_cost(&item(TPL, 6, 225.0)), 500);
        // Fully broken costs the whole recipe.
        assert_eq!(d.repair_cost(&item(TPL, 6, 0.0)), 1000);
        // At max it is free.
        assert_eq!(d.repair_cost(&item(TPL, 6, 450.0)), 0);
        // A hair below max still costs at least 1 — never a free repair.
        assert_eq!(d.repair_cost(&item(TPL, 6, 449.99)), 1);
    }

    /// **The tracker #30 acceptance test.** Repair all, then visit again with no
    /// fighting in between: the quoted cost must be ZERO and every slot must be
    /// at max condition for its level and type.
    #[test]
    fn repair_all_then_second_visit_costs_zero_and_everything_is_at_max() {
        let d = data();
        let mut inv = inventory();
        let mut w = wallet(1_000_000);

        // A mixed loadout: several temper levels, an unknown template, an
        // already-full item, and backpack gear as well as equipped gear.
        let e1 = Uuid::from_u128(0xa1);
        let e2 = Uuid::from_u128(0xa2);
        let e3 = Uuid::from_u128(0xa3);
        equip(&mut inv, e1, Uuid::from_u128(0xf1), item(TPL, 0, 10.0));
        equip(&mut inv, e2, Uuid::from_u128(0xf2), item(TPL, 10, 100.0));
        equip(&mut inv, e3, Uuid::from_u128(0xf3), item(TPL_UNKNOWN, 3, 5.0));
        let b1 = Uuid::from_u128(0xb1);
        let b2 = Uuid::from_u128(0xb2);
        let b3 = Uuid::from_u128(0xb3);
        inv.backpack.items.0.insert(b1, item(TPL, 7, 1.0));
        inv.backpack.items.0.insert(b2, item(TPL, 4, 384.0));
        inv.backpack.items.0.insert(b3, item(TPL, 6, 450.0)); // already full

        // First visit: there is a bill, and it covers every worn item.
        let before = repair_all_cost(&d, &inv);
        assert!(before > 0, "a worn loadout must quote a nonzero repair bill");
        let to_repair: Vec<Uuid> = repairable_items(&d, &inv)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            to_repair.len(),
            5,
            "every worn item is repairable, including the unknown template"
        );

        let mut tracker = InventoryChangeTracker::default();
        let outcome = apply_repairs(&d, &to_repair, &mut inv, &mut w, &mut tracker);
        assert!(outcome.unaffordable.is_empty(), "rich character pays for all");
        assert!(outcome.unknown.is_empty());
        assert_eq!(outcome.repaired.len(), 5);
        assert_eq!(outcome.gold_spent, before);

        // SECOND VISIT — the assertion the report is about.
        assert_eq!(
            repair_all_cost(&d, &inv),
            0,
            "second visit with no fighting in between must quote ZERO gold"
        );
        assert!(
            repairable_items(&d, &inv).is_empty(),
            "no item may still be below max condition"
        );

        // ... and every slot really is at its own max for its level and type.
        for e in inv.loadout.equipped_items.0.values() {
            let max = d.max_durability_or_default(&e.item).unwrap();
            assert_eq!(
                e.item.durability, max,
                "equipped slot {} not at max condition",
                e.slot
            );
        }
        for (id, it) in inv.backpack.items.0.iter() {
            let max = d.max_durability_or_default(it).unwrap();
            assert_eq!(it.durability, max, "backpack item {id} not at max condition");
        }
        // Temper-specific maxima, not one global number.
        assert_eq!(inv.loadout.equipped_items.0[&Uuid::from_u128(0xf1)].item.durability, 325.0);
        assert_eq!(inv.loadout.equipped_items.0[&Uuid::from_u128(0xf2)].item.durability, 675.0);
        assert_eq!(inv.backpack.items.0[&b1].durability, 484.375);
    }

    /// A repair-all over an inventory the tables know NOTHING about still has to
    /// end at zero cost — this is the regression that the capture-derived table
    /// could not pass, because a missing row meant a silent skip.
    #[test]
    fn repair_all_is_complete_even_when_no_template_is_known() {
        let d = RepairData::default();
        assert!(d.is_empty(), "empty tables — the worst case");
        let mut inv = inventory();
        let mut w = wallet(1_000);
        equip(&mut inv, Uuid::from_u128(0xa1), Uuid::from_u128(0xf1), item(TPL, 3, 7.0));
        inv.backpack
            .items
            .0
            .insert(Uuid::from_u128(0xb1), item(TPL_UNKNOWN, 9, 2.0));

        let ids: Vec<Uuid> = repairable_items(&d, &inv).into_iter().map(|(i, _)| i).collect();
        assert_eq!(ids.len(), 2);
        let mut tracker = InventoryChangeTracker::default();
        apply_repairs(&d, &ids, &mut inv, &mut w, &mut tracker);

        assert_eq!(repair_all_cost(&d, &inv), 0);
        assert!(repairable_items(&d, &inv).is_empty());
    }

    #[test]
    fn tracker_carries_both_equipped_and_backpack_repairs_into_the_diff() {
        let d = data();
        let mut inv = inventory();
        let mut w = wallet(1_000_000);
        let slot = Uuid::from_u128(0xf1);
        let eid = Uuid::from_u128(0xa1);
        let bid = Uuid::from_u128(0xb1);
        equip(&mut inv, eid, slot, item(TPL, 10, 1.0));
        inv.backpack.items.0.insert(bid, item(TPL, 0, 1.0));

        let mut tracker = InventoryChangeTracker::default();
        apply_repairs(&d, &[eid, bid], &mut inv, &mut w, &mut tracker);
        inv.backpack_version += 1;

        let update = inv.generate_client_update(&tracker);
        assert!(update.loadout.equipped_items.0.contains_key(&slot));
        assert!(update.backpack.items.0.contains_key(&bid));
        assert_eq!(update.backpack_version, 2);
    }

    #[test]
    fn a_poor_character_repairs_what_it_can_afford_and_is_charged_only_for_that() {
        let d = data();
        let mut inv = inventory();
        // Two broken items: temper 0 (max 325, 1000 gold) and temper 10
        // (max 675, 1000 gold). Both cost the full recipe when broken.
        let a = Uuid::from_u128(0xb1);
        let b = Uuid::from_u128(0xb2);
        inv.backpack.items.0.insert(a, item(TPL, 0, 0.0));
        inv.backpack.items.0.insert(b, item(TPL, 10, 0.0));
        let mut w = wallet(1_200);

        let mut tracker = InventoryChangeTracker::default();
        let outcome = apply_repairs(&d, &[a, b], &mut inv, &mut w, &mut tracker);

        assert_eq!(outcome.repaired.len(), 1, "only one is affordable");
        assert_eq!(outcome.unaffordable.len(), 1);
        assert_eq!(outcome.gold_spent, 1_000);
        assert_eq!(w.balance(GOLD), 200, "charged only for what was repaired");
    }

    #[test]
    fn an_already_full_item_is_neither_charged_nor_reported() {
        let d = data();
        let mut inv = inventory();
        let id = Uuid::from_u128(0xb1);
        inv.backpack.items.0.insert(id, item(TPL, 6, 450.0));
        let mut w = wallet(5_000);

        let mut tracker = InventoryChangeTracker::default();
        let outcome = apply_repairs(&d, &[id], &mut inv, &mut w, &mut tracker);
        assert!(outcome.repaired.is_empty());
        assert_eq!(outcome.gold_spent, 0);
        assert_eq!(w.balance(GOLD), 5_000);
    }

    #[test]
    fn stale_client_item_ids_are_reported_not_fatal() {
        let d = data();
        let mut inv = inventory();
        let mut w = wallet(5_000);
        let ghost = Uuid::from_u128(0xdead);
        let mut tracker = InventoryChangeTracker::default();
        let outcome = apply_repairs(&d, &[ghost], &mut inv, &mut w, &mut tracker);
        assert_eq!(outcome.unknown, vec![ghost]);
        assert!(outcome.repaired.is_empty());
    }

    /// The committed APK-derived tables must load, cover every breakable
    /// template across all 11 temper levels, and price the real recipes.
    #[test]
    fn committed_apk_tables_load_and_are_complete() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/");
        let dur: serde_json::Value = serde_json::from_reader(std::io::BufReader::new(
            std::fs::File::open(format!("{dir}item_durability.json"))
                .expect("item_durability.json is committed"),
        ))
        .expect("item_durability.json parses");
        let costs: serde_json::Value = serde_json::from_reader(std::io::BufReader::new(
            std::fs::File::open(format!("{dir}repair_costs.json"))
                .expect("repair_costs.json is committed"),
        ))
        .expect("repair_costs.json parses");

        let d = RepairData::from_json(&dur, &costs);
        // 672 breakable templates in the APK; the capture-derived table had 218.
        assert!(
            d.template_count() >= 600,
            "durability table covers {} templates, expected the full APK set",
            d.template_count()
        );
        assert!(
            d.cost_template_count() >= 600,
            "repair-cost table covers {} templates",
            d.cost_template_count()
        );

        // Every row must have a usable value at every temper level, or the
        // silent-skip defect comes back for that item.
        for (tpl, levels) in d.max_durability.iter() {
            for (lvl, v) in levels.iter().enumerate() {
                assert!(*v > 0.0, "template {tpl} temper {lvl} has no max durability");
            }
        }

        // Spot-check values cross-validated against retail captures: the
        // temper-10 ceiling for tier-10 gear is 675, and no table row may
        // exceed it (a runaway ladder would overshoot the client's own max and
        // draw a condition bar past full).
        assert!(
            d.max_durability.values().any(|l| l[10] == 675.0),
            "the retail-observed temper-10 675 durability ceiling is present"
        );
        assert!(
            d.max_durability.values().all(|l| l.iter().all(|v| *v <= 675.0)),
            "no template exceeds the retail-observed 675 durability ceiling"
        );
    }
}
