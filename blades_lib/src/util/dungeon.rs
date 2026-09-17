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

use serde::Deserialize;
use uuid::Uuid;

use crate::{
    game_data::GameData,
    user_data::{
        ChestGeneratedData, DungeonEnemyResult, DungeonGeneratedData, DungeonItemResult, Item,
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

// The APK carries the exact tier and quantity on every dungeon chest spawn,
// but the old parsed.json extractor discarded both fields and retained only
// the spawn id. This sidecar was extracted from the same 137 dungeon assets:
// all 292 ids in parsed.json are present, including seven groups with more
// than one chest. Keep it compiled into the binary like interactable loot so
// a missing production bind-mount cannot silently restore the tier-1 stub.
static CHEST_TIERS_RAW: &str = include_str!("../chest_tiers.json");

// How many loot results retail put on each floor-item spawn point.
//
// `generate_for_dungeon` emitted exactly ONE per spawn. Retail emitted more on
// 4,100 of 13,170 spawn OBSERVATIONS (31.1%), which is 97 of the 1,285 distinct
// spawn points (7.6%). The two differ because the multi-result spawns sit in
// dungeons players ran far more often: the seven-result spawn alone was seen
// 1,895 times, and one spawn carries twenty-four. So a minority of floor piles
// paid a fraction of what retail put there, on a majority of the runs (#173).
//
// This is not a corpus gap: all 20 loot tables reachable from a dungeon spawn
// are already in `interactable_loot.json`, and all 1,976 parsed.json spawns
// resolve to a known interactable. The contents were right; the COUNT was wrong.
//
// The count is a property of the SPAWN POINT, measured: of the 969 spawns
// observed more than once, 967 always produced the same number and only two
// varied. A spawn absent from this file keeps one result -- the size is never
// invented.
static FLOOR_PILE_SIZES_RAW: &str = include_str!("../floor_pile_sizes.json");

#[derive(Deserialize)]
struct ChestTierCorpus {
    chests: HashMap<Uuid, ChestSpawnDefinition>,
}

#[derive(Deserialize)]
struct ChestSpawnDefinition {
    /// The APK's own value, which is -1 on one spawn.
    rarity: i64,
    /// The same number with the negative filtered out. Generation reads `rarity`
    /// now (see #288 — retail really did send -1), so this is only read by the
    /// corpus tests that pin the tier histogram.
    #[cfg_attr(not(test), allow(dead_code))]
    tier: Option<u64>,
    quantity: u64,
}

fn chest_tiers() -> &'static ChestTierCorpus {
    static TABLE: std::sync::OnceLock<ChestTierCorpus> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(CHEST_TIERS_RAW).unwrap_or_else(|_| ChestTierCorpus {
            chests: HashMap::new(),
        })
    })
}

fn floor_pile_sizes() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(FLOOR_PILE_SIZES_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "spawns": {} }))
    })
}

