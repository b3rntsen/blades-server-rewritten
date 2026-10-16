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

// WHICH tables hang on each floor spawn, also measured rather than derived.
//
// A spawn's tables came from its interactable in parsed.json, and that mapping
// is the APK's, not retail's. Retail sent the tables per SPAWN POINT, and on
// 700 of the 1,285 spawns in the corpus it named at least one table the
// interactable does not. Most of that is town-resource loot -- but one is
// 8858f284, a table whose 269 observed draws are 269 DoorKeys, which retail
// hung on three of the sixty pots in The Wizard's Challenge. Without it the key
// never drops and the locked door cannot open (#192).
//
// Unioned with the interactable's own tables rather than replacing them,
// because the gap is one-directional: 700 spawns gain a table, 6 would lose
// one. A spawn absent from this file keeps exactly the tables it had.
static SPAWN_LOOT_TABLES_RAW: &str = include_str!("../spawn_loot_tables.json");

// The same floor-loot results, banded by the ENEMY LEVEL of the dungeon they were
// rolled for. `interactable_loot.json` pools every level, so a level-2 pile could
// draw what retail only ever rolled at level 80: every new player got an Ebony
// Ingot in the tutorial (tracker #8), where retail rolled that table 954 times at
// levels 1-10 and never once produced one (its first is at level 42).
//
// Only tables with enough observations for two bands are here; the rest are
// still drawn pooled. Built by `script/mine_interactable_loot_by_level.py`.
static INTERACTABLE_LOOT_BY_LEVEL_RAW: &str = include_str!("../interactable_loot_by_level.json");

// How many containers the client places for each item spawn group: the APK's
// `_spawnGroupsItem[]._quantity`, which parsed.json dropped. It is what a spawn
// retail was never captured on falls back to (see `floor_pile_size`). Built by
// `script/extract_item_spawn_quantities.py`; compiled in, so a merge ships it.
static ITEM_SPAWN_QUANTITIES_RAW: &str = include_str!("../item_spawn_quantities.json");

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
    /// The APK `ChestCycle` this spawn draws its tier from, when it has one.
    #[serde(default, rename = "chestCycle")]
    chest_cycle: Option<Uuid>,
}

/// One `ChestCycle.AdditionalChestRule`: `chests` positions in `start..=end` of
/// every pass through the cycle hold `rarity` instead of the base chest.
struct CycleRule {
    chests: usize,
    rarity: i64,
    start: u64,
    end: u64,
}

/// A shipped `ChestCycle` whose base `_rarityList` is one rarity throughout.
struct ChestCycle {
    id: Uuid,
    length: u64,
    base: i64,
    rules: &'static [CycleRule],
}

/// The two chest cycles a dungeon spawn names, from the APK's `ChestCycleData`
/// (both are the job chests in `JobSpawnGroupsReference`; no other spawn of the
/// 292 names one). Retail's job chests followed them: over 463 distinct captured
/// jobs the main chest was wooden 412, silver 35, gold 16, and the secret-room
/// chest silver 420, gold 40, Elder 3. We sent the base rarity, wooden and
/// silver, every time, so a job could never hold a gold or Elder chest (#337).
const CHEST_CYCLES: &[ChestCycle] = &[
    // "New Jobs Chest Cycle (Non-secret)": 100 x wooden; per pass one gold in each
    // quarter, ten silver anywhere -- 86 / 10 / 4 by construction.
    ChestCycle {
        id: Uuid::from_u128(0x5b20edcb_ed53_44fe_805a_20a02fa3183e),
        length: 100,
        base: 1,
        rules: &[
            CycleRule { chests: 1, rarity: 3, start: 2, end: 24 },
            CycleRule { chests: 1, rarity: 3, start: 26, end: 49 },
            CycleRule { chests: 1, rarity: 3, start: 51, end: 74 },
            CycleRule { chests: 1, rarity: 3, start: 76, end: 99 },
            CycleRule { chests: 10, rarity: 2, start: 2, end: 99 },
        ],
    },
    // "Job Secrets Chest Cycle": 500 x silver; per pass three Elder and fifty gold.
    ChestCycle {
        id: Uuid::from_u128(0xe814bc37_93eb_4480_817f_115edfb8ef11),
        length: 500,
        base: 2,
        rules: &[
            CycleRule { chests: 1, rarity: 4, start: 50, end: 125 },
            CycleRule { chests: 1, rarity: 4, start: 175, end: 300 },
            CycleRule { chests: 1, rarity: 4, start: 350, end: 475 },
            CycleRule { chests: 10, rarity: 3, start: 5, end: 100 },
            CycleRule { chests: 10, rarity: 3, start: 101, end: 200 },
            CycleRule { chests: 10, rarity: 3, start: 201, end: 300 },
            CycleRule { chests: 10, rarity: 3, start: 301, end: 400 },
            CycleRule { chests: 10, rarity: 3, start: 401, end: 499 },
        ],
    },
];

/// The tier a cycle gives the chest at a hashed place in it.
///
/// Retail kept a position per player and placed each rule's chests at random
/// in its window; its `allowRepeatSecs` cooldowns need that state, which we do
/// not keep. So the pass and the position are derived from the run instead:
/// the rarities come out in the proportions the cycle is built from, and the
/// same run always describes the same chest. The first rule listed wins a
/// position two rules picked.
fn cycle_tier(cycle: &ChestCycle, ctx: LootSeedContext, spawn_id: &Uuid, index: usize) -> i64 {
    let h = loot_seed(ctx, spawn_id, &cycle.id, index);
    let position = h % cycle.length;
    let pass = h / cycle.length;
    for (r, rule) in cycle.rules.iter().enumerate() {
        let mut rng = LootRng(mix64(pass ^ (r as u64).rotate_left(48) ^ cycle.id.as_u128() as u64));
        let width = rule.end - rule.start + 1;
        let mut placed: Vec<u64> = Vec::with_capacity(rule.chests);
        while placed.len() < rule.chests.min(width as usize) {
            let p = rule.start + rng.next() % width;
            if !placed.contains(&p) {
                placed.push(p);
            }
        }
        if placed.contains(&position) {
            return rule.rarity;
        }
    }
    cycle.base
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

fn item_spawn_quantities() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(ITEM_SPAWN_QUANTITIES_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "groups": {} }))
    })
}

/// How many results this floor spawn holds: what retail was observed sending,
/// else how many containers the APK places for it, else one.
///
/// The APK count is not a guess at retail: it equals the retail count on 1,283
/// of the 1,285 captured spawns, and the two exceptions are job-reference groups
/// that are captured, so the retail value wins there. Without it a spawn retail
/// was never captured on got ONE result however many containers the client put
/// down: the 7 / 3 / 1 breakables of EQ30, EQ40 and seven other uncaptured
/// event dungeons came out 1 / 1 / 1, and eight of eleven containers opened
/// empty (#353, #358, #365).
fn floor_pile_size(spawn_id: &Uuid) -> usize {
    let key = spawn_id.to_string();
    floor_pile_sizes()
        .get("spawns")
        .and_then(|s| s.get(&key))
        .and_then(|e| e.get("results"))
        .and_then(|n| n.as_u64())
        .or_else(|| item_spawn_quantities()["groups"].get(&key).and_then(|n| n.as_u64()))
        .unwrap_or(1)
        .max(1) as usize
}

fn spawn_loot_tables() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(SPAWN_LOOT_TABLES_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "spawns": {} }))
    })
}

/// The loot tables this floor spawn carries: what retail was observed sending,
/// unioned with what the spawn's interactable names.
///
/// Order is deterministic (the interactable's first, then the mined additions in
/// corpus order) and each table is rolled under its own seed, so adding one
/// never moves the result of another.
fn tables_for_spawn(spawn_id: &Uuid, from_interactable: &[Uuid]) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = from_interactable.to_vec();
    let mined = spawn_loot_tables()
        .get("spawns")
        .and_then(|s| s.get(spawn_id.to_string()))
        .and_then(|e| e.get("tables"))
        .and_then(|t| t.as_array());
    for t in mined.into_iter().flatten() {
        if let Some(uuid) = t.as_str().and_then(|s| Uuid::parse_str(s).ok()) {
            if !out.contains(&uuid) {
                out.push(uuid);
            }
        }
    }
    out
}

fn interactable_loot_by_level() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        // Malformed degrades to "no level-keyed tables": every table is drawn
        // pooled, which is the behaviour that preceded this file.
        serde_json::from_str(INTERACTABLE_LOOT_BY_LEVEL_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "tables": {} }))
    })
}

/// The results one floor table draws from at `enemy_level`: the retail results
/// rolled at the levels nearest it when retail was observed often enough to
/// band the table, otherwise the pooled table.
fn interactable_table_results(table_id: &Uuid, enemy_level: i64) -> Option<Vec<&'static serde_json::Value>> {
    let key = table_id.to_string();
    let by_level = interactable_loot_by_level();
    if let Some(entry) = by_level["tables"].get(&key) {
        let window = by_level["_meta"]["windowObservations"].as_u64().unwrap_or(0);
        return floor_window_results(entry, enemy_level, window);
    }
    interactable_loot()
        .get("tables")
        .and_then(|t| t.get(&key))
        .and_then(|e| e.get("results"))
        .and_then(|r| r.as_array())
        .map(|r| r.iter().collect())
}

