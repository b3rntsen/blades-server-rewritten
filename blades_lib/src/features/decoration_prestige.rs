//! Town prestige for a crafted decoration (tracker #353).
//!
//! Retail paid town prestige (`town.levelInfo.experiencePoints`) for a
//! decoration when the player ACQUIRED it — buying it or crafting it — and never
//! for placing it: the client's `TownPointsData.PointsRewardedForPropType` is
//! declared but no field of the shipped `TownPointsData` holds it, and seven
//! retail prop placements moved the town's XP by 0.
//!
//! A bought decoration's prestige rides on its offer's `townXp`. A CRAFTED one
//! has no reward of its own to carry it: the APK's `Recipe` has no prestige
//! field. The number is on the Workshop's `ItemBundleGenerationData` for the
//! same decoration, which is what this table maps each recipe to. Retail capture
//! 17253 is the proof: collecting an Ancient Tree craft paid +91, and the
//! Workshop bundle for the Ancient Tree carries `town_xp` 91.
use serde::Deserialize;
use uuid::Uuid;

static RAW: &str = include_str!("../decoration_craft_prestige.json");

#[derive(Deserialize)]
struct Corpus {
    recipes: std::collections::HashMap<Uuid, Entry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    output_item_template_id: Uuid,
    town_xp: u64,
}

fn corpus() -> &'static Corpus {
    static TABLE: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(RAW).unwrap_or_else(|e| panic!("decoration_craft_prestige.json: {e}"))
    })
}

/// The town prestige one unit of this recipe's decoration pays when collected,
/// with the decoration's template id. `None` for every recipe that is not a
/// prestige-paying decoration craft.
pub fn craft_prestige(recipe_id: &Uuid) -> Option<(Uuid, u64)> {
    corpus()
        .recipes
        .get(recipe_id)
        .map(|e| (e.output_item_template_id, e.town_xp))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCIENT_TREE_RECIPE: &str = "6d164b3d-6532-4c4b-adcc-b4b8bc75f2dd";
    const ANCIENT_TREE: &str = "d25c8faa-5943-4ab1-99d2-aab025ecb581";

    /// The one retail measurement: capture 17253 paid +91 for an Ancient Tree.
    #[test]
    fn the_ancient_tree_pays_what_retail_paid() {
        assert_eq!(
            craft_prestige(&Uuid::parse_str(ANCIENT_TREE_RECIPE).unwrap()),
            Some((Uuid::parse_str(ANCIENT_TREE).unwrap(), 91))
        );
    }

    #[test]
    fn every_entry_pays_something_and_agrees_with_the_recipe_output() {
        assert_eq!(corpus().recipes.len(), 39, "the 39 Workshop decorations");
        for (recipe, e) in &corpus().recipes {
            assert!(e.town_xp > 0, "{recipe} pays nothing");
            let out = crate::features::recipe_outputs::output_for(recipe)
                .unwrap_or_else(|| panic!("{recipe} has no APK output"));
            assert_eq!(out.output_item_template_id, e.output_item_template_id, "{recipe}");
        }
    }

    #[test]
    fn a_non_decoration_recipe_pays_nothing() {
        assert_eq!(craft_prestige(&Uuid::nil()), None);
    }
}
