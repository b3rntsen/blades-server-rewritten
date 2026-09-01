//! Building `DungeonGeneratedData` for a dungeon.
//!
//! ## Why this is shared
//!
//! The client is told which dungeon to load and, separately, what is inside it.
//! Those two must describe the SAME dungeon: the generated data is keyed by the
//! dungeon's own spawn-group / chest / item ids, and the client looks up each id
//! as it populates the level.
//!
//! Hand it ids from a different dungeon and nothing resolves — no enemies
//! spawn, so no `enemy_killed` action is ever sent, so the run cannot progress.
//! That is exactly what the Abyss did: it served a hard-coded stub whose two
//! spawn groups exist only in the floor-1 dungeon, so floor 1 played and every
//! other floor hung. Six of the seven live runs sat at floor 0 with nothing
//! completed.
//!
//! The quest path had always generated this correctly from the real dungeon.
//! Rather than a second implementation for the Abyss, that logic lives here and
//! both call it.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{
    game_data::GameData,
    static_data::StaticData,
    economy::RewardGrant,
    user_data::{
        ChestGeneratedData, DungeonEnemyResult, DungeonGeneratedData, DungeonItemResult,
        LootTableResult,
    },
};

/// Build the generated data for `dungeon_uuid`, with every enemy at
/// `enemy_level` and worth `given_xp`.
///
/// Returns `None` when the dungeon is not in `parsed.json` — the caller decides
/// whether that is fatal. A malformed item spawn is skipped rather than
/// panicking: a partial `parsed.json` must not take down the request, the item
/// simply does not appear.
pub fn generate_for_dungeon(
    game_data: &GameData,
    static_data: &StaticData,
    dungeon_uuid: &Uuid,
    enemy_level: i64,
    given_xp: u64,
) -> Option<DungeonGeneratedData> {
    let dungeon = game_data.dungeons.get(dungeon_uuid)?;

    // Get chest_loots for item generation
    let chest_loots = &static_data.chest_loots;

    Some(DungeonGeneratedData {
        enemy_generated_data: dungeon
            .spawn_info
            .enemy_spawn_groups
            .iter()
            .map(|(spawn_group_id, spawn_group)| {
                let mut enemies_info = Vec::new();
                for _ in 0..spawn_group.quantity.max(1) {
                    enemies_info.push(vec![DungeonEnemyResult {
                        enemy_level,
                        given_xp,
                        spawn_group_loot: HashMap::default(),
                        loot_table_loot: HashMap::default(),
                    }]);
                }
                (*spawn_group_id, enemies_info)
            })
            .collect(),
        chest_generated_data: dungeon
            .spawn_info
            .chest
            .iter()
            .map(|(chest_spawn_id, _)| (*chest_spawn_id, vec![ChestGeneratedData { tier: 1 }]))
            .collect(),
        item_generated_data: dungeon
            .spawn_info
            .item
            .iter()
            .filter_map(|(item_spawn_id, spawn_info)| {
                let picked = spawn_info.apparition_settings.first()?;
                let interactable = game_data.interactables.get(&picked.interactable_uuid)?;
                
                // Generate loot for the item
                let mut loot_table_loot = HashMap::new();
                for (loot_key, _) in &interactable.loot_table {
                    // Pick a random loot entry from chest_loots
                    if let Some(loot_entry) = pick_loot_for_item(chest_loots, loot_key) {
                        loot_table_loot.insert(*loot_key, loot_entry);
                    }
                }
                
                Some((
                    *item_spawn_id,
                    vec![DungeonItemResult {
                        loot_table_loot,
                    }],
                ))
            })
            .collect(),
        algorithm_version: 1,
        version: 0,
    })
}

/// Helper function to pick loot for an item based on the loot key
fn pick_loot_for_item(chest_loots: &[RewardGrant], loot_key: &Uuid) -> Option<LootTableResult> {
    if chest_loots.is_empty() {
        return None;
    }
    
    // Use the loot_key to deterministically pick a loot entry
    let hash = loot_key.as_u128() as usize;
    let selected = &chest_loots[hash % chest_loots.len()];
    
    // Convert RewardGrant to LootTableResult
    let mut result = LootTableResult::default();
    
    // Add stackable items
    for (item_id, quantity) in &selected.stackable_items {
        result.stackable_items.insert(*item_id, *quantity);
    }
    
    // Add currencies
    for (currency_id, amount) in &selected.currencies {
        result.currencies.insert(*currency_id, *amount);
    }
    
    // Add items (copy them, IDs will be re-minted when collected)
    for reward_item in &selected.items {
        let new_item = reward_item.item.clone();

        result.item.0.insert(reward_item.id, new_item);
    }
    
    Some(result)
}