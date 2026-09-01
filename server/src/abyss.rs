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
//!
//! ## Scoring and rewards
//!
//! Both come from `AbyssScaling`, the game's own ScriptableObject, extracted from the
//! APK bundles and shipped in `deploy/static/abyss.json`:
//!
//! * `/update` score — `killScoreMultiplier * GetKillScore(enemyLevel - initialPlayerLevel)`
//!   per kill, reading `sameLevelKillScore` / `underLeveledKillScore` /
//!   `overLeveledKillScore`. This used to be a flat `1` per kill; a same-level kill is
//!   worth `10`.
//! * `/end` reward — `Σ over rewarded floors of baseReward(floorIndex) * multiplier(offset)`
//!   with `offset = thatSlice'sDifficultyLevel - initialPlayerLevel`. So the payout scales
//!   with BOTH depth and how far above your level you fought. It used to be
//!   `floors * 195` gold / `floors * 64` XP — one guess produced by dividing a single
//!   captured total (~2923 gold / 958 XP) by an assumed floor count, i.e. fitted with zero
//!   degrees of freedom, so its apparent agreement with that total meant nothing.
//! * A floor cleared with NO kill grants no per-floor reward
//!   (`DATA_HAS_GOTTEN_KILL_SINCE_FLOOR_CHANGE` / `_floorsWithNoRewards` in `dump.cs`).
//!
//! Every number is confirmed against 18 retail `/end` captures, five of them
//! single-floor, exact to the unit — see the tests at the bottom of this file.
//!
//! ## `initialPlayerLevel` is an open question
//!
//! It drives BOTH formulas and we do not know how retail derives it. It is NOT the
//! character level and not a fixed offset from it: captured (charLevel → ipl) pairs run
//! 7→10, 3→4, 38→40, 34→38, 66→67, 79→75, 81→76, 93→81, 100→84 — it tracks power and
//! diverges DOWNWARD at high level. `start_abyss` still writes the character's level,
//! which is therefore wrong for high-level characters; everything downstream reads the
//! value persisted on the run rather than recomputing it, so fixing the derivation later
//! is a one-line change confined to `start_abyss`.

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

#[post("/api/game/v1/public/characters/{character_id}/abysses/current")]
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

/// Gold currency UUID (captured from both abyss and quest loot responses).
const GOLD_CURRENCY_UUID: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

#[post(
    "/api/game/v1/public/characters/{character_id}/abysses/current/start"
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

            // Generated data for the floor the run STARTS on — which is
            // `slices[0]`, not floor 1: a resumed run's first slice is the
            // floor the player is returning to. Serving floor 1's data here is
            // what hung every resumed run. Computed before `run` is moved into
            // the persisted state.
            // `.iter().next()` rather than `.first()`: diesel's `FirstDsl` is in
            // scope here and shadows the slice method.
            let gen_data = run
                .slices
                .iter()
                .next()
                .and_then(|slice| build_generated_data(&app_state, slice))
                .unwrap_or_else(empty_generated_data);

            // Persist run into server_state
            entry.server_state.0.abyss = Some(run);
            save_economy(&mut conn, character_id, &entry).await?;

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

/// One `enemy_killed` action from the client.
///
/// NOTHING on this action is trusted for scoring. `xp_reward` in particular is a
/// client-supplied number and using it would be a straight score/XP exploit; it is
/// parsed only so an unexpected shape does not reject the whole body. The enemy's level
/// comes from the server's own slice (`difficulty_level`) — the same value the server
/// put into that floor's generated data — and the score comes from the static tables.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnemyKilledAction {
    #[allow(dead_code)]
    spawn_group_id: Uuid,
    #[allow(dead_code)]
    spawner_index: usize,
    #[allow(dead_code)]
    enemy_index: usize,
    /// Client-reported XP. NEVER used — see the type doc.
    #[allow(dead_code)]
    xp_reward: f64,
    #[allow(dead_code)]
    time: u64,
}

/// `abyss_slice_completed` — the action that ends a floor. Carries only `time`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SliceCompletedAction {
    #[allow(dead_code)]
    #[serde(default)]
    time: u64,
}

/// `revive` — the player spent gems to continue after dying.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ReviveAction {
    #[allow(dead_code)]
    #[serde(default)]
    gems_payment: u64,
    #[allow(dead_code)]
    #[serde(default)]
    time: u64,
}