/// The results of the level bands nearest `enemy_level`, widened one band at a
/// time until they hold at least `window` observations (tracker #374).
///
/// Starts at the band holding the level, or the nearest one, as
/// [`enemy_table_results`] does, then adds whichever neighbouring band is closer
/// in level -- the lower one on a tie -- so the window stays centred on the
/// player instead of being anchored where the table's data happens to start.
///
/// The bands it replaced were greedy runs of 300 observations from level 1 up.
/// Retail rolled its medium and large containers too rarely to fill many of
/// those, so they came out as [41, 87] (49173c2c) and [29, 87] (6cb97b90): a
/// level-43 and a level-78 character drew from the same results, and the large
/// container paid both about 1,080 gold where retail paid level-40-43 characters
/// 886 and level-100 ones 1,278. With one band per level and a 200-observation
/// window those two draws come from levels 24-58 and 70-85, and pay 778 and 1,224.
///
/// `window == 0` is exactly the old single-band lookup.
fn floor_window_results(
    entry: &'static serde_json::Value,
    enemy_level: i64,
    window: u64,
) -> Option<Vec<&'static serde_json::Value>> {
    if entry["levelKeyed"].as_bool() != Some(true) {
        return entry["results"].as_array().map(|r| r.iter().collect());
    }
    let mut bands: Vec<(i64, i64, &'static serde_json::Value)> = entry["byLevel"]
        .as_array()?
        .iter()
        .map(|b| {
            (
                b["minEnemyLevel"].as_i64().unwrap_or(i64::MIN),
                b["maxEnemyLevel"].as_i64().unwrap_or(i64::MAX),
                b,
            )
        })
        .collect();
    if bands.is_empty() {
        return None;
    }
    bands.sort_by_key(|(lo, _, _)| *lo);
    let distance = |(lo, hi, _): &(i64, i64, &serde_json::Value)| {
        if enemy_level < *lo {
            lo.saturating_sub(enemy_level)
        } else if enemy_level > *hi {
            enemy_level.saturating_sub(*hi)
        } else {
            0
        }
    };
    // The first band at the smallest distance: the containing one, else the
    // nearest, the lower of two equally near.
    let start = (0..bands.len()).min_by_key(|&i| (distance(&bands[i]), i))?;
    let observations = |b: &serde_json::Value| b["observations"].as_u64().unwrap_or(0);
    let (mut lo, mut hi) = (start, start);
    let mut held = observations(bands[start].2);
    while held < window && (lo > 0 || hi + 1 < bands.len()) {
        let below = (lo > 0).then(|| distance(&bands[lo - 1]));
        let above = (hi + 1 < bands.len()).then(|| distance(&bands[hi + 1]));
        let take_below = match (below, above) {
            (Some(b), Some(a)) => b <= a,
            (Some(_), None) => true,
            _ => false,
        };
        if take_below {
            lo -= 1;
            held += observations(bands[lo].2);
        } else {
            hi += 1;
            held += observations(bands[hi].2);
        }
    }
    Some(
        bands[lo..=hi]
            .iter()
            .filter_map(|(_, _, b)| b["results"].as_array())
            .flatten()
            .collect(),
    )
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

#[derive(Clone, Copy)]
struct LootSeedContext {
    dungeon_uuid: Uuid,
    run_seed: u64,
    /// An Abyss floor: its enemies roll the Abyss corpus ([`abyss_enemy_loot`]).
    abyss: bool,
    /// The character level an Abyss floor's bonus material is banded to
    /// ([`abyss_bonus_material`], #383). `None`: no bonus (every non-Abyss floor,
    /// and an Abyss run started before the bonus existed).
    bonus_level: Option<i64>,
    /// How many levels above each enemy's (and the dungeon's) own level its loot is
    /// rolled: [`crate::features::loot_level::dungeon_loot_shift`] for a character
    /// paid as level 100 (#398), zero otherwise. Never part of the seed, so a shift
    /// changes which band a draw reads and nothing else.
    loot_shift: i64,
}

fn default_seed_context(dungeon_uuid: &Uuid) -> LootSeedContext {
    LootSeedContext {
        dungeon_uuid: *dungeon_uuid,
        run_seed: 0,
        abyss: false,
        bonus_level: None,
        loot_shift: 0,
    }
}

fn seeded_context(dungeon_uuid: &Uuid, run_seed: u64) -> LootSeedContext {
    LootSeedContext {
        dungeon_uuid: *dungeon_uuid,
        run_seed,
        abyss: false,
        bonus_level: None,
        loot_shift: 0,
    }
}

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Stable loot seed for one run of a repeatable quest instance.
pub fn run_loot_seed(instance_id: &Uuid, run_index: u64) -> u64 {
    mix64(
        (instance_id.as_u128() as u64)
            ^ ((instance_id.as_u128() >> 64) as u64).rotate_left(17)
            ^ run_index.wrapping_mul(0x9E37_79B9_7F4A_7C15),
    )
}

/// Deterministic per (dungeon, spawn, table) so a re-fetch of the same dungeon
/// yields the same contents -- the client is told once what a barrel holds and
/// must still find it there when it breaks it.
fn loot_seed(ctx: LootSeedContext, spawn_id: &Uuid, table_id: &Uuid, result_index: usize) -> u64 {
    let dungeon_uuid = &ctx.dungeon_uuid;
    let x = dungeon_uuid.as_u128() as u64
        ^ (spawn_id.as_u128() as u64).rotate_left(21)
        ^ (table_id.as_u128() as u64).rotate_left(42)
        ^ ctx.run_seed.rotate_left(7)
        // Without this every result in a pile of seven would be the same draw,
        // which turns "seven results" into "one stack, seven times".
        ^ (result_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mix64(x)
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
///
/// `enemy_level` picks the level band where the table has one (see
/// `INTERACTABLE_LOOT_BY_LEVEL_RAW`); the seed does not depend on it.
fn roll_loot_table_with_context(
    ctx: LootSeedContext,
    spawn_id: &Uuid,
    table_id: &Uuid,
    result_index: usize,
    enemy_level: i64,
) -> LootTableResult {
    let mut out = LootTableResult::default();
    let Some(results) = interactable_table_results(table_id, enemy_level + ctx.loot_shift) else {
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

    let mut pick = loot_seed(ctx, spawn_id, table_id, result_index) % total;
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

/// One container's roll of a floor table: retail's draw, except that a breakable
/// outside the Abyss holds exactly its size class's number of loot stacks
/// ([`crate::features::beginner_economy::container_stacks`]: 2 / 4 / 6).
///
/// Owner-approved deviation from retail, #388, 2026-10-16. The client places the
/// containers (the APK's spawn points), so the server cannot add one; it fills
/// those it has. The first draw is retail's whole result, as before, its gold
/// included. Further draws of its size's breakable table (the same table, but for
/// a job's, which pays one kind only), each under its own seed, add stacks of
/// kinds the container does not hold yet until it holds its number; a draw
/// with more stacks than the number keeps the first ones in id order. Gold is
/// only ever the first draw's, so a container's coin stays retail's.
fn roll_container_table(
    ctx: LootSeedContext,
    spawn_id: &Uuid,
    group_name: Option<&str>,
    table_id: &Uuid,
    result_index: usize,
    enemy_level: i64,
) -> LootTableResult {
    use crate::features::beginner_economy::{container_class, CONTAINER_FILL_MAX_DRAWS};

    let mut out = roll_loot_table_with_context(ctx, spawn_id, table_id, result_index, enemy_level);
    // Crafting materials come in 5, 10 or 20 (owner-approved deviation, #388).
    let material_seed = loot_seed(ctx, spawn_id, table_id, result_index) ^ 0x388_C047_A1E5;
    let Some(class) = container_class(table_id, group_name).filter(|_| !ctx.abyss) else {
        crate::features::beginner_economy::size_material_stacks(&mut out.stackable_items, material_seed);
        return out;
    };
    let want = class.stacks;
    let sorted = |stacks: &HashMap<Uuid, u64>| {
        let mut v: Vec<(Uuid, u64)> = stacks.iter().map(|(k, q)| (*k, *q)).collect();
        v.sort_unstable();
        v
    };
    if out.stackable_items.len() > want {
        out.stackable_items = sorted(&out.stackable_items).into_iter().take(want).collect();
    }
    for draw in 1..=CONTAINER_FILL_MAX_DRAWS {
        if out.stackable_items.len() >= want {
            break;
        }
        // Far past any pile's own indices, so a fill draw is never another
        // container's draw.
        let index = result_index + (draw << 20);
        let extra = roll_loot_table_with_context(ctx, spawn_id, &class.fill_table, index, enemy_level);
        for (id, q) in sorted(&extra.stackable_items) {
            if out.stackable_items.len() < want {
                out.stackable_items.entry(id).or_insert(q);
            }
        }
    }
    crate::features::beginner_economy::size_material_stacks(&mut out.stackable_items, material_seed);
    out
}

#[cfg(test)]
fn roll_loot_table(
    dungeon_uuid: &Uuid,
    spawn_id: &Uuid,
    table_id: &Uuid,
    result_index: usize,
    enemy_level: i64,
) -> LootTableResult {
    roll_loot_table_with_context(
        default_seed_context(dungeon_uuid),
        spawn_id,
        table_id,
        result_index,
        enemy_level,
    )
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

// Abyss corpses (report #381). The Abyss sends its floors as
// `abyssDungeonGeneratedData`, which the two corpora above were never mined
// from, so no abyss spawn group was in `enemy_group_tables.json` and every abyss
// enemy fell back to the general table 871c2e9b -- Pearl and Garlic on shallow
// floors, Quicksilver as the only ingot at enemy level 40-44, Nightshade at
// 46-50, all of it the same for every run of a floor.
//
// Retail's abyss enemies are the APK's "... Family Abyss" families, and every one
// rolls 2d366ee0 (some add 18bafcb5 or 50cd3882), never 871c2e9b
// (reference/game-defs/enemies.json). Over 80 captured retail abyss floors,
// 2d366ee0 paid gold and nothing else (189 of 189, ~2 gold per enemy level),
// 18bafcb5 one Atronite 5 times in 31, and 50cd3882 a Dragon Claw or Scale
// stack. Built by `script/mine_abyss_enemy_loot.py`.
static ABYSS_ENEMY_LOOT_RAW: &str = include_str!("../abyss_enemy_loot.json");

fn abyss_enemy_loot() -> &'static serde_json::Value {
    static TABLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(ABYSS_ENEMY_LOOT_RAW)
            .unwrap_or_else(|_| serde_json::json!({ "groups": {}, "pooledSets": [], "tables": {} }))
    })
}

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
    ctx: LootSeedContext,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
) -> LootRng {
    let dungeon_uuid = &ctx.dungeon_uuid;
    LootRng(
        (dungeon_uuid.as_u128() as u64)
            ^ (spawn_group_id.as_u128() as u64).rotate_left(21)
            ^ ((dungeon_uuid.as_u128() >> 64) as u64).rotate_left(11)
            ^ ctx.run_seed.rotate_left(29)
            ^ (spawner_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (enemy_index as u64).rotate_left(37),
    )
}

/// Draw one entry from an array of `{..., "n": weight}` observations.
fn draw_weighted<'a, V: std::borrow::Borrow<serde_json::Value>>(
    rng: &mut LootRng,
    options: &'a [V],
) -> Option<&'a serde_json::Value> {
    let total: u64 = options.iter().filter_map(|o| o.borrow()["n"].as_u64()).sum();
    if total == 0 {
        return None;
    }
    let mut pick = rng.next() % total;
    for o in options {
        let o = o.borrow();
        let n = o["n"].as_u64().unwrap_or(0);
        if pick < n {
            return Some(o);
        }
        pick -= n;
    }
    options.last().map(|o| o.borrow())
}

/// The results to draw from for one table at one enemy level.
///
/// Shared by enemy loot and the level-banded floor loot, which use the same shape.
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
    roll_enemy_loot_with_context(
        default_seed_context(dungeon_uuid),
        spawn_group_id,
        spawner_index,
        enemy_index,
        enemy_level,
    )
}

fn roll_enemy_loot_with_context(
    ctx: LootSeedContext,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
    enemy_level: i64,
) -> HashMap<Uuid, LootTableResult> {
    let mut rng = enemy_rng(ctx, spawn_group_id, spawner_index, enemy_index);
    let mut out = HashMap::new();
    // The enemy keeps its level; its loot is read at the level-100 character's (#398).
    let enemy_level = enemy_level + ctx.loot_shift;

    let group = if ctx.abyss {
        // An abyss group retail never showed us rolls what abyss enemies rolled
        // as a whole -- the abyss gold table, not the general one.
        abyss_enemy_loot()["groups"]
            .get(spawn_group_id.to_string())
            .map(|g| &g["sets"])
            .or(Some(&abyss_enemy_loot()["pooledSets"]))
    } else {
        enemy_group_tables()["groups"].get(spawn_group_id.to_string()).map(|g| &g["sets"])
    };
    let tables: Vec<Uuid> = match group.and_then(|sets| sets.as_array()) {
        Some(sets) => draw_weighted(&mut rng, sets)
            .and_then(|s| s["tables"].as_array())
            .map(|ts| ts.iter().filter_map(|t| t.as_str()?.parse().ok()).collect())
            .unwrap_or_default(),
        // Never observed: see GOLD_LOOT_TABLE_ID.
        None => vec![Uuid::from_u128(GOLD_LOOT_TABLE_ID)],
    };

    for table_id in tables {
        let abyss_table = ctx
            .abyss
            .then(|| abyss_enemy_loot()["tables"].get(table_id.to_string()))
            .flatten();
        let results: Option<Vec<&serde_json::Value>> = match abyss_table
            .or_else(|| enemy_loot()["tables"].get(table_id.to_string()))
        {
            Some(entry) => enemy_table_results(entry, enemy_level).map(|r| r.iter().collect()),
            // A table left out of the ENEMY corpus as too thin to model. The
            // floor-loot corpus may still know it: 8858f284 is the DoorKey table,
            // 25 enemy observations (too few to keep) but 269 of 269 DoorKeys on
            // the pots it also hangs on. Sending it empty meant the key-holder
            // in EQ13 (101b2285) and the three in SQ205_B never dropped their key,
            // and the door behind them never opened (report #236).
            None => interactable_table_results(&table_id, enemy_level),
        };
        let mut result = LootTableResult::default();
        // Known to neither corpus: retail keyed it on this enemy, so the key is
        // still sent, empty -- dropping the key would be a shape retail never sent.
        if let Some(results) = results {
            if let Some(drawn) = draw_weighted(&mut rng, &results) {
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
                    if let Ok(mut item) = serde_json::from_value::<Item>(raw.clone()) {
                        let key = rng.uuid();
                        // A ring or necklace is graded afresh by the one jewellery
                        // rule (#393: 8 / 17 / 75% one / two / three properties,
                        // "überall"); the recorded instance's grade was retail's
                        // roll, 34 / 30 / 36% over the corpus. Seeded from the
                        // key so no other draw on this corpse moves.
                        let seed = (key.as_u128() as u64) ^ ((key.as_u128() >> 64) as u64);
                        crate::features::legendary_gear::regrade_jewelry(&mut item, seed);
                        result.item.0.insert(key, item);
                    }
                }
            }
        }
        out.insert(table_id, result);
    }

    // #383: the bonus stack, under the table it was drawn from. 871c2e9b is the
    // key every Abyss corpse carried before #381, so the client already reads it
    // on an Abyss enemy.
    // #388: every crafting material on the corpse in 5, 10 or 20 (owner-approved
    // deviation from retail), before the bonus joins it.
    size_corpse_materials(ctx, spawn_group_id, spawner_index, enemy_index, &mut out);
    if let Some((item, qty)) = roll_abyss_bonus(ctx, spawn_group_id, spawner_index, enemy_index)
        .map(|drawn| bonus_as_paid(ctx, spawn_group_id, spawner_index, enemy_index, drawn))
    {
        *out.entry(Uuid::from_u128(GOLD_LOOT_TABLE_ID))
            .or_default()
            .stackable_items
            .entry(item)
            .or_default() += qty;
    }

    out
}

// -- Abyss bonus material (#383) ---------------------------------------------
//
// NOT RETAIL. An owner-approved deviation (report #383, 2026-10-13), requested by
// HauDrauf: most players lost their retail characters and restart from scratch,
// and #381 (fork #509) took the Abyss back to retail's corpse loot -- gold, and a
// stackable on ~4% of corpses. On top of that retail loot, a share of Abyss
// corpses now also drop ONE material stack, drawn from the retail dungeon enemy
// table (871c2e9b) at the CHARACTER's level rather than the floor's, so a
// level-100 character finds level-100 materials (no Pearl, no Garlic) and a
// level-20 one level-20 ones. Stackables only: the table's gear results (the
// dragon weapons and armour HauDrauf asked to keep out) are never read.

/// Share of Abyss corpses that drop a bonus material stack on top of retail's
/// Abyss loot. Owner-approved deviation from retail (#383, 2026-10-13).
///
/// Tuned to "like in normal quests" (HauDrauf, #383, after 23 runs): our own
/// generators put a material on 54.4% of normal-quest corpses (town jobs and
/// story quests, character levels 7/20/45/100: 56.7/54.5/52.8/53.7%), retail's
/// own table paying a stackable on about half its results at every level.
/// Retail's Abyss loot already carries a material on 2-5% of corpses (#381's
/// Atronite), so 54% here puts an Abyss corpse at about 55%, within 3 points of
/// normal quests at each of those levels. Was 0.18.
pub const ABYSS_BONUS_MATERIAL_SHARE: f64 = 0.54;

/// Share of those bonus stacks that are an enchanting ingredient
/// ([`ENCHANTING_INGREDIENTS`]); the rest are any other material the table pays
/// at the character's level. The owner also decided to raise enchanting
/// ingredients -- retail's table pays them in ~13% of its stacks. Owner-approved
/// deviation from retail (#383, 2026-10-13).
pub const ABYSS_BONUS_ENCHANTING_SHARE: f64 = 1.0 / 3.0;

/// The bonus draws from the table's level bands within this many levels of the
/// character's level. A single band is thin (level 85 saw one enchanting
/// ingredient, so ±0 would always be Fire Salts); ±2 keeps the draw to what
/// retail paid at that level. With nothing in the window, the nearest band that
/// has a candidate is used.
const ABYSS_BONUS_LEVEL_WINDOW: i64 = 2;

/// The previous name of [`EFFECT_INGREDIENTS`]; the same set.
pub const ENCHANTING_INGREDIENTS: [u128; 7] = EFFECT_INGREDIENTS;

/// The effect (enchanting) ingredients: retail's own Sigil-shop
/// "EffectIngredients" group -- the seven `SigilShop_EffectIngredients_*`
/// offers, whose `AllIngredients` bundle is exactly these seven templates
/// (`deploy/static/global_shop_offer_contents.json`). HauDrauf asked that they be
/// "these and treated the same" (#383, 2026-10-13), so the Abyss bonus draws them
/// uniformly ([`effect_ingredient_candidates`]).
///
/// `8b7d0044` was labelled "Beehive Husk" here (and still is in
/// `ingredient_values.json`); the game data names it Deathbell (`parsed.json`
/// `Items.Name.Deathbell`, and the Sigil shop's Deathbell offer pays it). The
/// client has a "Beehive Husk" string but no item template, so there is no
/// eighth ingredient to keep or remove -- pending HauDrauf's answer on #383,
/// nothing was dropped.
pub const EFFECT_INGREDIENTS: [u128; 7] = [
    0x05b4dd6b_796f_4088_a772_0e33ea3db976, // Frost Salts
    0xf1c5c03c_5297_48fb_a9d2_5f32bffb467c, // Fire Salts
    0x10c54e43_a2a1_4833_9491_4c19149f43b5, // Void Salts
    0x9687d83c_aa7b_4cf3_a69f_0bba8204fa61, // Glow Dust
    0x07f380dd_f123_4ef7_9b12_8b9fdb03c3e1, // Imp Stool
    0x7116a2a8_ac2d_4cd9_8b7c_b80c397d3f50, // Honeycomb
    0x8b7d0044_3a38_4ee0_af45_d2892b0f508d, // Deathbell
];

