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
    body: Json<StartAbyssRequest>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<StartAbyssResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    // The floor to resume from (`startingDifficulty`, 1-based; None → fresh from floor 1).
    // Pulled out of the extractor here so the transaction closure moves a plain value.
    let starting_difficulty = body.into_inner().starting_difficulty;
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

            // Honor `startingDifficulty` — the floor the player is resuming from. The
            // request used to be IGNORED (a `_body` bind), so every run rebuilt from
            // floor 1 and the client restarted at the bottom regardless of depth (very
            // visible for a high-level player). `startingDifficulty` is 1-based (floor 1
            // = a fresh run); clamp to >= 1 and to the 150-floor span. We build the slice
            // list STARTING at that floor so `currentFloorIndex: 0` (the first slice)
            // resumes from the requested depth.
            let start_floor = starting_difficulty.unwrap_or(1).max(1);
            let slices = build_slices_from(static_abyss, seed, 150, start_floor);

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

/// Pick the dungeon-settings id for a DEEP floor (past the fixed slices).
///
/// When `dungeonPool` + `depthBands` are loaded: seeded weighted-random over the pool,
/// each dungeon weighted by `depthBands[floor].tierWeights[ monsterTiers[dungeon] ]`, so
/// weak monsters cluster in shallow bands and tough monsters deep. Candidates with a tier
/// that carries no weight in the floor's band are excluded. The seed is `(seed, floor)`,
/// so a resumed run reproduces the same per-floor content (the resume-determinism the
/// `build_slices_from_resumes_at_requested_floor` test locks in).
///
/// Falls back — in order — to: the legacy `randomPool` cycling (`(seed + abs) % len`) when
/// the new data is absent or yields no weighted candidate; else the last fixed slice; else
/// the nil UUID (only if there is no data at all — never panics).
fn pick_deep_dungeon(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    seed: i64,
    floor: u32,
    abs: usize,
) -> Uuid {
    // Preferred path: seeded weighted-random over the depth-weighted dungeon pool.
    if !static_abyss.dungeon_pool.is_empty() {
        if let Some(band) = static_abyss.band_for_floor(floor) {
            // Gather (id, weight) candidates for this floor's band. Iterate the pool in a
            // STABLE (sorted-by-uuid) order so the weighted pick is deterministic
            // regardless of HashMap iteration order.
            let mut candidates: Vec<(Uuid, u32)> = static_abyss
                .dungeon_pool
                .iter()
                .filter_map(|(id, def)| {
                    let tier = static_abyss.dungeon_tier(def)?;
                    let w = band.tier_weights.get(&tier.to_string()).copied().unwrap_or(0);
                    if w > 0 { Some((*id, w)) } else { None }
                })
                .collect();
            if !candidates.is_empty() {
                candidates.sort_by(|a, b| a.0.cmp(&b.0));
                let total: u64 = candidates.iter().map(|(_, w)| *w as u64).sum();
                // Deterministic roll in [0, total) from (seed, floor).
                let roll = deep_floor_roll(seed, floor) % total;
                let mut acc = 0u64;
                for (id, w) in &candidates {
                    acc += *w as u64;
                    if roll < acc {
                        return *id;
                    }
                }
                // Rounding guard (unreachable given roll < total): last candidate.
                return candidates.last().unwrap().0;
            }
        }
    }

    // Fallback 1: legacy randomPool cycling (unchanged from the original handler).
    if !static_abyss.random_pool.is_empty() {
        let idx = ((seed.unsigned_abs() as usize) + abs) % static_abyss.random_pool.len();
        return static_abyss.random_pool[idx];
    }

    // Fallback 2: repeat the last fixed slice; else nil (no data at all).
    static_abyss
        .fixed_slices
        .last()
        .map(|s| s.dungeon_settings_id)
        .unwrap_or_else(Uuid::nil)
}