/// How many results this floor spawn holds. One when never observed, which is
/// exactly the behaviour that preceded this change.
fn floor_pile_size(spawn_id: &Uuid) -> usize {
    floor_pile_sizes()
        .get("spawns")
        .and_then(|s| s.get(spawn_id.to_string()))
        .and_then(|e| e.get("results"))
        .and_then(|n| n.as_u64())
        .unwrap_or(1)
        .max(1) as usize
}

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
fn loot_seed(dungeon_uuid: &Uuid, spawn_id: &Uuid, table_id: &Uuid, result_index: usize) -> u64 {
    let mut x = dungeon_uuid.as_u128() as u64
        ^ (spawn_id.as_u128() as u64).rotate_left(21)
        ^ (table_id.as_u128() as u64).rotate_left(42)
        // Without this every result in a pile of seven would be the same draw,
        // which turns "seven results" into "one stack, seven times".
        ^ (result_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64 finaliser
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// One roll of a loot table, weighted by how often retail produced each outcome.
///
/// A whole RESULT is drawn, not a single item. Retail's results routinely carry
/// several stacks at once -- across the corpus, 68,000 results held one item,
/// 9,659 held two, 5,096 held three and 1,379 held four -- so drawing item by
/// item could never reproduce a breakable that "spews five things" (#104). It
/// also keeps which items appeared TOGETHER, instead of inventing a
/// distribution over combinations.
///
/// The empty result is one of the drawn outcomes (68,081 of them), so a barrel
/// that gives nothing stays as common as retail made it.
fn roll_loot_table(
    dungeon_uuid: &Uuid,
    spawn_id: &Uuid,
    table_id: &Uuid,
    result_index: usize,
) -> LootTableResult {
    let mut out = LootTableResult::default();
    let Some(results) = interactable_loot()
        .get("tables")
        .and_then(|t| t.get(table_id.to_string()))
        .and_then(|e| e.get("results"))
        .and_then(|r| r.as_array())
    else {
        // A table we never observed stays empty, exactly as before.
        return out;
    };

    let total: u64 = results
        .iter()
        .filter_map(|r| r.get("n").and_then(|v| v.as_u64()))
        .sum();
    if total == 0 {
        return out;
    }

    let mut pick = loot_seed(dungeon_uuid, spawn_id, table_id, result_index) % total;
    for r in results {
        let n = r.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
        if pick >= n {
            pick -= n;
            continue;
        }
        let loot = r.get("loot");
        if let Some(items) = loot.and_then(|l| l.get("stackableItems")).and_then(|v| v.as_object()) {
            for (id, qty) in items {
                if let (Ok(uuid), Some(q)) = (Uuid::parse_str(id), qty.as_u64()) {
                    out.stackable_items.insert(uuid, q);
                }
            }
        }
        if let Some(curs) = loot.and_then(|l| l.get("currencies")).and_then(|v| v.as_object()) {
            for (id, amt) in curs {
                if let (Ok(uuid), Some(a)) = (Uuid::parse_str(id), amt.as_u64()) {
                    out.currencies.insert(uuid, a);
                }
            }
        }
        return out;
    }
    out
}

// -- enemy loot -------------------------------------------------------------
//
// Enemies dropped nothing: every `DungeonEnemyResult` was pushed with
// `loot_table_loot: HashMap::default()` and nothing ever filled it, so
// `merged_loot_table()` was empty for every corpse in the game (#152).
//
// Retail put the answer on the wire. Its dungeon-generation response carries,
// per enemy, the loot that enemy will yield -- rolled at generation time,
// exactly as we already do for breakables. Two corpora mined from captured
// retail traffic reproduce it:
//
//   enemy_loot.json         WHAT each loot table drops. Whole results with an
//                           observation count, 19 tables over 49,602
//                           observations. Level-keyed where retail's content
//                           moves with enemy level, which is most of the
//                           payout: the gold table is 59% of all observations
//                           and pays 5 gold at level 1 and 343 at level 90
//                           (r=0.968). Drawing it level-blind would hand a
//                           level-1 enemy 376 gold.
//   enemy_group_tables.json WHICH tables an enemy of a given spawn group rolls.
//                           parsed.json does not say -- its enemy spawn groups
//                           carry `quantity` and nothing else -- so without
//                           this the first corpus cannot be addressed at all.
static ENEMY_LOOT_RAW: &str = include_str!("../enemy_loot.json");
static ENEMY_GROUP_TABLES_RAW: &str = include_str!("../enemy_group_tables.json");

/// The gold table, and the fallback for a spawn group the corpus never saw.
///
/// 1,066 of parsed.json's 1,956 enemy spawn groups were observed (51.6% of enemy
/// instances). For the rest there is no observed answer, and "drop nothing" is
/// the wrong guess: 90.7% of all observed enemy results roll this table, and
/// gold is the only drop whose amount the corpus can scale to any level. An
/// unobserved enemy therefore pays level-appropriate gold and nothing else --
/// modest, never a boss item on a level-3 rat, and visibly better than a corpse
/// that is always empty.
const GOLD_LOOT_TABLE_ID: u128 = 0x871c2e9b_7e7a_4564_a022_e435dfb8a436;

fn enemy_loot() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(ENEMY_LOOT_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "tables": {} }))
    })
}

fn enemy_group_tables() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(ENEMY_GROUP_TABLES_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "groups": {} }))
    })
}

/// splitmix64 as a stream, so the successive draws that make up one enemy's
/// loot -- which table set, which result per table, which instance id per item
/// -- do not correlate with each other. Deterministic: the same enemy in the
/// same dungeon is described identically every time the client asks, which is
/// the same guarantee `roll_loot_table` gives a barrel.
struct LootRng(u64);

impl LootRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uuid4-shaped instance id. Retail minted a fresh uuid4 for every gear
    /// drop and the corpus strips them, so one must be made here -- but it is
    /// derived from the stream rather than random, because the generated data
    /// is re-served on every quest-list fetch and the id the client was told
    /// must still be the id it finds on the corpse.
    fn uuid(&mut self) -> Uuid {
        let (hi, lo) = (self.next(), self.next());
        let mut b = (((hi as u128) << 64) | lo as u128).to_be_bytes();
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Uuid::from_bytes(b)
    }
}

fn enemy_rng(
    dungeon_uuid: &Uuid,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
) -> LootRng {
    LootRng(
        (dungeon_uuid.as_u128() as u64)
            ^ (spawn_group_id.as_u128() as u64).rotate_left(21)
            ^ ((dungeon_uuid.as_u128() >> 64) as u64).rotate_left(11)
            ^ (spawner_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (enemy_index as u64).rotate_left(37),
    )
}

/// Draw one entry from an array of `{..., "n": weight}` observations.
fn draw_weighted<'a>(rng: &mut LootRng, options: &'a [serde_json::Value]) -> Option<&'a serde_json::Value> {
    let total: u64 = options.iter().filter_map(|o| o["n"].as_u64()).sum();
    if total == 0 {
        return None;
    }
    let mut pick = rng.next() % total;
    for o in options {
        let n = o["n"].as_u64().unwrap_or(0);
        if pick < n {
            return Some(o);
        }
        pick -= n;
    }
    options.last()
}

/// The results to draw from for one table at one enemy level.
///
/// `levelKeyed` is measured, not assumed, and only some tables carry it; for the
/// rest `results` is pooled over every level and `byLevel` is null. The nearest
/// band is used when no band contains the level, which is what happens above
/// and below the level range retail was observed at -- clamping is better than
/// dropping the table, whose only alternative outcome is an empty corpse.
fn enemy_table_results<'a>(entry: &'a serde_json::Value, enemy_level: i64) -> Option<&'a Vec<serde_json::Value>> {
    if entry["levelKeyed"].as_bool() != Some(true) {
        return entry["results"].as_array();
    }
    let bands = entry["byLevel"].as_array()?;
    let mut nearest: Option<(i64, &serde_json::Value)> = None;
    for band in bands {
        let lo = band["minEnemyLevel"].as_i64().unwrap_or(i64::MIN);
        let hi = band["maxEnemyLevel"].as_i64().unwrap_or(i64::MAX);
        if enemy_level >= lo && enemy_level <= hi {
            return band["results"].as_array();
        }
        let distance = if enemy_level < lo { lo - enemy_level } else { enemy_level - hi };
        if nearest.map_or(true, |(d, _)| distance < d) {
            nearest = Some((distance, band));
        }
    }
    nearest.and_then(|(_, b)| b["results"].as_array())
}