/// How heavily a town material ([`TOWN_MATERIALS`]) weighs in the bonus draw,
/// against how often retail's table paid it. Owner-approved tuning (#383,
/// 2026-10-13): HauDrauf, after 23 runs, "there have been too many building
/// materials from the start; you can lower them a bit". Retail's table pays
/// town materials in 63-65% of its non-enchanting stacks at every level; at half
/// weight they are 46-48% of them (the odds against every other material halve).
pub const ABYSS_BONUS_TOWN_MATERIAL_WEIGHT: f64 = 0.5;

/// The materials whose only use is the town, as the game data classifies them:
/// the APK's "Building Material" tag (`Item.TownResource.*`, the inputs of town
/// building construction) and "Decoration Material" tag
/// (`Item.DecorationResource.*`, the inputs of decoration crafting and of the
/// Stone and Castle building styles). None is an input of smithing, tempering,
/// enchanting or repair. Animal Hide is not one: it is a smithing material.
pub const TOWN_MATERIALS: [u128; 9] = [
    0xe7193116_d761_479b_8a20_5633737977f5, // Lumber
    0xfd67bbc6_20f4_44a3_9614_28265ebb8c67, // Limestone
    0x42d91529_c88b_4c5b_815b_b55508b4e7ef, // Clay
    0x61d5e5c7_78b8_4fc3_ba6a_5b7c2a15866d, // Marble
    0x38d32048_ce01_4390_a4f0_cdb94ef3ce72, // Bronze
    0xb94c2028_dba7_44bd_b6f9_6f85ab0195b6, // Seed
    0x8ac9076c_cba9_4cf4_a8a5_d2303aab22b0, // Crystal
    0x574ab81c_1ba1_4127_a4aa_e9cbba5767ad, // Fabric
    0x6bd960ca_9af3_4b33_9ec0_5c06d50ad43e, // Dye
];

/// A candidate's weight in the bonus draw: how often retail paid it, times
/// [`ABYSS_BONUS_TOWN_MATERIAL_WEIGHT`] for a town material.
pub fn abyss_bonus_weight(candidate: &BonusCandidate) -> f64 {
    let scale = if TOWN_MATERIALS.contains(&candidate.0.as_u128()) {
        ABYSS_BONUS_TOWN_MATERIAL_WEIGHT
    } else {
        1.0
    };
    candidate.2 as f64 * scale
}

/// Highest character level the candidate lists are built for; above it, clamps.
const ABYSS_BONUS_MAX_LEVEL: i64 = 100;

/// One candidate stack: (item, quantity retail paid it in, times observed).
pub type BonusCandidate = (Uuid, u64, u64);

/// The bonus candidates at `level`: `[other materials, enchanting ingredients]`,
/// each with how often retail's table paid that exact stack; the draw weighs
/// them by [`abyss_bonus_weight`].
pub fn abyss_bonus_candidates(level: i64, enchanting: bool) -> &'static [BonusCandidate] {
    static BY_LEVEL: std::sync::OnceLock<Vec<[Vec<BonusCandidate>; 2]>> = std::sync::OnceLock::new();
    let table = BY_LEVEL.get_or_init(|| {
        (0..=ABYSS_BONUS_MAX_LEVEL)
            .map(|level| [build_bonus_candidates(level, false), build_bonus_candidates(level, true)])
            .collect()
    });
    &table[level.clamp(1, ABYSS_BONUS_MAX_LEVEL) as usize][usize::from(enchanting)]
}

fn build_bonus_candidates(level: i64, enchanting: bool) -> Vec<BonusCandidate> {
    if enchanting {
        return effect_ingredient_candidates(level);
    }
    let Some(bands) = enemy_loot()["tables"]
        .get(Uuid::from_u128(GOLD_LOOT_TABLE_ID).to_string())
        .and_then(|t| t["byLevel"].as_array())
    else {
        return Vec::new();
    };
    let range = |band: &serde_json::Value| {
        (
            band["minEnemyLevel"].as_i64().unwrap_or(i64::MIN),
            band["maxEnemyLevel"].as_i64().unwrap_or(i64::MAX),
        )
    };
    let stacks = |band: &serde_json::Value| -> Vec<BonusCandidate> {
        let mut out: Vec<BonusCandidate> = Vec::new();
        for result in band["results"].as_array().into_iter().flatten() {
            let n = result["n"].as_u64().unwrap_or(0);
            // `stackableItems` only: a result's `items` (gear) is never a candidate.
            for (id, qty) in result["loot"]["stackableItems"].as_object().into_iter().flatten() {
                let (Ok(item), Some(qty)) = (Uuid::parse_str(id), qty.as_u64()) else { continue };
                if qty == 0 || item.as_u128() == DOOR_KEY_ITEM_ID {
                    continue;
                }
                if EFFECT_INGREDIENTS.contains(&item.as_u128()) != enchanting {
                    continue;
                }
                match out.iter_mut().find(|c| c.0 == item && c.1 == qty) {
                    Some(c) => c.2 += n,
                    None => out.push((item, qty, n)),
                }
            }
        }
        out
    };
    let (lo, hi) = (level - ABYSS_BONUS_LEVEL_WINDOW, level + ABYSS_BONUS_LEVEL_WINDOW);
    let mut window: Vec<BonusCandidate> = Vec::new();
    for band in bands.iter().filter(|b| range(b).0 <= hi && range(b).1 >= lo) {
        for c in stacks(band) {
            match window.iter_mut().find(|w| w.0 == c.0 && w.1 == c.1) {
                Some(w) => w.2 += c.2,
                None => window.push(c),
            }
        }
    }
    if !window.is_empty() {
        window.sort();
        return window;
    }
    // Nothing of this kind near the level: the nearest band that has some.
    let distance = |band: &serde_json::Value| {
        let (min, max) = range(band);
        if level < min { min - level } else if level > max { level - max } else { 0 }
    };
    let mut nearest: Vec<&serde_json::Value> = bands.iter().collect();
    nearest.sort_by_key(|b| distance(b));
    let mut found = nearest.into_iter().map(stacks).find(|c| !c.is_empty()).unwrap_or_default();
    found.sort();
    found
}

/// Each effect ingredient's total weight in the enchanting bonus draw.
const EFFECT_INGREDIENT_WEIGHT: u64 = 1_000_000;

/// The enchanting half of the bonus at `level` (#383): every one of the
/// [`EFFECT_INGREDIENTS`] at every level, each with the same total weight
/// ([`EFFECT_INGREDIENT_WEIGHT`]), so the draw picks one uniformly. A level where
/// retail's table paid few or none of them still draws all seven. Within one
/// ingredient, the stack size follows what retail's table (871c2e9b) paid it in
/// within [`ABYSS_BONUS_LEVEL_WINDOW`] of `level`, weighted by how often; if the
/// table never paid it near `level`, its nearest band that did.
fn effect_ingredient_candidates(level: i64) -> Vec<BonusCandidate> {
    let Some(bands) = enemy_loot()["tables"]
        .get(Uuid::from_u128(GOLD_LOOT_TABLE_ID).to_string())
        .and_then(|t| t["byLevel"].as_array())
    else {
        return Vec::new();
    };
    let range = |band: &serde_json::Value| {
        (
            band["minEnemyLevel"].as_i64().unwrap_or(i64::MIN),
            band["maxEnemyLevel"].as_i64().unwrap_or(i64::MAX),
        )
    };
    let distance = |band: &serde_json::Value| {
        let (min, max) = range(band);
        if level < min { min - level } else if level > max { level - max } else { 0 }
    };
    let mut nearest: Vec<&serde_json::Value> = bands.iter().collect();
    nearest.sort_by_key(|b| distance(b));
    let (lo, hi) = (level - ABYSS_BONUS_LEVEL_WINDOW, level + ABYSS_BONUS_LEVEL_WINDOW);

    let mut out: Vec<BonusCandidate> = Vec::new();
    for id in EFFECT_INGREDIENTS {
        let item = Uuid::from_u128(id);
        let key = item.to_string();
        let mut stacks = effect_stack_sizes(&key, bands.iter().filter(|b| range(b).0 <= hi && range(b).1 >= lo));
        if stacks.is_empty() {
            stacks = nearest.iter().map(|b| effect_stack_sizes(&key, [*b])).find(|s| !s.is_empty()).unwrap_or_default();
        }
        if stacks.is_empty() {
            stacks.push((1, 1)); // never paid at all: one at a time
        }
        stacks.sort();
        // Scale this ingredient's observations to EFFECT_INGREDIENT_WEIGHT, the
        // rounding remainder on its most common size, so every ingredient's total
        // is exactly the same.
        let observed: u64 = stacks.iter().map(|s| s.1).sum();
        let mut weights: Vec<u64> = stacks.iter().map(|s| s.1 * EFFECT_INGREDIENT_WEIGHT / observed).collect();
        let most = (0..stacks.len()).max_by_key(|i| stacks[*i].1).unwrap_or(0);
        weights[most] += EFFECT_INGREDIENT_WEIGHT - weights.iter().sum::<u64>();
        out.extend(stacks.iter().zip(weights).filter(|(_, w)| *w > 0).map(|((qty, _), w)| (item, *qty, w)));
    }
    out.sort();
    out
}

/// (quantity, times observed) of every stack of `item` retail's table paid in `bands`.
fn effect_stack_sizes<'a>(item: &str, bands: impl IntoIterator<Item = &'a serde_json::Value>) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for result in bands.into_iter().flat_map(|band| band["results"].as_array().into_iter().flatten()) {
        let n = result["n"].as_u64().unwrap_or(0);
        let Some(qty) = result["loot"]["stackableItems"][item].as_u64().filter(|q| *q > 0 && n > 0) else { continue };
        match out.iter_mut().find(|s| s.0 == qty) {
            Some(s) => s.1 += n,
            None => out.push((qty, n)),
        }
    }
    out
}

/// The bonus material stack (if any) one Abyss corpse drops on top of retail's
/// loot (#383): `None` for ~46% of corpses. A function of the run seed and the
/// corpse's identity alone, on its own random stream, so it never moves the
/// retail draws next to it and a replayed floor drops the same thing.
pub fn abyss_bonus_material(
    dungeon_uuid: &Uuid,
    run_seed: u64,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
    character_level: i64,
) -> Option<(Uuid, u64)> {
    let ctx = LootSeedContext {
        abyss: true,
        bonus_level: Some(character_level),
        ..seeded_context(dungeon_uuid, run_seed)
    };
    roll_abyss_bonus(ctx, spawn_group_id, spawner_index, enemy_index)
}

fn roll_abyss_bonus(
    ctx: LootSeedContext,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
) -> Option<(Uuid, u64)> {
    // Banded to the level the character is PAID as: a level-49 character finds what
    // a level-100 one does (#398).
    let level = crate::features::loot_level::effective_loot_level(ctx.bonus_level.filter(|_| ctx.abyss)?);
    // Salted, so the stream is independent of the retail draws for this corpse.
    const SALT: u64 = 0x0383_ab55_b0b5_0383;
    let mut rng = LootRng(mix64(enemy_rng(ctx, spawn_group_id, spawner_index, enemy_index).0 ^ SALT));
    let unit = |x: u64| (x >> 11) as f64 / (1u64 << 53) as f64;
    if unit(rng.next()) >= ABYSS_BONUS_MATERIAL_SHARE {
        return None;
    }
    let enchanting = unit(rng.next()) < ABYSS_BONUS_ENCHANTING_SHARE;
    let candidates = abyss_bonus_candidates(level, enchanting);
    let total: f64 = candidates.iter().map(abyss_bonus_weight).sum();
    if total <= 0.0 {
        return None;
    }
    let mut pick = unit(rng.next()) * total;
    for candidate in candidates {
        let weight = abyss_bonus_weight(candidate);
        if pick < weight {
            return Some((candidate.0, candidate.1));
        }
        pick -= weight;
    }
    // Float rounding left the pick past the end: the last candidate.
    candidates.last().map(|c| (c.0, c.1))
}

// -- spawn-group loot -------------------------------------------------------
//
// `spawnGroupLoot` is the other half of an enemy's corpse, and it is how retail
// handed a key-holder its key. Over 66,994 captured enemy results it was filled
// 38 times, and every one of the 38 is the same bare result, one DoorKey. The
// complete protected corpus attributes all 38: 26 are EQ15's Mercenary and 12
// cover exactly three other groups. Every observation is enemy zero of spawner
// zero. A Battle Unceasing's boss sits behind a locked door and the Mercenary
// holds the key. We sent `{}` because the corpus called one item "too little to
// model" -- so the Mercenary died keyless and the quest could not be finished
// (report #236).
//
// Not rolled: every observed result for these exact positions carried it. A
// group absent here, or another spawner of one of these groups, sends `{}`, as
// retail did for the other 66,956 results.
const DOOR_KEY_ITEM_ID: u128 = 0xfaa3aeb3_9284_4d83_8981_1af00e3a6398;
const SPAWN_GROUP_LOOT: &[(u128, u128, u64)] = &[
    // EQ15_Stone_DungeonSettings, "Enemy.Name.Mercenary".
    (0xa91ebfe6_0167_4643_bd6f_ed27d8dfad41, DOOR_KEY_ITEM_ID, 1),
    (0x9cb35702_def0_441e_b760_9c8a23ffd8bf, DOOR_KEY_ITEM_ID, 1),
    (0x3d0a198d_6ab7_4c54_addf_1e67c55c3e78, DOOR_KEY_ITEM_ID, 1),
    (0x681c0add_1f49_46cc_bafb_e240680406e7, DOOR_KEY_ITEM_ID, 1),
];

/// The fixed `spawnGroupLoot` retail gave the first enemy of `spawn_group_id`.
pub fn spawn_group_loot(spawn_group_id: &Uuid, spawner_index: usize) -> LootTableResult {
    let mut out = LootTableResult::default();
    if spawner_index != 0 {
        return out;
    }
    for (group, item, quantity) in SPAWN_GROUP_LOOT {
        if spawn_group_id.as_u128() == *group {
            out.stackable_items.insert(Uuid::from_u128(*item), *quantity);
        }
    }
    out
}