/// The six `/update` action types the client actually sends.
///
/// Only one arm used to exist (`EnemyKilled`); the other five fell into `Unknown` and
/// were dropped, `abyss_slice_completed` — the floor-advance signal — among them. The
/// three arms not acted on yet (`combat_completed` gear durability,
/// `enemy_loot_collected`, `item_consumed`) are named rather than swallowed so the next
/// change can see them, and so a body carrying them is not silently reduced to "unknown".
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AbyssUpdateAction {
    EnemyKilled(EnemyKilledAction),
    AbyssSliceCompleted(SliceCompletedAction),
    Revive(ReviveAction),
    /// Gear durability after a fight. Not applied yet.
    CombatCompleted(Value),
    /// Loot the player picked up off a corpse. Not applied yet — the server does not
    /// generate abyss enemy loot at all (see the follow-ups in the PR).
    EnemyLootCollected(Value),
    /// A potion/food used mid-run. Not applied yet.
    ItemConsumed(Value),
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAbyssRequest {
    /// `{"b64": ""}` in 711 of 711 captured bodies — nothing to read.
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
    "/api/game/v1/public/characters/{character_id}/abysses/current/update"
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

            if let Some(run) = entry.server_state.0.abyss.as_mut() {
                apply_actions(&app_state.static_data.abyss, run, &body.actions);

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
    "/api/game/v1/public/characters/{character_id}/abysses/current/end"
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
        let app_state = app_state.clone();
        async move {
            let mut entry =
                load_economy_for_update(&mut conn, &session.session, character_id).await?;

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

            // Sum the per-floor base rewards, each scaled by how far that floor's
            // difficulty sat above the level the run started at. Lenient: no active run
            // → no reward.
            let reward = match entry.server_state.0.abyss.as_ref() {
                Some(run) => end_run_reward(&app_state.static_data.abyss, run),
                None => RewardGrant::default(),
            };

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

/// Generated dungeon data for ONE abyss floor, built from that floor's ACTUAL
/// dungeon.
///
/// WHY THIS IS NOT A CONSTANT ANY MORE
///
/// This used to return a hard-coded stub whose two spawn groups
/// (`c41668b3…`, `9a057ca6…`) exist only in the floor-1 dungeon
/// `663053f0…`. The client looks each generated id up as it populates the
/// level, so on any other floor nothing resolved: no enemies spawned, no
/// `enemy_killed` action was ever sent, and the run could not advance.
///
/// That was not theoretical. Of the seven live runs in prod, the only one
/// making progress was a fresh floor-1 run; the six that had resumed deeper
/// (floors 30, 43, 78, 149…) all sat at `currentFloorIndex: 0` with zero
/// floors completed — the reported "abyss just hung".
///
/// Enemy level comes from the slice's own `difficulty_level`, which the floor
/// ramp already produces (1 at floor 1, 400 near the top). The stub said every
/// enemy was level 1, which is also why a deep floor would have been trivial
/// had it spawned at all.
///
/// `None` when the floor's dungeon is missing from `parsed.json`; the caller
/// serves an empty body rather than data for the wrong dungeon, because the
/// wrong dungeon is what caused the hang.
/// An empty body, for a floor whose dungeon is not in `parsed.json`.
///
/// Deliberately empty rather than a stand-in from some other dungeon: ids from
/// the wrong dungeon are precisely what hung the run, and an empty body at
/// least fails visibly instead of silently pointing the client at enemies that
/// are not there.
fn empty_generated_data() -> DungeonGeneratedData {
    DungeonGeneratedData {
        enemy_generated_data: Default::default(),
        chest_generated_data: Default::default(),
        item_generated_data: Default::default(),
        algorithm_version: 1,
        version: 0,
    }
}

fn build_generated_data(
    app_state: &ServerGlobal,
    slice: &AbyssSliceEntry,
) -> Option<DungeonGeneratedData> {
    blades_lib::util::dungeon::generate_for_dungeon(
        &app_state.game_data,
        &app_state.static_data,
        &slice.dungeon_settings_id,
        slice.difficulty_level as i64,
        0,
    )
}

/// Apply one `/update` body's actions to the run, in the order the client sent them.
///
/// Order matters: a body can carry the last kill of a floor AND that floor's
/// `abyss_slice_completed`, and the kill has to be credited to the floor the player was
/// still standing on when it happened.
fn apply_actions(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    run: &mut AbyssRun,
    actions: &[AbyssUpdateAction],
) {
    for action in actions {
        match action {
            AbyssUpdateAction::EnemyKilled(_) => {
                let slice = run.slices.get(run.current_floor_index);
                run.score += kills_score(static_abyss, slice, run.initial_player_level, 1);
                // The kill gate for the end-of-run reward.
                if let Some(slice) = run.slices.get_mut(run.current_floor_index) {
                    slice.enemy_killed = true;
                }
            }
            AbyssUpdateAction::AbyssSliceCompleted(_) => {
                // THE floor-advance signal — retail advances on this action, full stop.
                // This handler used to advance on any `enemy_killed` instead, because
                // `abyss_slice_completed` was one of the five action types that fell into
                // the enum's `Unknown` arm and were dropped. That approximation completed
                // a floor on its FIRST kill, so a floor the player abandoned halfway
                // still counted as cleared and still paid out.
                if let Some(slice) = run.slices.get_mut(run.current_floor_index) {
                    slice.completed = true;
                }
                if run.current_floor_index + 1 < run.slices.len() {
                    run.current_floor_index += 1;
                }
            }
            AbyssUpdateAction::Revive(_) => {
                run.revive_count += 1;
            }
            // Parsed, named, and deliberately not acted on yet — see the enum's doc.
            AbyssUpdateAction::CombatCompleted(_)
            | AbyssUpdateAction::EnemyLootCollected(_)
            | AbyssUpdateAction::ItemConsumed(_)
            | AbyssUpdateAction::Unknown => {}
        }
    }
}

/// The `killScoreMultiplier` used when the server cannot identify the enemy variant.
///
/// Every enemy carries one in the game data (`enemies.json` `variants[*].stats
/// .killScoreMultiplier`: 0.33 on 22 critter variants, 1.0 on 559, 2.0 on 50 bosses).
/// The server cannot read it: `deploy/static/parsed.json` keeps only `{"quantity": N}`
/// for all 1,956 enemy spawn groups — the extractor was narrowed on the enemy path
/// specifically (item spawn groups in the same file keep their full structure). With no
/// variant id anywhere in the request or in the generated data, there is nothing to look
/// the multiplier up by, so every kill scores as a normal enemy. Restoring the variant
/// to `parsed.json` is a separate extraction job; when it lands, multiply here.
const FALLBACK_KILL_SCORE_MULTIPLIER: f64 = 1.0;

/// Score for `count` kills on `slice`, for a run started at `initial_player_level`.
///
/// The enemy level is the slice's own `difficulty_level` — the value the server itself
/// wrote into that floor's generated data, so it is authoritative and not client input.
/// With no slice (a run whose floor pointer is past the end) nothing scores.
fn kills_score(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    slice: Option<&AbyssSliceEntry>,
    initial_player_level: u32,
    count: usize,
) -> f64 {
    let Some(slice) = slice else { return 0.0 };
    let level_delta = slice.difficulty_level as i32 - initial_player_level as i32;
    let per_kill = static_abyss.kill_score(level_delta) as f64;
    FALLBACK_KILL_SCORE_MULTIPLIER * per_kill * count as f64
}

/// Which floors of a finished run pay out.
///
/// A floor must be completed AND have had a kill on it: `dump.cs` tracks
/// `DATA_HAS_GOTTEN_KILL_SINCE_FLOOR_CHANGE` and collects `_floorsWithNoRewards`. The
/// gate is load-bearing, not defensive — it turns four apparently anomalous captured
/// `/end` payouts into exact fits. One run completed five floors but spent 79 seconds on
/// floor 147 with zero actions; dropping that floor reproduces the observed reward
/// exactly. Another, whose only completed floor had no kill, paid no gold and no XP.
fn rewarded_floors(run: &AbyssRun) -> impl Iterator<Item = &AbyssSliceEntry> {
    run.slices.iter().filter(|s| s.completed && s.enemy_killed)
}

/// The `/end` reward: `Σ over rewarded floors of baseReward(floorIndex) * multiplier(offset)`,
/// `offset = thatSlice'sDifficultyLevel - initialPlayerLevel`.
///
/// Both halves come from `AbyssScaling` (see `deploy/static/abyss.json`). The offset uses
/// the slice's own generated difficulty, NOT its floor index: the two diverge as soon as
/// a run starts below the player's level (a captured run at `initialPlayerLevel` 4
/// carried difficulties 1,2,3,4,6,8,10,14,18 on floors 1–9). The summed float is rounded
/// half-up — several captured runs land exactly on `.5`, so the rounding mode is
/// observable and this is the observed one.
fn end_run_reward(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    run: &AbyssRun,
) -> RewardGrant {
    use std::collections::HashMap;

    let mut gold = 0.0f64;
    let mut xp = 0.0f64;
    for slice in rewarded_floors(run) {
        let (base_gold, base_xp) = static_abyss.base_rewards_for_floor(slice.floor_index);
        let offset = slice.difficulty_level as i32 - run.initial_player_level as i32;
        let (gold_mult, xp_mult) = static_abyss.multiplier_for_offset(offset);
        gold += base_gold as f64 * gold_mult;
        xp += base_xp as f64 * xp_mult;
    }

    let gold = gold.round().max(0.0) as u64;
    let xp = xp.round().max(0.0) as u64;
    if gold == 0 && xp == 0 {
        return RewardGrant::default();
    }

    let gold_uuid = Uuid::parse_str(GOLD_CURRENCY_UUID).unwrap();
    RewardGrant {
        currencies: {
            let mut m = HashMap::new();
            if gold > 0 {
                m.insert(gold_uuid, gold);
            }
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

    // ────────────────────────────────────────────────────────────────────────
    // Scoring + reward scaling — against the REAL shipped tables
    //
    // Every test below reads `deploy/static/abyss.json`, the same file the server
    // loads, so it pins the DATA as well as the code. Fixtures that restate the
    // implementation's own assumption have shipped green against broken code in this
    // repo three times; the anchors here are captured `/end` totals, which neither half
    // of the model was fitted to.
    // ────────────────────────────────────────────────────────────────────────

    /// The real `deploy/static/abyss.json`, deserialized exactly as the server does.
    fn real_static_abyss() -> AbyssStaticData {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/abyss.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid abyss.json")
    }

    fn gold_uuid() -> Uuid {
        Uuid::parse_str(GOLD_CURRENCY_UUID).unwrap()
    }

    /// Build a run from explicit `(floor_index, difficulty_level)` pairs, every floor
    /// completed with a kill. The difficulties come from the captures, not from our own
    /// generator — a fixture that fed its own slice-picking back in would prove nothing
    /// about the reward model.
    fn run_from(floors: &[(u32, u32)], initial_player_level: u32) -> AbyssRun {
        let slices = floors
            .iter()
            .map(|&(floor_index, difficulty_level)| AbyssSliceEntry {
                dungeon_settings_id: Uuid::nil(),
                difficulty_level,
                hardcore: false,
                slice_index: floor_index.saturating_sub(1),
                floor_index,
                completed: true,
                enemy_killed: true,
            })
            .collect::<Vec<_>>();
        let n = slices.len();
        AbyssRun {
            slices,
            revive_count: 0,
            initial_player_level,
            seed: 1,
            score: 0.0,
            algorithm_version: 1,
            version: 1,
            current_floor_index: n,
        }
    }

    /// The per-floor base-reward table is the extracted one, not the old flat guess.
    ///
    /// `perFloorData[i] = (10 + 8i, 10 + 2i)` where `i` is the wire `floorIndex`. Five
    /// SINGLE-floor captured `/end` responses pin five separate rows exactly — a
    /// single-floor run has no summation to hide an error in, so each is a direct read
    /// of one row (the ×6 ones divided by the plateau multiplier).
    #[test]
    fn per_floor_base_rewards_are_the_extracted_table() {
        let sd = real_static_abyss();
        assert_eq!(sd.per_floor_rewards.entries.len(), 150, "150 rows, indices 0–149");

        // The five single-floor captures, as base rewards (observed payout / multiplier).
        for (floor_index, gold, xp) in [
            (49u32, 402u64, 108u64),   // observed 402 / 108 at ×1
            (90, 730, 190),            // observed 4380 / 1140 at ×6
            (118, 954, 246),           // observed 5724 / 1476 at ×6, 3 runs
            (147, 1186, 304),          // observed 7116 / 1824 at ×6
            (149, 1202, 308),          // observed 7212 / 1848 at ×6, 3 runs
        ] {
            assert_eq!(
                sd.base_rewards_for_floor(floor_index),
                (gold, xp),
                "floorIndex {floor_index}"
            );
        }

        // The whole table is linear, and it is NOT the off-by-one variant (8i+2 / 2i+8)
        // that an earlier reading of the asset produced: that gives 394 for floorIndex
        // 49 where the capture says 402.
        for i in 0..150u32 {
            assert_eq!(
                sd.base_rewards_for_floor(i),
                (10 + 8 * i as u64, 10 + 2 * i as u64),
                "row {i}"
            );
        }
        assert_ne!(sd.base_rewards_for_floor(49), (394, 106), "not the off-by-one ramp");

        // The old model claimed a flat 195 gold / 64 XP on EVERY floor.
        assert_ne!(sd.base_rewards_for_floor(1), (195, 64));
        // Past the last row the last row repeats rather than falling off to zero.
        assert_eq!(sd.base_rewards_for_floor(150), (1202, 308));
        assert_eq!(sd.base_rewards_for_floor(9_999), (1202, 308));
    }

    /// The scaling curve is the 6 measured breakpoints. Below offset 0 the multiplier is
    /// 1.0 — the bare base reward — and above offset 14 it plateaus at ×6.
    #[test]
    fn scaling_curve_breakpoints_floor_at_one_and_plateau() {
        let sd = real_static_abyss();
        for (offset, expected) in [
            (0, 1.25),
            (1, 1.25),
            (2, 2.0),
            (3, 2.0),
            (4, 3.0),
            (5, 3.0),
            (6, 4.0),
            (9, 4.0),
            (10, 5.0),
            (13, 5.0),
            (14, 6.0),
        ] {
            let (g, x) = sd.multiplier_for_offset(offset);
            assert_eq!(g, expected, "offset {offset} gold multiplier");
            assert_eq!(x, expected, "offset {offset} xp multiplier — gold and xp are equal");
        }
        // Below offset 0: 1.0. Not a 0.5 penalty (the invented `{-5, 0.5, 0.5}` row this
        // file used to carry) and not a clamp up to the offset-0 row's ×1.25.
        for offset in [-1, -5, -10, -24, -99] {
            assert_eq!(
                sd.multiplier_for_offset(offset),
                (1.0, 1.0),
                "offset {offset} gets the unmultiplied base"
            );
        }
        // Above the last breakpoint: plateau, not extrapolation.
        for offset in [15, 18, 40, 99, 400] {
            assert_eq!(sd.multiplier_for_offset(offset), (6.0, 6.0), "offset {offset} plateaus");
        }
    }

    /// THE IDENTITY CHECKS. These are the tests that prove the reward MODEL rather than
    /// restating the implementation, so they are spelled out.
    ///
    /// Each case is a real retail `/end` capture: the run's floor indices, the slices'
    /// generated difficulty levels and the run's `initialPlayerLevel` are the observed
    /// inputs, and the assertion is the observed payout. Nothing in the model was fitted
    /// to any of them — the per-floor rows and the curve were read out of `AbyssScaling`,
    /// and the fits are exact to the unit, not approximate.
    ///
    /// Contrast the model this replaces: `floors * 195` gold was obtained by dividing
    /// case A's own total by an assumed floor count, so it had zero degrees of freedom
    /// and its agreement with case A was arithmetic, not evidence. It also has no term
    /// for `initialPlayerLevel` or slice difficulty at all, so it cannot fit cases A and
    /// B simultaneously.
    ///
    /// Case A, floor by floor (base = 10+8i / 10+2i, offset = difficulty - 10):
    ///   floors 1-9   offsets -9..-1 → ×1.00 · gold 450 = 450.0,  xp 180 = 180.0
    ///   floor 10     offset  0      → ×1.25 · gold  90 = 112.5,  xp  30 =  37.5
    ///   floor 11     offset  2      → ×2.00 · gold  98 = 196.0,  xp  32 =  64.0
    ///   floor 12     offset  4      → ×3.00 · gold 106 = 318.0,  xp  34 = 102.0
    ///   floor 13     offset  6      → ×4.00 · gold 114 = 456.0,  xp  36 = 144.0
    ///   floor 14     offset 10      → ×5.00 · gold 122 = 610.0,  xp  38 = 190.0
    ///   floor 15     offset 14      → ×6.00 · gold 130 = 780.0,  xp  40 = 240.0
    ///                                          total  = 2922.5 → 2923,      957.5 → 958
    #[test]
    fn end_reward_reproduces_the_captured_runs_exactly() {
        let sd = real_static_abyss();

        // Case A — 15 floors, initialPlayerLevel 10, character level 7→8.
        let a = run_from(
            &[
                (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7), (8, 8),
                (9, 9), (10, 10), (11, 12), (12, 14), (13, 16), (14, 20), (15, 24),
            ],
            10,
        );
        let ra = end_run_reward(&sd, &a);
        assert_eq!(ra.currencies[&gold_uuid()], 2923, "case A gold");
        assert_eq!(ra.character_xp, 958, "case A XP");

        // Case B — 8 floors, initialPlayerLevel 4. The slice difficulties (1,2,3,4,6,8,
        // 10,14) are NOT the floor indices, so this case fails for any model that keys
        // the multiplier on depth alone.
        let b = run_from(
            &[(1, 1), (2, 2), (3, 3), (4, 4), (5, 6), (6, 8), (7, 10), (8, 14)],
            4,
        );
        let rb = end_run_reward(&sd, &b);
        assert_eq!(rb.currencies[&gold_uuid()], 1039, "case B gold");
        assert_eq!(rb.character_xp, 397, "case B XP");

        // Case C — one floor, index 49, at initialPlayerLevel 59. Offset -10, so the
        // bare base reward. This is the case that rules out a 1.25 floor on the curve:
        // 402 × 1.25 = 502.5, which is not what retail paid.
        let c = run_from(&[(49, 49)], 59);
        let rc = end_run_reward(&sd, &c);
        assert_eq!(rc.currencies[&gold_uuid()], 402, "case C gold — unmultiplied");
        assert_eq!(rc.character_xp, 108, "case C XP — unmultiplied");

        // Case D — one floor, index 149, deep enough to plateau at ×6.
        let d = run_from(&[(149, 200)], 1);
        let rd = end_run_reward(&sd, &d);
        assert_eq!(rd.currencies[&gold_uuid()], 7212, "case D gold");
        assert_eq!(rd.character_xp, 1848, "case D XP");
    }

    /// Both identity cases land on exactly `.5` before rounding, so the rounding mode is
    /// observable: retail rounds HALF-UP. Pinned separately from the totals so a change
    /// of rounding mode names itself instead of showing up as a one-gold mystery.
    #[test]
    fn fractional_totals_round_half_up() {
        let sd = real_static_abyss();
        // A single floor at ×1.25 on an even base gives a .5 total: 10+8·1 = 18 → 22.5.
        let run = run_from(&[(1, 10)], 10);
        assert_eq!(sd.multiplier_for_offset(0), (1.25, 1.25));
        assert_eq!(sd.base_rewards_for_floor(1), (18, 12));
        let r = end_run_reward(&sd, &run);
        assert_eq!(r.currencies[&gold_uuid()], 23, "22.5 rounds up to 23, not down to 22");
        assert_eq!(r.character_xp, 15, "15.0 exactly");
    }

    /// The reward depends on how far the slices sat above the level you started at, not
    /// only on how deep you went. The model this replaces was a function of floor count
    /// alone, so it cannot express this at all.
    #[test]
    fn end_reward_depends_on_initial_player_level_not_just_depth() {
        let sd = real_static_abyss();
        let floors: Vec<(u32, u32)> = (1..=15).map(|f| (f, f)).collect();
        let low = end_run_reward(&sd, &run_from(&floors, 1));
        let high = end_run_reward(&sd, &run_from(&floors, 60));
        assert!(
            low.currencies[&gold_uuid()] > high.currencies[&gold_uuid()],
            "the same 15 floors pay MORE to a run started at level 1 than at level 60: \
             {} vs {}",
            low.currencies[&gold_uuid()],
            high.currencies[&gold_uuid()]
        );
        // The level-60 run is entirely below offset 0 → every floor pays its bare base.
        let base_gold: u64 = floors.iter().map(|&(f, _)| sd.base_rewards_for_floor(f).0).sum();
        assert_eq!(high.currencies[&gold_uuid()], base_gold);
    }

    /// A floor cleared with NO kill pays nothing (`_floorsWithNoRewards`). Measured: a
    /// captured run whose only completed floor had no kill returned no gold and no XP.
    #[test]
    fn a_floor_cleared_without_a_kill_pays_nothing() {
        let sd = real_static_abyss();
        let floors: Vec<(u32, u32)> = vec![
            (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7), (8, 8),
            (9, 9), (10, 10), (11, 12), (12, 14), (13, 16), (14, 20), (15, 24),
        ];
        let full_reward = end_run_reward(&sd, &run_from(&floors, 10));

        // Same run, but the deepest floor was walked through without a kill. Floor 15
        // was worth 130 × 6 = 780 gold and 40 × 6 = 240 XP.
        let mut no_kill = run_from(&floors, 10);
        no_kill.slices[14].enemy_killed = false;
        let no_kill_reward = end_run_reward(&sd, &no_kill);
        assert_eq!(
            full_reward.currencies[&gold_uuid()] - no_kill_reward.currencies[&gold_uuid()],
            780,
            "the kill-less floor's 780 gold is withheld"
        );
        assert_eq!(full_reward.character_xp - no_kill_reward.character_xp, 240);

        // A run whose only completed floor had no kill pays nothing at all.
        let mut only_floor = run_from(&[(49, 49)], 59);
        only_floor.slices[0].enemy_killed = false;
        assert!(end_run_reward(&sd, &only_floor).is_empty(), "no kill → no reward");

        // Neither does a floor that had a kill but was never completed.
        let mut incomplete = run_from(&[(49, 49)], 59);
        incomplete.slices[0].completed = false;
        assert!(end_run_reward(&sd, &incomplete).is_empty(), "not completed → no reward");
    }

    /// An empty run pays nothing (unchanged behaviour, kept pinned).
    #[test]
    fn end_reward_zero_floors() {
        let sd = real_static_abyss();
        assert!(end_run_reward(&sd, &run_from(&[], 25)).is_empty(), "no floors → no reward");
    }

    /// A same-level kill scores 10, not 1. The handler used to add a flat
    /// `enemy_killed_count as f64`, so this is 10x its old value.
    #[test]
    fn a_same_level_kill_scores_ten_not_one() {
        let sd = real_static_abyss();
        assert_eq!(sd.kill_score(0), 10, "sameLevelKillScore");

        let slice = AbyssSliceEntry {
            dungeon_settings_id: Uuid::nil(),
            difficulty_level: 40,
            hardcore: false,
            slice_index: 0,
            floor_index: 1,
            completed: false,
            enemy_killed: false,
        };
        // One kill on a floor whose difficulty equals the run's starting level.
        assert_eq!(kills_score(&sd, Some(&slice), 40, 1), 10.0);
        // Three kills → 30, where the old code gave 3.
        assert_eq!(kills_score(&sd, Some(&slice), 40, 3), 30.0);
        assert_eq!(kills_score(&sd, None, 40, 3), 0.0, "no slice → no score");
    }

    /// The kill-score tables are level-scaled in both directions and flat-tailed.
    /// The index alignment asserted here is backed by wire evidence — see
    /// `kill_score_alignment_matches_the_captured_score` below.
    #[test]
    fn kill_score_scales_with_level_delta() {
        let sd = real_static_abyss();
        assert_eq!(sd.kill_scores.under_leveled_kill_score.len(), 100);
        assert_eq!(sd.kill_scores.over_leveled_kill_score.len(), 60);

        // Under-levelled player (enemy above you): the ramp above 10.
        assert_eq!(sd.kill_score(1), 12);
        assert_eq!(sd.kill_score(2), 15);
        assert_eq!(sd.kill_score(3), 19);
        assert_eq!(sd.kill_score(4), 24);
        assert_eq!(sd.kill_score(5), 30);
        assert_eq!(sd.kill_score(6), 40);
        // Over-levelled player (enemy below you): the ramp below 10.
        assert_eq!(sd.kill_score(-1), 10);
        assert_eq!(sd.kill_score(-2), 8);
        assert_eq!(sd.kill_score(-3), 5);
        assert_eq!(sd.kill_score(-6), 2);
        // Past either table, the last entry repeats — never 0, never a panic.
        assert_eq!(sd.kill_score(400), *sd.kill_scores.under_leveled_kill_score.last().unwrap());
        assert_eq!(sd.kill_score(-400), *sd.kill_scores.over_leveled_kill_score.last().unwrap());
        assert_eq!(sd.kill_score(-400), 1);
    }

    /// The evidence for the index alignment, written down as an assertion.
    ///
    /// A captured floor-43 run at `initialPlayerLevel` 45 (`levelDelta` -2) reported a
    /// score of 16.0. Under this alignment `over[1] = 8`, and 16 = 8 × 2 with a boss's
    /// `killScoreMultiplier`. The obvious alternative alignment — `over[-delta]`, i.e.
    /// `over[2] = 5` — cannot reach 16 under ANY of the three multipliers the game data
    /// uses (0.33 / 1.0 / 2.0), which is what makes the observation discriminating rather
    /// than merely consistent.
    #[test]
    fn kill_score_alignment_matches_the_captured_score() {
        let sd = real_static_abyss();
        let chosen = sd.kill_score(-2);
        assert_eq!(chosen, 8, "over[-delta-1] = over[1]");
        assert!(
            [0.33f64, 1.0, 2.0].iter().any(|m| (chosen as f64 * m - 16.0).abs() < 1e-9),
            "the chosen alignment reaches the observed 16.0"
        );
        let alternative = sd.kill_scores.over_leveled_kill_score[2];
        assert_eq!(alternative, 5, "the alternative alignment would read over[2]");
        assert!(
            ![0.33f64, 1.0, 2.0].iter().any(|m| (alternative as f64 * m - 16.0).abs() < 1e-9),
            "and the alternative cannot reach 16.0 under any killScoreMultiplier — \
             which is why the capture discriminates between them"
        );
    }

    /// `_slicesCountAbovePlayerLevel` is 20. The file shipped 2 — wrong by 10x — and the
    /// Rust default is 20 too, so a stale data file cannot quietly restore the old value.
    #[test]
    fn slices_count_above_player_level_is_twenty() {
        assert_eq!(real_static_abyss().scaling_backend.slices_count_above_player_level, 20);
        assert_eq!(
            AbyssStaticData::default().scaling_backend.slices_count_above_player_level,
            20,
            "the built-in default must not be the old 2 either"
        );
    }

    /// `deploy/static/` is a bind-mounted data directory: merging this repo ships CODE
    /// but not DATA, so between the merge and `deploy/arena.sh static` the server runs
    /// new code against the OLD `abyss.json`. Prove that window pays the same rewards
    /// rather than zero, by running the model against static data with no tables at all.
    #[test]
    fn the_built_in_fallback_matches_the_shipped_tables() {
        let real = real_static_abyss();
        let mut bare = real.clone();
        bare.per_floor_rewards = Default::default();
        bare.scaling_curve = Default::default();
        bare.kill_scores = Default::default();

        for floor in 0..=150u32 {
            assert_eq!(
                bare.base_rewards_for_floor(floor),
                real.base_rewards_for_floor(floor),
                "floor {floor} base reward"
            );
        }
        for offset in -30..=40 {
            assert_eq!(
                bare.multiplier_for_offset(offset),
                real.multiplier_for_offset(offset),
                "offset {offset} multiplier"
            );
        }
        for delta in -120..=120 {
            assert_eq!(bare.kill_score(delta), real.kill_score(delta), "delta {delta}");
        }

        // And the identity case still lands on the captured total with no data at all.
        let run = run_from(
            &[
                (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7), (8, 8),
                (9, 9), (10, 10), (11, 12), (12, 14), (13, 16), (14, 20), (15, 24),
            ],
            10,
        );
        assert_eq!(
            end_run_reward(&bare, &run).currencies[&gold_uuid()],
            2923,
            "an un-deployed abyss.json must not zero out the reward"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // /update action handling
    // ────────────────────────────────────────────────────────────────────────

    /// All six real action types parse into their own arm. Five of them used to fall
    /// into `Unknown` and be dropped — `abyss_slice_completed`, the floor-advance
    /// signal, among them.
    #[test]
    fn all_six_client_actions_parse_into_their_own_arm() {
        let body = serde_json::json!({
            "currentState": {"b64": ""},
            "actions": [
                {"type": "enemy_killed", "spawnGroupId": Uuid::nil(), "spawnerIndex": 0,
                 "enemyIndex": 0, "xpReward": 12.0, "time": 1},
                {"type": "combat_completed", "items": [{"id": Uuid::nil(), "durability": 90}],
                 "time": 2},
                {"type": "enemy_loot_collected", "spawnGroupId": Uuid::nil(),
                 "spawnerIndex": 0, "enemyIndex": 0, "loot": {"currencies": {}}, "time": 3},
                {"type": "item_consumed", "itemTemplateId": Uuid::nil(), "time": 4},
                {"type": "abyss_slice_completed", "time": 5},
                {"type": "revive", "gemsPayment": 20, "time": 6},
            ]
        });
        let req: UpdateAbyssRequest = serde_json::from_value(body).expect("parses");
        assert_eq!(req.actions.len(), 6);
        assert!(matches!(req.actions[0], AbyssUpdateAction::EnemyKilled(_)));
        assert!(matches!(req.actions[1], AbyssUpdateAction::CombatCompleted(_)));
        assert!(matches!(req.actions[2], AbyssUpdateAction::EnemyLootCollected(_)));
        assert!(matches!(req.actions[3], AbyssUpdateAction::ItemConsumed(_)));
        assert!(matches!(req.actions[4], AbyssUpdateAction::AbyssSliceCompleted(_)));
        assert!(matches!(req.actions[5], AbyssUpdateAction::Revive(_)));
        assert!(
            !req.actions.iter().any(|a| matches!(a, AbyssUpdateAction::Unknown)),
            "no real action may land in Unknown"
        );
        // An action type we have never seen still parses rather than 400-ing the body.
        let odd: UpdateAbyssRequest =
            serde_json::from_value(serde_json::json!({"actions": [{"type": "who_knows"}]}))
                .expect("unknown types stay lenient");
        assert!(matches!(odd.actions[0], AbyssUpdateAction::Unknown));
    }

    fn parse_actions(v: serde_json::Value) -> Vec<AbyssUpdateAction> {
        serde_json::from_value::<UpdateAbyssRequest>(serde_json::json!({"actions": v}))
            .expect("parses")
            .actions
    }

    fn kill(time: u64) -> serde_json::Value {
        serde_json::json!({"type": "enemy_killed", "spawnGroupId": Uuid::nil(),
                           "spawnerIndex": 0, "enemyIndex": 0, "xpReward": 0.0, "time": time})
    }

    /// A kill alone must NOT complete or advance a floor; only
    /// `abyss_slice_completed` does. The old handler completed and advanced on the first
    /// kill, so a player who killed one enemy and quit banked the whole floor.
    #[test]
    fn only_abyss_slice_completed_advances_the_floor() {
        let sd = real_static_abyss();
        let mut run = run_from(&[(1, 10), (2, 12), (3, 14)], 10);
        for s in run.slices.iter_mut() {
            s.completed = false;
            s.enemy_killed = false;
        }
        run.current_floor_index = 0;

        apply_actions(&sd, &mut run, &parse_actions(serde_json::json!([kill(1), kill(2)])));
        assert!(run.slices[0].enemy_killed, "the kills are recorded");
        assert!(!run.slices[0].completed, "but two kills do not clear the floor");
        assert_eq!(run.current_floor_index, 0, "and do not advance it");
        assert!(
            end_run_reward(&sd, &run).is_empty(),
            "an un-completed floor pays nothing"
        );

        apply_actions(
            &sd,
            &mut run,
            &parse_actions(serde_json::json!([{"type": "abyss_slice_completed", "time": 3}])),
        );
        assert!(run.slices[0].completed, "the slice-completed action clears it");
        assert_eq!(run.current_floor_index, 1, "and advances to the next floor");
        assert_eq!(
            end_run_reward(&sd, &run).currencies[&gold_uuid()],
            23,
            "floor 1 at offset 0: 18 × 1.25 = 22.5 → 23"
        );
    }

    /// A body carrying a floor's last kill AND its `abyss_slice_completed` credits the
    /// kill to the floor the player was on, not to the next one.
    #[test]
    fn a_kill_in_the_same_body_as_the_completion_credits_the_old_floor() {
        let sd = real_static_abyss();
        // Floor 1 difficulty 10 (delta 0 → 10 points), floor 2 difficulty 16 (delta 6 →
        // 40 points). Crediting the kill to the wrong floor would score 40, not 10.
        let mut run = run_from(&[(1, 10), (2, 16)], 10);
        for s in run.slices.iter_mut() {
            s.completed = false;
            s.enemy_killed = false;
        }
        run.current_floor_index = 0;

        apply_actions(
            &sd,
            &mut run,
            &parse_actions(
                serde_json::json!([kill(1), {"type": "abyss_slice_completed", "time": 2}]),
            ),
        );
        assert_eq!(run.score, 10.0, "scored on floor 1's difficulty, not floor 2's");
        assert!(run.slices[0].enemy_killed && run.slices[0].completed);
        assert!(!run.slices[1].enemy_killed, "floor 2 got nothing");
        assert_eq!(run.current_floor_index, 1);
    }

    /// `revive` increments the revive count. It used to be dropped, so `reviveCount` was
    /// always 0 on the wire no matter how many gems the player spent.
    #[test]
    fn revive_actions_are_counted() {
        let sd = real_static_abyss();
        let mut run = run_from(&[(1, 10)], 10);
        assert_eq!(run.revive_count, 0);
        apply_actions(
            &sd,
            &mut run,
            &parse_actions(serde_json::json!([
                {"type": "revive", "gemsPayment": 20, "time": 1},
                {"type": "revive", "gemsPayment": 40, "time": 2},
            ])),
        );
        assert_eq!(run.revive_count, 2);
    }

    /// Score accrues at the table rate per kill, and it is the SERVER's slice difficulty
    /// that sets the rate — not the client's `xpReward`, which is ignored.
    #[test]
    fn score_uses_server_side_difficulty_not_client_input() {
        let sd = real_static_abyss();
        let mut run = run_from(&[(1, 10)], 10);
        run.slices[0].completed = false;
        run.slices[0].enemy_killed = false;
        run.current_floor_index = 0;

        // A client claiming an enormous xpReward earns exactly the same 10 points.
        let actions = parse_actions(serde_json::json!([
            {"type": "enemy_killed", "spawnGroupId": Uuid::nil(), "spawnerIndex": 0,
             "enemyIndex": 0, "xpReward": 999999.0, "time": 1},
        ]));
        apply_actions(&sd, &mut run, &actions);
        assert_eq!(run.score, 10.0, "client-supplied xpReward must not reach the score");
    }

    #[test]
    fn generate_seed_deterministic() {
        let id = Uuid::parse_str("78f2b668-97ff-45d0-99fa-7343fd059480").unwrap();
        let s1 = generate_seed(id);
        let s2 = generate_seed(id);
        assert_eq!(s1, s2, "same id → same seed");
        assert_ne!(s1, 0, "non-zero seed");
    }

    fn game_data() -> blades_lib::game_data::GameData {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    fn slice_for(dungeon: &str, difficulty: u32, floor: u32) -> AbyssSliceEntry {
        AbyssSliceEntry {
            dungeon_settings_id: Uuid::parse_str(dungeon).unwrap(),
            difficulty_level: difficulty,
            hardcore: false,
            slice_index: 0,
            floor_index: floor,
            completed: false,
            enemy_killed: false,
        }
    }

    /// EVERY floor's generated data must describe THAT floor's dungeon.
    ///
    /// WHY THIS EXISTS
    ///
    /// The test that used to sit here asserted the opposite: that
    /// `build_generated_data()` always returns the two spawn groups
    /// `c41668b3…` / `9a057ca6…`. Those exist only in the floor-1 dungeon
    /// `663053f0…`, so the assertion was really "the Abyss always serves
    /// floor 1's enemies" — the bug, written down as the requirement, which is
    /// why it stayed green while six of seven live runs sat frozen at floor 0.
    ///
    /// The client resolves each generated id against the dungeon it was told to
    /// load. Ids from another dungeon resolve to nothing, so no enemy spawns, so
    /// no `enemy_killed` is sent, so the floor never completes.
    #[test]
    fn generated_data_matches_the_floors_own_dungeon() {
        let gd = game_data();
        // Floor 1, and three dungeons taken from real resumed runs in prod
        // (floors 30, 78 and 149) that were hung.
        for (dungeon, difficulty, floor) in [
            ("663053f0-3a46-4012-b004-6cb2e907f33c", 1u32, 1u32),
            ("85bf1a3a-0006-4abf-993c-f483ec7db298", 136, 30),
            ("65375990-e5b3-41cf-b5d3-cbe2c740cb1d", 400, 78),
            ("ef24eeb3-8181-48e2-ae3d-320bd6f5992c", 400, 149),
        ] {
            let uuid = Uuid::parse_str(dungeon).unwrap();
            let expected: std::collections::HashSet<Uuid> = gd
                .dungeons
                .get(&uuid)
                .unwrap_or_else(|| panic!("floor {floor} dungeon {dungeon} in parsed.json"))
                .spawn_info
                .enemy_spawn_groups
                .keys()
                .copied()
                .collect();
            assert!(!expected.is_empty(), "floor {floor} dungeon has spawn groups");

            let data = blades_lib::util::dungeon::generate_for_dungeon(
                &gd,
                &uuid,
                difficulty as i64,
                0,
            )
            .unwrap_or_else(|| panic!("floor {floor} generated data"));

            let got: std::collections::HashSet<Uuid> =
                data.enemy_generated_data.keys().copied().collect();
            assert_eq!(
                got, expected,
                "floor {floor} must be given its OWN spawn groups, not another dungeon's"
            );
            // And the enemies must be at the floor's difficulty, not level 1 —
            // the stub hard-coded 1, so a deep floor would have been trivial.
            for enemies in data.enemy_generated_data.values() {
                for e in enemies.iter().flatten() {
                    assert_eq!(e.enemy_level, difficulty as i64, "floor {floor} enemy level");
                }
            }
        }
    }

    /// The floor-1 dungeon is the ONLY one carrying the spawn groups the old
    /// stub hard-coded. This is the measurement that explains the bug: it is
    /// why floor 1 played and every other floor hung.
    #[test]
    fn the_old_stubs_spawn_groups_are_floor_one_only() {
        let gd = game_data();
        let a = Uuid::parse_str("c41668b3-ad8b-42b4-ba5d-a0574039a3cc").unwrap();
        let b = Uuid::parse_str("9a057ca6-5f8d-4700-8665-6c56de0e1103").unwrap();
        let floor1 = Uuid::parse_str("663053f0-3a46-4012-b004-6cb2e907f33c").unwrap();

        let f1 = &gd.dungeons[&floor1].spawn_info.enemy_spawn_groups;
        assert!(f1.contains_key(&a) && f1.contains_key(&b), "floor 1 has both");

        for (id, d) in &gd.dungeons {
            if *id == floor1 {
                continue;
            }
            let g = &d.spawn_info.enemy_spawn_groups;
            assert!(
                !g.contains_key(&a) && !g.contains_key(&b),
                "dungeon {id} also carries a stub spawn group — the explanation \
                 for the hang would not hold"
            );
        }
    }

    /// A run resuming deep starts on the floor it resumed to, so `slices[0]`
    /// is that floor — not floor 1. Serving floor 1's data to a resumed run is
    /// what hung six of the seven live runs.
    #[test]
    fn a_resumed_runs_first_slice_is_the_resumed_floor() {
        let sd = test_static_abyss();
        let slices = build_slices_from(&sd, 12345, 150, 78);
        assert_eq!(slices[0].floor_index, 78);
        // The guard that matters: whatever start_abyss serves must come from
        // slices[0], whose dungeon is NOT the floor-1 dungeon.
        let floor1 = build_slices_from(&sd, 12345, 150, 1)[0].dungeon_settings_id;
        assert_ne!(
            slices[0].dungeon_settings_id, floor1,
            "a floor-78 resume must not be handed the floor-1 dungeon"
        );
    }

    /// A start floor past the top yields an empty run rather than a slice list
    /// that can never advance.
    #[test]
    fn a_start_floor_past_the_top_is_empty_not_stuck() {
        let sd = test_static_abyss();
        assert!(build_slices_from(&sd, 1, 150, 151).is_empty());
    }

    #[test]
    fn a_floor_whose_dungeon_is_unknown_serves_an_empty_body() {
        let gd = game_data();
        assert!(
            blades_lib::util::dungeon::generate_for_dungeon(&gd, &Uuid::nil(), 1, 0).is_none(),
            "an unknown dungeon must yield None, never another dungeon's ids"
        );
        let empty = empty_generated_data();
        assert!(empty.enemy_generated_data.is_empty());
    }

}
