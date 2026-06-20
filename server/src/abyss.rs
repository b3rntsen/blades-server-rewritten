//! Abyss endless-dungeon mode endpoints.
//!
//! Wire shapes confirmed against `api_captures` (character 78f2b668 / 97cf5fa6):
//!
//! `POST /abysses/current`            → `{abyss: null | AbyssWire}`
//! `POST /abysses/current/start`      → `{abyss: AbyssWire, abyssDungeonGeneratedData: {...}}`
//! `POST /abysses/current/update`     → `{abyssFutureRewards, character, abyssProgress, inventory}`
//! `POST /abysses/current/end`        → `{reward, character, wallet, inventory}`
//!
//! State is persisted in `characters.server_state` JSONB (`server_state.abyss`).
//! Rewards on `/end` scale with the highest floor reached:
//!   - Gold: `50 * floors_completed`
//!   - XP:   `10 * floors_completed`
//!   (plausible proxy; prod used item drops + currency packs scaled by difficulty)

use std::sync::Arc;

use actix_web::{
    post,
    web::{self, Json},
};
use blades_lib::{
    economy::{RewardGrant, apply_reward},
    server_state::{AbyssRun, AbyssSliceEntry},
    user_data::{CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
                DungeonGeneratedData, InventoryChangeTracker},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
    util::check_permission_for_character_and_get_it,
};

// ────────────────────────────────────────────────────────────────────────────
// Wire types
// ────────────────────────────────────────────────────────────────────────────

/// One slice as the client expects it.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AbyssSliceWire {
    dungeon_settings_id: Uuid,
    difficulty_level: u32,
    hardcore: bool,
    slice_index: u32,
    floor_index: u32,
    completed: bool,
    enemy_killed: bool,
}

/// The `abyss` object returned inside `/current` and `/start`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AbyssWire {
    slices: Vec<AbyssSliceWire>,
    revive_count: u32,
    initial_player_level: u32,
    seed: i64,
    score: f64,
    algorithm_version: u32,
    version: u32,
    abyss_future_rewards: Vec<AbyssFutureRewardWire>,
}

/// One future-reward threshold wire entry.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AbyssFutureRewardWire {
    reward: AbyssFutureRewardInner,
    score: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AbyssFutureRewardInner {
    stackable_items: std::collections::HashMap<Uuid, u64>,
}