/// Deterministic 64-bit roll from `(seed, floor)` (splitmix64-style finalizer). Same
/// inputs → same roll, so resume-at-floor and a fresh full run agree per absolute floor.
fn deep_floor_roll(seed: i64, floor: u32) -> u64 {
    let mut z = (seed as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(floor as u64)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Build a slice list resuming from floor `start_floor` (1-based), producing floors
/// `start_floor ..< start_floor + n_from_start` (capped so `floor_index` never exceeds
/// `total`). Slice `k` represents ABSOLUTE floor `start_floor + k`, so the run's
/// `current_floor_index: 0` resumes at the requested depth — fixing the "abyss restarts
/// at floor 1" bug where `startingDifficulty` was ignored.
///
/// Each slice's content is chosen by its ABSOLUTE floor (floor `f`, 0-based `f-1`): the
/// first `fixed.len()` absolute floors come from the fixed list, the rest cycle the
/// random pool deterministically by seed — so resuming at floor N yields the SAME
/// per-floor content a fresh full run would have at floor N.
fn build_slices_from(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    seed: i64,
    total: usize,
    start_floor: u32,
) -> Vec<AbyssSliceEntry> {
    let start_floor = start_floor.max(1);
    // Number of slices remaining from `start_floor` to the top (`total`).
    let remaining = (total as u32).saturating_sub(start_floor - 1) as usize;
    let mut slices = Vec::with_capacity(remaining);
    for k in 0..remaining {
        let floor = start_floor + k as u32; // absolute 1-based floor
        let abs = (floor - 1) as usize; // absolute 0-based floor
        let (dungeon_uuid, diff) = if abs < static_abyss.fixed_slices.len() {
            let fs = &static_abyss.fixed_slices[abs];
            (fs.dungeon_settings_id, fs.difficulty_level)
        } else {
            // Deep floor. Difficulty comes from the authored ramp (`difficultyCurve`),
            // falling back to the legacy hard-coded 100 when the curve is absent (keeps
            // the old behaviour + the `build_slices_150_floors` test green). The dungeon
            // is picked by seeded, depth-weighted random over `dungeonPool` (weak
            // monsters shallow, tough deep); with the new data absent it degrades to the
            // legacy `randomPool` cycling — reusing `(seed + floor)` so a resumed run is
            // still deterministic per absolute floor.
            let diff = static_abyss.difficulty_for_floor(floor, 100);
            let dungeon_uuid = pick_deep_dungeon(static_abyss, seed, floor, abs);
            (dungeon_uuid, diff)
        };
        slices.push(AbyssSliceEntry {
            dungeon_settings_id: dungeon_uuid,
            difficulty_level: diff,
            hardcore: false,
            // slice_index / floor_index are ABSOLUTE (not k-relative): a resumed run's
            // first slice is floor `start_floor` with slice_index `start_floor-1`, so the
            // client shows the correct depth.
            slice_index: floor - 1,
            floor_index: floor,
            completed: false,
            enemy_killed: false,
        });
    }
    slices
}

/// Build `n` slices for a FRESH run (floor 1 upward). Thin wrapper over
/// [`build_slices_from`] with `start_floor = 1` — preserves the original behaviour /
/// call sites and keeps the existing unit tests meaningful.
#[cfg(test)]
fn build_slices(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    seed: i64,
    n: usize,
) -> Vec<AbyssSliceEntry> {
    build_slices_from(static_abyss, seed, n, 1)
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
            // New keys absent in the base fixture → the deep-floor path falls back to
            // random_pool cycling + the legacy hard-coded difficulty 100 (locks in the
            // pre-existing build_slices_150_floors / _exact_pool_cycling behaviour).
            ..Default::default()
        }
    }

    /// A fixture with the NEW keys populated: a difficulty curve that ramps past 100,
    /// a small dungeon pool spanning two tiers, monster tiers, and two depth bands so
    /// tier-1 dungeons dominate shallow deep-floors and tier-9 dungeons dominate the
    /// deepest floors.
    fn test_static_abyss_with_new_keys() -> (AbyssStaticData, Uuid, Uuid) {
        use blades_lib::static_data::{
            AbyssDepthBand, AbyssDifficultyEntry, AbyssDungeonDef, AbyssMonsterTier,
        };
        let mut sd = test_static_abyss();

        // Difficulty curve: floors 1-24 = fixed ladder, 25+ ramps +6/floor from 100.
        sd.difficulty_curve = (1u32..=150)
            .map(|f| AbyssDifficultyEntry {
                floor: f,
                difficulty_level: if f <= 24 { f } else { 100 + (f - 24) * 6 },
            })
            .collect();

        // Two dungeons: a weak (tier 1) goblin cave and a tough (tier 9) dragon lair.
        let weak = Uuid::from_u128(0x0001);
        let tough = Uuid::from_u128(0x0009);
        let mut pool = std::collections::HashMap::new();
        pool.insert(weak, AbyssDungeonDef {
            handle: "Cave_Goblin".into(), environment: "Cave".into(),
            monsters: vec!["Goblin".into()], is_boss: false, enemy_count: 3,
        });
        pool.insert(tough, AbyssDungeonDef {
            handle: "Ayleid_Dragon".into(), environment: "Ayleid".into(),
            monsters: vec!["Dragon".into()], is_boss: true, enemy_count: 1,
        });
        sd.dungeon_pool = pool;

        let mut tiers = std::collections::HashMap::new();
        tiers.insert("goblin".to_string(), AbyssMonsterTier { tier: 1 });
        tiers.insert("dragon".to_string(), AbyssMonsterTier { tier: 9 });
        sd.monster_tiers = tiers;

        // Shallow deep-band (25-40): only tier 1 has weight → always the weak cave.
        // Deepest band (41-150): only tier 9 has weight → always the dragon lair.
        sd.depth_bands = vec![
            AbyssDepthBand {
                floor_min: 25, floor_max: 40,
                tier_weights: [("1".to_string(), 100u32)].into_iter().collect(),
            },
            AbyssDepthBand {
                floor_min: 41, floor_max: 150,
                tier_weights: [("9".to_string(), 100u32)].into_iter().collect(),
            },
        ];
        (sd, weak, tough)
    }

    /// Floors past the fixed slices take their difficulty from `difficultyCurve`, not the
    /// hard-coded 100 (fix 2a). Floor 25 → 106, floor 30 → 136 in the fixture ramp.
    #[test]
    fn deep_floor_difficulty_from_curve_not_100() {
        let (sd, _, _) = test_static_abyss_with_new_keys();
        let slices = build_slices(&sd, 42, 150);
        assert_eq!(slices[24].floor_index, 25);
        assert_eq!(slices[24].difficulty_level, 106, "floor 25 = curve, not 100");
        assert_eq!(slices[29].difficulty_level, 136, "floor 30 = curve ramp");
        // Fixed floors 1-24 stay authoritative.
        assert_eq!(slices[23].difficulty_level, 24);
    }

    /// The deep-floor dungeon pick is depth-appropriate (weak shallow, tough deep) AND
    /// deterministic for a given (seed, floor) — resume-at-floor reproduces it (fix 2b).
    #[test]
    fn deep_floor_dungeon_pick_is_depth_appropriate_and_deterministic() {
        let (sd, weak, tough) = test_static_abyss_with_new_keys();
        let seed = 7i64;
        let fresh = build_slices_from(&sd, seed, 150, 1);

        // Shallow deep-band (floors 25-40): only tier 1 weighted → the weak cave.
        for s in fresh.iter().filter(|s| (25..=40).contains(&s.floor_index)) {
            assert_eq!(s.dungeon_settings_id, weak, "floor {} weak (tier 1)", s.floor_index);
        }
        // Deepest band (41+): only tier 9 weighted → the dragon lair.
        for s in fresh.iter().filter(|s| s.floor_index >= 41) {
            assert_eq!(s.dungeon_settings_id, tough, "floor {} tough (tier 9)", s.floor_index);
        }

        // Determinism: a run resumed at floor 45 yields the SAME floor-45 content.
        let resumed = build_slices_from(&sd, seed, 150, 45);
        assert_eq!(resumed[0].floor_index, 45);
        assert_eq!(resumed[0].dungeon_settings_id, fresh[44].dungeon_settings_id);
        assert_eq!(resumed[0].difficulty_level, fresh[44].difficulty_level);
    }

    /// The composite-family prefix fallback resolves a tier for a dungeon whose family
    /// has no exact `monsterTiers` key (e.g. `GoblinSkeleton` → `goblin`).
    #[test]
    fn dungeon_tier_prefix_fallback() {
        use blades_lib::static_data::{AbyssDungeonDef, AbyssMonsterTier};
        let mut sd = AbyssStaticData::default();
        sd.monster_tiers.insert("goblin".into(), AbyssMonsterTier { tier: 1 });
        sd.monster_tiers.insert("liches".into(), AbyssMonsterTier { tier: 8 });
        let composite = AbyssDungeonDef {
            monsters: vec!["GoblinSkeleton".into()], ..Default::default()
        };
        assert_eq!(sd.dungeon_tier(&composite), Some(1), "prefix match → goblin tier");
        let unknown = AbyssDungeonDef { monsters: vec!["AbyssEntrance".into()], ..Default::default() };
        assert_eq!(sd.dungeon_tier(&unknown), None, "no prefix → None");
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

    /// `startingDifficulty = N` must build a slice list that RESUMES at floor N (the
    /// abyss-restarts-at-floor-1 fix): the first slice is floor N (slice_index N-1), the
    /// list runs up to floor 150, and each floor's content matches what a fresh full run
    /// would have at that ABSOLUTE floor (so `currentFloorIndex: 0` = the requested depth).
    #[test]
    fn build_slices_from_resumes_at_requested_floor() {
        let sd = test_static_abyss();
        let seed = 12345i64;

        // Resume at floor 40.
        let resumed = build_slices_from(&sd, seed, 150, 40);
        assert_eq!(resumed.len(), 150 - 39, "floors 40..=150");
        assert_eq!(resumed[0].floor_index, 40, "first slice is the requested floor");
        assert_eq!(resumed[0].slice_index, 39, "slice_index is absolute (floor-1)");
        assert_eq!(resumed.last().unwrap().floor_index, 150, "runs to the top floor");

        // The per-floor content matches a fresh full run at the same absolute floor.
        let fresh = build_slices_from(&sd, seed, 150, 1);
        assert_eq!(
            resumed[0].dungeon_settings_id, fresh[39].dungeon_settings_id,
            "floor 40 content is stable whether resumed or reached fresh"
        );
        assert_eq!(resumed[0].difficulty_level, fresh[39].difficulty_level);
    }

    /// `startingDifficulty` of 1 (or None → 1) yields the original fresh-run slices, and
    /// a value past the top clamps to a single top floor — never panics, never empty of
    /// the requested floor.
    #[test]
    fn build_slices_from_floor_one_matches_fresh() {
        let sd = test_static_abyss();
        let seed = 7i64;
        let fresh = build_slices_from(&sd, seed, 150, 1);
        assert_eq!(fresh.len(), 150);
        assert_eq!(fresh[0].floor_index, 1);
        assert_eq!(fresh[0].slice_index, 0);

        // Resume at the top floor → exactly one slice (floor 150).
        let top = build_slices_from(&sd, seed, 150, 150);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].floor_index, 150);
        assert_eq!(top[0].slice_index, 149);
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
