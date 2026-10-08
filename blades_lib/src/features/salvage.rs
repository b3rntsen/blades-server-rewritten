//! Salvage — `POST /salvages`.
//!
//! Break gear down at the smithy into crafting materials. The handler removes the
//! salvaged item(s); this layer works out what they yield.
//!
//! ## Where the yield comes from
//! The APK's own `SalvagingRecipe` table (`salvage_outputs.json`, compiled in, produced
//! by the capture repo's `reference/game-defs/extract/x_recipe_io.py`). Each of its 681
//! recipes carries 11 rows, `levels[temperingLevel]`, and each row lists components that
//! roll uniformly in `[min, max]` (every shipped `odds` is 1.0). That indexing is not a
//! guess: all 1,743 retail single-item salvages in the 2026-06-07 capture snapshot whose
//! item state was also captured land inside `levels[item.temperingLevel]`.
//!
//! Before this table the server had only the 122 recipes it happened to see in traffic
//! (`deploy/static/salvage_recipes.json`), one fixed bundle each regardless of the item's
//! tempering. The other 559 — every Dragonbone and Stalhrim item among them — salvaged to
//! nothing (tracker report #366). That captured map is kept only as a last resort for a
//! recipe id the APK does not ship.
//!
//! Captured: request `{salvageInfos:[{recipeId,itemId}], buildingId}` ->
//! `{reward:{stackableItems:{…}}, inventory:{backpack:{removedItems, stackableItems}}}`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use uuid::Uuid;

use crate::economy::RewardGrant;

static SALVAGE_OUTPUTS_RAW: &str = include_str!("../salvage_outputs.json");

/// One component a salvage level can yield.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SalvageOutput {
    pub item_template_id: Uuid,
    pub min: u64,
    pub max: u64,
    #[serde(default = "one")]
    pub odds: f64,
}

fn one() -> f64 {
    1.0
}

/// One APK `SalvagingRecipe`.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SalvageRecipe {
    #[serde(default)]
    pub name: Option<String>,
    pub input_item_template_id: Option<Uuid>,
    /// `levels[temperingLevel]`; the client ships 11 (tempering 0..=10).
    pub levels: Vec<Vec<SalvageOutput>>,
}

#[derive(Deserialize)]
struct Corpus {
    recipes: HashMap<Uuid, SalvageRecipe>,
}

struct Table {
    recipes: HashMap<Uuid, SalvageRecipe>,
    by_template: HashMap<Uuid, Uuid>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let recipes = serde_json::from_str::<Corpus>(SALVAGE_OUTPUTS_RAW)
            .map(|c| c.recipes)
            .unwrap_or_default();
        let by_template = recipes
            .iter()
            .filter_map(|(rid, r)| r.input_item_template_id.map(|t| (t, *rid)))
            .collect();
        Table {
            recipes,
            by_template,
        }
    })
}

/// The APK salvage recipe with this id.
pub fn recipe(recipe_id: &Uuid) -> Option<&'static SalvageRecipe> {
    table().recipes.get(recipe_id)
}

/// How many APK salvage recipes are compiled in, for tests and diagnostics.
pub fn len() -> usize {
    table().recipes.len()
}

/// The recipe that salvages `item_template_id`. The client picks it the same way
/// (`RecipeManager.GetSalvageRecipeForItem`), so this is what a well-formed request
/// names anyway.
fn recipe_for_template(item_template_id: &Uuid) -> Option<&'static SalvageRecipe> {
    table()
        .by_template
        .get(item_template_id)
        .and_then(|rid| table().recipes.get(rid))
}

/// The recipe to salvage this item with: the one the client named, unless it is for a
/// different item (then the item's own recipe wins — the yield follows the item, not the
/// request).
fn resolve(recipe_id: &Uuid, item_template_id: &Uuid) -> Option<&'static SalvageRecipe> {
    match recipe(recipe_id) {
        Some(r) if r.input_item_template_id.is_none_or(|t| t == *item_template_id) => Some(r),
        _ => recipe_for_template(item_template_id).or_else(|| recipe(recipe_id)),
    }
}

/// One item being salvaged.
#[derive(Clone, Copy, Debug)]
pub struct SalvagedItem {
    pub recipe_id: Uuid,
    pub item_template_id: Uuid,
    pub tempering_level: u64,
}