// ────────────────────────────────────────────────────────────────────────────
// POST /abysses/current  — get current run (null if none)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct GetAbyssResponse {
    abyss: Option<AbyssWire>,
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/abysses/current")]
pub async fn get_abyss(
    path: web::Path<Uuid>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<GetAbyssResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let app_state = app_state.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    let entry = load_economy(&mut conn, &session.session, character_id).await?;
    let run = entry.server_state.0.abyss.as_ref();
    let wire = run.map(|r| run_to_wire(r, &app_state));
    Ok(Json(GetAbyssResponse { abyss: wire }))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /abysses/current/start
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAbyssRequest {
    starting_difficulty: Option<u32>,
}

/// The `abyssDungeonGeneratedData` object returned alongside the run on `/start`.
/// This is a top-level key in the response (not nested inside `abyss`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AbyssDungeonGeneratedData {
    quest_id: Uuid,
    #[serde(flatten)]
    inner: DungeonGeneratedData,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartAbyssResponse {
    abyss: AbyssWire,
    abyss_dungeon_generated_data: AbyssDungeonGeneratedData,
}

/// Sentinel UUID used for abyss generated-data questId (captured from prod).
const ABYSS_QUEST_ID: &str = "ab133000-0000-0000-0000-000000000000";

/// Known spawn-group UUIDs seen in the captured abyss floor 1 generated data.
/// We use these same IDs so the client can resolve them. The gold drops here are
/// plausible proxies; prod values varied by difficulty/floor.
const SPAWN_GROUP_A: &str = "c41668b3-ad8b-42b4-ba5d-a0574039a3cc";
const SPAWN_GROUP_B: &str = "9a057ca6-5f8d-4700-8665-6c56de0e1103";
/// Gold currency UUID (captured from both abyss and quest loot responses).
const GOLD_CURRENCY_UUID: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";
/// Generic loot-table UUID observed in the floor-1 captured generated data.
const LOOT_TABLE_UUID: &str = "2d366ee0-8087-4d1d-8161-64a7b3e14f93";

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/abysses/current/start"
)]
pub async fn start_abyss(
    path: web::Path<Uuid>,
    _body: Json<StartAbyssRequest>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<StartAbyssResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let app_state = app_state.into_inner(); // Arc<ServerGlobal>
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(|mut conn| {
        let app_state = app_state.clone();
        async move {
            let mut entry =
                load_economy_for_update(&mut conn, &session.session, character_id).await?;

            let player_level = entry.character.0.level as u32;
            let seed = generate_seed(character_id);
            let static_abyss = &app_state.static_data.abyss;

            let slices = build_slices(static_abyss, seed, 150);

            let run = AbyssRun {
                slices,
                revive_count: 0,
                initial_player_level: player_level,
                seed,
                score: 0.0,
                algorithm_version: static_abyss.algorithm_version.max(1),
                version: 1,
                current_floor_index: 0,
            };

            let wire = run_to_wire(&run, &app_state);

            // Persist run into server_state
            entry.server_state.0.abyss = Some(run);
            save_economy(&mut conn, character_id, &entry).await?;

            // Build the generated dungeon data for the current (first) floor.
            let gen_data = build_generated_data();

            Ok::<_, BladeApiError>(Json(StartAbyssResponse {
                abyss: wire,
                abyss_dungeon_generated_data: AbyssDungeonGeneratedData {
                    quest_id: Uuid::parse_str(ABYSS_QUEST_ID).unwrap(),
                    inner: gen_data,
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

// ────────────────────────────────────────────────────────────────────────────
// POST /abysses/current/update
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnemyKilledAction {
    #[allow(dead_code)]
    spawn_group_id: Uuid,
    #[allow(dead_code)]
    spawner_index: usize,
    #[allow(dead_code)]
    enemy_index: usize,
    #[allow(dead_code)]
    xp_reward: f64,
    #[allow(dead_code)]
    time: u64,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AbyssUpdateAction {
    EnemyKilled(EnemyKilledAction),
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAbyssRequest {
    #[allow(dead_code)]
    current_state: Option<Value>,
    #[serde(default)]
    actions: Vec<AbyssUpdateAction>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AbyssProgressWire {
    revive_count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAbyssResponse {
    abyss_future_rewards: Vec<AbyssFutureRewardWire>,
    character: CompleteCharacterWithIdWithoutData,
    abyss_progress: AbyssProgressWire,
    inventory: CompleteInventoryUpdate,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/abysses/current/update"
)]
pub async fn update_abyss(
    path: web::Path<Uuid>,
    body: Json<UpdateAbyssRequest>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<UpdateAbyssResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let app_state = app_state.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(|mut conn| {
        let app_state = app_state.clone();
        async move {
            let mut entry =
                load_economy_for_update(&mut conn, &session.session, character_id).await?;

            let tracker = InventoryChangeTracker::default();

            // Advance floor: mark current floor completed, move to next.
            if let Some(run) = entry.server_state.0.abyss.as_mut() {
                // Count enemy_killed actions that advance the floor.
                let enemy_killed_count = body.actions.iter().filter(|a| {
                    matches!(a, AbyssUpdateAction::EnemyKilled(_))
                }).count();

                // Mark the current floor completed when any enemy-killed action arrives
                // (the client sends one action per enemy; the floor completes when all die).
                // Lenient: we advance on ANY enemy_killed — avoids stalling on sparse captures.
                if enemy_killed_count > 0 {
                    if let Some(slice) = run.slices.get_mut(run.current_floor_index) {
                        slice.enemy_killed = true;
                        slice.completed = true;
                    }
                    // Score: 1 point per enemy killed
                    run.score += enemy_killed_count as f64;
                    // Advance floor pointer
                    if run.current_floor_index + 1 < run.slices.len() {
                        run.current_floor_index += 1;
                    }
                }

                let revive_count = run.revive_count;
                let future_rewards = build_future_rewards(&app_state);

                save_economy(&mut conn, character_id, &entry).await?;

                let inv = entry.inventory.0.generate_client_update(&tracker);

                Ok::<_, BladeApiError>(Json(UpdateAbyssResponse {
                    abyss_future_rewards: future_rewards,
                    character: CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: entry.character.0,
                    },
                    abyss_progress: AbyssProgressWire { revive_count },
                    inventory: inv,
                }))
            } else {
                // No active run — lenient: return empty progress rather than 404.
                let inv = entry.inventory.0.generate_client_update(&tracker);
                Ok::<_, BladeApiError>(Json(UpdateAbyssResponse {
                    abyss_future_rewards: build_future_rewards(&app_state),
                    character: CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: entry.character.0,
                    },
                    abyss_progress: AbyssProgressWire { revive_count: 0 },
                    inventory: inv,
                }))
            }
        }
        .scope_boxed()
    })
    .await
}

// ────────────────────────────────────────────────────────────────────────────
// POST /abysses/current/end
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndAbyssRequest {
    #[serde(default)]
    #[allow(dead_code)]
    actions: Vec<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EndAbyssResponse {
    reward: RewardGrant,
    character: CompleteCharacterWithIdWithoutData,
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/abysses/current/end"
)]
pub async fn end_abyss(
    path: web::Path<Uuid>,
    _body: Json<EndAbyssRequest>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<EndAbyssResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let app_state = app_state.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(|mut conn| {
        async move {
            let mut entry =
                load_economy_for_update(&mut conn, &session.session, character_id).await?;

            // Determine floors completed. Lenient: if no active run, grant nothing.
            let floors_completed = entry.server_state.0.abyss.as_ref()
                .map(|r| r.slices.iter().filter(|s| s.completed).count())
                .unwrap_or(0);

            // Update maximumAbyssLevelReached (the floorIndex = slice_index+1 of the last
            // completed slice; prod captures show it equals the highest floorIndex reached).
            let max_floor = entry.server_state.0.abyss.as_ref()
                .and_then(|r| r.slices.iter().filter(|s| s.completed).last())
                .map(|s| s.floor_index)
                .unwrap_or(0);

            if max_floor as u16 > entry.character.0.maximum_abyss_level_reached {
                entry.character.0.maximum_abyss_level_reached = max_floor as u16;
            }
            entry.character.0.version += 1;

            // Scale rewards: gold + XP proportional to floors reached.
            let reward = scale_reward(floors_completed as u32);

            let mut tracker = InventoryChangeTracker::default();
            apply_reward(
                &reward,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut entry.character.0,
                &mut tracker,
            );
            if !reward.stackable_items.is_empty() || !reward.items.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }

            // Clear the run.
            entry.server_state.0.abyss = None;

            let inv = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let character = entry.character.0.clone();

            save_economy(&mut conn, character_id, &entry).await?;

            Ok::<_, BladeApiError>(Json(EndAbyssResponse {
                reward,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character,
                },
                wallet,
                inventory: inv,
            }))
        }
        .scope_boxed()
    })
    .await
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Load the character economy entry (read-only — no row lock).
async fn load_economy(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &crate::session::Session,
    character_id: Uuid,
) -> Result<CharacterDbEntryEconomy, BladeApiError> {
    let _ = check_permission_for_character_and_get_it(conn, session, character_id).await?;

    use crate::schema::characters;
    let entry = characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(session.user_id))
        .select(CharacterDbEntryEconomy::as_select())
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(BladeApiError::unauthorized)?;
    Ok(entry)
}

/// Load the character economy entry with a FOR NO KEY UPDATE row lock (inside txn).
async fn load_economy_for_update(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &crate::session::Session,
    character_id: Uuid,
) -> Result<CharacterDbEntryEconomy, BladeApiError> {
    use crate::schema::characters;
    let entry = characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(session.user_id))
        .select(CharacterDbEntryEconomy::as_select())
        .for_no_key_update()
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(BladeApiError::unauthorized)?;
    Ok(entry)
}

/// Write the economy entry back.
async fn save_economy(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
    entry: &CharacterDbEntryEconomy,
) -> Result<(), BladeApiError> {
    use crate::schema::characters;
    diesel::update(characters::table)
        .filter(characters::id.eq(character_id))
        .set(entry)
        .execute(conn)
        .await?;
    Ok(())
}

/// Deterministic seed from character UUID (XOR of upper/lower 64-bit halves).
fn generate_seed(character_id: Uuid) -> i64 {
    let b = character_id.as_bytes();
    let hi = i64::from_le_bytes(b[0..8].try_into().unwrap());
    let lo = i64::from_le_bytes(b[8..16].try_into().unwrap());
    hi ^ lo
}

/// Build `n` slices: first `min(n, fixed.len())` from the fixed list, the rest
/// cycling through the random pool deterministically using the run seed.
fn build_slices(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    seed: i64,
    n: usize,
) -> Vec<AbyssSliceEntry> {
    let mut slices = Vec::with_capacity(n);
    for i in 0..n {
        let (dungeon_uuid, diff) = if i < static_abyss.fixed_slices.len() {
            let fs = &static_abyss.fixed_slices[i];
            (fs.dungeon_settings_id, fs.difficulty_level)
        } else if !static_abyss.random_pool.is_empty() {
            let idx = ((seed.unsigned_abs() as usize) + i) % static_abyss.random_pool.len();
            (static_abyss.random_pool[idx], 100)
        } else {
            // Fallback: repeat last fixed slice
            let last = static_abyss.fixed_slices.last().unwrap();
            (last.dungeon_settings_id, 100)
        };
        slices.push(AbyssSliceEntry {
            dungeon_settings_id: dungeon_uuid,
            difficulty_level: diff,
            hardcore: false,
            slice_index: i as u32,
            floor_index: (i + 1) as u32,
            completed: false,
            enemy_killed: false,
        });
    }
    slices
}

/// Convert a server-side `AbyssRun` to the wire shape.
fn run_to_wire(run: &AbyssRun, app_state: &ServerGlobal) -> AbyssWire {
    let slices = run.slices.iter().map(|s| AbyssSliceWire {
        dungeon_settings_id: s.dungeon_settings_id,
        difficulty_level: s.difficulty_level,
        hardcore: s.hardcore,
        slice_index: s.slice_index,
        floor_index: s.floor_index,
        completed: s.completed,
        enemy_killed: s.enemy_killed,
    }).collect();

    AbyssWire {
        slices,
        revive_count: run.revive_count,
        initial_player_level: run.initial_player_level,
        seed: run.seed,
        score: run.score,
        algorithm_version: run.algorithm_version,
        version: run.version,
        abyss_future_rewards: build_future_rewards(app_state),
    }
}

/// Build the future-rewards wire list from static data.
fn build_future_rewards(app_state: &ServerGlobal) -> Vec<AbyssFutureRewardWire> {
    app_state.static_data.abyss.future_rewards.iter().map(|fr| {
        AbyssFutureRewardWire {
            score: fr.score,
            reward: AbyssFutureRewardInner {
                stackable_items: fr.stackable_items.clone(),
            },
        }
    }).collect()
}

/// Build the per-floor generated dungeon data.
/// Uses the same spawn-group UUIDs observed in the captured floor-1 data so the
/// client can match enemies. Gold drops scale minimally with difficulty — lenient.
fn build_generated_data() -> DungeonGeneratedData {
    use std::collections::HashMap;
    use blades_lib::user_data::{LootTableResult, DungeonEnemyResult};

    let gold = Uuid::parse_str(GOLD_CURRENCY_UUID).unwrap();
    let loot_table = Uuid::parse_str(LOOT_TABLE_UUID).unwrap();
    let sg_a = Uuid::parse_str(SPAWN_GROUP_A).unwrap();
    let sg_b = Uuid::parse_str(SPAWN_GROUP_B).unwrap();

    let make_enemy = |gold_amount: u64| DungeonEnemyResult {
        enemy_level: 1,
        given_xp: 0,
        spawn_group_loot: HashMap::new(),
        loot_table_loot: {
            let mut m = HashMap::new();
            m.insert(loot_table, LootTableResult {
                currencies: {
                    let mut c = HashMap::new();
                    c.insert(gold, gold_amount);
                    c
                },
                ..Default::default()
            });
            m
        },
    };

    DungeonGeneratedData {
        enemy_generated_data: {
            let mut m = HashMap::new();
            // spawn group A: 2 spawners × 1 enemy each (matching captured shape)
            m.insert(sg_a, vec![vec![make_enemy(4)], vec![make_enemy(4)]]);
            // spawn group B: 1 spawner × 1 enemy
            m.insert(sg_b, vec![vec![make_enemy(6)]]);
            m
        },
        item_generated_data: HashMap::new(),
        chest_generated_data: HashMap::new(),
        algorithm_version: 1,
        version: 0,
    }
}

/// Floor-scaled reward for `/end`. Assumption (prod not fully captured):
///   gold = 50 * floors_completed
///   xp   = 10 * floors_completed
/// Both are plausible lower bounds; the captured first-end response showed
/// 2923 gold / 958 XP for ~15 floors.
fn scale_reward(floors_completed: u32) -> RewardGrant {
    use std::collections::HashMap;

    if floors_completed == 0 {
        return RewardGrant::default();
    }

    let gold_uuid = Uuid::parse_str(GOLD_CURRENCY_UUID).unwrap();
    let gold = (floors_completed as u64) * 195; // ~2923 / 15
    let xp = (floors_completed as u64) * 64;    // ~958 / 15

    RewardGrant {
        currencies: {
            let mut m = HashMap::new();
            m.insert(gold_uuid, gold);
            m
        },
        character_xp: xp,
        ..Default::default()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::static_data::{AbyssStaticData, AbyssFixedSlice, AbyssFutureRewardDef};

    fn test_static_abyss() -> AbyssStaticData {
        let fixed: Vec<AbyssFixedSlice> = (1u32..=24).map(|i| AbyssFixedSlice {
            dungeon_settings_id: Uuid::new_v4(),
            difficulty_level: i,
        }).collect();
        let pool: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        AbyssStaticData {
            fixed_slices: fixed,
            random_pool: pool,
            future_rewards: vec![AbyssFutureRewardDef {
                score: 35,
                stackable_items: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(Uuid::new_v4(), 1u64);
                    m
                },
            }],
            algorithm_version: 1,
            total_pregen_floors: 150,
        }
    }

    #[test]
    fn build_slices_150_floors() {
        let sd = test_static_abyss();
        let seed = 12345i64;
        let slices = build_slices(&sd, seed, 150);
        assert_eq!(slices.len(), 150);
        // First 24: fixed slices, correct floor indices
        assert_eq!(slices[0].floor_index, 1);
        assert_eq!(slices[0].slice_index, 0);
        assert_eq!(slices[23].floor_index, 24);
        assert_eq!(slices[23].difficulty_level, 24);
        // Floors 25+: from random pool, all diff=100
        assert_eq!(slices[24].difficulty_level, 100);
        assert_eq!(slices[24].floor_index, 25);
        assert_eq!(slices[149].floor_index, 150);
        // No completed/enemy_killed flags set at start
        assert!(slices.iter().all(|s| !s.completed && !s.enemy_killed));
    }

    #[test]
    fn build_slices_exact_pool_cycling() {
        let sd = test_static_abyss();
        let seed = 0i64;
        let slices = build_slices(&sd, seed, 30);
        // Floors 25–30 must all come from the pool (5 entries) in deterministic order
        for s in &slices[24..30] {
            assert!(sd.random_pool.contains(&s.dungeon_settings_id),
                "floor {} dungeon not in pool", s.floor_index);
        }
    }

    #[test]
    fn scale_reward_zero_floors() {
        let r = scale_reward(0);
        assert!(r.is_empty(), "zero floors → no reward");
    }

    #[test]
    fn scale_reward_15_floors() {
        let r = scale_reward(15);
        let gold = Uuid::parse_str(GOLD_CURRENCY_UUID).unwrap();
        assert!(r.currencies.contains_key(&gold), "gold reward present");
        assert!(*r.currencies.get(&gold).unwrap() > 0);
        assert!(r.character_xp > 0);
    }

    #[test]
    fn scale_reward_scales_linearly() {
        let r10 = scale_reward(10);
        let r20 = scale_reward(20);
        let gold = Uuid::parse_str(GOLD_CURRENCY_UUID).unwrap();
        assert_eq!(
            r20.currencies[&gold],
            r10.currencies[&gold] * 2,
            "gold scales linearly"
        );
        assert_eq!(r20.character_xp, r10.character_xp * 2, "xp scales linearly");
    }

    #[test]
    fn generate_seed_deterministic() {
        let id = Uuid::parse_str("78f2b668-97ff-45d0-99fa-7343fd059480").unwrap();
        let s1 = generate_seed(id);
        let s2 = generate_seed(id);
        assert_eq!(s1, s2, "same id → same seed");
        assert_ne!(s1, 0, "non-zero seed");
    }

    #[test]
    fn build_generated_data_has_expected_spawn_groups() {
        let gd = build_generated_data();
        let sg_a = Uuid::parse_str(SPAWN_GROUP_A).unwrap();
        let sg_b = Uuid::parse_str(SPAWN_GROUP_B).unwrap();
        assert!(gd.enemy_generated_data.contains_key(&sg_a));
        assert!(gd.enemy_generated_data.contains_key(&sg_b));
        // sg_a: 2 spawners × 1 enemy
        assert_eq!(gd.enemy_generated_data[&sg_a].len(), 2);
        // sg_b: 1 spawner × 1 enemy
        assert_eq!(gd.enemy_generated_data[&sg_b].len(), 1);
    }
}