/// One enemy's `lootTableLoot`, rolled the way retail rolled it.
///
/// Two draws, because the two corpora answer two different questions. First
/// WHICH tables this enemy rolls: a spawn group's table set is usually fixed,
/// but the four groups that vary hold more than half of all observed enemy
/// results -- they spawn a mix of enemy types -- so the set is drawn from the
/// distribution the group was observed with rather than pinned to its
/// commonest. Then WHAT each of those tables gives at this enemy's level.
///
/// A whole result is drawn per table, not item by item: retail's results
/// routinely carry a stack and a coin drop together, and independent per-item
/// draws cannot express which things appeared TOGETHER (the bug that once left
/// a barrel unable to yield more than one stack, fork #104). The empty result
/// is one of the outcomes for the same reason -- an enemy that drops nothing is
/// what retail did most of the time on most tables.
pub fn roll_enemy_loot(
    dungeon_uuid: &Uuid,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
    enemy_level: i64,
) -> HashMap<Uuid, LootTableResult> {
    let mut rng = enemy_rng(dungeon_uuid, spawn_group_id, spawner_index, enemy_index);
    let mut out = HashMap::new();

    let group = enemy_group_tables()["groups"].get(spawn_group_id.to_string());
    let tables: Vec<Uuid> = match group.and_then(|g| g["sets"].as_array()) {
        Some(sets) => draw_weighted(&mut rng, sets)
            .and_then(|s| s["tables"].as_array())
            .map(|ts| ts.iter().filter_map(|t| t.as_str()?.parse().ok()).collect())
            .unwrap_or_default(),
        // Never observed: see GOLD_LOOT_TABLE_ID.
        None => vec![Uuid::from_u128(GOLD_LOOT_TABLE_ID)],
    };

    for table_id in tables {
        let Some(entry) = enemy_loot()["tables"].get(table_id.to_string()) else {
            // A table left out of the corpus as too thin to model. Retail keyed
            // it on this enemy, so the key is sent, empty -- dropping the key
            // would be a shape retail never sent.
            out.insert(table_id, LootTableResult::default());
            continue;
        };
        let mut result = LootTableResult::default();
        if let Some(results) = enemy_table_results(entry, enemy_level) {
            if let Some(drawn) = draw_weighted(&mut rng, results) {
                let loot = &drawn["loot"];
                if let Some(stacks) = loot["stackableItems"].as_object() {
                    for (id, qty) in stacks {
                        if let (Ok(uuid), Some(q)) = (Uuid::parse_str(id), qty.as_u64()) {
                            result.stackable_items.insert(uuid, q);
                        }
                    }
                }
                if let Some(currencies) = loot["currencies"].as_object() {
                    for (id, amount) in currencies {
                        if let (Ok(uuid), Some(a)) = (Uuid::parse_str(id), amount.as_u64()) {
                            result.currencies.insert(uuid, a);
                        }
                    }
                }
                for raw in loot["items"].as_array().into_iter().flatten() {
                    // The corpus stores a gear instance in retail's own wire
                    // shape, so `Item` deserializes it directly. A malformed
                    // one is skipped, never fatal: a partial corpus must not
                    // take down dungeon generation.
                    if let Ok(item) = serde_json::from_value::<Item>(raw.clone()) {
                        result.item.0.insert(rng.uuid(), item);
                    }
                }
            }
        }
        out.insert(table_id, result);
    }

    out
}