/// Roll the components for one APK recipe at `tempering_level` (clamped to the last
/// row, as retail's tables stop at 10). `pick(lo, hi)` must return a uniform value in
/// `lo..=hi`; it is injected so this crate needs no RNG and tests can pin the roll.
pub fn roll_recipe(
    recipe: &SalvageRecipe,
    tempering_level: u64,
    pick: &mut impl FnMut(u64, u64) -> u64,
) -> HashMap<Uuid, u64> {
    let mut out = HashMap::new();
    let Some(last) = recipe.levels.len().checked_sub(1) else {
        return out;
    };
    let row = &recipe.levels[(tempering_level.min(last as u64)) as usize];
    for o in row {
        if o.odds < 1.0 && (pick(0, 999_999) as f64) >= o.odds * 1_000_000.0 {
            continue;
        }
        let (lo, hi) = (o.min.min(o.max), o.min.max(o.max));
        let n = if lo == hi { lo } else { pick(lo, hi) };
        if n > 0 {
            *out.entry(o.item_template_id).or_insert(0) += n;
        }
    }
    out
}

/// The whole reward for a salvage request. APK recipe first; the captured
/// representative bundle (`captured`, `salvage_recipes.json`) only for a recipe id the
/// APK does not ship.
pub fn salvage_reward(
    items: &[SalvagedItem],
    captured: &HashMap<Uuid, HashMap<Uuid, u64>>,
    pick: &mut impl FnMut(u64, u64) -> u64,
) -> RewardGrant {
    let mut reward = RewardGrant::default();
    for item in items {
        let mats = match resolve(&item.recipe_id, &item.item_template_id) {
            Some(r) => roll_recipe(r, item.tempering_level, pick),
            None => captured.get(&item.recipe_id).cloned().unwrap_or_default(),
        };
        for (template, count) in mats {
            *reward.stackable_items.entry(template).or_insert(0) += count;
        }
    }
    reward
}

