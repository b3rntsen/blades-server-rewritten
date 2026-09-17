//! Recipe id → the item that recipe produces.
//!
//! WHY THIS EXISTS. The client sends a `recipeId` for every craft. The server
//! could resolve almost none of them: `recipes.json` captured 34 and
//! `smith_craftables` a handful, so nearly every real craft fell through to a
//! fallback that granted an item whose id was the RECIPE's id. A recipe id is
//! not an item template; the client cannot resolve it, and the save stops
//! loading. Five characters carried one, two could not start the game, and one
//! of them hit it twice.
//!
//! That fallback now refuses the craft rather than corrupting a save. This table
//! is what lets it SUCCEED instead — 898 recipes with a real output, straight
//! out of the APK's own `recipe_crafting_types` metadata. Not mined from
//! traffic, not inferred from names: the APK says which template each recipe
//! makes.
//!
//! The name route was tried first and rejected with numbers: the ids the client
//! sends are `Items.Name.*` localisation seeds, and only 239 of 2,590 such seeds
//! resolve to exactly one item. Three of the four ids seen on production resolve
//! to none at all. This table resolves all four exactly.
//!
//! WHAT IS NOT HERE. Repairing, Tempering, Salvaging and Enchanting modify an
//! existing item and declare no output; they live in `item_mod_recipes.json` and
//! their absence is deliberate, not a gap.

use serde::Deserialize;
use uuid::Uuid;

static RECIPE_OUTPUTS_RAW: &str = include_str!("../recipe_outputs.json");

#[derive(Deserialize)]
struct Corpus {
    recipes: std::collections::HashMap<Uuid, RecipeOutput>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RecipeOutput {
    pub output_item_template_id: Uuid,
    pub crafting_type_id: Option<Uuid>,
    pub crafting_type: Option<String>,
    pub name: Option<String>,
}

fn corpus() -> &'static Corpus {
    static TABLE: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(RECIPE_OUTPUTS_RAW).unwrap_or_else(|_| Corpus {
            recipes: std::collections::HashMap::new(),
        })
    })
}

/// What this recipe produces, if the APK declares an output for it.
pub fn output_for(recipe_id: &Uuid) -> Option<&'static RecipeOutput> {
    corpus().recipes.get(recipe_id)
}

/// How many recipes carry an output, for tests and diagnostics.
pub fn len() -> usize {
    corpus().recipes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table must be compiled in and hold what was extracted. Parsed
    /// explicitly so a deserialization failure names itself instead of degrading
    /// to an empty map — which would silently restore "every craft is refused".
    #[test]
    fn the_table_loads() {
        if let Err(e) = serde_json::from_str::<Corpus>(RECIPE_OUTPUTS_RAW) {
            panic!("recipe_outputs.json failed to parse: {e}");
        }
        assert_eq!(len(), 898, "898 recipes declare an output");
    }

    /// The four recipe ids that actually broke players' saves must now resolve.
    /// These are not hypothetical: each was found in a live character's backpack.
    #[test]
    fn the_recipes_that_broke_saves_now_resolve() {
        for (id, want_name) in [
            ("9f089463-ccc5-4a9e-88c9-9e2f380ff829", "Steel Longsword"),
            ("83e71505-e6ef-467e-97f6-77853a7d5c07", "Warrior's Fire"),
            ("668a077b-2a2e-477b-894d-cb0878fa7dd3", "Dragonbone Longsword"),
        ] {
            let id: Uuid = id.parse().unwrap();
            let got = output_for(&id).unwrap_or_else(|| panic!("{want_name} does not resolve"));
            assert_eq!(got.name.as_deref(), Some(want_name));
            assert!(!got.output_item_template_id.is_nil());
            assert_ne!(
                got.output_item_template_id, id,
                "the output must not be the recipe id — that is the original bug"
            );
        }
    }

    /// No entry may map a recipe to ITSELF. That is exactly the shape that broke
    /// the saves, and it must be impossible to reintroduce through the data.
    #[test]
    fn no_recipe_outputs_its_own_id() {
        let mut offenders = Vec::new();
        for (rid, out) in &corpus().recipes {
            if &out.output_item_template_id == rid {
                offenders.push(*rid);
            }
        }
        assert!(offenders.is_empty(), "recipes mapping to their own id: {offenders:?}");
    }

    /// Only the crafting types that PRODUCE an item are here. Repair, temper,
    /// salvage and enchant modify an existing one and belong elsewhere; if they
    /// appeared here they would be granted as new items.
    #[test]
    fn only_producing_crafting_types_are_present() {
        let mut kinds = std::collections::BTreeMap::new();
        for out in corpus().recipes.values() {
            *kinds.entry(out.crafting_type.clone().unwrap_or_default()).or_insert(0) += 1;
        }
        assert_eq!(kinds.get("Smithing"), Some(&617));
        assert_eq!(kinds.get("DecorationCrafting"), Some(&141));
        assert_eq!(kinds.get("Alchemy"), Some(&140));
        for banned in ["Repairing", "Tempering", "Salvaging", "Enchanting"] {
            assert!(kinds.get(banned).is_none(), "{banned} must not declare an output here");
        }
    }
}
