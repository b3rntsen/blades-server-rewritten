//! Smithing recipe id → what a forge craft costs, and how the client gates it.
//!
//! From the APK's `Recipe._inputs` (`smithing_inputs.json`, compiled in, produced by
//! the capture repo's `reference/game-defs/extract/x_recipe_io.py`). Gold
//! (`f8d27767`) is one of the inputs and is a wallet currency; everything else is a
//! backpack stackable. Plain forge crafts used to be free on this server (the inputs
//! had never been extracted); retail charged them when the craft started.
//!
//! The file also carries the two client-side gates, for diagnosis (the server does not
//! enforce them — the client never offers a recipe that fails them):
//!
//! * `tier` — `RecipeManager.CanCraftRecipe` refuses a recipe whose output's
//!   `ItemTemplate._tier` exceeds the Forge's building level.
//! * `requiredIncompleteObjectives` — `Recipe.IsUnlocked` is false once any of these
//!   objectives is COMPLETE. 407 of the 617 Smithing recipes carry `MQ03Objective1`, an early
//!   main-quest step, so they are locked for every veteran player. That includes all
//!   nine Stalhrim weapons: retail only ever let a level-33 player forge the five
//!   Stalhrim armour pieces (tier 8, Forge level 8).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use uuid::Uuid;

static SMITHING_INPUTS_RAW: &str = include_str!("../smithing_inputs.json");

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SmithingRecipe {
    #[serde(default)]
    pub name: Option<String>,
    pub output_item_template_id: Option<Uuid>,
    #[serde(default)]
    pub tier: Option<u32>,
    #[serde(default)]
    pub required_incomplete_objectives: Vec<Uuid>,
    /// template → quantity, per crafted item.
    pub inputs: HashMap<Uuid, u64>,
}

#[derive(Deserialize)]
struct Corpus {
    recipes: HashMap<Uuid, SmithingRecipe>,
}

fn table() -> &'static HashMap<Uuid, SmithingRecipe> {
    static TABLE: OnceLock<HashMap<Uuid, SmithingRecipe>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<Corpus>(SMITHING_INPUTS_RAW)
            .map(|c| c.recipes)
            .unwrap_or_default()
    })
}

/// The APK Smithing recipe with this id.
pub fn recipe(recipe_id: &Uuid) -> Option<&'static SmithingRecipe> {
    table().get(recipe_id)
}

/// What starting `batch_size` crafts of this recipe costs, or `None` when it is not a
/// Smithing recipe the APK ships (alchemy/decoration crafts stay uncharged for now).
pub fn inputs_for(recipe_id: &Uuid, batch_size: u32) -> Option<Vec<(Uuid, u64)>> {
    let r = recipe(recipe_id)?;
    let n = u64::from(batch_size.max(1));
    let mut v: Vec<(Uuid, u64)> = r.inputs.iter().map(|(t, q)| (*t, q * n)).collect();
    v.sort();
    Some(v)
}

pub fn len() -> usize {
    table().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Uuid {
        s.parse().unwrap()
    }

    const STALHRIM_BOOTS_RECIPE: &str = "f1721857-3b94-4d2b-9a5d-9e6eaf8a2184";
    const STALHRIM: &str = "ba9fe442-2cf8-48a8-9416-6d91c2a00cab";
    const MALACHITE: &str = "85ed5500-3581-4699-8095-4b5ff6514355";
    const QUICKSILVER: &str = "e80bee76-f92c-4005-9eff-20d1e8c64d24";
    const GOLD: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

    #[test]
    fn the_table_loads() {
        if let Err(e) = serde_json::from_str::<Corpus>(SMITHING_INPUTS_RAW) {
            panic!("smithing_inputs.json failed to parse: {e}");
        }
        assert_eq!(len(), 617, "the client ships 617 Smithing recipes");
        for (rid, r) in table() {
            assert!(!r.inputs.is_empty(), "{rid} has no inputs");
            // Every output must also be what recipe_outputs mints for this recipe.
            let out = crate::features::recipe_outputs::output_for(rid)
                .unwrap_or_else(|| panic!("{rid} missing from recipe_outputs"));
            assert_eq!(r.output_item_template_id, Some(out.output_item_template_id), "{rid}");
        }
    }

    #[test]
    fn stalhrim_boots_cost_what_retail_charged() {
        let got = inputs_for(&u(STALHRIM_BOOTS_RECIPE), 1).unwrap();
        let want: HashMap<Uuid, u64> = HashMap::from([
            (u(STALHRIM), 8),
            (u(MALACHITE), 5),
            (u(QUICKSILVER), 3),
            (u(GOLD), 3367),
        ]);
        assert_eq!(got.into_iter().collect::<HashMap<_, _>>(), want);
        let r = recipe(&u(STALHRIM_BOOTS_RECIPE)).unwrap();
        assert_eq!(r.tier, Some(8));
        assert!(r.required_incomplete_objectives.is_empty());
    }

    /// The finding behind half of report #366: Stalhrim WEAPONS are locked by the
    /// client for anyone past MQ03, Stalhrim ARMOUR is not.
    #[test]
    fn only_stalhrim_armour_is_unlocked_for_veterans() {
        let mut open = Vec::new();
        let mut locked = 0;
        for r in table().values() {
            let name = r.name.clone().unwrap_or_default();
            if !name.starts_with("Stalhrim") || r.tier != Some(8) {
                continue;
            }
            if r.required_incomplete_objectives.is_empty() {
                open.push(name);
            } else {
                locked += 1;
            }
        }
        open.sort();
        assert_eq!(
            open,
            [
                "Stalhrim Armor",
                "Stalhrim Boots",
                "Stalhrim Gauntlets",
                "Stalhrim Helmet",
                "Stalhrim Shield"
            ]
        );
        assert_eq!(locked, 9);
    }

    #[test]
    fn batch_multiplies_the_cost() {
        let one = inputs_for(&u(STALHRIM_BOOTS_RECIPE), 1).unwrap();
        let three = inputs_for(&u(STALHRIM_BOOTS_RECIPE), 3).unwrap();
        for ((t1, q1), (t3, q3)) in one.iter().zip(three.iter()) {
            assert_eq!(t1, t3);
            assert_eq!(q1 * 3, *q3);
        }
        assert!(inputs_for(&Uuid::nil(), 1).is_none());
    }
}