// -- enemies per spawner ----------------------------------------------------
//
// A spawn group is `quantity` spawners (the outer list of `enemyGeneratedData`)
// and each spawner holds `_countPerSpawnerMin..Max` enemies (the inner list).
// parsed.json kept the first and dropped the second, so every spawner got ONE
// enemy. The client spawns exactly as many as the inner list holds, so 96 of
// the APK's 1,956 groups ran short -- fewer kills, less XP, less loot -- and
// "The Troll Trap" (45c042e1) could not be finished at all: its objective counts
// the three trolls of group d971b82c, and we sent one (report #301).
//
// Retail's inner list was within min..max in 7,666 of 7,666 distinct group
// generations, and the 13 generations of a min-2/max-3 group were all 3, so the
// generator fills every spawner to `max`. Built from the APK by
// `script/extract_spawn_group_counts.py`; compiled in, so a merge ships it.
static SPAWN_GROUP_COUNTS_RAW: &str = include_str!("../spawn_group_counts.json");

#[derive(Deserialize, Default)]
struct SpawnGroupCounts {
    groups: HashMap<Uuid, SpawnGroupCount>,
}

#[derive(Deserialize)]
struct SpawnGroupCount {
    /// Read only by the corpus tests that pin min <= max.
    #[cfg_attr(not(test), allow(dead_code))]
    min: u64,
    max: u64,
    /// The APK's `_levelDeltaSequence`: spawner `i` stands at the dungeon's level
    /// plus `seq[i % len]`. Empty for most groups.
    #[serde(default, rename = "levelDeltaSequence")]
    level_delta_sequence: Vec<i64>,
}

fn spawn_group_counts() -> &'static SpawnGroupCounts {
    static TABLE: std::sync::OnceLock<SpawnGroupCounts> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(SPAWN_GROUP_COUNTS_RAW).unwrap_or_default())
}

/// How many enemies stand at each spawner of `spawn_group_id`: the APK's
/// `_countPerSpawnerMax`, or one for a group the APK does not have (our own
/// content, or a future dungeon) -- the behaviour that preceded #301.
pub fn enemies_per_spawner(spawn_group_id: &Uuid) -> usize {
    spawn_group_counts()
        .groups
        .get(spawn_group_id)
        .map_or(1, |c| c.max)
        .max(1) as usize
}

/// How many levels above (or below) the dungeon's level spawner `spawner_index`
/// of `spawn_group_id` stands: the APK's `_levelDeltaSequence`, cycled over the
/// spawners. Zero for a group without one.
///
/// Retail applied it to every EVENT dungeon: 1,641 of 1,643 enemies in the
/// captured event generated data sit at `difficultyLevel + delta`, 589 of them on
/// a non-zero delta -- `Boss 01` of EQ25 at 78 on a level-72 quest. Story quests
/// do NOT follow it (their `difficultyLevel` is not the enemy level), so only the
/// event path reads this.
pub fn spawn_group_level_delta(spawn_group_id: &Uuid, spawner_index: usize) -> i64 {
    spawn_group_counts()
        .groups
        .get(spawn_group_id)
        .map(|c| &c.level_delta_sequence)
        .filter(|seq| !seq.is_empty())
        .map_or(0, |seq| seq[spawner_index % seq.len()])
}

/// The level and experience of each enemy a generator writes.
#[derive(Clone, Copy)]
pub enum EnemyLevels<'a> {
    /// Every enemy at one level, worth one XP figure: story quests, jobs, the Abyss.
    Flat { level: i64, given_xp: u64 },
    /// The dungeon's level plus each group's [`spawn_group_level_delta`], with
    /// the XP of the level the enemy actually stands at: event dungeons.
    WithDeltas {
        base: i64,
        scaling: &'a crate::static_data::QuestLevelScaling,
    },
}

impl EnemyLevels<'_> {
    fn at(&self, spawn_group_id: &Uuid, spawner_index: usize) -> (i64, u64) {
        match *self {
            EnemyLevels::Flat { level, given_xp } => (level, given_xp),
            EnemyLevels::WithDeltas { base, scaling } => {
                let level = (base + spawn_group_level_delta(spawn_group_id, spawner_index)).max(1);
                (level, scaling.given_xp(level))
            }
        }
    }

    /// The dungeon's own level, which floor loot is rolled at.
    fn base(&self) -> i64 {
        match *self {
            EnemyLevels::Flat { level, .. } => level,
            EnemyLevels::WithDeltas { base, .. } => base,
        }
    }
}

/// Is `table_id` one the enemy corpus left out, so that rows generated before
/// report #236 carry it as an empty result by construction rather than by draw?
///
/// The repair of stored quest rows asks this before filling an empty result: an
/// empty roll of a modelled table is an ordinary outcome and must stay empty.
pub fn enemy_table_is_unmodelled(table_id: &Uuid) -> bool {
    enemy_loot()["tables"].get(table_id.to_string()).is_none()
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
    generate_for_dungeon_inner(
        game_data,
        dungeon_uuid,
        default_seed_context(dungeon_uuid),
        EnemyLevels::Flat { level: enemy_level, given_xp },
    )
}

pub fn generate_for_dungeon_with_seed(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    run_seed: u64,
    enemy_level: i64,
    given_xp: u64,
) -> Option<DungeonGeneratedData> {
    generate_for_dungeon_inner(
        game_data,
        dungeon_uuid,
        seeded_context(dungeon_uuid, run_seed),
        EnemyLevels::Flat { level: enemy_level, given_xp },
    )
}

/// One Abyss floor: [`generate_for_dungeon_with_seed`], but its enemies roll the
/// retail Abyss corpse loot (gold, the odd Atronite) instead of the dungeon
/// enemy corpus (report #381), seeded per run so each run's corpses differ.
///
/// `character_level` adds the #383 bonus material ([`abyss_bonus_material`]),
/// banded to that level; `None` serves retail's Abyss loot alone.
pub fn generate_for_abyss_floor(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    run_seed: u64,
    enemy_level: i64,
    character_level: Option<i64>,
) -> Option<DungeonGeneratedData> {
    let mut data = generate_for_dungeon_inner(
        game_data,
        dungeon_uuid,
        LootSeedContext {
            abyss: true,
            bonus_level: character_level,
            ..seeded_context(dungeon_uuid, run_seed)
        },
        EnemyLevels::Flat { level: enemy_level, given_xp: 0 },
    )?;
    // The three best soul gems at 45-48 (owner-approved deviation, #388); the
    // corpses' bonus gem is paid so where it is merged ([`abyss_bonus_as_paid`]).
    if let Some(level) = character_level {
        crate::features::beginner_economy::top_container_soul_gems(&mut data, level, false);
    }
    Some(data)
}

/// [`generate_for_dungeon_levelled_with_seed`] (unseeded for `run_seed: None`), with
/// every enemy's and container's loot rolled `loot_shift` levels above the level it
/// stands at (#398, [`crate::features::loot_level::dungeon_loot_shift`]). Enemy
/// levels and XP are exactly `levels`'.
pub fn generate_for_dungeon_with_loot_shift(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    run_seed: Option<u64>,
    levels: EnemyLevels,
    loot_shift: i64,
) -> Option<DungeonGeneratedData> {
    let ctx = match run_seed {
        Some(seed) => seeded_context(dungeon_uuid, seed),
        None => default_seed_context(dungeon_uuid),
    };
    generate_for_dungeon_inner(game_data, dungeon_uuid, LootSeedContext { loot_shift, ..ctx }, levels)
}

/// [`generate_for_dungeon`] with each enemy's level and XP from `levels`, unseeded.
pub fn generate_for_dungeon_levelled(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    levels: EnemyLevels,
) -> Option<DungeonGeneratedData> {
    generate_for_dungeon_inner(game_data, dungeon_uuid, default_seed_context(dungeon_uuid), levels)
}

/// [`generate_for_dungeon_with_seed`] with each enemy's level and XP from `levels`.
pub fn generate_for_dungeon_levelled_with_seed(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    run_seed: u64,
    levels: EnemyLevels,
) -> Option<DungeonGeneratedData> {
    generate_for_dungeon_inner(
        game_data,
        dungeon_uuid,
        seeded_context(dungeon_uuid, run_seed),
        levels,
    )
}