/// Which dungeon owns this enemy spawn group, if exactly one does.
///
/// THE VARIANT MISMATCH (#174). Retail builds several versions of a dungeon —
/// `EQ22_SQ102_DungeonSettings_A`, `_B`, `_C` — and 23 families in `parsed.json`
/// have them. A quest names exactly one, always the `_A`, and every one of those
/// 23 families gives its variants **completely different** enemy spawn groups: not
/// one group is shared between any two variants.
///
/// The client does not always walk the one we named. When it walks `_B`, every
/// kill it reports names a spawner we have no data for, so `dungeon_update` logs
/// "not in generated data (stale)" and throws the kill away — no experience, no
/// loot, for that whole stage. 15 of the 22 such warnings in eleven days are
/// exactly this.
///
/// Because the variants share no groups, the reported spawner identifies the
/// variant unambiguously, which is what makes the repair in `dungeon_update` safe:
/// the client tells us which version it is in, and we can generate the right data
/// instead of discarding its progress.
///
/// `None` when no dungeon owns the group, or when more than one does — in which
/// case the answer is not unambiguous and the caller must not guess.
pub fn dungeon_owning_spawn_group(game_data: &GameData, group_id: &Uuid) -> Option<Uuid> {
    let mut found = None;
    for (dungeon_id, dungeon) in &game_data.dungeons {
        if dungeon.spawn_info.enemy_spawn_groups.contains_key(group_id) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(*dungeon_id);
        }
    }
    found
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
                for spawner_index in 0..spawn_group.quantity.max(1) as usize {
                    enemies_info.push(vec![DungeonEnemyResult {
                        enemy_level,
                        given_xp,
                        // Retail's own `spawnGroupLoot` is left empty deliberately:
                        // 38 of 66,994 captured enemy results carried one and every
                        // observation is the same single item, too little to model.
                        spawn_group_loot: HashMap::default(),
                        loot_table_loot: roll_enemy_loot(
                            dungeon_uuid,
                            spawn_group_id,
                            spawner_index,
                            0,
                            enemy_level,
                        ),
                    }]);
                }
                (*spawn_group_id, enemies_info)
            })
            .collect(),
        chest_generated_data: dungeon
            .spawn_info
            .chest
            .iter()
            .map(|(chest_spawn_id, _)| {
                let definition = chest_tiers().chests.get(chest_spawn_id);
                // The APK rarity, verbatim -- INCLUDING -1.
                //
                // That -1 was treated as "unset" and replaced with tier 1. Retail
                // sent -1 for that chest in all 5 captured generations, so tier 1
                // was our invention. A spawn genuinely absent from the corpus
                // (a future dungeon) still falls back to 1.
                let tier = definition
                    .map(|d| d.rarity)
                    .unwrap_or(1);
                let quantity = definition.map(|d| d.quantity).unwrap_or(1).max(1);
                (
                    *chest_spawn_id,
                    (0..quantity).map(|_| ChestGeneratedData { tier }).collect(),
                )
            })
            .collect(),
        item_generated_data: dungeon
            .spawn_info
            .item
            .iter()
            .filter_map(|(item_spawn_id, spawn_info)| {
                let picked = spawn_info.apparition_settings.first()?;
                let interactable = game_data.interactables.get(&picked.interactable_uuid)?;
                // One result per thing retail put on this spawn, each rolled
                // separately — a pile of seven is seven draws, not one repeated.
                let pile = (0..floor_pile_size(item_spawn_id))
                    .map(|result_index| DungeonItemResult {
                        loot_table_loot: interactable
                            .loot_table
                            .iter()
                            .map(|(k, _)| {
                                (
                                    *k,
                                    roll_loot_table(
                                        dungeon_uuid,
                                        item_spawn_id,
                                        k,
                                        result_index,
                                    ),
                                )
                            })
                            .collect(),
                    })
                    .collect();
                Some((*item_spawn_id, pile))
            })
            .collect(),
        algorithm_version: 1,
        version: 0,
    })
}

#[cfg(test)]
mod chest_generation_tests {
    use super::*;

    #[test]
    fn apk_chest_corpus_is_complete_and_not_the_old_stub() {
        let corpus = chest_tiers();
        assert_eq!(corpus.chests.len(), 292, "the APK has 292 chest spawn groups");
        assert_eq!(
            corpus.chests.values().filter(|c| c.tier == Some(1)).count(),
            136
        );
        assert_eq!(
            corpus.chests.values().filter(|c| c.tier == Some(2)).count(),
            107
        );
        assert_eq!(
            corpus.chests.values().filter(|c| c.tier == Some(3)).count(),
            48
        );
        assert_eq!(corpus.chests.values().filter(|c| c.tier.is_none()).count(), 1);
        assert_eq!(
            corpus.chests.values().filter(|c| c.quantity > 1).count(),
            7,
            "multi-chest groups must not collapse back to one"
        );
    }

    #[test]
    fn every_parsed_chest_uses_its_apk_tier_and_quantity() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(path).expect("read parsed.json");
        let game_data: GameData = serde_json::from_str(&raw).expect("parse game data");
        let mut checked = 0;
        let mut negative = 0;

        for (dungeon_id, dungeon) in &game_data.dungeons {
            let Some(generated) = generate_for_dungeon(&game_data, dungeon_id, 1, 1) else {
                panic!("known dungeon did not generate");
            };
            for chest_id in dungeon.spawn_info.chest.keys() {
                let expected = chest_tiers()
                    .chests
                    .get(chest_id)
                    .unwrap_or_else(|| panic!("parsed chest {chest_id} missing from APK sidecar"));
                let actual = generated
                    .chest_generated_data
                    .get(chest_id)
                    .expect("generated chest group");
                assert_eq!(actual.len(), expected.quantity.max(1) as usize, "{chest_id}");
                // The APK rarity verbatim, INCLUDING the one spawn whose rarity
                // is -1. This used to read `tier.unwrap_or(1)`, which is what
                // turned retail's -1 into a 1 on the wire.
                assert!(
                    actual.iter().all(|c| c.tier == expected.rarity),
                    "{chest_id}: generated {:?}, APK rarity {}",
                    actual.iter().map(|c| c.tier).collect::<Vec<_>>(),
                    expected.rarity
                );
                if expected.rarity < 0 {
                    negative += 1;
                }
                checked += 1;
            }
        }

