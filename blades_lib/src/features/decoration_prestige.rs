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
//! field. The number is the decoration's own `PropData._txpReward`
//! (`PropDataList`), which is what the client's Workshop screen prints as the
//! craft's prestige reward (`CraftingMenuCraftDetailsPanel.SetItem`). All 141
//! craftable decorations have one (report #597). The APK states it a second
//! time on the `ItemBundleGenerationData` that sells that one decoration — the
//! Workshop's, or the gem store's `IAP_Deco_` bundle (report #538) — and the
//! two agree on 72 of the 74 that have both; the table keeps the larger
//! (`script/extract_decoration_craft_prestige.py`). Retail capture 17253 is the
//! proof: collecting an Ancient Tree craft paid +91, the Ancient Tree's
//! `_txpReward` and its Workshop bundle's `town_xp`.
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

    /// Report #538: a Workshop recipe for a decoration the gem store sold paid
    /// nothing, although the APK prices that decoration's prestige on its own
    /// single-item store bundle (`IAP_Deco_M_FighterTrophy`, `town_xp` 80).
    #[test]
    fn a_store_decoration_crafted_at_the_workshop_pays_its_store_prestige() {
        const FIGHTERS_TROPHY_RECIPE: &str = "a9db0fbb-e2f8-478e-ad18-02b85a1558aa";
        const FIGHTERS_TROPHY: &str = "fa792ef4-4ef2-4f7b-80a9-8e6b6e205374";
        assert_eq!(
            craft_prestige(&Uuid::parse_str(FIGHTERS_TROPHY_RECIPE).unwrap()),
            Some((Uuid::parse_str(FIGHTERS_TROPHY).unwrap(), 80))
        );
    }

    fn pays(recipe: &str) -> Option<u64> {
        craft_prestige(&Uuid::parse_str(recipe).unwrap()).map(|(_, xp)| xp)
    }

    /// Report #597 (and the rest of #538): a decoration no bundle sells on its
    /// own paid nothing, although the Workshop screen promised the player its
    /// `PropData._txpReward`. These are the APK's numbers.
    #[test]
    fn a_decoration_without_a_bundle_pays_its_prop_prestige() {
        for (recipe, name, apk) in [
            (
                "a05ce92d-e6e4-4535-94c4-c32602bc0eb2",
                "The King's Sentry",
                150,
            ),
            ("63dba2e6-def3-49eb-b9f0-11e6453a0103", "Potted Trees", 6),
            (
                "02680656-48db-4192-84fe-f5141c317729",
                "Blades Memorial",
                140,
            ),
            (
                "62f2dad0-ccc8-4020-a702-b1380ba6de24",
                "The Lich's Defeat",
                159,
            ),
            (
                "d5fbd96c-d82d-4a00-a411-2537929822c1",
                "Stalwart Lightkeeper",
                100,
            ),
            (
                "8ff994a3-a075-4119-b79a-3bb3c0d0f531",
                "Whiterun Banner",
                12,
            ),
        ] {
            assert_eq!(pays(recipe), Some(apk), "{name}");
        }
    }

    /// Where the APK's two numbers differ the larger one stays, so nothing paid
    /// before #597 went down: Hircine's Prize (prop 127, store bundle 142) and
    /// the Imperial Bath (prop 24, store bundle 89).
    #[test]
    fn a_decoration_priced_twice_keeps_the_larger_number() {
        assert_eq!(pays("3f2eecee-d295-4060-a0ea-867b27761408"), Some(142));
        assert_eq!(pays("fa6407a0-599b-4d31-a1d8-39bd46b096c2"), Some(89));
    }

    /// Every DecorationCrafting recipe the APK ships pays prestige, and only
    /// those are in the table.
    #[test]
    fn every_decoration_recipe_pays_and_agrees_with_the_recipe_output() {
        assert_eq!(corpus().recipes.len(), 141);
        assert_eq!(
            corpus().recipes.values().map(|e| e.town_xp).sum::<u64>(),
            2527 + 3875,
            "the 67 added by #597 and the 74 paid before it"
        );
        for (recipe, e) in &corpus().recipes {
            assert!(e.town_xp > 0, "{recipe} pays nothing");
            let out = crate::features::recipe_outputs::output_for(recipe)
                .unwrap_or_else(|| panic!("{recipe} has no APK output"));
            assert_eq!(
                out.output_item_template_id, e.output_item_template_id,
                "{recipe}"
            );
            assert_eq!(
                out.crafting_type.as_deref(),
                Some("DecorationCrafting"),
                "{recipe}"
            );
        }
        let outputs: serde_json::Value =
            serde_json::from_str(include_str!("../recipe_outputs.json")).unwrap();
        let decorations = outputs["recipes"]
            .as_object()
            .unwrap()
            .values()
            .filter(|out| out["craftingType"] == "DecorationCrafting")
            .count();
        assert_eq!(
            decorations, 141,
            "a decoration recipe is missing from the table"
        );
    }

    #[test]
    fn a_non_decoration_recipe_pays_nothing() {
        assert_eq!(craft_prestige(&Uuid::nil()), None);
    }
}