fn generate_for_dungeon_inner(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    seed_context: LootSeedContext,
    levels: EnemyLevels,
) -> Option<DungeonGeneratedData> {
    let dungeon = game_data.dungeons.get(dungeon_uuid)?;
    let enemy_level = levels.base();

    let mut data = DungeonGeneratedData {
        enemy_generated_data: dungeon
            .spawn_info
            .enemy_spawn_groups
            .iter()
            .map(|(spawn_group_id, spawn_group)| {
                let mut enemies_info = Vec::new();
                let per_spawner = enemies_per_spawner(spawn_group_id);
                for spawner_index in 0..spawn_group.quantity.max(1) as usize {
                    let (enemy_level, given_xp) = levels.at(spawn_group_id, spawner_index);
                    enemies_info.push(
                        (0..per_spawner)
                            .map(|enemy_index| DungeonEnemyResult {
                                enemy_level,
                                given_xp,
                                // Retail's fixed per-group result -- the key-holder's
                                // key. Every observation was enemy zero.
                                spawn_group_loot: if enemy_index == 0 {
                                    spawn_group_loot(spawn_group_id, spawner_index)
                                } else {
                                    LootTableResult::default()
                                },
                                loot_table_loot: roll_enemy_loot_with_context(
                                    seed_context,
                                    spawn_group_id,
                                    spawner_index,
                                    enemy_index,
                                    enemy_level,
                                ),
                            })
                            .collect(),
                    );
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
                let cycle = definition
                    .and_then(|d| d.chest_cycle)
                    .and_then(|id| CHEST_CYCLES.iter().find(|c| c.id == id));
                (
                    *chest_spawn_id,
                    (0..quantity as usize)
                        .map(|index| ChestGeneratedData {
                            tier: match cycle {
                                Some(cycle) => cycle_tier(cycle, seed_context, chest_spawn_id, index),
                                None => tier,
                            },
                        })
                        .collect(),
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
                let from_interactable: Vec<Uuid> =
                    interactable.loot_table.keys().copied().collect();
                // What the interactable names, plus what retail was observed
                // hanging on this spawn point and the APK mapping omits (#192).
                let tables = tables_for_spawn(item_spawn_id, &from_interactable);
                // One result per thing retail put on this spawn, each rolled
                // separately — a pile of seven is seven draws, not one repeated.
                let pile = (0..floor_pile_size(item_spawn_id))
                    .map(|result_index| DungeonItemResult {
                        loot_table_loot: tables
                            .iter()
                            .map(|k| {
                                (
                                    *k,
                                    roll_container_table(
                                        seed_context,
                                        item_spawn_id,
                                        spawn_info.name.as_deref(),
                                        k,
                                        result_index,
                                        enemy_level,
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
    };
    beginner_economy_loot(&mut data, seed_context);
    Some(data)
}

/// A corpse's crafting materials in 5, 10 or 20, each table's under its own seed
/// (owner-approved deviation from retail, #388, 2026-10-16). Keyed off the corpse
/// and table, never the loot stream, so no other draw moves.
fn size_corpse_materials(
    ctx: LootSeedContext,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
    out: &mut HashMap<Uuid, LootTableResult>,
) {
    let corpse = enemy_rng(ctx, spawn_group_id, spawner_index, enemy_index).0 ^ 0x0388_5743_4B53_0388;
    for (table, loot) in out.iter_mut() {
        let seed = mix64(corpse ^ (table.as_u128() as u64).rotate_left(13));
        crate::features::beginner_economy::size_material_stacks(&mut loot.stackable_items, seed);
    }
}

/// The #383 Abyss bonus stack `drawn` for a corpse as it is paid under the
/// beginner economy (owner-approved deviation from retail, #388, 2026-10-16): a
/// soul gem is one of the three best at 45-48, a material 5, 10 or 20. Its own
/// seeded stream, so no retail draw moves.
fn bonus_as_paid(
    ctx: LootSeedContext,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
    drawn: (Uuid, u64),
) -> (Uuid, u64) {
    let Some(level) = ctx.bonus_level else { return drawn };
    let seed = mix64(enemy_rng(ctx, spawn_group_id, spawner_index, enemy_index).0 ^ 0x0388_B045_5EED_0388);
    let mut stack = HashMap::from([drawn]);
    crate::features::beginner_economy::top_soul_gems(&mut stack, level, seed);
    crate::features::beginner_economy::size_material_stacks(&mut stack, mix64(seed));
    stack.into_iter().next().unwrap_or(drawn)
}

/// [`abyss_bonus_material`] as the corpse pays it ([`bonus_as_paid`], #388).
pub fn abyss_bonus_as_paid(
    dungeon_uuid: &Uuid,
    run_seed: u64,
    spawn_group_id: &Uuid,
    spawner_index: usize,
    enemy_index: usize,
    character_level: i64,
) -> Option<(Uuid, u64)> {
    let ctx = LootSeedContext {
        abyss: true,
        bonus_level: Some(character_level),
        ..seeded_context(dungeon_uuid, run_seed)
    };
    roll_abyss_bonus(ctx, spawn_group_id, spawner_index, enemy_index)
        .map(|drawn| bonus_as_paid(ctx, spawn_group_id, spawner_index, enemy_index, drawn))
}

/// The beginner economy (#388) on a generated dungeon's corpse and container
/// loot: gems 20% more on average
/// ([`crate::features::beginner_economy::with_gem_bonus_drawn`]). Seeded by the
/// dungeon, run and each result's own keys, so a re-fetch pays the same.
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
fn beginner_economy_loot(data: &mut DungeonGeneratedData, ctx: LootSeedContext) {
    use crate::features::beginner_economy::add_drawn_gem_bonus;
    let base = mix64(ctx.dungeon_uuid.as_u128() as u64 ^ ctx.run_seed.rotate_left(7) ^ 0x388_0000_6E35);
    let key = |group: &Uuid, a: usize, b: usize, table: &Uuid| {
        mix64(
            base ^ (group.as_u128() as u64).rotate_left(13)
                ^ (a as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (b as u64).rotate_left(40)
                ^ (table.as_u128() as u64).rotate_left(29),
        )
    };
    for (group, spawners) in &mut data.enemy_generated_data {
        for (a, enemies) in spawners.iter_mut().enumerate() {
            for (b, enemy) in enemies.iter_mut().enumerate() {
                add_drawn_gem_bonus(&mut enemy.spawn_group_loot.currencies, key(group, a, b, &Uuid::nil()));
                for (table, loot) in &mut enemy.loot_table_loot {
                    add_drawn_gem_bonus(&mut loot.currencies, key(group, a, b, table));
                }
            }
        }
    }
    for (group, pile) in &mut data.item_generated_data {
        for (a, result) in pile.iter_mut().enumerate() {
            for (table, loot) in &mut result.loot_table_loot {
                add_drawn_gem_bonus(&mut loot.currencies, key(group, a, usize::MAX, table));
            }
        }
    }
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
                // The two job chests draw from a chest cycle instead (#337), whose
                // base is this same rarity; their mix is pinned in server's
                // `report_337_job_drops_and_families`.
                if expected.chest_cycle.is_some() {
                    assert!(
                        CHEST_CYCLES.iter().any(|c| Some(c.id) == expected.chest_cycle && c.base == expected.rarity),
                        "{chest_id} names a cycle we do not model, or one whose base is not its rarity"
                    );
                    checked += 1;
                    continue;
                }
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
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0, 30);
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
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0, 30);
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
        let a = roll_loot_table(&d, &s, &tid, 0, 30);
        let b = roll_loot_table(&d, &s, &tid, 0, 30);
        assert_eq!(a.stackable_items, b.stackable_items, "same barrel, different loot");

        // control: a DIFFERENT spawn should not be forced to match, or the
        // stability check above would hold trivially for everything.
        let mut differs = false;
        for other in 0..60u128 {
            let c = roll_loot_table(&d, &Uuid::from_u128(other), &tid, 0, 30);
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
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0, 30);
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
                let got = roll_loot_table(&dungeon, &Uuid::from_u128(s), &table_id, 0, 30);
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
mod floor_loot_level_tests {
    use super::*;

    const EBONY_INGOT: &str = "75112030-b248-49b0-9c70-0da8dea150d1";
    /// The dungeon of the first story quest, 5ad30483 (the tutorial).
    const TUTORIAL_DUNGEON: &str = "51470ac0-46eb-4790-8de1-d51d7c36908c";
    /// The common materials table, and the tutorial pile that rolled Ebony.
    const MATERIALS_TABLE: &str = "7ee92f4b-18e7-4d15-91c6-ed0436180b81";
    const TUTORIAL_EBONY_PILE: &str = "a8e0681b-f6f3-4bfa-a38e-6ee7b8d4bbe8";

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(path).expect("read parsed.json");
        serde_json::from_str(&raw).expect("parse game data")
    }

    fn ebony_piles(generated: &DungeonGeneratedData) -> Vec<Uuid> {
        let ebony: Uuid = EBONY_INGOT.parse().unwrap();
        let mut out: Vec<Uuid> = generated
            .item_generated_data
            .iter()
            .filter(|(_, pile)| {
                pile.iter().any(|r| {
                    r.loot_table_loot
                        .values()
                        .any(|l| l.stackable_items.contains_key(&ebony))
                })
            })
            .map(|(id, _)| *id)
            .collect();
        out.sort();
        out
    }

    /// Tracker #8: every new player found an Ebony Ingot in the tutorial.
    ///
    /// The tutorial is generated at enemy level 2, and the pile `a8e0681b` drew
    /// Ebony out of the materials table because that table was pooled over every
    /// level. Both characters in the report have it in exactly that pile. Retail
    /// rolled the materials table 954 times at levels 1-10 and produced no Ebony
    /// at all; its lowest is level 42.
    #[test]
    fn the_tutorial_does_not_hand_a_level_two_player_ebony() {
        let game_data = game_data();
        let generated =
            generate_for_dungeon(&game_data, &TUTORIAL_DUNGEON.parse().unwrap(), 2, 100)
                .expect("the tutorial dungeon is in parsed.json");
        assert!(
            generated
                .item_generated_data
                .contains_key(&TUTORIAL_EBONY_PILE.parse().unwrap()),
            "the pile from the report is still generated, so the test is looking at it"
        );
        assert_eq!(ebony_piles(&generated), Vec::<Uuid>::new(), "Ebony in a level-2 tutorial");
    }

    /// Not only the tutorial: no floor pile in any dungeon may roll Ebony below
    /// level 31, the lowest level retail ever put it on a floor. The control is
    /// the other end: at level 75 it must still be reachable somewhere, or the
    /// banding has simply deleted Ebony and the first assertion is vacuous.
    #[test]
    fn floor_ebony_follows_the_level_retail_gave_it() {
        let game_data = game_data();
        let mut low = Vec::new();
        let mut high = 0usize;
        for dungeon in game_data.dungeons.keys() {
            for level in [1, 2, 5, 10, 20] {
                if let Some(g) = generate_for_dungeon(&game_data, dungeon, level, 100) {
                    for pile in ebony_piles(&g) {
                        low.push((*dungeon, level, pile));
                    }
                }
            }
            if let Some(g) = generate_for_dungeon(&game_data, dungeon, 75, 100) {
                high += ebony_piles(&g).len();
            }
        }
        assert!(low.is_empty(), "Ebony rolled on the floor below level 31: {low:?}");
        assert!(high > 0, "no floor pile rolls Ebony even at level 75 -- the control failed");
    }

    /// The band is the level's, not the nearest pooled guess: the materials
    /// table at level 2 draws only from results retail rolled at low level.
    #[test]
    fn a_banded_table_draws_from_its_level_band() {
        let table: Uuid = MATERIALS_TABLE.parse().unwrap();
        let low = interactable_table_results(&table, 2).expect("banded");
        let pooled = interactable_loot()["tables"][MATERIALS_TABLE]["results"]
            .as_array()
            .expect("pooled");
        assert!(!low.iter().any(|r| r.to_string().contains(EBONY_INGOT)));
        assert!(
            pooled.iter().any(|r| r.to_string().contains(EBONY_INGOT)),
            "control: the pooled table does carry Ebony, which is the bug"
        );
        // A table too thin to band still draws pooled, as before.
        let thin: Uuid = "b9240d3c-dffc-474b-a773-b7588bf9bb71".parse().unwrap();
        assert!(interactable_loot_by_level()["tables"].get(thin.to_string()).is_none());
        assert_eq!(
            interactable_table_results(&thin, 2).map(|r| r.into_iter().cloned().collect::<Vec<_>>()),
            interactable_loot()["tables"][thin.to_string()]["results"].as_array().cloned()
        );
    }
}

/// Medium and large containers pay by the player's level, as retail paid them
/// (tracker #374).
///
/// Retail, event containers in the 2026-06-07 snapshot: the large container
/// (Breakable_T3, table 6cb97b90) averaged 886 gold for level 40-43 characters
/// and 1,278 for level-100 ones (difficulty 77-78); the medium one (Breakable_T2,
/// 49173c2c) 160 and 207. The greedy 300-observation bands gave both tables one
/// band from the 40s to 87, so a level-43 and a level-78 character drew the same
/// results -- about 1,076 and 220 gold -- the high-level player short and the
/// mid-level one over. That is the report: Sephoris, level 43, getting the larger
/// stacks, HauDrauf, level 100, the smaller.
#[cfg(test)]
mod container_pay_by_level_374 {
    use super::*;

    const GOLD: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";
    const SMALL: &str = "7ee92f4b-18e7-4d15-91c6-ed0436180b81";
    const MEDIUM: &str = "49173c2c-e34c-4664-b2cd-d1bae46c39fa";
    const LARGE: &str = "6cb97b90-6ab2-4ddc-bc81-4f7db6db3e7f";

    fn draws(table: &str, level: i64) -> Vec<&'static serde_json::Value> {
        interactable_table_results(&table.parse().unwrap(), level).expect("a banded table")
    }

    /// The expected gold of one draw, straight from the weights (no sampling).
    fn expected_gold(table: &str, level: i64) -> f64 {
        let results = draws(table, level);
        let total: f64 = results.iter().map(|r| r["n"].as_f64().unwrap()).sum();
        results
            .iter()
            .map(|r| r["loot"]["currencies"][GOLD].as_f64().unwrap_or(0.0) * r["n"].as_f64().unwrap())
            .sum::<f64>()
            / total
    }

    /// The levels of the bands a draw at `level` reads, found by identity.
    fn levels_read(table: &str, level: i64) -> Vec<i64> {
        let picked = draws(table, level);
        let mut out: Vec<i64> = interactable_loot_by_level()["tables"][table]["byLevel"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|band| {
                band["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| picked.iter().any(|p| std::ptr::eq(*p, r)))
            })
            .map(|band| band["minEnemyLevel"].as_i64().unwrap())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_level_78_large_container_pays_what_retail_paid_level_100_characters() {
        let large = expected_gold(LARGE, 78);
        assert!(
            (1_150.0..=1_400.0).contains(&large),
            "large container at level 78: {large:.0} gold, retail 1,278 (was 1,076)"
        );
        let medium = expected_gold(MEDIUM, 78);
        assert!(
            (186.0..=240.0).contains(&medium),
            "medium container at level 78: {medium:.0} gold, retail 207"
        );
    }

    #[test]
    fn a_level_43_medium_container_pays_what_retail_paid_level_40_to_43_characters() {
        let medium = expected_gold(MEDIUM, 43);
        assert!(
            (136.0..=184.0).contains(&medium),
            "medium container at level 43: {medium:.0} gold, retail 160 (was 220)"
        );
        let large = expected_gold(LARGE, 43);
        assert!(
            (700.0..=1_000.0).contains(&large),
            "large container at level 43: {large:.0} gold, retail 886 (was 1,076)"
        );
    }

    /// The property that broke: a bigger player gets a bigger large container.
    /// Retail's ratio between level-100 and level-40-43 characters is 1.44.
    #[test]
    fn the_large_container_grows_with_the_players_level() {
        let ratio = expected_gold(LARGE, 78) / expected_gold(LARGE, 43);
        assert!(ratio >= 1.3, "level 78 / level 43 large-container gold = {ratio:.2}");
        let ratio = expected_gold(MEDIUM, 78) / expected_gold(MEDIUM, 43);
        assert!(ratio >= 1.2, "level 78 / level 43 medium-container gold = {ratio:.2}");
    }

    /// The window is centred on the player: a level-43 draw reads nothing retail
    /// rolled at 70 or above, and a level-78 draw nothing below 60.
    #[test]
    fn a_draw_reads_only_levels_near_the_player() {
        for table in [MEDIUM, LARGE] {
            let mid = levels_read(table, 43);
            assert!(mid.iter().all(|&l| l < 70), "{table} at 43 reads {mid:?}");
            assert!(mid[0] <= 43 && 43 <= mid[mid.len() - 1], "{table} at 43 reads {mid:?}");
            let high = levels_read(table, 78);
            assert!(high.iter().all(|&l| l >= 60), "{table} at 78 reads {high:?}");
        }
    }

    /// CONTROL: where retail rolled a table often the window is the one level.
    /// The small container was rolled 643 times at exactly level 72.
    #[test]
    fn a_densely_observed_level_reads_only_itself() {
        assert_eq!(levels_read(SMALL, 72), vec![72]);
    }

    /// The sidecar is the per-level shape the window reads.
    #[test]
    fn the_sidecar_has_one_band_per_level_and_a_window() {
        let corpus = interactable_loot_by_level();
        assert_eq!(corpus["_meta"]["windowObservations"].as_u64(), Some(200));
        for (table, entry) in corpus["tables"].as_object().unwrap() {
            for band in entry["byLevel"].as_array().unwrap() {
                assert_eq!(band["minEnemyLevel"], band["maxEnemyLevel"], "{table}");
            }
        }
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
mod effect_ingredient_tests {
    //! #383: the enchanting half of the Abyss bonus is the Sigil shop's seven
    //! effect ingredients, drawn uniformly at every level.
    use super::*;

    fn static_json(name: &str) -> serde_json::Value {
        let path = format!("{}/../deploy/static/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).expect(&path)).expect(name)
    }

    fn set(ids: impl IntoIterator<Item = Uuid>) -> std::collections::BTreeSet<Uuid> {
        ids.into_iter().collect()
    }

    fn effect_ingredients() -> std::collections::BTreeSet<Uuid> {
        set(EFFECT_INGREDIENTS.iter().map(|id| Uuid::from_u128(*id)))
    }

    const DEATHBELL: &str = "8b7d0044-3a38-4ee0-af45-d2892b0f508d";
    const FROST_SALTS: &str = "05b4dd6b-796f-4088-a772-0e33ea3db976";

    /// [`EFFECT_INGREDIENTS`] is retail's own grouping: the Sigil shop's
    /// `EffectIngredients_AllIngredients` bundle pays exactly these seven, each
    /// single-ingredient offer pays one of them, and the game data names each one as
    /// its offer does -- `8b7d0044` is Deathbell, not "Beehive Husk".
    #[test]
    fn effect_ingredients_are_the_sigil_shops_group() {
        let offers = static_json("global_shop_offer_contents.json");
        let items = static_json("parsed.json");
        let mut singles = Vec::new();
        for offer in offers["offers"].as_object().unwrap().values() {
            let Some(name) = offer["editorName"].as_str().and_then(|n| n.strip_prefix("SigilShop_EffectIngredients_"))
            else {
                continue;
            };
            let paid = set(offer["contents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| Uuid::parse_str(c["itemTemplateId"].as_str().unwrap()).unwrap()));
            if name == "AllIngredients" {
                assert_eq!(paid, effect_ingredients(), "the AllIngredients bundle");
                continue;
            }
            assert_eq!(paid.len(), 1, "{name}");
            let id = *paid.first().unwrap();
            assert_eq!(items["items_template"][id.to_string()]["name"], format!("Items.Name.{name}"), "{id}");
            singles.push(id);
        }
        assert_eq!(set(singles.iter().copied()), effect_ingredients());
        assert_eq!(singles.len(), 7);
        assert!(effect_ingredients().contains(&Uuid::parse_str(DEATHBELL).unwrap()));
        assert_eq!(items["items_template"][DEATHBELL]["name"], "Items.Name.Deathbell");
        assert_eq!(ENCHANTING_INGREDIENTS, EFFECT_INGREDIENTS);
    }

    /// The quantities retail's table paid `item` in, in bands overlapping `lo..=hi`.
    fn retail_sizes(item: &Uuid, lo: i64, hi: i64) -> std::collections::BTreeSet<u64> {
        let bands = enemy_loot()["tables"][Uuid::from_u128(GOLD_LOOT_TABLE_ID).to_string()]["byLevel"]
            .as_array()
            .unwrap();
        bands
            .iter()
            .filter(|b| b["minEnemyLevel"].as_i64().unwrap() <= hi && b["maxEnemyLevel"].as_i64().unwrap() >= lo)
            .flat_map(|b| b["results"].as_array().unwrap())
            .filter_map(|r| r["loot"]["stackableItems"][item.to_string()].as_u64())
            .filter(|q| *q > 0)
            .collect()
    }

    /// At every level, all seven are candidates with the same total weight, and
    /// each one's stack sizes are what retail's table paid it in near the level --
    /// or, where it never did, at the nearest level it did.
    #[test]
    fn every_level_offers_all_seven_with_equal_weight_at_retail_stack_sizes() {
        for level in 1..=ABYSS_BONUS_MAX_LEVEL {
            let candidates = abyss_bonus_candidates(level, true);
            assert_eq!(set(candidates.iter().map(|c| c.0)), effect_ingredients(), "level {level}");
            for item in effect_ingredients() {
                let mine: Vec<&BonusCandidate> = candidates.iter().filter(|c| c.0 == item).collect();
                let weight: u64 = mine.iter().map(|c| c.2).sum();
                assert_eq!(weight, EFFECT_INGREDIENT_WEIGHT, "level {level}: {item}");
                let near = retail_sizes(&item, level - ABYSS_BONUS_LEVEL_WINDOW, level + ABYSS_BONUS_LEVEL_WINDOW);
                // Where retail paid it near the level, exactly those sizes; otherwise
                // the sizes of the nearest band that paid it (some band within 12).
                let expected = if near.is_empty() {
                    (1..=12)
                        .map(|d| {
                            let below = retail_sizes(&item, level - d, level - d);
                            if below.is_empty() { retail_sizes(&item, level + d, level + d) } else { below }
                        })
                        .find(|s| !s.is_empty())
                        .unwrap_or_else(|| panic!("level {level}: {item} never paid within 12 levels"))
                } else {
                    near
                };
                let drawn: std::collections::BTreeSet<u64> = mine.iter().map(|c| c.1).collect();
                assert_eq!(drawn.len(), mine.len(), "level {level}: {item} lists a size twice");
                assert!(drawn.is_subset(&expected), "level {level}: {item} {drawn:?} not in {expected:?}");
            }
        }
        // Control: retail's table never paid Frost Salts above level 88, so level 100
        // draws it at the 87-88 band's sizes; and the materials half is unchanged in
        // kind -- no effect ingredient in it.
        let frost = Uuid::parse_str(FROST_SALTS).unwrap();
        assert!(retail_sizes(&frost, 98, 100).is_empty());
        assert!(abyss_bonus_candidates(100, true).iter().any(|c| c.0 == frost));
        for level in 1..=ABYSS_BONUS_MAX_LEVEL {
            assert!(abyss_bonus_candidates(level, false).iter().all(|c| !EFFECT_INGREDIENTS.contains(&c.0.as_u128())));
        }
    }

    /// Sampled over many corpse identities, each of the seven is 1/7 of the
    /// enchanting bonus (within 3 points) at levels 7, 20, 45 and 100, Deathbell
    /// included -- where retail's table weighted them 7-33%.
    #[test]
    fn the_enchanting_bonus_draws_each_ingredient_equally() {
        let names = static_json("parsed.json");
        let name = |id: &Uuid| names["items_template"][id.to_string()]["name"].as_str().unwrap_or("?").to_string();
        let dungeon = Uuid::from_u128(0x383e);
        let deathbell = Uuid::parse_str(DEATHBELL).unwrap();
        for level in [7i64, 20, 45, 100] {
            let mut drawn: HashMap<Uuid, usize> = HashMap::new();
            let mut total = 0usize;
            for i in 0..150_000usize {
                let group = Uuid::from_u128(i as u128 * 0x9E37_79B9);
                let run_seed = (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
                let Some((item, qty)) = abyss_bonus_material(&dungeon, run_seed, &group, i % 3, i % 2, level) else {
                    continue;
                };
                if !EFFECT_INGREDIENTS.contains(&item.as_u128()) {
                    continue;
                }
                assert!(qty > 0);
                total += 1;
                *drawn.entry(item).or_default() += 1;
            }
            // Retail's table near this level, the weighting before #383's change.
            let retail: HashMap<Uuid, u64> = effect_ingredients()
                .into_iter()
                .map(|item| {
                    let bands = enemy_loot()["tables"][Uuid::from_u128(GOLD_LOOT_TABLE_ID).to_string()]["byLevel"]
                        .as_array()
                        .unwrap();
                    let n = bands
                        .iter()
                        .filter(|b| {
                            b["minEnemyLevel"].as_i64().unwrap() <= level + ABYSS_BONUS_LEVEL_WINDOW
                                && b["maxEnemyLevel"].as_i64().unwrap() >= level - ABYSS_BONUS_LEVEL_WINDOW
                        })
                        .flat_map(|b| b["results"].as_array().unwrap())
                        .filter(|r| r["loot"]["stackableItems"][item.to_string()].as_u64().is_some_and(|q| q > 0))
                        .map(|r| r["n"].as_u64().unwrap())
                        .sum();
                    (item, n)
                })
                .collect();
            let retail_total: u64 = retail.values().sum();
            let mut line = Vec::new();
            for item in effect_ingredients() {
                let share = drawn.get(&item).copied().unwrap_or(0) as f64 / total as f64;
                line.push(format!(
                    "{} {:.1}% (retail {:.1}%)",
                    name(&item).trim_start_matches("Items.Name."),
                    100.0 * share,
                    100.0 * retail[&item] as f64 / retail_total as f64
                ));
                assert!((share - 1.0 / 7.0).abs() <= 0.03, "level {level}: {} {share}", name(&item));
            }
            println!("level {level}: {total} enchanting draws: {}", line.join(", "));
            assert!(total >= 3000, "level {level}: {total} draws is too few");
            assert!(drawn.get(&deathbell).copied().unwrap_or(0) > 0, "level {level}: no Deathbell");
        }
    }
}

#[cfg(test)]
mod abyss_bonus_material_tests {
    //! #383: the owner-approved bonus material on Abyss corpses.
    use super::*;

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }
    const PEARL: &str = "89ece62c-9ff6-470a-a152-d45ef4e0c222";
    const GARLIC: &str = "fbf96b07-e9aa-4157-8761-10179fa05138";
    const DRAGON_SCALE: &str = "34ddfe0e-5119-4e52-8eef-a77b6bc810a7";

    /// The levels retail's general enemy table paid each stack at, read straight
    /// from the corpus (independent of the candidate builder).
    fn retail_levels(item: &Uuid) -> (i64, i64) {
        let bands = enemy_loot()["tables"][Uuid::from_u128(GOLD_LOOT_TABLE_ID).to_string()]["byLevel"]
            .as_array()
            .unwrap();
        let (mut lo, mut hi) = (i64::MAX, i64::MIN);
        for band in bands {
            let paid = band["results"].as_array().unwrap().iter().any(|r| {
                r["loot"]["stackableItems"].as_object().is_some_and(|m| m.contains_key(&item.to_string()))
            });
            if paid {
                lo = lo.min(band["minEnemyLevel"].as_i64().unwrap());
                hi = hi.max(band["maxEnemyLevel"].as_i64().unwrap());
            }
        }
        (lo, hi)
    }

    #[test]
    fn every_level_has_both_kinds_and_only_the_right_kind() {
        for level in 1..=100 {
            let materials = abyss_bonus_candidates(level, false);
            let enchanting = abyss_bonus_candidates(level, true);
            assert!(!materials.is_empty() && !enchanting.is_empty(), "level {level}");
            assert!(enchanting.iter().all(|c| ENCHANTING_INGREDIENTS.contains(&c.0.as_u128())));
            assert!(materials.iter().all(|c| !ENCHANTING_INGREDIENTS.contains(&c.0.as_u128())));
            assert!(materials.iter().chain(enchanting).all(|c| c.1 > 0 && c.2 > 0));
        }
    }

    /// Every candidate is something retail's table paid within the window of the
    /// character's level, so a level-100 character never sees Pearl or Garlic
    /// (retail: level 1-30) and a level-7 one never Dragon Scale (25+).
    #[test]
    fn candidates_are_what_retail_paid_at_the_characters_level() {
        for level in 1..=100i64 {
            for c in abyss_bonus_candidates(level, false).iter().chain(abyss_bonus_candidates(level, true)) {
                let (lo, hi) = retail_levels(&c.0);
                assert!(
                    lo <= level + ABYSS_BONUS_LEVEL_WINDOW + 2 && hi + 12 >= level,
                    "level {level}: {} paid only at {lo}-{hi}",
                    c.0
                );
            }
        }
        let at = |level| {
            abyss_bonus_candidates(level, false).iter().map(|c| c.0).collect::<std::collections::HashSet<_>>()
        };
        assert!(!at(100).contains(&uuid(PEARL)) && !at(100).contains(&uuid(GARLIC)));
        assert!(!at(7).contains(&uuid(DRAGON_SCALE)));
        // Control: retail did pay Pearl and Garlic at level 7, and dragon parts at 100.
        assert!(at(7).contains(&uuid(PEARL)) && at(7).contains(&uuid(GARLIC)));
        assert!(at(100).contains(&uuid(DRAGON_SCALE)));
    }

    /// Sampled over many corpse identities: ~54% drop a bonus, a third of those an
    /// enchanting ingredient; stable per identity, absent without a level.
    #[test]
    fn the_bonus_share_and_mix_hold_and_are_deterministic() {
        let dungeon = Uuid::from_u128(0xab133);
        for level in [7i64, 43, 100] {
            let (mut bonus, mut enchanting, draws) = (0usize, 0usize, 60_000usize);
            for i in 0..draws {
                let group = Uuid::from_u128(i as u128 * 0x9E37_79B9);
                let run_seed = (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
                let drawn = abyss_bonus_material(&dungeon, run_seed, &group, i % 3, i % 2, level);
                assert_eq!(drawn, abyss_bonus_material(&dungeon, run_seed, &group, i % 3, i % 2, level));
                if let Some((item, _)) = drawn {
                    bonus += 1;
                    enchanting += usize::from(ENCHANTING_INGREDIENTS.contains(&item.as_u128()));
                }
            }
            let share = bonus as f64 / draws as f64;
            let ench = enchanting as f64 / bonus as f64;
            println!("level {level}: bonus {share:.4}, enchanting {ench:.4}");
            assert!((share - ABYSS_BONUS_MATERIAL_SHARE).abs() < 0.01, "level {level}: {share}");
            assert!((ench - ABYSS_BONUS_ENCHANTING_SHARE).abs() < 0.02, "level {level}: {ench}");
        }
        // No bonus on a non-Abyss context or without a level.
        let group = Uuid::from_u128(1);
        let ctx = seeded_context(&dungeon, 5);
        assert!((0..2000).all(|i| roll_abyss_bonus(ctx, &group, i, 0).is_none()));
        let abyss_no_level = LootSeedContext { abyss: true, ..ctx };
        assert!((0..2000).all(|i| roll_abyss_bonus(abyss_no_level, &group, i, 0).is_none()));
    }

    /// `parsed.json`'s item templates: the client's name key and item type.
    fn item_templates() -> HashMap<Uuid, crate::game_data::GameDataItem> {
        #[derive(serde::Deserialize)]
        struct Items {
            items_template: HashMap<Uuid, crate::game_data::GameDataItem>,
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(path).expect("read parsed.json");
        serde_json::from_str::<Items>(&raw).expect("parsed.json items").items_template
    }

    fn every_candidate() -> std::collections::BTreeSet<Uuid> {
        (1..=ABYSS_BONUS_MAX_LEVEL)
            .flat_map(|level| abyss_bonus_candidates(level, false).iter().chain(abyss_bonus_candidates(level, true)))
            .map(|c| c.0)
            .collect()
    }

    /// No gear, ever: every stack the bonus can pay, at every level, is an item
    /// template of type 8 (material), never a weapon, armour or jewellery.
    #[test]
    fn every_candidate_is_a_material_never_gear() {
        let items = item_templates();
        for id in every_candidate() {
            let item = items.get(&id).unwrap_or_else(|| panic!("{id} is not an item template"));
            assert_eq!(item.r#type, 8, "{id} ({}) is not a material", item.name);
        }
        // Control: the table this draws from does pay gear, which is never read.
        let bands = enemy_loot()["tables"][Uuid::from_u128(GOLD_LOOT_TABLE_ID).to_string()]["byLevel"]
            .as_array()
            .unwrap();
        assert!(bands.iter().flat_map(|b| b["results"].as_array().unwrap()).any(|r| r["loot"]["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty())));
    }

    /// [`TOWN_MATERIALS`] is the game data's own classification: of everything the
    /// bonus can pay, exactly the items the client names `Item.TownResource.*` or
    /// `Item.DecorationResource.*`. None of them is an input of smithing, enchanting,
    /// tempering or repair, so the town is their only use.
    #[test]
    fn town_materials_are_the_town_only_materials_of_the_game_data() {
        let items = item_templates();
        let town_named: std::collections::BTreeSet<Uuid> = every_candidate()
            .into_iter()
            .filter(|id| {
                let name = &items[id].name;
                name.starts_with("Item.TownResource.") || name.starts_with("Item.DecorationResource.")
            })
            .collect();
        let shipped: std::collections::BTreeSet<Uuid> = TOWN_MATERIALS.iter().map(|id| Uuid::from_u128(*id)).collect();
        assert_eq!(town_named, shipped);

        let file = |name: &str| {
            std::fs::read_to_string(format!("{}/../deploy/static/{name}", env!("CARGO_MANIFEST_DIR")))
                .or_else(|_| std::fs::read_to_string(format!("{}/src/{name}", env!("CARGO_MANIFEST_DIR"))))
                .unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let gear_inputs =
            ["smithing_inputs.json", "enchanting.json", "item_mod_recipes.json", "repair_costs.json"].map(file);
        let buildings = file("building_upgrades.json");
        for id in &shipped {
            let id = id.to_string();
            assert!(gear_inputs.iter().all(|f| !f.contains(&id)), "{id} is a gear-crafting input");
        }
        // Controls: the building materials really are building inputs (Marble and
        // Bronze as building styles), and Animal Hide, asked about with them, is a
        // smithing input -- so it stays at full weight.
        for id in [0xe7193116_d761_479b_8a20_5633737977f5u128, 0xfd67bbc6_20f4_44a3_9614_28265ebb8c67,
            0x42d91529_c88b_4c5b_815b_b55508b4e7ef, 0x61d5e5c7_78b8_4fc3_ba6a_5b7c2a15866d,
            0x38d32048_ce01_4390_a4f0_cdb94ef3ce72]
        {
            assert!(buildings.contains(&Uuid::from_u128(id).to_string()), "{id:x} builds nothing");
        }
        let animal_hide = uuid("51f7612f-a797-4417-8a92-493b0aae7f45");
        assert!(every_candidate().contains(&animal_hide));
        assert_eq!(items[&animal_hide].name, "Items.Name.AnimalHide");
        assert!(gear_inputs[0].contains(&animal_hide.to_string()), "Animal Hide is a smithing input");
        assert!(!shipped.contains(&animal_hide));
    }

    /// #383 tuning: town materials weigh [`ABYSS_BONUS_TOWN_MATERIAL_WEIGHT`] in the
    /// draw, so against every other material their odds halve at every level, and
    /// the sampled draw follows.
    #[test]
    fn town_materials_weigh_half_in_the_bonus_draw() {
        let town = |c: &&BonusCandidate| TOWN_MATERIALS.contains(&c.0.as_u128());
        let share = |level: i64, weight: &dyn Fn(&BonusCandidate) -> f64| {
            let candidates = abyss_bonus_candidates(level, false);
            let all: f64 = candidates.iter().map(weight).sum();
            candidates.iter().filter(town).map(weight).sum::<f64>() / all
        };
        let odds = |s: f64| s / (1.0 - s);
        for level in 1..=ABYSS_BONUS_MAX_LEVEL {
            let before = share(level, &|c| c.2 as f64);
            let after = share(level, &abyss_bonus_weight);
            assert!(before > 0.0 && before < 1.0, "level {level}: {before}");
            assert!(
                (odds(after) / odds(before) - ABYSS_BONUS_TOWN_MATERIAL_WEIGHT).abs() < 1e-9,
                "level {level}: {before} -> {after}"
            );
        }
        // The enchanting draw has no town material, so it is untouched.
        assert!((1..=ABYSS_BONUS_MAX_LEVEL).all(|l| abyss_bonus_candidates(l, true).iter().all(|c| !town(&c))));

        let dungeon = Uuid::from_u128(0x383b);
        for level in [7i64, 20, 45, 100] {
            let (mut materials, mut towns) = (0usize, 0usize);
            for i in 0..60_000usize {
                let group = Uuid::from_u128(i as u128 * 0x9E37_79B9);
                let run_seed = (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
                let Some((item, _)) = abyss_bonus_material(&dungeon, run_seed, &group, i % 3, i % 2, level) else {
                    continue;
                };
                if ENCHANTING_INGREDIENTS.contains(&item.as_u128()) {
                    continue;
                }
                materials += 1;
                towns += usize::from(TOWN_MATERIALS.contains(&item.as_u128()));
            }
            let (before, after) = (share(level, &|c| c.2 as f64), share(level, &abyss_bonus_weight));
            let measured = towns as f64 / materials as f64;
            println!(
                "level {level}: town share of non-enchanting bonus stacks: retail {before:.3}, \
                 half weight {after:.3}, drawn {measured:.3} ({towns}/{materials})"
            );
            assert!((measured - after).abs() < 0.02, "level {level}: drew {measured}, expected {after}");
        }
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

    /// #393 (HauDrauf, 2026-10-15, "überall, keine Unterschiede"): a ring or
    /// necklace off a corpse has one, two or three GRADING properties 8 / 17 / 75%
    /// of the time, like every other source. Before, it was the recorded retail
    /// instance's: 34 / 30 / 36% over the 552 in the corpus.
    #[test]
    fn enemy_loot_jewellery_follows_the_property_count_split() {
        use crate::features::jewelry_grades::JEWELRY_PROPERTY_COUNT_PERCENT;
        // A group the corpus never saw rolls the general table, which pays most
        // of the recorded jewellery (about 4% of corpses at enemy level 13).
        let group = Uuid::from_u128(0x3930_0000_0000_4000_8000_0000_0000_0393);
        let mut count = [0u64; 4];
        let mut enemy = 0usize;
        while count.iter().sum::<u64>() < 2500 && enemy < 400_000 {
            let dungeon = Uuid::from_u128(0x393 + (enemy / 64) as u128);
            for loot in roll_enemy_loot(&dungeon, &group, 0, enemy % 64, 13).values() {
                for item in loot.item.0.values() {
                    if item.grade.is_none() {
                        continue;
                    }
                    let g = &item.properties.grading;
                    assert_eq!(item.grade, Some(g.iter().map(|p| p.tier).sum()), "grade = sum of tiers");
                    count[g.len()] += 1;
                }
            }
            enemy += 1;
        }
        let n = count.iter().sum::<u64>();
        assert!(n >= 2000, "only {n} jewels on {enemy} corpses");
        eprintln!("#393 enemy loot: {count:?} of {n}");
        for (props, pct) in JEWELRY_PROPERTY_COUNT_PERCENT {
            let got = count[props] as f64 * 100.0 / n as f64;
            assert!((got - f64::from(pct)).abs() <= 3.0, "{props} properties: {got:.1}% (want {pct}%)");
        }
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

    #[test]
    fn a_run_seed_changes_the_described_loot_but_stays_stable() {
        let game_data = game_data();
        let instance = Uuid::from_u128(0x2500_0000_0000_0000_0000_0000_0000_0252);
        let seed_a = run_loot_seed(&instance, 0);
        let seed_b = run_loot_seed(&instance, 1);
        let mut found_seeded_loot = false;

        for dungeon_id in game_data.dungeons.keys() {
            let a = generate_for_dungeon_with_seed(&game_data, dungeon_id, seed_a, 20, 5).unwrap();
            let a_again =
                generate_for_dungeon_with_seed(&game_data, dungeon_id, seed_a, 20, 5).unwrap();
            assert_eq!(
                serde_json::to_value(&a).unwrap(),
                serde_json::to_value(&a_again).unwrap(),
                "the same run seed must describe the same dungeon"
            );

            let b = generate_for_dungeon_with_seed(&game_data, dungeon_id, seed_b, 20, 5).unwrap();
            if serde_json::to_value(&a).unwrap() != serde_json::to_value(&b).unwrap() {
                found_seeded_loot = true;
                break;
            }
        }

        assert!(
            found_seeded_loot,
            "changing the run seed never changed any generated dungeon loot"
        );
    }
}

#[cfg(test)]
mod spawn_table_tests {
    use super::*;

    /// The Wizard's Challenge (SQ206), where breaking the pots is the puzzle.
    const WIZARDS_CHALLENGE: &str = "108a7290-cea1-4af2-b58d-9c592af7d9d8";
    /// `Items.Name.DoorKey`. 269 of the table's 269 observed draws are this.
    const DOOR_KEY: &str = "faa3aeb3-9284-4d83-8981-1af00e3a6398";
    const KEY_TABLE: &str = "8858f284-4f33-4da4-8085-0befa7ef2637";
    /// The three pots retail hung the key table on, of that dungeon's sixty.
    const KEY_SPAWNS: [&str; 3] = [
        "9f2a4d7d-debf-457f-8007-19a0e40dfb0c",
        "588b0b05-d460-4c07-96f5-5a9c18f882c2",
        "5a448ce2-8c35-4e22-b4c9-323a2b9a85ff",
    ];
    /// The table all three resolve to through their interactable: Lumber, and
    /// never a key. This is the whole bug in one constant.
    const LUMBER_TABLE: &str = "49173c2c-e34c-4664-b2cd-d1bae46c39fa";
    /// Retail hung this one on the same pots too; 74% of its draws are empty.
    const LIMESTONE_TABLE: &str = "37a41796-7a32-4119-8444-60c48a445b5b";

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("read parsed.json"))
            .expect("parse game data")
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).expect("uuid")
    }

    /// Compiled in and holding what was mined. Parsed explicitly so a
    /// deserialization failure names itself instead of silently degrading to
    /// "no spawn has an extra table", which is the pre-fix behaviour and looks
    /// exactly like working code.
    #[test]
    fn the_spawn_table_sidecar_loads() {
        let v: serde_json::Value =
            serde_json::from_str(SPAWN_LOOT_TABLES_RAW).expect("spawn_loot_tables.json must parse");
        let spawns = v["spawns"].as_object().expect("spawns object");
        assert_eq!(spawns.len(), 1285, "the mined floor spawn points");
        assert_eq!(v["_meta"]["dungeons"].as_u64(), Some(3033));

        let on_key_table: Vec<&String> = spawns
            .iter()
            .filter(|(_, e)| {
                e["tables"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|v| v.as_str() == Some(KEY_TABLE)))
            })
            .map(|(k, _)| k)
            .collect();
        assert_eq!(on_key_table.len(), 11, "spawn points carrying the DoorKey table");
    }

    #[test]
    fn every_mined_spawn_is_a_real_floor_spawn() {
        let game_data = game_data();
        let known: std::collections::HashSet<Uuid> = game_data
            .dungeons
            .values()
            .flat_map(|d| d.spawn_info.item.keys().copied())
            .collect();
        let v: serde_json::Value = serde_json::from_str(SPAWN_LOOT_TABLES_RAW).unwrap();
        let unknown: Vec<&String> = v["spawns"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|id| !known.contains(&uuid(id)))
            .collect();
        assert!(unknown.is_empty(), "mined spawns absent from parsed.json: {unknown:?}");
    }

    /// THE BUG (#192). Three of the sixty pots in The Wizard's Challenge hold
    /// the key that opens the final door. Their interactable names only the
    /// Lumber table, so the key never dropped and the quest could not be
    /// finished — "I'm not getting any keys for a pot".
    #[test]
    fn the_wizards_challenge_pots_yield_the_door_key() {
        let game_data = game_data();
        let generated =
            generate_for_dungeon(&game_data, &uuid(WIZARDS_CHALLENGE), 20, 5).expect("dungeon");
        assert_eq!(generated.item_generated_data.len(), 60, "retail's sixty pots");

        for spawn in KEY_SPAWNS {
            let results = generated
                .item_generated_data
                .get(&uuid(spawn))
                .unwrap_or_else(|| panic!("{spawn} is a spawn of this dungeon"));
            let keys: u64 = results
                .iter()
                .flat_map(|r| r.loot_table_loot.values())
                .filter_map(|loot| loot.stackable_items.get(&uuid(DOOR_KEY)))
                .sum();
            assert!(keys >= 1, "pot {spawn} yielded no door key");
        }
    }

    /// CONTROL, and the one that keeps the test above honest: the other
    /// fifty-seven pots must NOT hand out keys. A repair that sprayed the key
    /// table over the dungeon would pass the test above and make the puzzle
    /// meaningless.
    #[test]
    fn the_other_pots_do_not() {
        let game_data = game_data();
        let generated =
            generate_for_dungeon(&game_data, &uuid(WIZARDS_CHALLENGE), 20, 5).expect("dungeon");
        let key_spawns: std::collections::HashSet<Uuid> =
            KEY_SPAWNS.iter().map(|s| uuid(s)).collect();
        let stray: Vec<Uuid> = generated
            .item_generated_data
            .iter()
            .filter(|(spawn, results)| {
                !key_spawns.contains(spawn)
                    && results.iter().any(|r| {
                        r.loot_table_loot
                            .values()
                            .any(|l| l.stackable_items.contains_key(&uuid(DOOR_KEY)))
                    })
            })
            .map(|(spawn, _)| *spawn)
            .collect();
        assert!(stray.is_empty(), "pots that should hold no key do: {stray:?}");
    }

    /// The union never takes a table away. Six of the 1,285 mined spawns name
    /// fewer tables than their interactable does — a corpus gap, not a
    /// correction — and replacing instead of unioning would silently empty
    /// them.
    #[test]
    fn the_interactables_own_tables_survive_the_union() {
        // That pot gains two: the key, and the Limestone table retail also hung
        // on it. The interactable's own table stays, and stays first.
        assert_eq!(
            tables_for_spawn(&uuid(KEY_SPAWNS[0]), &[uuid(LUMBER_TABLE)]),
            vec![uuid(LUMBER_TABLE), uuid(LIMESTONE_TABLE), uuid(KEY_TABLE)],
            "the interactable's table comes first and the mined ones are added"
        );
        // A table already present is not added twice: a duplicate would collapse
        // in the HashMap anyway, but the vector is what the roll iterates.
        assert_eq!(
            tables_for_spawn(
                &uuid(KEY_SPAWNS[0]),
                &[uuid(KEY_TABLE), uuid(LUMBER_TABLE), uuid(LIMESTONE_TABLE)]
            )
            .len(),
            3
        );
    }

    /// CONTROL: a spawn the corpus never saw keeps exactly what it had. 691 of
    /// the 1,976 floor spawns are unobserved, and inventing a table for them
    /// would be fabrication rather than repair.
    #[test]
    fn an_unobserved_spawn_is_left_alone() {
        // A uuid no dungeon uses; the corpus is keyed by real spawn ids.
        let never_seen = uuid("ffffffff-ffff-4fff-8fff-ffffffffffff");
        assert_eq!(
            tables_for_spawn(&never_seen, &[uuid(LUMBER_TABLE)]),
            vec![uuid(LUMBER_TABLE)]
        );
        assert!(tables_for_spawn(&never_seen, &[]).is_empty());
    }

    /// Adding a table must not move what the others pay. Each table is rolled
    /// under its own seed, so the Lumber result on a key pot is the same value
    /// it was before the key table joined it — otherwise this fix would quietly
    /// re-roll loot across 700 spawns.
    #[test]
    fn adding_a_table_does_not_disturb_the_others() {
        let game_data = game_data();
        let dungeon = uuid(WIZARDS_CHALLENGE);
        let generated = generate_for_dungeon(&game_data, &dungeon, 20, 5).expect("dungeon");
        let spawn = uuid(KEY_SPAWNS[0]);
        let lumber = uuid(LUMBER_TABLE);
        let from_generation = generated.item_generated_data[&spawn][0]
            .loot_table_loot
            .get(&lumber)
            .expect("the interactable's own table is still rolled");
        // The pot's table is the medium breakable's, filled to its stack count
        // (#388) under the table's own seeds — still independent of the key table.
        let name = game_data.dungeons[&dungeon].spawn_info.item[&spawn].name.clone();
        let direct =
            roll_container_table(default_seed_context(&dungeon), &spawn, name.as_deref(), &lumber, 0, 20);
        assert_eq!(
            serde_json::to_value(from_generation).unwrap(),
            serde_json::to_value(&direct).unwrap(),
        );
    }

    /// The corpus pays out where the APK mapping said nothing. Not an assertion
    /// about one dungeon: across every dungeon, the number of floor spawns that
    /// produce something must rise well past what the interactables alone reach.
    #[test]
    fn the_corpus_recovers_loot_across_the_game() {
        let game_data = game_data();
        let mut paying = 0;
        let mut with_extra_table = 0;
        for dungeon_id in game_data.dungeons.keys() {
            let generated = generate_for_dungeon(&game_data, dungeon_id, 20, 5).unwrap();
            for (spawn, results) in &generated.item_generated_data {
                let from_interactable = game_data
                    .dungeons
                    .get(dungeon_id)
                    .and_then(|d| d.spawn_info.item.get(spawn))
                    .and_then(|s| s.apparition_settings.first())
                    .and_then(|a| game_data.interactables.get(&a.interactable_uuid))
                    .map(|i| i.loot_table.len())
                    .unwrap_or(0);
                let rolled = results.first().map(|r| r.loot_table_loot.len()).unwrap_or(0);
                if rolled > from_interactable {
                    with_extra_table += 1;
                }
                if results.iter().any(|r| {
                    r.loot_table_loot.values().any(|l| {
                        !l.stackable_items.is_empty() || !l.currencies.is_empty() || !l.item.is_empty()
                    })
                }) {
                    paying += 1;
                }
            }
        }
        assert!(
            with_extra_table >= 690,
            "only {with_extra_table} spawns gained a table; the corpus adds one to 700"
        );
        assert!(paying > 600, "only {paying} floor spawns pay anything");
    }
}

/// Report #236: "In Battle Unceasing sigil quest Merc didn't drop a key so
/// unable to complete quest." Enemies that hold a door key.
#[cfg(test)]
mod key_holder_tests {
    use super::*;

    /// EQ15_Stone_DungeonSettings, the dungeon of "A Battle Unceasing".
    const EQ15: &str = "924f1147-fd7f-4736-9e2d-f33fa942dbdd";
    /// Its Mercenary: the enemy the key rides on.
    const MERCENARY: &str = "a91ebfe6-0167-4643-bd6f-ed27d8dfad41";
    const DOOR_KEY: &str = "faa3aeb3-9284-4d83-8981-1af00e3a6398";
    const KEY_TABLE: &str = "8858f284-4f33-4da4-8085-0befa7ef2637";
    /// The groups retail keyed the DoorKey loot TABLE on: EQ13's key-holder,
    /// SQ201's, and three in SQ205_B.
    const KEY_TABLE_GROUPS: [(&str, &str); 5] = [
        ("413d5dfb-13b6-4a7c-84b1-81e008648f19", "101b2285-0679-4d5c-92d5-a34045f145ce"),
        ("0ded6e84-d942-434b-8563-33ef301f6189", "379775a2-8015-4c8c-a503-0691597528a0"),
        ("7f3ca11c-bfc7-40ec-bf72-7fb5c45ddbd0", "51ccfdc2-e8f5-4ff3-bc62-0d72eee97387"),
        ("7f3ca11c-bfc7-40ec-bf72-7fb5c45ddbd0", "7c4c0cc5-4240-4676-a372-6fc06bbeb5d9"),
        ("7f3ca11c-bfc7-40ec-bf72-7fb5c45ddbd0", "bd3eecc3-5f10-41ad-8a6f-f796e17d7d6e"),
    ];

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("read parsed.json"))
            .expect("parse game data")
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).expect("uuid")
    }

    fn keys_on(enemy: &DungeonEnemyResult) -> u64 {
        enemy
            .merged_loot_table()
            .stackable_items
            .get(&uuid(DOOR_KEY))
            .copied()
            .unwrap_or(0)
    }

    /// THE BUG. The Mercenary must die holding the key, at any level: the event
    /// is served to level-1 and level-89 characters alike.
    #[test]
    fn the_eq15_mercenary_drops_the_door_key() {
        let game_data = game_data();
        for level in [1, 14, 89] {
            let generated = generate_for_dungeon(&game_data, &uuid(EQ15), level, 100).unwrap();
            let mercs = &generated.enemy_generated_data[&uuid(MERCENARY)];
            assert_eq!(mercs.len(), 1, "one Mercenary spawner");
            let merc = &mercs[0][0];
            assert_eq!(keys_on(merc), 1, "level {level}: the Mercenary holds no key");
            // Where retail put it: the bare spawnGroupLoot result.
            assert_eq!(
                serde_json::to_value(&merc.spawn_group_loot).unwrap(),
                serde_json::json!({ "stackableItems": { DOOR_KEY: 1 } }),
            );
        }
    }

    /// CONTROL: one key, on one enemy. A key on every corpse would pass the test
    /// above and make the locked door meaningless.
    #[test]
    fn no_other_eq15_enemy_holds_a_key() {
        let game_data = game_data();
        let generated = generate_for_dungeon(&game_data, &uuid(EQ15), 14, 100).unwrap();
        let mut keys = 0;
        for (group, spawners) in &generated.enemy_generated_data {
            for enemy in spawners.iter().flatten() {
                if *group != uuid(MERCENARY) {
                    assert_eq!(keys_on(enemy), 0, "group {group} holds a key");
                    assert!(enemy.spawn_group_loot.is_empty(), "group {group}");
                }
                keys += keys_on(enemy);
            }
        }
        assert_eq!(keys, 1, "exactly one key in the dungeon");
    }

    /// CONTROL across the whole game: the only filled positions are the four
    /// exact group/spawner pairs in the complete protected capture corpus.
    #[test]
    fn spawn_group_loot_is_empty_everywhere_else() {
        let game_data = game_data();
        let mut filled = std::collections::HashSet::new();
        for dungeon in game_data.dungeons.values() {
            for (group, spawn) in &dungeon.spawn_info.enemy_spawn_groups {
                for spawner_index in 0..spawn.quantity.max(1) as usize {
                    if !spawn_group_loot(group, spawner_index).is_empty() {
                        filled.insert((*group, spawner_index));
                    }
                }
            }
        }
        let expected = [
            MERCENARY,
            "9cb35702-def0-441e-b760-9c8a23ffd8bf",
            "3d0a198d-6ab7-4c54-addf-1e67c55c3e78",
            "681c0add-1f49-46cc-bafb-e240680406e7",
        ]
        .into_iter()
        .map(|group| (uuid(group), 0))
        .collect();
        assert_eq!(filled, expected);
    }

    /// The same bug by the other road. These five groups roll the DoorKey TABLE
    /// in lootTableLoot, which the enemy corpus left out as too thin; it was sent
    /// empty, so their keys never dropped either.
    #[test]
    fn enemies_on_the_key_table_drop_the_key() {
        for (dungeon, group) in KEY_TABLE_GROUPS {
            for level in [1, 40, 89] {
                let loot = roll_enemy_loot(&uuid(dungeon), &uuid(group), 0, 0, level);
                let key_roll = loot
                    .get(&uuid(KEY_TABLE))
                    .unwrap_or_else(|| panic!("{group} no longer rolls the key table"));
                assert_eq!(
                    key_roll.stackable_items.get(&uuid(DOOR_KEY)),
                    Some(&1),
                    "{group} at level {level}"
                );
                assert!(enemy_table_is_unmodelled(&uuid(KEY_TABLE)));
            }
        }
    }

    /// CONTROL: a table known to NEITHER corpus is still sent empty, not
    /// invented. 2d366ee0 is one the enemy corpus also left out.
    #[test]
    fn a_table_no_corpus_knows_stays_empty() {
        let unknown = uuid("2d366ee0-8087-4d1d-8161-64a7b3e14f93");
        assert!(enemy_table_is_unmodelled(&unknown));
        assert!(interactable_table_results(&unknown, 30).is_none());
        let groups: Vec<Uuid> = enemy_group_tables()["groups"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, g)| g.to_string().contains("2d366ee0-8087-4d1d-8161-64a7b3e14f93"))
            .map(|(id, _)| uuid(id))
            .collect();
        assert!(!groups.is_empty(), "the corpus keys this table on some group");
        let game_data = game_data();
        for group in groups {
            let dungeon = dungeon_owning_spawn_group(&game_data, &group).unwrap_or(uuid(EQ15));
            for spawner in 0..4 {
                if let Some(roll) = roll_enemy_loot(&dungeon, &group, spawner, 0, 30).get(&unknown) {
                    assert!(roll.is_empty(), "{group}: invented loot for an unknown table");
                }
            }
        }
        // And the gold table is modelled, so it is never "unmodelled".
        assert!(!enemy_table_is_unmodelled(&Uuid::from_u128(GOLD_LOOT_TABLE_ID)));
    }
}

/// Report #301: "The Troll Trap" cannot be finished. Its kill objective counts
/// the trolls of ONE spawner, which retail filled with three; we sent one.
#[cfg(test)]
mod spawner_count_tests {
    use super::*;

    /// GH02_DungeonSettings, the dungeon of "The Troll Trap" (quest 45c042e1).
    const GH02: &str = "4216705a-c11d-4a6c-ba68-7b4c94adf1ff";
    /// The troll group its objective counts (spawner b25cbdbd).
    const TROLLS: &str = "d971b82c-7113-4c95-84da-cbb2a4ad29f3";

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("read parsed.json"))
            .expect("parse game data")
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).expect("uuid")
    }

    /// Per-spawner list lengths in an `enemyGeneratedData` map.
    fn shape(generated: &DungeonGeneratedData) -> HashMap<Uuid, Vec<usize>> {
        generated
            .enemy_generated_data
            .iter()
            .map(|(group, spawners)| (*group, spawners.iter().map(Vec::len).collect()))
            .collect()
    }

    /// Every spawner whose enemy count is not the APK's max, as
    /// (group, spawner index, got, want).
    fn short_spawners(generated: &DungeonGeneratedData) -> Vec<(Uuid, usize, usize, usize)> {
        let mut out = Vec::new();
        for (group, spawners) in &generated.enemy_generated_data {
            let want = spawn_group_counts()
                .groups
                .get(group)
                .map_or(1, |c| c.max as usize);
            for (i, enemies) in spawners.iter().enumerate() {
                if enemies.len() != want {
                    out.push((*group, i, enemies.len(), want));
                }
            }
        }
        out
    }

    #[test]
    fn the_counts_sidecar_loads_and_is_sane() {
        let counts = &spawn_group_counts().groups;
        assert_eq!(counts.len(), 1956, "the APK has 1,956 enemy spawn groups");
        assert_eq!(counts.values().filter(|c| c.max > 1).count(), 96);
        for (group, c) in counts {
            assert!(
                1 <= c.min && c.min <= c.max,
                "{group}: {}..{}",
                c.min,
                c.max
            );
        }
        assert_eq!(enemies_per_spawner(&uuid(TROLLS)), 3);
        // Not an APK group (our own content, a future dungeon): one, as before.
        assert_eq!(enemies_per_spawner(&Uuid::nil()), 1);
    }

    /// THE BUG, against retail's own answer: `/quests/45c042e1…/accept`
    /// (api_captures 64099) sent this dungeon one spawner per group, three
    /// trolls in d971b82c, two spiders each in d8f00672 and 4e41da9a, and one
    /// enemy everywhere else.
    #[test]
    fn the_troll_trap_matches_retails_accept() {
        let generated = generate_for_dungeon(&game_data(), &uuid(GH02), 14, 100).unwrap();
        let retail: HashMap<Uuid, Vec<usize>> = [
            (TROLLS, 3),
            ("d8f00672-6937-410c-b17c-11a91aedded2", 2),
            ("4e41da9a-dd8d-4a3b-8be5-d7c99e86beb8", 2),
            ("f5408a4f-15a5-4ffb-a84a-5ae2f10f7b05", 1),
            ("e7855391-22dc-438f-8ccc-3e5387dea24f", 1),
            ("56ea94f0-b613-4fd3-8bee-e0e193db63a8", 1),
            ("aa1d8511-a559-40d4-aea8-40c08e0d9846", 1),
            ("b28562b5-03c7-48f8-b9ef-c12d0966d7b7", 1),
        ]
        .into_iter()
        .map(|(g, n)| (uuid(g), vec![n]))
        .collect();
        assert_eq!(shape(&generated), retail);
    }

    /// IDENTITY across the whole game: every spawner of every group holds the
    /// APK's max -- and the 96 groups above one are all actually exercised.
    #[test]
    fn every_spawner_holds_the_apks_max() {
        let game_data = game_data();
        let mut multi_seen = std::collections::HashSet::new();
        for dungeon_id in game_data.dungeons.keys() {
            let generated = generate_for_dungeon(&game_data, dungeon_id, 20, 5).unwrap();
            assert_eq!(short_spawners(&generated), vec![], "dungeon {dungeon_id}");
            for (group, spawners) in &generated.enemy_generated_data {
                if spawners.iter().any(|e| e.len() > 1) {
                    multi_seen.insert(*group);
                }
            }
        }
        assert_eq!(multi_seen.len(), 96, "a max>1 group was never generated");
    }

    /// NEGATIVE CONTROL: the identity check above fails on the old shape (one
    /// enemy per spawner), so it cannot pass vacuously.
    #[test]
    fn the_identity_check_rejects_one_enemy_per_spawner() {
        let mut generated = generate_for_dungeon(&game_data(), &uuid(GH02), 14, 100).unwrap();
        for enemies in generated.enemy_generated_data.values_mut().flatten() {
            enemies.truncate(1);
        }
        let mut short: Vec<_> = short_spawners(&generated)
            .into_iter()
            .map(|(g, _, got, want)| (g.to_string()[..8].to_owned(), got, want))
            .collect();
        short.sort();
        assert_eq!(
            short,
            vec![
                ("4e41da9a".to_owned(), 1, 2),
                ("d8f00672".to_owned(), 1, 2),
                ("d971b82c".to_owned(), 1, 3),
            ]
        );
    }

    /// Adding enemies must not re-roll the ones a player could already have
    /// seen: enemy 0 is exactly the roll it always was, the others are rolled
    /// with their own index, and the key stays on enemy 0 only.
    #[test]
    fn extra_enemies_roll_their_own_loot_and_enemy_zero_is_unchanged() {
        let dungeon = uuid(GH02);
        let generated = generate_for_dungeon(&game_data(), &dungeon, 14, 100).unwrap();
        let trolls = &generated.enemy_generated_data[&uuid(TROLLS)][0];
        for (enemy_index, troll) in trolls.iter().enumerate() {
            assert_eq!(
                serde_json::to_value(&troll.loot_table_loot).unwrap(),
                serde_json::to_value(roll_enemy_loot(&dungeon, &uuid(TROLLS), 0, enemy_index, 14))
                    .unwrap(),
                "troll {enemy_index}"
            );
            assert_eq!((troll.enemy_level, troll.given_xp), (14, 100));
        }
        // The key-holder (one enemy per spawner) is untouched by this change, and a
        // multi-enemy group never puts spawnGroupLoot past enemy zero.
        for spawners in generated.enemy_generated_data.values() {
            for enemies in spawners {
                assert!(enemies[1..].iter().all(|e| e.spawn_group_loot.is_empty()));
            }
        }
    }
}