        assert_eq!(checked, 292, "the test must cover the complete APK corpus");
        // Exactly one spawn carries a negative rarity, and retail sent -1 for it
        // in all 5 captured generations. If this reaches 0 the value has been
        // silently clamped again.
        assert_eq!(negative, 1, "the -1 chest must still reach the wire as -1");
    }
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
            .flat_map(|t| t["results"].as_array().cloned().unwrap_or_default())
            .filter_map(|r| r["n"].as_u64())
            .sum();
        assert!(observations > 100_000, "only {observations} observations");
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
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0);
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
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0);
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
        let a = roll_loot_table(&d, &s, &tid, 0);
        let b = roll_loot_table(&d, &s, &tid, 0);
        assert_eq!(a.stackable_items, b.stackable_items, "same barrel, different loot");

        // control: a DIFFERENT spawn should not be forced to match, or the
        // stability check above would hold trivially for everything.
        let mut differs = false;
        for other in 0..60u128 {
            let c = roll_loot_table(&d, &Uuid::from_u128(other), &tid, 0);
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
            let allowed: std::collections::HashSet<String> = entry["results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .flat_map(|r| {
                    r["loot"]["stackableItems"]
                        .as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect();
            for s in 0..25u128 {
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0);
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

    /// A breakable must be able to spew SEVERAL stacks at once.
    ///
    /// My first pass mined (itemId, quantity) pairs independently and drew one,
    /// so a barrel could never yield more than a single stack — reported as
    /// "breakables are only spewing 1 item but they usually spew 5" (#104).
    /// Retail's results carry 2, 3 and 4 items together (9,659 / 5,096 / 1,379
    /// observations), so a multi-item roll has to be reachable.
    #[test]
    fn a_roll_can_yield_several_items_at_once() {
        let tables = table()["tables"].as_object().unwrap();
        let dungeon = Uuid::from_u128(0xD3);
        let mut multi = 0;
        let mut single = 0;
        let mut biggest = 0;

        for tid in tables.keys() {
            let table_id: Uuid = tid.parse().unwrap();
            for s in 0..200u128 {
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0);
                let n = got.stackable_items.len();
                biggest = biggest.max(n);
                if n > 1 {
                    multi += 1;
                } else if n == 1 {
                    single += 1;
                }
            }
        }
        assert!(
            multi > 0,
            "not one roll produced more than a single stack (biggest was {biggest}) — \
             breakables can still only spew one thing"
        );
        // Control: single-item results must still dominate, or we have swung too
        // far and made every barrel a jackpot.
        assert!(single > multi, "multi-item rolls ({multi}) outnumber single ({single})");
    }
}

#[cfg(test)]
mod variant_owner_tests {
    use super::*;

    /// A DUNGEON VARIANT IS IDENTIFIABLE FROM ONE SPAWN GROUP.
    ///
    /// That is what makes the `dungeon_update` repair safe. Retail builds several
    /// versions of a dungeon (`_A`, `_B`, `_C`); 23 families in `parsed.json` have
    /// them, and in all 23 the variants share NOT ONE enemy spawn group — so a
    /// reported spawner names its variant unambiguously.
    ///
    /// Asserted against the shipped data rather than a fixture, because the whole
    /// claim is about that data.
    #[test]
    fn variants_never_share_an_enemy_spawn_group() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let dungeons = parsed["dungeons"].as_object().expect("dungeons");

        // Group by handle stem: EQ22_SQ102_DungeonSettings_A -> EQ22_SQ102_DungeonSettings
        let mut families: std::collections::HashMap<String, Vec<&str>> = Default::default();
        for (id, d) in dungeons {
            let h = d["handle"].as_str().unwrap_or("");
            if let Some(stem) = h.strip_suffix("_A")
                .or_else(|| h.strip_suffix("_B"))
                .or_else(|| h.strip_suffix("_C"))
                .or_else(|| h.strip_suffix("_D"))
            {
                families.entry(stem.to_string()).or_default().push(id);
            }
        }
        let multi: Vec<_> = families.values().filter(|v| v.len() > 1).collect();
        assert!(!multi.is_empty(), "no variant families found — the premise is gone");

        let groups_of = |id: &str| -> std::collections::HashSet<String> {
            dungeons[id]["spawn_info"]["enemy_spawn_groups"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        };
        for fam in &multi {
            for (i, a) in fam.iter().enumerate() {
                for b in fam.iter().skip(i + 1) {
                    let overlap: Vec<_> = groups_of(a).intersection(&groups_of(b)).cloned().collect();
                    assert!(
                        overlap.is_empty(),
                        "variants {a} and {b} share spawn group(s) {overlap:?} — a reported \
                         spawner would no longer identify one variant"
                    );
                }
            }
        }
    }

    /// THE CONTROL: the lookup must find a real group, and must refuse an unknown
    /// one. A function that returned `Some` for everything would satisfy the repair
    /// path while attributing the player to an arbitrary dungeon.
    #[test]
    fn the_owner_lookup_finds_real_groups_and_refuses_unknown_ones() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&p).unwrap();
        let gd: GameData = serde_json::from_str(&raw).expect("parsed.json loads as GameData");

        // A group we know exists: take one from any dungeon.
        let (want_dungeon, want_group) = gd
            .dungeons
            .iter()
            .find_map(|(id, d)| d.spawn_info.enemy_spawn_groups.keys().next().map(|g| (*id, *g)))
            .expect("some dungeon has an enemy spawn group");
        assert_eq!(
            dungeon_owning_spawn_group(&gd, &want_group),
            Some(want_dungeon),
            "a real spawn group must resolve to its own dungeon"
        );

        // And one that exists nowhere.
        assert_eq!(
            dungeon_owning_spawn_group(&gd, &Uuid::from_u128(0xDEADBEEF)),
            None,
            "an unknown group must not be attributed to any dungeon"
        );
    }
}

#[cfg(test)]
mod enemy_loot_tests {
    use super::*;

