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
// -- interactable loot ------------------------------------------------------
//
// `GameDataInteractable::loot_table` is a `HashMap<Uuid, EmptyStruct>`: parsed.json
// carries the loot-table IDS with no contents. So every breakable and floor pickup
// was generated with `LootTableResult::default()` -- empty -- and the client was
// told the barrel contains nothing (tracker #100, #102).
//
// Retail's own `itemGeneratedData[].lootTableLoot` is keyed by exactly those ids
// and DOES carry contents. The table below is 108,122 observations over 25 loot
// tables, mined from captured responses -- including how often each table rolled
// EMPTY, because retail's breakables frequently give nothing and reproducing the
// hit rate matters as much as reproducing the contents.
static INTERACTABLE_LOOT_RAW: &str = include_str!("../interactable_loot.json");

fn interactable_loot() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(INTERACTABLE_LOOT_RAW).unwrap_or_else(|e| {
            // blades_lib has no logger; a malformed table degrades to empty, which is
            // exactly the behaviour that preceded this change.
            let _ = e;
            serde_json::json!({ "tables": {} })
        })
    })
}

/// Deterministic per (dungeon, spawn, table) so a re-fetch of the same dungeon
/// yields the same contents -- the client is told once what a barrel holds and
/// must still find it there when it breaks it.
fn loot_seed(dungeon_uuid: &Uuid, spawn_id: &Uuid, table_id: &Uuid) -> u64 {
    let mut x = dungeon_uuid.as_u128() as u64
        ^ (spawn_id.as_u128() as u64).rotate_left(21)
        ^ (table_id.as_u128() as u64).rotate_left(42);
    // splitmix64 finaliser
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// One roll of a loot table, weighted by how often retail produced each outcome.
///
/// The empty outcome is a real one: across the corpus these tables rolled empty
/// 18,583 times for a single table alone. Dropping it would make every barrel
/// pay, which is not what retail did.
fn roll_loot_table(dungeon_uuid: &Uuid, spawn_id: &Uuid, table_id: &Uuid) -> LootTableResult {
    let mut out = LootTableResult::default();
    let Some(entry) = interactable_loot()
        .get("tables")
        .and_then(|t| t.get(table_id.to_string()))
    else {
        // A table we never observed stays empty, exactly as before.
        return out;
    };

    let empty_n = entry.get("emptyObservations").and_then(|v| v.as_u64()).unwrap_or(0);
    let rolls = entry.get("rolls").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let total: u64 = empty_n
        + rolls.iter().filter_map(|r| r.get("n").and_then(|v| v.as_u64())).sum::<u64>();
    if total == 0 {
        return out;
    }

    let mut pick = loot_seed(dungeon_uuid, spawn_id, table_id) % total;
    if pick < empty_n {
        return out;
    }
    pick -= empty_n;

    for r in &rolls {
        let n = r.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
        if pick < n {
            let (Some(item), Some(qty)) = (
                r.get("itemId").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok()),
                r.get("quantity").and_then(|v| v.as_u64()),
            ) else {
                return out;
            };
            out.stackable_items.insert(item, qty);
            return out;
        }
        pick -= n;
    }
    out
}

pub fn generate_for_dungeon(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    enemy_level: i64,
    given_xp: u64,
) -> Option<DungeonGeneratedData> {
    let dungeon = game_data.dungeons.get(dungeon_uuid)?;

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
                Some((
                    *item_spawn_id,
                    vec![DungeonItemResult {
                        loot_table_loot: interactable
                            .loot_table
                            .iter()
                            .map(|(k, _)| {
                                (*k, roll_loot_table(dungeon_uuid, item_spawn_id, k))
                            })
                            .collect(),
                    }],
                ))
            })
            .collect(),
        algorithm_version: 1,
        version: 0,
    })
}


#[cfg(test)]
mod interactable_loot_tests {
    use super::*;