/// Sum the captured representative yield for a set of salvage recipes (the pre-APK
/// behaviour; [`salvage_reward`] uses it only as a fallback).
pub fn salvage_materials(
    recipe_ids: &[Uuid],
    recipes: &HashMap<Uuid, HashMap<Uuid, u64>>,
) -> RewardGrant {
    let mut reward = RewardGrant::default();
    for rid in recipe_ids {
        if let Some(mats) = recipes.get(rid) {
            for (template, count) in mats {
                *reward.stackable_items.entry(*template).or_insert(0) += *count;
            }
        }
    }
    reward
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Uuid {
        s.parse().unwrap()
    }

    const DRAGON_BONES: &str = "d523932f-8c7f-4192-9112-5dbd60883c2b";
    const EBONY_INGOT: &str = "75112030-b248-49b0-9c70-0da8dea150d1";
    const MALACHITE_INGOT: &str = "85ed5500-3581-4699-8095-4b5ff6514355";
    const STALHRIM: &str = "ba9fe442-2cf8-48a8-9416-6d91c2a00cab";

    const DRAGONBONE_DAGGER_SALVAGE: &str = "a4bda7d6-4d98-4dd9-a876-3b521b7e5196";
    const STALHRIM_ARMOR_SALVAGE: &str = "b83d4efe-c147-427f-8055-b15cfd1d7303";
    const STALHRIM_ARMOR: &str = "c74b89cb-19cc-4529-9c7e-ec6b53bcac17";

    fn lowest(lo: u64, _hi: u64) -> u64 {
        lo
    }
    fn highest(_lo: u64, hi: u64) -> u64 {
        hi
    }

    #[test]
    fn the_table_loads() {
        if let Err(e) = serde_json::from_str::<Corpus>(SALVAGE_OUTPUTS_RAW) {
            panic!("salvage_outputs.json failed to parse: {e}");
        }
        assert_eq!(len(), 681, "the client ships 681 Salvaging recipes");
        for (rid, r) in &table().recipes {
            assert_eq!(r.levels.len(), 11, "{rid}: one row per tempering level 0..=10");
            assert!(r.input_item_template_id.is_some(), "{rid}: no input item");
        }
    }

    /// Report #366: salvaging Dragonbone gear gave nothing. Retail gives Dragon Bones +
    /// an Ebony Ingot, and Malachite as well once the item is tempered.
    #[test]
    fn dragonbone_salvage_yields_dragon_bones() {
        let r = recipe(&u(DRAGONBONE_DAGGER_SALVAGE)).expect("Dragonbone Dagger salvage");
        assert_eq!(r.name.as_deref(), Some("Dragonbone Dagger"));
        let item = SalvagedItem {
            recipe_id: u(DRAGONBONE_DAGGER_SALVAGE),
            item_template_id: r.input_item_template_id.unwrap(),
            tempering_level: 0,
        };
        let got = salvage_reward(&[item], &HashMap::new(), &mut lowest);
        assert_eq!(got.stackable_items.get(&u(DRAGON_BONES)), Some(&2));
        assert_eq!(got.stackable_items.get(&u(EBONY_INGOT)), Some(&1));
        assert_eq!(got.stackable_items.len(), 2);

        let t3 = salvage_reward(
            &[SalvagedItem {
                tempering_level: 3,
                ..item
            }],
            &HashMap::new(),
            &mut highest,
        );
        assert_eq!(t3.stackable_items.get(&u(DRAGON_BONES)), Some(&8));
        assert_eq!(t3.stackable_items.get(&u(EBONY_INGOT)), Some(&1));
        assert_eq!(t3.stackable_items.get(&u(MALACHITE_INGOT)), Some(&3));
    }

    /// Report #366: Stalhrim gear had no salvage recipe either.
    #[test]
    fn stalhrim_salvage_yields_stalhrim() {
        let item = SalvagedItem {
            recipe_id: u(STALHRIM_ARMOR_SALVAGE),
            item_template_id: u(STALHRIM_ARMOR),
            tempering_level: 0,
        };
        let lo = salvage_reward(&[item], &HashMap::new(), &mut lowest);
        let hi = salvage_reward(&[item], &HashMap::new(), &mut highest);
        assert_eq!(lo.stackable_items.get(&u(STALHRIM)), Some(&2));
        assert_eq!(hi.stackable_items.get(&u(STALHRIM)), Some(&3));
        assert_eq!(lo.stackable_items.get(&u(MALACHITE_INGOT)), Some(&1));
        // A tempering level past the table uses its last row.
        let past = salvage_reward(
            &[SalvagedItem {
                tempering_level: 99,
                ..item
            }],
            &HashMap::new(),
            &mut lowest,
        );
        assert!(past.stackable_items[&u(STALHRIM)] > lo.stackable_items[&u(STALHRIM)]);
    }

    /// The yield follows the ITEM: a request naming another item's recipe salvages with
    /// the item's own one.
    #[test]
    fn a_mismatched_recipe_id_uses_the_items_own_recipe() {
        let item = SalvagedItem {
            recipe_id: u(DRAGONBONE_DAGGER_SALVAGE),
            item_template_id: u(STALHRIM_ARMOR),
            tempering_level: 0,
        };
        let got = salvage_reward(&[item], &HashMap::new(), &mut lowest);
        assert!(got.stackable_items.contains_key(&u(STALHRIM)));
        assert!(!got.stackable_items.contains_key(&u(DRAGON_BONES)));
    }

    /// A recipe the APK does not know still gets the captured representative bundle.
    #[test]
    fn unknown_recipe_falls_back_to_the_captured_bundle() {
        let rid = Uuid::from_u128(77);
        let m = Uuid::from_u128(1);
        let captured = HashMap::from([(rid, HashMap::from([(m, 4)]))]);
        let item = SalvagedItem {
            recipe_id: rid,
            item_template_id: Uuid::from_u128(78),
            tempering_level: 0,
        };
        let got = salvage_reward(&[item], &captured, &mut lowest);
        assert_eq!(got.stackable_items[&m], 4);
    }

    #[test]
    fn sums_materials_across_recipes() {
        let m = Uuid::from_u128(1);
        let r1 = Uuid::from_u128(10);
        let r2 = Uuid::from_u128(11);
        let recipes = HashMap::from([
            (r1, HashMap::from([(m, 2)])),
            (r2, HashMap::from([(m, 3)])),
        ]);
        let reward = salvage_materials(&[r1, r2], &recipes);
        assert_eq!(reward.stackable_items[&m], 5);
    }

    #[test]
    fn unknown_recipe_yields_nothing() {
        let reward = salvage_materials(&[Uuid::from_u128(99)], &HashMap::new());
        assert!(reward.stackable_items.is_empty());
    }
}