    /// Retail's gold currency.
    const GOLD_CURRENCY: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(path).expect("read parsed.json");
        serde_json::from_str(&raw).expect("parse game data")
    }

    fn all_spawn_groups(game_data: &GameData) -> Vec<(Uuid, Uuid)> {
        let mut out = Vec::new();
        for (dungeon_id, dungeon) in &game_data.dungeons {
            for group_id in dungeon.spawn_info.enemy_spawn_groups.keys() {
                out.push((*dungeon_id, *group_id));
            }
        }
        out.sort();
        out
    }

    /// Both corpora must actually be compiled in. Everything below is vacuous
    /// without them, and an `include_str!` that silently resolves to something
    /// else is exactly how the chest sidecar broke once.
    #[test]
    fn the_enemy_loot_corpora_load() {
        let tables = enemy_loot()["tables"].as_object().expect("loot tables");
        assert_eq!(tables.len(), 19, "the mined loot tables");
        assert_eq!(
            enemy_loot()["_meta"]["observations"].as_u64(),
            Some(49_602),
            "loot corpus observation count"
        );

        let groups = enemy_group_tables()["groups"].as_object().expect("groups");
        assert_eq!(groups.len(), 1066, "the mined spawn groups");
        assert_eq!(
            enemy_group_tables()["_meta"]["enemyResults"].as_u64(),
            Some(32_592),
            "group corpus observation count"
        );
    }

    /// Every spawn group in the group corpus must be a group parsed.json really
    /// has. A mined id that matches nothing would mean the mapping was keyed on
    /// the wrong field and every lookup silently falls through to the gold
    /// fallback -- which looks like working code.
    #[test]
    fn every_corpus_group_is_a_real_dungeon_spawn_group() {
        let game_data = game_data();
        let known: std::collections::HashSet<Uuid> = all_spawn_groups(&game_data)
            .into_iter()
            .map(|(_, g)| g)
            .collect();
        let mut unknown = Vec::new();
        for id in enemy_group_tables()["groups"].as_object().unwrap().keys() {
            let group: Uuid = id.parse().expect("group id is a uuid");
            if !known.contains(&group) {
                unknown.push(group);
            }
        }
        assert!(unknown.is_empty(), "corpus groups absent from parsed.json: {unknown:?}");
    }

    /// THE BUG (#152): every corpse in the game was empty.
    #[test]
    fn enemies_now_drop_things() {
        let game_data = game_data();
        let mut paid = 0;
        let mut total = 0;
        for (dungeon_id, group_id) in all_spawn_groups(&game_data) {
            let loot = roll_enemy_loot(&dungeon_id, &group_id, 0, 0, 20);
            total += 1;
            if loot.values().any(|l| {
                !l.currencies.is_empty() || !l.stackable_items.is_empty() || !l.item.is_empty()
            }) {
                paid += 1;
            }
        }
        assert!(total > 1_900, "expected every parsed.json spawn group, got {total}");
        assert!(
            paid * 2 > total,
            "only {paid} of {total} enemies dropped anything -- corpses are still mostly empty"
        );
    }

    /// Empty is a real outcome. A build where every enemy pays is as wrong as
    /// one where none do.
    #[test]
    fn an_empty_corpse_remains_possible() {
        let game_data = game_data();
        let groups = all_spawn_groups(&game_data);
        let mut empty = 0;
        let mut total = 0;
        for (dungeon_id, group_id) in &groups {
            for spawner in 0..6 {
                let loot = roll_enemy_loot(dungeon_id, group_id, spawner, 0, 20);
                total += 1;
                if loot.values().all(|l| {
                    l.currencies.is_empty() && l.stackable_items.is_empty() && l.item.is_empty()
                }) {
                    empty += 1;
                }
            }
        }
        assert!(empty > 0, "every one of {total} corpses paid out; retail's did not");
        assert!(empty < total, "not one of {total} corpses paid out");
    }

    /// THE LEVEL CONTROL. The gold table is 59% of all observations and its
    /// payout scales with enemy level (r=0.968 over 29,556 observations):
    /// weighted mean 5 gold at level 1, 343 at levels 89-100. A consumer that
    /// ignored `levelKeyed` and pooled the table would hand a level-1 enemy
    /// several hundred gold, which is both a broken economy and indistinguishable
    /// from working code without this test.
    #[test]
    fn gold_scales_with_enemy_level() {
        let game_data = game_data();
        let gold: Uuid = GOLD_CURRENCY.parse().unwrap();
        let groups = all_spawn_groups(&game_data);

        let mean_at = |level: i64| -> f64 {
            let mut sum = 0u64;
            let mut n = 0u64;
            for (dungeon_id, group_id) in &groups {
                for spawner in 0..4 {
                    let loot = roll_enemy_loot(dungeon_id, group_id, spawner, 0, level);
                    sum += loot.values().filter_map(|l| l.currencies.get(&gold)).sum::<u64>();
                    n += 1;
                }
            }
            sum as f64 / n as f64
        };

        let low = mean_at(1);
        let high = mean_at(90);
        assert!(low > 0.0, "level-1 enemies paid no gold at all");
        assert!(
            low < 25.0,
            "level-1 enemies averaged {low:.1} gold -- retail's weighted mean at level 1 is 5, so \
             the level bands are being ignored and the table drawn pooled"
        );
        assert!(
            high > 150.0,
            "level-90 enemies averaged only {high:.1} gold -- retail's is 343"
        );
        assert!(high > low * 10.0, "gold barely moved with level: {low:.1} -> {high:.1}");
    }

    /// A group the corpus never saw still pays, and pays only gold.
    #[test]
    fn an_unobserved_spawn_group_falls_back_to_gold_alone() {
        let game_data = game_data();
        let observed = enemy_group_tables()["groups"].as_object().unwrap();
        let unobserved: Vec<(Uuid, Uuid)> = all_spawn_groups(&game_data)
            .into_iter()
            .filter(|(_, g)| !observed.contains_key(&g.to_string()))
            .collect();
        assert!(
            unobserved.len() > 800,
            "expected the ~890 unobserved groups, got {}",
            unobserved.len()
        );

        let gold_table = Uuid::from_u128(GOLD_LOOT_TABLE_ID);
        let gold: Uuid = GOLD_CURRENCY.parse().unwrap();
        let mut paid = 0;
        for (dungeon_id, group_id) in unobserved.iter().take(200) {
            let loot = roll_enemy_loot(dungeon_id, group_id, 0, 0, 50);
            assert_eq!(
                loot.keys().collect::<Vec<_>>(),
                vec![&gold_table],
                "an unobserved group must roll the gold table and nothing else"
            );
            if loot[&gold_table].currencies.contains_key(&gold) {
                paid += 1;
            }
        }
        assert!(paid > 100, "only {paid} of 200 unobserved enemies paid gold");
    }

    /// Only tables the group was actually observed rolling may come back.
    #[test]
    fn a_drawn_table_is_one_the_group_was_observed_with() {
        let game_data = game_data();
        let corpus = enemy_group_tables()["groups"].as_object().unwrap();
        let mut checked = 0;
        for (dungeon_id, group_id) in all_spawn_groups(&game_data) {
            let Some(entry) = corpus.get(&group_id.to_string()) else {
                continue;
            };
            let allowed: std::collections::HashSet<String> = entry["sets"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|s| s["tables"].as_array().unwrap())
                .map(|t| t.as_str().unwrap().to_string())
                .collect();
            for spawner in 0..4 {
                for table in roll_enemy_loot(&dungeon_id, &group_id, spawner, 0, 30).keys() {
                    assert!(
                        allowed.contains(&table.to_string()),
                        "group {group_id} was never observed rolling {table}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 1_000, "only {checked} tables checked");
    }

    /// Gear must come out as real items, with an instance id that is unique
    /// across enemies and stable for one enemy. Retail minted a uuid4 per drop
    /// and the corpus strips it; replaying one id everywhere would give every
    /// player the same instance, which the arena ships to the opponent client.
    #[test]
    fn gear_drops_get_unique_stable_instance_ids() {
        let game_data = game_data();
        let mut ids = std::collections::HashSet::new();
        let mut items = 0;
        let mut collisions = 0;
        for (dungeon_id, group_id) in all_spawn_groups(&game_data) {
            for spawner in 0..4 {
                let loot = roll_enemy_loot(&dungeon_id, &group_id, spawner, 0, 60);
                // stable: the same enemy, described again, is the same item
                let again = roll_enemy_loot(&dungeon_id, &group_id, spawner, 0, 60);
                for (table, result) in &loot {
                    let repeat = &again[table];
                    assert_eq!(
                        result.item.0.keys().collect::<std::collections::HashSet<_>>(),
                        repeat.item.0.keys().collect::<std::collections::HashSet<_>>(),
                        "the same corpse was described with different instance ids"
                    );
                    for (id, item) in &result.item.0 {
                        items += 1;
                        if !ids.insert(*id) {
                            collisions += 1;
                        }
                        assert_eq!(id.get_version_num(), 4, "instance ids must look like uuid4");
                        assert!(!item.item_template_id.is_nil(), "gear with no template");
                    }
                }
            }
        }
        assert!(items > 50, "only {items} gear drops over the whole game -- expected more");
        assert_eq!(collisions, 0, "{collisions} of {items} gear drops shared an instance id");
    }

    /// The loot the client is told about is the loot the server will pay out:
    /// `generate_for_dungeon` must put it on every enemy, not just return it.
    #[test]
    fn generated_dungeons_carry_the_loot() {
        let game_data = game_data();
        let mut with_loot = 0;
        let mut enemies = 0;
        for dungeon_id in game_data.dungeons.keys() {
            let generated = generate_for_dungeon(&game_data, dungeon_id, 40, 10).unwrap();
            for spawners in generated.enemy_generated_data.values() {
                for spawner in spawners {
                    for enemy in spawner {
                        enemies += 1;
                        if !enemy.merged_loot_table().currencies.is_empty() {
                            with_loot += 1;
                        }
                    }
                }
            }
        }
        assert!(enemies > 2_000, "only {enemies} enemies generated");
        assert!(
            with_loot * 2 > enemies,
            "only {with_loot} of {enemies} generated enemies carry currency"
        );
    }
}

#[cfg(test)]
mod floor_pile_tests {
    use super::*;

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("read parsed.json"))
            .expect("parse game data")
    }

    /// The sidecar must be compiled in and hold what was mined. Parsed
    /// explicitly so a deserialization failure names itself rather than
    /// silently degrading to "every pile is one".
    #[test]
    fn the_pile_size_sidecar_loads() {
        let v: serde_json::Value = serde_json::from_str(FLOOR_PILE_SIZES_RAW)
            .expect("floor_pile_sizes.json must parse");
        let spawns = v["spawns"].as_object().expect("spawns object");
        assert_eq!(spawns.len(), 1285, "the mined floor spawn points");
        assert_eq!(v["_meta"]["dungeons"].as_u64(), Some(3033));
    }

    /// Every mined spawn must be a floor spawn parsed.json really has. An id
    /// that matches nothing would mean every lookup falls through to one result
    /// — which looks exactly like working code.
    #[test]
    fn every_mined_spawn_is_a_real_floor_spawn() {
        let game_data = game_data();
        let known: std::collections::HashSet<Uuid> = game_data
            .dungeons
            .values()
            .flat_map(|d| d.spawn_info.item.keys().copied())
            .collect();
        let v: serde_json::Value = serde_json::from_str(FLOOR_PILE_SIZES_RAW).unwrap();
        let mut unknown = Vec::new();
        for id in v["spawns"].as_object().unwrap().keys() {
            let spawn: Uuid = id.parse().expect("spawn id is a uuid");
            if !known.contains(&spawn) {
                unknown.push(spawn);
            }
        }
        assert!(unknown.is_empty(), "mined spawns absent from parsed.json: {unknown:?}");
    }

    /// THE BUG: every floor pile held exactly one result, where retail put more
    /// on 31% of them.
    #[test]
    fn floor_piles_can_hold_more_than_one_result() {
        let game_data = game_data();
        let mut piles = 0;
        let mut multi = 0;
        let mut biggest = 0;
        for dungeon_id in game_data.dungeons.keys() {
            let generated = generate_for_dungeon(&game_data, dungeon_id, 20, 5).unwrap();
            for results in generated.item_generated_data.values() {
                piles += 1;
                biggest = biggest.max(results.len());
                if results.len() > 1 {
                    multi += 1;
                }
            }
        }
        assert!(piles > 1_900, "only {piles} floor piles generated");
        // 97 of the 1,285 mined spawn POINTS hold more than one result. That is
        // 7.6% of spawn points but 31.1% of spawn OBSERVATIONS, because the
        // multi-result spawns sit in dungeons players ran far more often — the
        // seven-result one alone was observed 1,895 times. Both numbers are
        // true; this asserts the per-spawn one, which is what generation sees.
        assert!(
            multi >= 90,
            "only {multi} of {piles} piles hold more than one result; 97 spawn points do"
        );
        assert!(biggest >= 7, "the biggest pile generated held {biggest}; retail's holds 24");
    }

    /// A pile of seven must be seven DRAWS, not one draw repeated — otherwise
    /// the count is right and the loot is still a single stack.
    #[test]
    fn the_results_in_one_pile_are_rolled_separately() {
        let game_data = game_data();
        let mut checked = 0;
        let mut differed = 0;
        for dungeon_id in game_data.dungeons.keys() {
            let generated = generate_for_dungeon(&game_data, dungeon_id, 20, 5).unwrap();
            for results in generated.item_generated_data.values() {
                if results.len() < 2 {
                    continue;
                }
                checked += 1;
                let first = serde_json::to_string(&results[0].loot_table_loot).unwrap();
                if results
                    .iter()
                    .any(|r| serde_json::to_string(&r.loot_table_loot).unwrap() != first)
                {
                    differed += 1;
                }
            }
        }
        assert!(checked >= 90, "only {checked} multi-result piles to check");
        assert!(
            differed * 2 > checked,
            "only {differed} of {checked} multi-result piles hold differing draws — \
             the pile is one result repeated"
        );
    }

    /// A spawn the corpus never saw keeps exactly one result. The size is never
    /// invented for an unobserved spawn.
    #[test]
    fn an_unobserved_spawn_still_holds_one_result() {
        assert_eq!(floor_pile_size(&Uuid::from_u128(0xDEAD_BEEF)), 1);
    }

    /// The same pile, described twice, must be identical — the client is told
    /// once what is on the floor and must still find it there.
    ///
    /// Compared per spawn, NOT by serialising the two maps and diffing the
    /// strings: `item_generated_data` is a `HashMap`, whose iteration order is
    /// randomised per process, so the string comparison passes locally and fails
    /// in CI for a reason that has nothing to do with stability. It did exactly
    /// that once.
    #[test]
    fn a_pile_is_stable_between_descriptions() {
        // Over EVERY dungeon, not `dungeons.keys().next()`: that picks an
        // arbitrary entry out of a HashMap and most dungeons have no floor
        // items at all, so the test compared nothing and failed its own guard
        // at random. It did exactly that before this comment existed.
        let game_data = game_data();
        let mut compared = 0;
        for dungeon_id in game_data.dungeons.keys() {
            let a = generate_for_dungeon(&game_data, dungeon_id, 20, 5).unwrap();
            let b = generate_for_dungeon(&game_data, dungeon_id, 20, 5).unwrap();
            assert_eq!(a.item_generated_data.len(), b.item_generated_data.len());
            for (spawn, first) in &a.item_generated_data {
                let second = b
                    .item_generated_data
                    .get(spawn)
                    .unwrap_or_else(|| panic!("spawn {spawn} vanished between descriptions"));
                assert_eq!(first.len(), second.len(), "pile size moved for spawn {spawn}");
                for (x, y) in first.iter().zip(second.iter()) {
                    assert_eq!(
                        serde_json::to_value(&x.loot_table_loot).unwrap(),
                        serde_json::to_value(&y.loot_table_loot).unwrap(),
                        "contents moved for spawn {spawn}"
                    );
                    compared += 1;
                }
            }
        }
        assert!(compared > 1_000, "only {compared} floor results compared");
    }
}