    fn table() -> &'static serde_json::Value {
        interactable_loot()
    }

    /// The corpus must actually be there. Everything below is vacuous without it.
    #[test]
    fn the_loot_table_corpus_loads() {
        let tables = table()["tables"].as_object().expect("tables object");
        assert!(tables.len() >= 20, "expected the mined tables, got {}", tables.len());
        let observations: u64 = tables
            .values()
            .flat_map(|t| t["rolls"].as_array().cloned().unwrap_or_default())
            .filter_map(|r| r["n"].as_u64())
            .sum();
        assert!(observations > 50_000, "only {observations} observations");
    }

    /// A breakable must be able to produce something. This is the bug: every loot
    /// table generated empty, so barrels and plants held nothing at all.
    #[test]
    fn some_rolls_produce_loot() {
        let tables = table()["tables"].as_object().unwrap();
        let dungeon = Uuid::from_u128(0xD0);
        let mut produced = 0;
        let mut checked = 0;

        for tid in tables.keys() {
            let table_id: Uuid = tid.parse().unwrap();
            // several spawns, because one spawn may legitimately roll empty
            for s in 0..40u128 {
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id);
                checked += 1;
                if !got.stackable_items.is_empty() {
                    produced += 1;
                }
            }
        }
        assert!(checked > 0);
        assert!(
            produced > 0,
            "not one of {checked} rolls produced loot — breakables are still empty"
        );
    }

    /// Empty is a real outcome, not a failure. Retail's tables rolled empty tens
    /// of thousands of times; a build where every barrel pays is as wrong as one
    /// where none do.
    #[test]
    fn empty_remains_possible() {
        let tables = table()["tables"].as_object().unwrap();
        let dungeon = Uuid::from_u128(0xD1);
        let mut empty = 0;
        let mut total = 0;
        for tid in tables.keys() {
            let table_id: Uuid = tid.parse().unwrap();
            for s in 0..40u128 {
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id);
                total += 1;
                if got.stackable_items.is_empty() && got.currencies.is_empty() {
                    empty += 1;
                }
            }
        }
        assert!(empty > 0, "every one of {total} rolls paid out; retail's did not");
        assert!(empty < total, "no roll paid out at all");
    }

    /// The same barrel must hold the same thing every time it is described.
    ///
    /// The client is told the dungeon's contents when it loads and must find them
    /// unchanged when it breaks the barrel; a re-roll would desync the two.
    #[test]
    fn a_roll_is_stable_for_the_same_barrel() {
        let tables = table()["tables"].as_object().unwrap();
        let tid: Uuid = tables.keys().next().unwrap().parse().unwrap();
        let d = Uuid::from_u128(7);
        let s = Uuid::from_u128(9);
        let a = roll_loot_table(&d, &s, &tid);
        let b = roll_loot_table(&d, &s, &tid);
        assert_eq!(a.stackable_items, b.stackable_items, "same barrel, different loot");

        // control: a DIFFERENT spawn should not be forced to match, or the
        // stability check above would hold trivially for everything.
        let mut differs = false;
        for other in 0..60u128 {
            let c = roll_loot_table(&d, &Uuid::from_u128(other), &tid);
            if c.stackable_items != a.stackable_items {
                differs = true;
                break;
            }
        }
        assert!(differs, "every spawn rolls identically — the seed is not varying");
    }

    /// Every item id we hand out must be one retail actually placed in that table.
    #[test]
    fn we_only_ever_emit_items_retail_used() {
        let tables = table()["tables"].as_object().unwrap();
        let dungeon = Uuid::from_u128(0xD2);
        let mut checked = 0;
        for (tid, entry) in tables {
            let table_id: Uuid = tid.parse().unwrap();
            let allowed: std::collections::HashSet<String> = entry["rolls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|r| r["itemId"].as_str().map(|s| s.to_string()))
                .collect();
            for s in 0..25u128 {
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id);
                for item in got.stackable_items.keys() {
                    assert!(
                        allowed.contains(&item.to_string()),
                        "table {tid} produced {item}, which retail never put in it"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no items emitted — the test proved nothing");
    }
}
