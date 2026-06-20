//! Salvage — `POST /salvages`.
//!
//! Break gear down at the smithy into crafting materials. Retail rolls the yield
//! randomly (the same recipe gives varying amounts), so we grant a *representative*
//! capture-derived material set per `recipeId` (deterministic, no RNG — documented
//! fidelity limit). The handler removes the salvaged item(s); this layer just sums the
//! material reward.
//!
//! Captured: request `{salvageInfos:[{recipeId,itemId}], buildingId}` ->
//! `{reward:{stackableItems:{…}}, inventory:{backpack:{removedItems, stackableItems}}}`.

use std::collections::HashMap;

use uuid::Uuid;

use crate::economy::RewardGrant;

/// Sum the representative material yield for a set of salvage recipes.
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
