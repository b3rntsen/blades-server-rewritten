//! Abyss endless-dungeon mode endpoints.
//!
//! Wire shapes confirmed against `api_captures` (character 78f2b668 / 97cf5fa6):
//!
//! `POST /abysses/current`            → `{abyss: null | AbyssWire}`
//! `POST /abysses/current/start`      → `{abyss: AbyssWire, abyssDungeonGeneratedData: {...}}`
//! `POST /abysses/current/update`     → `{reward?, abyssFutureRewards, character, abyssProgress, inventory}`
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
//! * Score-gauge rungs (`abyssFutureRewards`) — paid on the `/update` whose action
//!   crosses them, as a top-level `reward`, once per rung per run; see
//!   [`grant_reached_rungs`].
//! * A floor cleared with NO kill grants no per-floor reward
//!   (`DATA_HAS_GOTTEN_KILL_SINCE_FLOOR_CHANGE` / `_floorsWithNoRewards` in `dump.cs`).
//!
//! Every number is confirmed against 18 retail `/end` captures, five of them
//! single-floor, exact to the unit — see the tests at the bottom of this file.
//!
//! ## `initialPlayerLevel` is an open question
//!
//! It drives the kill score, the `/end` multiplier AND every slice's difficulty (see
//! `slice_difficulty`), and we do not know how retail derives it. It is NOT the
//! character level and not a fixed offset from it: captured (charLevel → ipl) pairs run
//! 7→10, 8→11, 3→4, 38→40, 34→38, 66→67, 79→75, 81→76, 93→81, 100→84 — it tracks power
//! and diverges DOWNWARD at high level. `start_abyss` still writes the character's level,
//! which is therefore wrong for most characters; everything downstream reads the value
//! persisted on the run (and the client reads the same value off `/start`), so client and
//! server agree with each other whatever it is, and fixing the derivation later is a
//! one-line change confined to `start_abyss`.

use std::{collections::HashMap, sync::Arc};

use actix_web::{
    post,
    web::{self, Json},
};
use blades_lib::{
    economy::{RewardGrant, apply_reward, consume_stackable, grant_chest},
    features::{abyss_kill_score, abyss_rewards, revive},
    server_state::{AbyssRun, AbyssSliceEntry},
    user_data::{CompleteCharacterWithIdWithoutData, CompleteInventory, CompleteInventoryUpdate,
                CompleteWallet, DungeonGeneratedData, InventoryChangeTracker},
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
///
/// The reward is a plain `RewardGrant`, which already omits every empty
/// collection — so a stackable rung serialises as `{"stackableItems":{…}}`, a
/// chest rung as `{"chests":[{"level":N,"tier":T}]}` and a gear rung as
/// `{"items":[…]}`, which is exactly the per-rung shape retail sent. The old
/// dedicated struct could only express stackables, so the chest and gear rungs
/// had no wire representation at all.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AbyssFutureRewardWire {
    reward: RewardGrant,
    score: u32,
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
    let wire = run.map(|r| run_to_wire(r, u64::from(entry.character.0.level)));
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

            // Honor `startingDifficulty` — the floor the player chose to start from. It
            // used to be IGNORED (a `_body` bind), so every run restarted at the bottom.
            //
            // The run always starts at POSITION 0. The client never reads the requested
            // floor when it builds the run: `GenerateAbyss` stores it and nothing reads it
            // back, and `DoGenerateAbyss` walks the slices from position 0, numbering each
            // floor `slices[0].floorIndex + position` (#294 RE). So the served list must
            // BEGIN at the chosen floor — which is exactly what retail sends: three
            // captured `/start`s at startingDifficulty 1 / 15 / 90 are 150 slices each,
            // sliceIndex 0..149, floorIndex N..N+149 (so past floor 150 when N > 1).
            let start_floor = starting_difficulty.unwrap_or(1).clamp(1, MAX_START_FLOOR);
            let slices = build_run_slices(static_abyss, seed, start_floor, player_level);

            let run = AbyssRun {
                slices,
                revive_count: 0,
                initial_player_level: player_level,
                seed,
                score: 0.0,
                algorithm_version: static_abyss.algorithm_version.max(1),
                version: 1,
                current_floor_index: 0,
                killed_enemies: Default::default(),
                collected_enemy_loot: Default::default(),
                granted_future_rewards: Default::default(),
            };

            let wire = run_to_wire(&run, u64::from(player_level));

            // Generated data for the floor the run STARTS on — which is
            // `slices[0]`, not floor 1: a resumed run's first slice is the
            // floor the player is returning to. Serving floor 1's data here is
            // what hung every resumed run. Computed before `run` is moved into
            // the persisted state.
            // `.iter().next()` rather than `.first()`: diesel's `FirstDsl` is in
            // scope here and shadows the slice method.
            let gen_data = run
                .slices
                .get(run.current_floor_index)
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

/// `revive` — the player stood back up after dying.
///
/// Paid in Scrolls of Revival, charged by the server: see
/// [`blades_lib::features::revive`]. `gemsPayment` is `false` in all 14 captured
/// abyss revives (and all 80 dungeon ones), and the APK's `_reviveItemCostList`
/// prices a revive purely in scrolls, so there is no gem tender to read here.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ReviveAction {
    /// A BOOL on the wire in all 14 captured revives — see
    /// [`count_from_bool_or_number`]. Never read; parsed so the body survives.
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "count_from_bool_or_number")]
    gems_payment: u64,
    #[allow(dead_code)]
    #[serde(default)]
    time: u64,
}

/// The six `/update` action types the client actually sends.
///
/// Only one arm used to exist (`EnemyKilled`); the other five fell into `Unknown` and
/// were dropped, `abyss_slice_completed` — the floor-advance signal — among them.
/// The one arm not acted on yet (`enemy_loot_collected`) is named rather than swallowed
/// so the next change can see it, and so a body carrying it is not silently reduced to
/// "unknown".
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CombatCompletedAction {
    #[serde(default)]
    items: Vec<DurabilityUpdate>,
    #[allow(dead_code)]
    #[serde(default)]
    time: u64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct DurabilityUpdate {
    id: Uuid,
    /// Sent as a JSON **string**, not a number: `"durability": "673.2878"`.
    ///
    /// All 838 durability values in the captured corpus are strings and not one
    /// is a number, so `f64` here rejected every post-fight update — and because
    /// serde fails the whole body, the entire request 400s, not just this field.
    ///
    /// The dungeon path already reads both forms
    /// ([`crate::dungeon_update::deserialize_f64_number_or_string`]); the Abyss
    /// kept its own `f64` and never got the same treatment. Reuse it rather than
    /// grow a second reader for one wire quirk.
    ///
    /// That is report #156. The shape on the wire is exact and repeatable:
    ///
    /// ```text
    /// 12:11:45  abysses/current/start   200
    /// 12:11:50  .../update              200   <- enemy_killed
    /// 12:12:15  .../update              200   <- enemy_killed
    /// 12:12:16  .../update              400   <- first combat_completed
    /// 12:12:19  .../update              400   <- client retries the same action
    /// 12:12:27  .../update              400      …forever
    /// ```
    ///
    /// The client shows "Network Not Reachable" and the run is dead about a
    /// minute in, every time. The reporter did it twice, five minutes apart, and
    /// both runs have byte-identical shapes.
    #[serde(deserialize_with = "crate::dungeon_update::deserialize_f64_number_or_string")]
    durability: f64,
}

/// Accept a JSON integer OR a JSON bool.
///
/// `gemsPayment` is a **bool** in all 14 captured revives — `true`/`false`, not a
/// count — so `u64` rejected every one of them, killing the whole update the way
/// the durability field did. Nothing reads the value; it is parsed only so the
/// body survives, and a bool maps to 1/0 rather than being invented.
fn count_from_bool_or_number<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match Value::deserialize(d)? {
        Value::Bool(b) => Ok(u64::from(b)),
        Value::Number(n) => Ok(n.as_u64().unwrap_or(0)),
        Value::Null => Ok(0),
        other => Err(D::Error::custom(format!(
            "gemsPayment must be a number or a bool, got {other}"
        ))),
    }
}

/// A potion or food used during the current run.
///
/// The client names the stackable template, never an instanced inventory id. Only a
/// currently equipped consumable is eligible, and the server debits its own backpack
/// count instead of trusting a request-side quantity.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ItemConsumedAction {
    item_template_id: Uuid,
    #[allow(dead_code)]
    #[serde(default)]
    time: u64,
}

/// A corpse the player looted during the current floor.
///
/// Only the generated-enemy identity is authoritative. `loot` is deliberately
/// accepted as an opaque value and ignored: trusting it would let a modified
/// client name its own payout. The server reconstructs the enemy from the same
/// deterministic generated data it sent when the floor opened.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnemyLootCollectedAction {
    spawn_group_id: Uuid,
    spawner_index: usize,
    enemy_index: usize,
    #[allow(dead_code)]
    #[serde(default)]
    loot: Value,
    #[allow(dead_code)]
    #[serde(default)]
    time: u64,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AbyssUpdateAction {
    /// The body is intentionally not trusted; the arm itself is the kill signal.
    #[allow(dead_code)]
    EnemyKilled(EnemyKilledAction),
    /// The body is intentionally not trusted; the arm itself advances the floor.
    #[allow(dead_code)]
    AbyssSliceCompleted(SliceCompletedAction),
    /// The body is intentionally not trusted; the arm itself counts the revive.
    #[allow(dead_code)]
    Revive(ReviveAction),
    /// Gear durability after a fight.
    CombatCompleted(CombatCompletedAction),
    /// Loot the player picked up off a corpse. The request names the corpse;
    /// the payout comes exclusively from server-generated floor data.
    EnemyLootCollected(EnemyLootCollectedAction),
    /// A potion/food used mid-run. The server removes one owned, equipped stack.
    ItemConsumed(ItemConsumedAction),
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
    /// The score-gauge rung(s) this request crossed, merged into one block —
    /// present only on the response that crosses (see [`grant_reached_rungs`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    reward: Option<RewardGrant>,
    abyss_future_rewards: Vec<AbyssFutureRewardWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    abyss_dungeon_generated_data: Option<AbyssDungeonGeneratedData>,
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

            let mut tracker = InventoryChangeTracker::default();

            if let Some(run) = entry.server_state.0.abyss.as_mut() {
                let enemy_loot = collect_enemy_loot(&app_state.game_data, run, &body.actions);
                let looted_inventory = enemy_loot
                    .iter()
                    .any(|grant| !grant.stackable_items.is_empty() || !grant.items.is_empty());
                for grant in &enemy_loot {
                    apply_reward(
                        grant,
                        &mut entry.wallet.0,
                        &mut entry.inventory.0,
                        &mut entry.character.0,
                        &mut tracker,
                    );
                }

                let revive_scrolls =
                    apply_actions(&app_state.static_data.abyss, run, &body.actions);

                // Pay every gauge rung the score now reaches, BEFORE the next rung
                // is advertised below — retail moves `abyssFutureRewards` on in the
                // same response that carries the granted `reward`.
                let rung_reward = grant_reached_rungs(
                    run,
                    u64::from(entry.character.0.level),
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                let rung_backpack = rung_reward.as_ref().is_some_and(|reward| {
                    !reward.stackable_items.is_empty() || !reward.items.is_empty()
                });
                if rung_reward
                    .as_ref()
                    .is_some_and(|reward| !reward.chests.is_empty())
                {
                    entry.inventory.0.treasury_version += 1;
                }

                let revive_count = run.revive_count;
                let future_rewards =
                    build_future_rewards(run.score, u64::from(entry.character.0.level), run.seed);
                let abyss_dungeon_generated_data =
                    active_floor_generated_data(&app_state.game_data, run);
                apply_combat_durability(&body.actions, &mut entry.inventory.0, &mut tracker);
                let consumed = apply_item_consumption(
                    &body.actions,
                    &mut entry.inventory.0,
                    &mut tracker,
                );
                // A revive is paid for by the server, not by a client-sent
                // `item_consumed`: all 14 captured abyss revives come back with the
                // Scroll of Revival at its new count (1802 -> 1800 -> 1796, the same
                // 1/2/4 ladder the quest dungeons charge).
                let charged_revive = revive_scrolls > 0
                    && consume_stackable(
                        &mut entry.inventory.0,
                        revive::REVIVE_SCROLL_TEMPLATE,
                        revive_scrolls,
                        &mut tracker,
                    )
                    .inspect_err(|error| {
                        log::warn!("abyss: revive charged nothing -- {error}");
                    })
                    .is_ok();

                if looted_inventory || rung_backpack || consumed > 0 || charged_revive {
                    // Retail increments once per inventory-mutating request, not once
                    // per action in the batch.
                    entry.inventory.0.backpack_version += 1;
                }

                save_economy(&mut conn, character_id, &entry).await?;

                let inv = entry.inventory.0.generate_client_update(&tracker);

                Ok::<_, BladeApiError>(Json(UpdateAbyssResponse {
                    reward: rung_reward,
                    abyss_future_rewards: future_rewards,
                    abyss_dungeon_generated_data,
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
                    reward: None,
                    // No active run: nothing to advertise against.
                    abyss_future_rewards: Vec::new(),
                    abyss_dungeon_generated_data: None,
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
    initial_player_level: u32,
) -> Vec<AbyssSliceEntry> {
    let start_floor = start_floor.max(1);
    // Number of slices remaining from `start_floor` to the top (`total`).
    let remaining = (total as u32).saturating_sub(start_floor - 1) as usize;
    let mut slices = Vec::with_capacity(remaining);
    for k in 0..remaining {
        let floor = start_floor + k as u32; // absolute 1-based floor
        let abs = (floor - 1) as usize; // absolute 0-based floor
        // The difficulty is relative to the run's `initialPlayerLevel`, never a
        // per-floor constant — see [`slice_difficulty`].
        let diff = slice_difficulty(floor, initial_player_level);
        let dungeon_uuid = if abs < static_abyss.fixed_slices.len() {
            static_abyss.fixed_slices[abs].dungeon_settings_id
        } else {
            // Deep floor: the dungeon is picked by seeded, depth-weighted random over
            // `dungeonPool` (weak monsters shallow, tough deep); with that data absent it
            // degrades to the legacy `randomPool` cycling — reusing `(seed + floor)` so a
            // resumed run is still deterministic per absolute floor.
            pick_deep_dungeon(static_abyss, seed, floor, abs)
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

/// `AbyssScaling._abyssScalingCurve[k].difficultyOffset` for k = 0..14, the full asset
/// (`deploy/static/abyss.json` stores only the first six rows, the ones whose gold/XP
/// multiplier differs; the rest repeat x6). Cross-checked against the independent
/// extraction in blades-capture `reference/game-defs/abyss.json` `scaling_curve`.
const DIFFICULTY_OFFSETS: [u32; 15] = [0, 2, 4, 6, 10, 14, 18, 24, 32, 40, 50, 60, 72, 84, 99];

/// The highest difficulty retail ever generated: every captured slice from floor
/// `initialPlayerLevel + 14` upward is exactly 100, out to floor 239.
const MAX_SLICE_DIFFICULTY: u32 = 100;

/// The `difficultyLevel` of absolute floor `floor` in a run started at
/// `initial_player_level` — retail's rule, which matches all 450 slices of the three
/// captured `/start`s (initialPlayerLevel 10 from floor 1, 11 from floor 15, 75 from
/// floor 90) and the initialPlayerLevel-4 run (1,2,3,4,6,8,10,14,18 on floors 1-9):
///
/// * floors at or below the player's level are their own number;
/// * floor `ipl + k` is `ipl + DIFFICULTY_OFFSETS[k]` — the gold/XP bonus curve's own
///   offsets, so floor `ipl + k` pays curve row k, which is the row the client's
///   `AbyssScaling.GetGoldAndXPBonusMultipliers(floorIndex, initialPlayerLevel)` shows;
/// * capped at [`MAX_SLICE_DIFFICULTY`].
///
/// This used to be a fixed ladder (1..10, 12, 14, 16, 20, 24, ... 100) copied from the
/// one captured run that happened to have initialPlayerLevel 10, ramping on to 400 past
/// floor 24. For any other player the bonus started on the wrong floor: at level 9,
/// floor 10 paid x1.25 while the client showed x2, floor 11 x2 against x3 — the
/// multiplier arriving a floor late (#312) — and above level 10 it arrived early. Past
/// floor 24 enemies ran up to level 400 where retail stops at 100.
fn slice_difficulty(floor: u32, initial_player_level: u32) -> u32 {
    let difficulty = if floor <= initial_player_level {
        floor
    } else {
        let k = ((floor - initial_player_level) as usize).min(DIFFICULTY_OFFSETS.len() - 1);
        initial_player_level + DIFFICULTY_OFFSETS[k]
    };
    difficulty.min(MAX_SLICE_DIFFICULTY)
}

/// How many floors one run is served with — retail's length for every start floor.
const RUN_FLOORS: u32 = 150;

/// A sanity cap on `startingDifficulty`. Retail measured a start at floor 90 (run to
/// 239); the cap only stops a bogus request from building absurd floor numbers.
const MAX_START_FLOOR: u32 = 10_000;

/// The slice list a run is SERVED with, retail's shape: [`RUN_FLOORS`] floors starting
/// at `start_floor`, `sliceIndex` = array position, `floorIndex` = `start_floor` +
/// position, nothing pre-completed.
///
/// #356 served all 150 floors from floor 1 and marked those below the start floor
/// `completed`, on the theory that the client starts at the first uncompleted slice.
/// It does not — the client parses only `dungeonSettingsId`, `difficultyLevel`,
/// `hardcore` and `floorIndex`, and builds from position 0 — so a floor-149 start put
/// the player on floor 1 while the server tracked floor 149 (#294).
///
/// Content is per ABSOLUTE floor, so floor 78 of a run started at 78 is the same
/// dungeon as floor 78 of a fresh run, and the same difficulty at the same
/// `initial_player_level` ([`slice_difficulty`]).
fn build_run_slices(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    seed: i64,
    start_floor: u32,
    initial_player_level: u32,
) -> Vec<AbyssSliceEntry> {
    let start_floor = start_floor.max(1);
    let last_floor = start_floor + RUN_FLOORS - 1;
    let mut slices = build_slices_from(
        static_abyss,
        seed,
        last_floor as usize,
        start_floor,
        initial_player_level,
    );
    for (position, slice) in slices.iter_mut().enumerate() {
        slice.slice_index = position as u32;
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
    build_slices_from(static_abyss, seed, n, 1, TEST_IPL)
}

/// The initialPlayerLevel the slice-building tests run at — the captured run's.
#[cfg(test)]
const TEST_IPL: u32 = 10;

/// Convert a server-side `AbyssRun` to the wire shape.
fn run_to_wire(run: &AbyssRun, character_level: u64) -> AbyssWire {
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
        abyss_future_rewards: build_future_rewards(run.score, character_level, run.seed),
    }
}

/// The future rewards to advertise: the NEXT rung this run has not reached.
///
/// This used to map the whole of `static_data.abyss.future_rewards` onto the
/// wire, which was only ever correct by accident — that list holds ONE rung, so
/// a run past score 35 was advertised the same reward for the rest of its life
/// and the nine rungs above it did not exist (#172). Retail sends exactly one
/// entry, the next unreached rung, in 707 of 707 captured responses.
///
/// The ladder and its contents now come from the mined corpus in
/// `blades_lib::features::abyss_rewards`, which also knows that the two chest
/// rungs are deterministic and cut to the player's own level.
fn build_future_rewards(score: f64, character_level: u64, run_seed: i64) -> Vec<AbyssFutureRewardWire> {
    abyss_rewards::next_future_reward(score, character_level, run_seed)
        .map(|(score, reward)| AbyssFutureRewardWire { score, reward })
        .into_iter()
        .collect()
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
        &slice.dungeon_settings_id,
        slice.difficulty_level as i64,
        0,
    )
}

fn active_floor_generated_data(
    game_data: &blades_lib::game_data::GameData,
    run: &AbyssRun,
) -> Option<AbyssDungeonGeneratedData> {
    run.slices
        .get(run.current_floor_index)
        .and_then(|slice| {
            blades_lib::util::dungeon::generate_for_dungeon(
                game_data,
                &slice.dungeon_settings_id,
                slice.difficulty_level as i64,
                0,
            )
        })
        .map(|inner| AbyssDungeonGeneratedData {
            quest_id: Uuid::parse_str(ABYSS_QUEST_ID).unwrap(),
            inner,
        })
}

fn abyss_enemy_key(
    floor_index: u32,
    spawn_group_id: Uuid,
    spawner_index: usize,
    enemy_index: usize,
) -> String {
    format!("{floor_index}:{spawn_group_id}:{spawner_index}:{enemy_index}")
}

/// Resolve corpse-loot actions against the generated data for the floor on which
/// each action occurred.
///
/// The client is allowed to identify a corpse, never to choose its contents. A
/// kill is recorded only when that identity exists in the server-generated floor;
/// loot is paid only after such a kill and only once. Both sets live in the
/// server-only abyss state, so retries are idempotent and no new client wire keys
/// are introduced.
fn collect_enemy_loot(
    game_data: &blades_lib::game_data::GameData,
    run: &mut AbyssRun,
    actions: &[AbyssUpdateAction],
) -> Vec<RewardGrant> {
    let mut active_slice = run.current_floor_index;
    let mut grants = Vec::new();
    // A request commonly carries `enemy_killed` and `enemy_loot_collected` for
    // the same floor. Generate that floor once, not once per action.
    let mut generated_by_slice: HashMap<usize, Option<DungeonGeneratedData>> = HashMap::new();

    for action in actions {
        match action {
            AbyssUpdateAction::EnemyKilled(kill) => {
                let Some(slice) = run.slices.get(active_slice) else {
                    continue;
                };
                let floor_index = slice.floor_index;
                let generated = generated_by_slice.entry(active_slice).or_insert_with(|| {
                    blades_lib::util::dungeon::generate_for_dungeon(
                        game_data,
                        &slice.dungeon_settings_id,
                        slice.difficulty_level as i64,
                        0,
                    )
                });
                let Some(generated) = generated.as_ref() else {
                    continue;
                };
                let enemy = blades_lib::user_data::EnemyIndex::new(
                    kill.spawn_group_id,
                    kill.spawner_index,
                    kill.enemy_index,
                );
                if generated.get_enemy(&enemy).is_some() {
                    run.killed_enemies.insert(abyss_enemy_key(
                        floor_index,
                        kill.spawn_group_id,
                        kill.spawner_index,
                        kill.enemy_index,
                    ));
                } else {
                    log::warn!(
                        "abyss: enemy_killed for unknown generated enemy {enemy} on floor {}",
                        floor_index
                    );
                }
            }
            AbyssUpdateAction::EnemyLootCollected(collected) => {
                let Some(slice) = run.slices.get(active_slice) else {
                    continue;
                };
                let floor_index = slice.floor_index;
                let key = abyss_enemy_key(
                    floor_index,
                    collected.spawn_group_id,
                    collected.spawner_index,
                    collected.enemy_index,
                );
                if !run.killed_enemies.contains(&key)
                    || run.collected_enemy_loot.contains(&key)
                {
                    continue;
                }

                let generated = generated_by_slice.entry(active_slice).or_insert_with(|| {
                    blades_lib::util::dungeon::generate_for_dungeon(
                        game_data,
                        &slice.dungeon_settings_id,
                        slice.difficulty_level as i64,
                        0,
                    )
                });
                let Some(generated) = generated.as_ref() else {
                    continue;
                };
                let enemy_index = blades_lib::user_data::EnemyIndex::new(
                    collected.spawn_group_id,
                    collected.spawner_index,
                    collected.enemy_index,
                );
                let Some(enemy) = generated.get_enemy(&enemy_index) else {
                    continue;
                };

                let loot = enemy.merged_loot_table();
                run.collected_enemy_loot.insert(key);
                grants.push(RewardGrant {
                    currencies: loot.currencies,
                    stackable_items: loot.stackable_items,
                    items: loot
                        .item
                        .0
                        .into_iter()
                        .map(|(id, item)| blades_lib::economy::RewardItem { id, item })
                        .collect(),
                    ..RewardGrant::default()
                });
            }
            AbyssUpdateAction::AbyssSliceCompleted(_) => {
                if active_slice + 1 < run.slices.len() {
                    active_slice += 1;
                }
            }
            _ => {}
        }
    }

    grants
}

/// Pay every score-gauge rung the run has reached and not yet been paid (#294, #8).
///
/// The server advertised the next rung (`abyssFutureRewards`) but never paid one:
/// `/end` pays the per-floor gold/XP and `/update` the corpse loot, and nothing
/// granted a rung when the score crossed it. Retail paid it on the `/update` whose
/// action crossed it (captured: the `enemy_killed` response), as a top-level
/// `reward` — `{"stackableItems":{…}}`, `{"chests":[{"id","tier","level"}]}` or
/// `{"items":[…]}` — with the matching `inventory.backpack` / `inventory.treasury`
/// delta, and the SAME response already advertising the next rung. One kill that
/// crossed 35 and 50 at once came back with both merged into one `reward`.
///
/// The reward is [`abyss_rewards::future_reward_for_rung`] — the resolution the
/// advertisement uses, same seed — so a rung pays exactly what it showed. Each
/// rung is recorded in `run.granted_future_rewards` before it is applied, which
/// makes a retried request pay nothing twice. The set lives on the run, and a new
/// run starts empty, because retail's gauge restarts at rung 35 every run.
///
/// Returns the merged grant, `None` when nothing was crossed. Does not bump the
/// inventory versions; the caller bumps once per request.
fn grant_reached_rungs(
    run: &mut AbyssRun,
    character_level: u64,
    wallet: &mut CompleteWallet,
    inventory: &mut CompleteInventory,
    character: &mut blades_lib::user_data::CompleteCharacter,
    tracker: &mut InventoryChangeTracker,
) -> Option<RewardGrant> {
    let due: Vec<u32> = abyss_rewards::reached_rungs(run.score)
        .filter(|rung| !run.granted_future_rewards.contains(rung))
        .collect();
    if due.is_empty() {
        return None;
    }

    let mut total = RewardGrant::default();
    for rung in due {
        run.granted_future_rewards.insert(rung);
        let Some(reward) = abyss_rewards::future_reward_for_rung(rung, character_level, run.seed)
        else {
            continue;
        };
        for (currency, amount) in reward.currencies {
            *total.currencies.entry(currency).or_default() += amount;
        }
        for (template, count) in reward.stackable_items {
            *total.stackable_items.entry(template).or_default() += count;
        }
        for mut item in reward.items {
            // The advertised instance id is a function of the run seed, and the
            // seed is per character, so a later run advertises the same id
            // again. Granting it twice would overwrite the first copy — and any
            // tempering on it — so a held id is re-minted.
            let held = inventory.backpack.items.0.contains_key(&item.id)
                || inventory
                    .loadout
                    .equipped_items
                    .0
                    .values()
                    .any(|equipped| equipped.id == item.id)
                || total.items.iter().any(|granted| granted.id == item.id);
            if held {
                item.id = Uuid::new_v4();
            }
            total.items.push(item);
        }
        total.chests.extend(reward.chests);
    }

    apply_reward(&total, wallet, inventory, character, tracker);
    for chest in &mut total.chests {
        // Retail echoes the treasury id it assigned: {"id":"1","tier":1,"level":7}.
        chest.id = Some(grant_chest(inventory, chest.tier, chest.level, tracker));
    }
    (!total.is_empty()).then_some(total)
}

/// Apply one `/update` body's actions to the run, in the order the client sent them.
///
/// Order matters: a body can carry the last kill of a floor AND that floor's
/// `abyss_slice_completed`, and the kill has to be credited to the floor the player was
/// still standing on when it happened.
/// Returns the Scrolls of Revival this batch's `revive` actions owe, priced off the
/// run's revive count as each one lands — a batch carrying two revives pays two
/// different rungs of the ladder. The caller charges it; `run` has no inventory.
fn apply_actions(
    static_abyss: &blades_lib::static_data::AbyssStaticData,
    run: &mut AbyssRun,
    actions: &[AbyssUpdateAction],
) -> u64 {
    let mut revive_scrolls = 0;
    for action in actions {
        match action {
            AbyssUpdateAction::EnemyKilled(kill) => {
                let slice = run.slices.get(run.current_floor_index);
                run.score += kills_score(
                    static_abyss,
                    slice,
                    run.initial_player_level,
                    Some(kill.spawn_group_id),
                    1,
                );
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
                // Priced BEFORE the increment: the first revive of a run is rung 0.
                revive_scrolls += revive::scroll_cost(u64::from(run.revive_count));
                run.revive_count += 1;
            }
            // Inventory changes are applied separately from run scoring below.
            AbyssUpdateAction::CombatCompleted(_) | AbyssUpdateAction::ItemConsumed(_) => {}
            // Parsed, named, and deliberately not acted on yet — see the enum's doc.
            AbyssUpdateAction::EnemyLootCollected(_) | AbyssUpdateAction::Unknown => {}
        }
    }
    revive_scrolls
}

/// Apply the durability reported after abyss combat to gear this character has equipped.
///
/// The client reports absolute durability. It is authoritative only in the damaging
/// direction: accepting a higher value would let a modified client repair for free, while
/// accepting an unknown id would let it invent inventory. Repeated updates in one request
/// are safe because every accepted value must be below the value already stored.
fn apply_combat_durability(
    actions: &[AbyssUpdateAction],
    inventory: &mut CompleteInventory,
    tracker: &mut InventoryChangeTracker,
) -> usize {
    let mut changed = 0;
    for action in actions {
        let AbyssUpdateAction::CombatCompleted(action) = action else {
            continue;
        };
        for update in &action.items {
            if !update.durability.is_finite() || update.durability < 0.0 {
                continue;
            }
            let Some(equipped) = inventory
                .loadout
                .equipped_items
                .0
                .values_mut()
                .find(|item| item.id == update.id)
            else {
                continue;
            };
            if update.durability >= equipped.item.durability {
                continue;
            }
            equipped.item.durability = update.durability;
            tracker
                .modified_loadout
                .modified_equipped_items
                .insert(equipped.slot);
            changed += 1;
        }
    }
    changed
}

/// Debit one unit of the named stackable for each `item_consumed` action, as retail
/// and the quest-dungeon handler (`dungeon_update::charge_stackable`) both do.
///
/// There is deliberately no check against `loadout.equipped_consumables`. That list is
/// only filled by an `equippedConsumables` field on `/loadouts/current`, which the client
/// does not send: 0 of 2,280 retail `/loadouts/current` captures and 0 of 606 retail
/// inventory reads carry it, and 0 of 540 prod characters hold one. Gating on it ignored
/// every abyss potion (84 `unequipped template` warnings from 2026-09-16 to 2026-10-03),
/// so the client healed and the count bounced back on the next sync. Retail debited
/// the one captured abyss `item_consumed` (Health T10 890 -> 889) and all 110 distinct
/// captured quest-dungeon ones, with no equipped-slot data on the server at all.
///
/// `consume_stackable` keeps it safe: it touches only the named template, leaves the
/// stack untouched when the player does not own one (never negative), and records the
/// diff the response must carry. A shortfall is logged, not failed: the same POST
/// carries the floor's kills and loot, which a 400 would throw away.
fn apply_item_consumption(
    actions: &[AbyssUpdateAction],
    inventory: &mut CompleteInventory,
    tracker: &mut InventoryChangeTracker,
) -> usize {
    let mut consumed = 0;
    for action in actions {
        let AbyssUpdateAction::ItemConsumed(action) = action else {
            continue;
        };
        match consume_stackable(inventory, action.item_template_id, 1, tracker) {
            Ok(()) => consumed += 1,
            Err(error) => log::warn!(
                "abyss: ignored item_consumed for unavailable template {}: {}",
                action.item_template_id,
                error
            ),
        }
    }
    consumed
}

/// The `killScoreMultiplier` for a kill neither multiplier table resolves.
///
/// Every enemy carries one in the game data (`enemies.json` `variants[*].stats
/// .killScoreMultiplier`: 0.33 on 22 critter variants, 1.0 on 559, 2.0 on 50 bosses),
/// and the client multiplies every kill by it before filling the reward gauge. The kill
/// names its spawn group, so the multiplier is that group's enemy's
/// ([`abyss_kill_score::kill_multiplier`]), falling back to the floor's; this is only for
/// a dungeon neither table holds (the four `AbyssEntrance` settings, which no slice uses).
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
    spawn_group_id: Option<Uuid>,
    count: usize,
) -> f64 {
    let Some(slice) = slice else { return 0.0 };
    let level_delta = slice.difficulty_level as i32 - initial_player_level as i32;
    let per_kill = static_abyss.kill_score(level_delta) as f64;
    // The server must cross each gauge rung on the kill the client does (#312). Scored
    // at 1.0 everywhere it ran ahead on critter floors; scored at the floor's lowest
    // multiplier it ran behind on mixed ones, leaving the client's gauge pinned full.
    let multiplier = abyss_kill_score::kill_multiplier(
        spawn_group_id,
        slice.dungeon_settings_id,
        slice.difficulty_level,
    )
    .unwrap_or(FALLBACK_KILL_SCORE_MULTIPLIER);
    multiplier * per_kill * count as f64
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
    use blades_lib::user_data::{Item, ItemPropertiesAll, SingleEquippedItem};

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

    /// The captured retail `/start`s: `(initialPlayerLevel, start floor, the captured
    /// difficulties from the start floor on, whether the rest of the 150 were all 100)`.
    /// The first three are whole `/start` responses (450 slices); the ipl-4 run is the
    /// nine floors that capture covered.
    const RETAIL_STARTS: [(u32, u32, &[u32], bool); 4] = [
        (10, 1, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 16, 20, 24, 28, 34, 42, 50, 60, 70, 82, 94], true),
        (11, 15, &[21, 25, 29, 35, 43, 51, 61, 71, 83, 95], true),
        (75, 90, &[], true),
        (4, 1, &[1, 2, 3, 4, 6, 8, 10, 14, 18], false),
    ];

    /// Retail's slice difficulty is relative to the run's `initialPlayerLevel`. The
    /// served list used to carry one fixed ladder (the ipl-10 run's) and a ramp to 400:
    /// the ipl-11 start at floor 15 came back 24, 28, 34 ... instead of 21, 25, 29, and
    /// the ipl-75 start at floor 90 came back 400 instead of 100.
    #[test]
    fn served_difficulties_are_the_captured_retail_ones() {
        let sd = real_static_abyss();
        for (ipl, start, head, rest_is_100) in RETAIL_STARTS {
            let served = build_run_slices(&sd, 7, start, ipl);
            assert_eq!(served.len(), 150);
            for (position, slice) in served.iter().enumerate() {
                let expected = match head.get(position) {
                    Some(d) => *d,
                    None if rest_is_100 => 100,
                    None => break,
                };
                assert_eq!(
                    slice.difficulty_level, expected,
                    "ipl {ipl}, start {start}: floor {}",
                    slice.floor_index
                );
            }
        }
    }

    /// What `/end` pays per floor is the bonus the client shows for that floor:
    /// `GetGoldAndXPBonusMultipliers(floorIndex, initialPlayerLevel)`, row
    /// `floorIndex - initialPlayerLevel` of the curve, none below the player's level.
    /// With the fixed ipl-10 ladder a level-9 player was paid a floor late (x1.25 on
    /// floor 10 where the client showed x2) and a level-13 player a floor early — the
    /// "multiplier kicks in later" in the report.
    #[test]
    fn the_bonus_multiplier_is_paid_on_the_floor_the_client_shows_it() {
        let sd = real_static_abyss();
        let client = |floor: u32, ipl: u32| -> f64 {
            if floor < ipl {
                return 1.0;
            }
            let row = DIFFICULTY_OFFSETS[((floor - ipl) as usize).min(DIFFICULTY_OFFSETS.len() - 1)];
            sd.multiplier_for_offset(row as i32).0
        };
        for ipl in [1, 4, 9, 10, 13, 30, 60, 75] {
            for slice in build_run_slices(&sd, 7, 1, ipl).iter().take(80) {
                let paid = sd
                    .multiplier_for_offset(slice.difficulty_level as i32 - ipl as i32)
                    .0;
                assert_eq!(
                    paid,
                    client(slice.floor_index, ipl),
                    "ipl {ipl}, floor {}",
                    slice.floor_index
                );
            }
        }
    }

    /// The deep-floor dungeon pick is depth-appropriate (weak shallow, tough deep) AND
    /// deterministic for a given (seed, floor) — resume-at-floor reproduces it (fix 2b).
    #[test]
    fn deep_floor_dungeon_pick_is_depth_appropriate_and_deterministic() {
        let (sd, weak, tough) = test_static_abyss_with_new_keys();
        let seed = 7i64;
        let fresh = build_slices_from(&sd, seed, 150, 1, TEST_IPL);

        // Shallow deep-band (floors 25-40): only tier 1 weighted → the weak cave.
        for s in fresh.iter().filter(|s| (25..=40).contains(&s.floor_index)) {
            assert_eq!(s.dungeon_settings_id, weak, "floor {} weak (tier 1)", s.floor_index);
        }
        // Deepest band (41+): only tier 9 weighted → the dragon lair.
        for s in fresh.iter().filter(|s| s.floor_index >= 41) {
            assert_eq!(s.dungeon_settings_id, tough, "floor {} tough (tier 9)", s.floor_index);
        }

        // Determinism: a run resumed at floor 45 yields the SAME floor-45 content.
        let resumed = build_slices_from(&sd, seed, 150, 45, TEST_IPL);
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
        // ipl 10: floor 23 is ipl + 13 (+84); floor 24 is past the ramp -> the cap.
        assert_eq!(slices[22].difficulty_level, 94);
        assert_eq!(slices[23].difficulty_level, 100);
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
        let resumed = build_slices_from(&sd, seed, 150, 40, TEST_IPL);
        assert_eq!(resumed.len(), 150 - 39, "floors 40..=150");
        assert_eq!(resumed[0].floor_index, 40, "first slice is the requested floor");
        assert_eq!(resumed[0].slice_index, 39, "slice_index is absolute (floor-1)");
        assert_eq!(resumed.last().unwrap().floor_index, 150, "runs to the top floor");

        // The per-floor content matches a fresh full run at the same absolute floor.
        let fresh = build_slices_from(&sd, seed, 150, 1, TEST_IPL);
        assert_eq!(
            resumed[0].dungeon_settings_id, fresh[39].dungeon_settings_id,
            "floor 40 content is stable whether resumed or reached fresh"
        );
        assert_eq!(resumed[0].difficulty_level, fresh[39].difficulty_level);
    }

    /// #294 / retail: a run started at floor N is 150 slices, `sliceIndex` 0..149 and
    /// `floorIndex` N..N+149, none pre-completed. The three captured retail `/start`s
    /// (startingDifficulty 1, 15, 90) all have exactly this shape.
    #[test]
    fn a_run_starts_at_the_chosen_floor_in_retail_shape() {
        let sd = test_static_abyss();
        for start in [1u32, 15, 90, 149] {
            let served = build_run_slices(&sd, 12345, start, TEST_IPL);
            assert_eq!(served.len(), 150, "start {start}: retail serves 150 floors");
            for (p, s) in served.iter().enumerate() {
                assert_eq!(s.slice_index as usize, p, "start {start}: sliceIndex = position");
                assert_eq!(s.floor_index, start + p as u32, "start {start}: floorIndex = N + position");
                assert!(!s.completed && !s.enemy_killed, "start {start}: nothing pre-completed");
            }
        }
    }

    /// The client builds from position 0, so the FIRST slice must be the chosen floor.
    /// This is the assertion #356's shape fails: it put floor 1 first.
    #[test]
    fn the_first_served_slice_is_the_chosen_floor() {
        let sd = test_static_abyss();
        assert_eq!(build_run_slices(&sd, 12345, 149, TEST_IPL)[0].floor_index, 149);
    }

    /// Content is chosen per absolute floor: resuming does not reshuffle the dungeons.
    #[test]
    fn a_resumed_floor_has_the_fresh_runs_content() {
        let sd = test_static_abyss();
        let resumed = build_run_slices(&sd, 12345, 78, TEST_IPL);
        let fresh = build_run_slices(&sd, 12345, 1, TEST_IPL);
        assert_eq!(resumed[0].dungeon_settings_id, fresh[77].dungeon_settings_id);
        assert_eq!(resumed[0].difficulty_level, fresh[77].difficulty_level);
    }

    /// CONTROL: a fresh run is the unchanged floor-1..150 list.
    #[test]
    fn a_fresh_run_is_unchanged() {
        let sd = test_static_abyss();
        let served = build_run_slices(&sd, 12345, 1, TEST_IPL);
        let fresh = build_slices_from(&sd, 12345, 150, 1, TEST_IPL);
        assert_eq!(served.len(), fresh.len());
        for (a, b) in served.iter().zip(fresh.iter()) {
            assert_eq!(a.slice_index, b.slice_index);
            assert_eq!(a.floor_index, b.floor_index);
            assert_eq!(a.dungeon_settings_id, b.dungeon_settings_id);
            assert_eq!(a.difficulty_level, b.difficulty_level);
        }
    }

    /// `startingDifficulty` of 1 (or None → 1) yields the original fresh-run slices, and
    /// a value past the top clamps to a single top floor — never panics, never empty of
    /// the requested floor.
    #[test]
    fn build_slices_from_floor_one_matches_fresh() {
        let sd = test_static_abyss();
        let seed = 7i64;
        let fresh = build_slices_from(&sd, seed, 150, 1, TEST_IPL);
        assert_eq!(fresh.len(), 150);
        assert_eq!(fresh[0].floor_index, 1);
        assert_eq!(fresh[0].slice_index, 0);

        // Resume at the top floor → exactly one slice (floor 150).
        let top = build_slices_from(&sd, seed, 150, 150, TEST_IPL);
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
            killed_enemies: Default::default(),
            collected_enemy_loot: Default::default(),
            granted_future_rewards: Default::default(),
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
        assert_eq!(kills_score(&sd, Some(&slice), 40, None, 1), 10.0);
        // Three kills → 30, where the old code gave 3.
        assert_eq!(kills_score(&sd, Some(&slice), 40, None, 3), 30.0);
        assert_eq!(kills_score(&sd, None, 40, None, 3), 0.0, "no slice → no score");
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
        let AbyssUpdateAction::CombatCompleted(combat) = &req.actions[1] else {
            panic!("combat_completed must have its own arm");
        };
        assert_eq!(combat.items.len(), 1);
        assert_eq!(combat.items[0].id, Uuid::nil());
        assert_eq!(combat.items[0].durability, 90.0);
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

    #[test]
    fn combat_durability_only_damages_owned_equipped_gear() {
        let slot = Uuid::new_v4();
        let item_id = Uuid::new_v4();
        let unknown_id = Uuid::new_v4();
        let mut inventory = CompleteInventory {
            backpack: Default::default(),
            loadout: Default::default(),
            treasury: Default::default(),
            overflow_treasury: Default::default(),
            backpack_version: 0,
            treasury_version: 0,
        };
        inventory.loadout.equipped_items.0.insert(
            slot,
            SingleEquippedItem {
                id: item_id,
                slot,
                item: Item {
                    item_template_id: Uuid::new_v4(),
                    grade: None,
                    tempering_level: 0,
                    durability: 100.0,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            },
        );

        let actions = parse_actions(serde_json::json!([
            {"type": "combat_completed", "items": [
                {"id": item_id, "durability": 75.0},
                {"id": item_id, "durability": 95.0},
                {"id": item_id, "durability": -1.0},
                {"id": unknown_id, "durability": 0.0}
            ]}
        ]));
        let mut tracker = InventoryChangeTracker::default();

        assert_eq!(
            apply_combat_durability(&actions, &mut inventory, &mut tracker),
            1
        );
        assert_eq!(
            inventory.loadout.equipped_items.0[&slot].item.durability,
            75.0
        );
        assert_eq!(
            tracker.modified_loadout.modified_equipped_items,
            std::collections::HashSet::from([slot])
        );

        let update = inventory.generate_client_update(&tracker);
        assert_eq!(
            update.loadout.equipped_items.0[&slot].item.durability,
            75.0,
            "the client diff must carry the changed durability"
        );
    }

    fn empty_inventory() -> CompleteInventory {
        CompleteInventory {
            backpack: Default::default(),
            loadout: Default::default(),
            treasury: Default::default(),
            overflow_treasury: Default::default(),
            backpack_version: 0,
            treasury_version: 0,
        }
    }

    /// The prod shape: the client never sends `equippedConsumables` on
    /// `/loadouts/current` (0 of 2,280 retail captures; 0 of 540 prod characters hold
    /// one), so the saved potion-slot list is empty. Retail still debits the drunk
    /// potion — its one captured abyss `item_consumed` took Health T10 from 890 to 889
    /// — and the old equipped-list gate ignored every abyss potion instead (84 journal
    /// warnings since 2026-09-16), handing them back on the next sync.
    #[test]
    fn item_consumed_debits_a_potion_that_is_not_in_the_saved_slots() {
        let health_t10: Uuid = "c2139cd9-1d9d-4d4e-80b2-133e07440158".parse().unwrap();
        let other = Uuid::new_v4();
        let mut inventory = empty_inventory();
        inventory.backpack.stackable_items.add(health_t10, 890);
        inventory.backpack.stackable_items.add(other, 7);
        assert!(inventory.loadout.equipped_consumables.is_empty());

        let actions = parse_actions(serde_json::json!([
            {"type": "item_consumed", "itemTemplateId": health_t10, "time": 1}
        ]));
        let mut tracker = InventoryChangeTracker::default();

        assert_eq!(
            apply_item_consumption(&actions, &mut inventory, &mut tracker),
            1
        );
        assert_eq!(inventory.backpack.stackable_items.count(health_t10), 889);
        assert_eq!(
            inventory.backpack.stackable_items.count(other),
            7,
            "other stacks untouched"
        );
        let update = inventory.generate_client_update(&tracker);
        assert_eq!(
            update.backpack.stackable_items.count(health_t10),
            889,
            "the response diff must carry the new count, as retail's did"
        );
        assert_eq!(
            update.backpack.stackable_items.count(other),
            0,
            "and nothing else"
        );
    }

    /// One unit per action, never below zero, and only the named template moves.
    #[test]
    fn item_consumed_debits_owned_stackables_and_never_goes_negative() {
        let potion = Uuid::new_v4();
        let equipped = Uuid::new_v4();
        let untouched = Uuid::new_v4();
        let mut inventory = empty_inventory();
        inventory.backpack.stackable_items.add(potion, 2);
        inventory.backpack.stackable_items.add(equipped, 4);
        inventory.backpack.stackable_items.add(untouched, 9);
        inventory.loadout.equipped_consumables.push(equipped);

        let actions = parse_actions(serde_json::json!([
            {"type": "item_consumed", "itemTemplateId": potion, "time": 1},
            {"type": "item_consumed", "itemTemplateId": potion, "time": 2},
            {"type": "item_consumed", "itemTemplateId": potion, "time": 3},
            {"type": "item_consumed", "itemTemplateId": equipped, "time": 4},
            {"type": "item_consumed", "itemTemplateId": Uuid::new_v4(), "time": 5}
        ]));
        let mut tracker = InventoryChangeTracker::default();

        assert_eq!(
            apply_item_consumption(&actions, &mut inventory, &mut tracker),
            3,
            "two owned potions plus the equipped one; the third potion and the unowned \
             template are ignored"
        );
        assert_eq!(inventory.backpack.stackable_items.count(potion), 0);
        assert_eq!(inventory.backpack.stackable_items.count(equipped), 3);
        assert_eq!(inventory.backpack.stackable_items.count(untouched), 9);
        assert!(
            !tracker
                .modified_backpack
                .stackable_items
                .contains(&untouched)
        );

        let update = inventory.generate_client_update(&tracker);
        assert!(
            update.backpack.removed_stackable_items.contains(&potion),
            "consuming the final potion must remove it in the client diff"
        );
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

    #[test]
    fn update_after_floor_completion_advertises_the_new_floors_generated_data() {
        let gd = game_data();
        let sd = real_static_abyss();
        let floor_1 = "663053f0-3a46-4012-b004-6cb2e907f33c";
        let floor_76 = "65375990-e5b3-41cf-b5d3-cbe2c740cb1d";
        let mut run = run_from(&[(1, 10), (76, 400)], 10);
        run.slices[0].dungeon_settings_id = Uuid::parse_str(floor_1).unwrap();
        run.slices[1].dungeon_settings_id = Uuid::parse_str(floor_76).unwrap();
        run.current_floor_index = 0;

        let before = active_floor_generated_data(&gd, &run).expect("floor 1 generated data");
        assert!(
            before
                .inner
                .enemy_generated_data
                .contains_key(&Uuid::parse_str("c41668b3-ad8b-42b4-ba5d-a0574039a3cc").unwrap()),
            "control: c416... is a floor-1 spawn group"
        );

        apply_actions(
            &sd,
            &mut run,
            &parse_actions(serde_json::json!([{"type": "abyss_slice_completed", "time": 1}])),
        );

        let after = active_floor_generated_data(&gd, &run).expect("floor 76 generated data");
        assert_eq!(run.current_floor_index, 1);
        assert!(
            !after
                .inner
                .enemy_generated_data
                .contains_key(&Uuid::parse_str("c41668b3-ad8b-42b4-ba5d-a0574039a3cc").unwrap()),
            "floor 76 must not keep advertising floor-1 enemy ids"
        );
        let expected: std::collections::HashSet<_> = gd
            .dungeons
            .get(&Uuid::parse_str(floor_76).unwrap())
            .unwrap()
            .spawn_info
            .enemy_spawn_groups
            .keys()
            .copied()
            .collect();
        let got: std::collections::HashSet<_> =
            after.inner.enemy_generated_data.keys().copied().collect();
        assert_eq!(got, expected);
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

    /// Abyss corpse loot comes from the same capture-derived generated data the
    /// server sent for the floor. The request may name a corpse, but it cannot
    /// choose the contents, claim a corpse that was not killed, or replay one.
    #[test]
    fn abyss_enemy_loot_is_server_owned_and_once_only() {
        let gd = game_data();

        // Find one deterministic generated enemy that actually pays. This keeps
        // the test tied to the compiled 49,602-observation corpus rather than a
        // hand-authored payout that could disagree with it.
        let mut fixture = None;
        'dungeons: for dungeon_id in gd.dungeons.keys() {
            let Some(generated) = blades_lib::util::dungeon::generate_for_dungeon(
                &gd,
                dungeon_id,
                40,
                0,
            ) else {
                continue;
            };
            for (spawn_group_id, spawners) in &generated.enemy_generated_data {
                for (spawner_index, enemies) in spawners.iter().enumerate() {
                    for (enemy_index, enemy) in enemies.iter().enumerate() {
                        let loot = enemy.merged_loot_table();
                        if !loot.currencies.is_empty()
                            || !loot.stackable_items.is_empty()
                            || !loot.item.is_empty()
                        {
                            fixture = Some((
                                *dungeon_id,
                                *spawn_group_id,
                                spawner_index,
                                enemy_index,
                                loot,
                            ));
                            break 'dungeons;
                        }
                    }
                }
            }
        }
        let (dungeon_id, spawn_group_id, spawner_index, enemy_index, expected) =
            fixture.expect("the retail-derived corpus must contain a paying enemy");

        let mut run = run_from(&[(1, 40)], 40);
        run.current_floor_index = 0;
        run.slices[0].dungeon_settings_id = dungeon_id;
        run.slices[0].completed = false;
        run.slices[0].enemy_killed = false;

        let actions = parse_actions(serde_json::json!([
            {
                "type": "enemy_killed",
                "spawnGroupId": spawn_group_id,
                "spawnerIndex": spawner_index,
                "enemyIndex": enemy_index,
                "xpReward": 999999999.0,
                "time": 1
            },
            {
                "type": "enemy_loot_collected",
                "spawnGroupId": spawn_group_id,
                "spawnerIndex": spawner_index,
                "enemyIndex": enemy_index,
                "loot": {"currencies": {GOLD_CURRENCY_UUID: 999999999}},
                "time": 2
            }
        ]));

        let grants = collect_enemy_loot(&gd, &mut run, &actions);
        assert_eq!(grants.len(), 1, "a killed generated corpse pays once");
        assert_eq!(grants[0].currencies, expected.currencies);
        assert_eq!(grants[0].stackable_items, expected.stackable_items);
        assert_eq!(grants[0].items.len(), expected.item.0.len());
        for item in &grants[0].items {
            assert_eq!(
                item.item.item_template_id,
                expected.item.0[&item.id].item_template_id,
                "the generated item, not the request body, is granted"
            );
        }
        assert_eq!(run.killed_enemies.len(), 1);
        assert_eq!(run.collected_enemy_loot.len(), 1);

        assert!(
            collect_enemy_loot(&gd, &mut run, &actions).is_empty(),
            "a retried body must not pay the corpse twice"
        );

        let loot_only = parse_actions(serde_json::json!([{
            "type": "enemy_loot_collected",
            "spawnGroupId": spawn_group_id,
            "spawnerIndex": spawner_index,
            "enemyIndex": enemy_index,
            "loot": {"currencies": {GOLD_CURRENCY_UUID: 999999999}},
            "time": 3
        }]));
        let mut unearned = run_from(&[(1, 40)], 40);
        unearned.current_floor_index = 0;
        unearned.slices[0].dungeon_settings_id = dungeon_id;
        assert!(collect_enemy_loot(&gd, &mut unearned, &loot_only).is_empty());
        assert!(unearned.collected_enemy_loot.is_empty());
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
        let slices = build_slices_from(&sd, 12345, 150, 78, TEST_IPL);
        assert_eq!(slices[0].floor_index, 78);
        // The guard that matters: whatever start_abyss serves must come from
        // slices[0], whose dungeon is NOT the floor-1 dungeon.
        let floor1 = build_slices_from(&sd, 12345, 150, 1, TEST_IPL)[0].dungeon_settings_id;
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
        assert!(build_slices_from(&sd, 1, 150, 151, TEST_IPL).is_empty());
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

    // ── Score-gauge rungs (#294, #8) ─────────────────────────────────────────

    struct Player {
        wallet: CompleteWallet,
        inventory: CompleteInventory,
        character: blades_lib::user_data::CompleteCharacter,
    }

    /// A level-40 run on a level-40 floor: every kill is a same-level kill worth 10.
    fn gauge_fixture() -> (AbyssRun, Player) {
        let mut run = run_from(&[(1, 40), (2, 40)], 40);
        run.current_floor_index = 0;
        for slice in &mut run.slices {
            slice.completed = false;
            slice.enemy_killed = false;
        }
        run.seed = 0x5EED_294;
        let mut character = blades_lib::user_data::CompleteCharacter::default();
        character.level = 40;
        let player = Player {
            wallet: CompleteWallet::default(),
            inventory: CompleteInventory {
                backpack: Default::default(),
                loadout: Default::default(),
                treasury: Default::default(),
                overflow_treasury: Default::default(),
                backpack_version: 0,
                treasury_version: 0,
            },
            character,
        };
        (run, player)
    }

    fn kills(n: u64) -> Vec<AbyssUpdateAction> {
        parse_actions(serde_json::Value::Array((0..n).map(kill).collect()))
    }

    /// One `/update`: score the actions, then pay what they crossed — the order the
    /// handler runs them in.
    fn update(
        run: &mut AbyssRun,
        player: &mut Player,
        actions: &[AbyssUpdateAction],
        tracker: &mut InventoryChangeTracker,
    ) -> Option<RewardGrant> {
        apply_actions(&real_static_abyss(), run, actions);
        grant_reached_rungs(
            run,
            u64::from(player.character.level),
            &mut player.wallet,
            &mut player.inventory,
            &mut player.character,
            tracker,
        )
    }

    fn advertised(run: &AbyssRun, level: u64) -> Option<(u32, RewardGrant)> {
        build_future_rewards(run.score, level, run.seed)
            .into_iter()
            .next()
            .map(|wire| (wire.score, wire.reward))
    }

    fn stack_counts(inventory: &CompleteInventory, grant: &RewardGrant) -> Vec<(Uuid, u64)> {
        let mut counts: Vec<_> = grant
            .stackable_items
            .keys()
            .map(|t| (*t, inventory.backpack.stackable_items.count(*t)))
            .collect();
        counts.sort();
        counts
    }

    /// NEGATIVE CONTROL — the bug. The update path as it stood (corpse loot +
    /// scoring) runs the score past the advertised rung and pays nothing for it.
    #[test]
    fn the_old_update_path_pays_no_rung() {
        let gd = game_data();
        let (mut run, mut player) = gauge_fixture();
        let (rung, promised) = advertised(&run, 40).unwrap();
        assert_eq!(rung, 35);

        let actions = kills(4);
        let mut tracker = InventoryChangeTracker::default();
        for grant in collect_enemy_loot(&gd, &mut run, &actions) {
            apply_reward(
                &grant,
                &mut player.wallet,
                &mut player.inventory,
                &mut player.character,
                &mut tracker,
            );
        }
        apply_actions(&real_static_abyss(), &mut run, &actions);

        assert_eq!(run.score, 40.0, "the gauge did fill past 35");
        assert_eq!(
            advertised(&run, 40).unwrap().0,
            50,
            "and the gauge moved on"
        );
        for template in promised.stackable_items.keys() {
            assert_eq!(
                player.inventory.backpack.stackable_items.count(*template),
                0,
                "without the grant step the advertised rung-35 reward never arrives"
            );
        }
        assert!(run.granted_future_rewards.is_empty());
    }

    /// Crossing one rung pays exactly what was advertised for it, once, and the
    /// advertisement moves on to the next rung in the same response.
    #[test]
    fn crossing_a_rung_pays_what_it_advertised_once() {
        let (mut run, mut player) = gauge_fixture();
        let (rung, promised) = advertised(&run, 40).unwrap();
        assert_eq!(rung, 35);
        assert!(!promised.is_empty());

        let mut tracker = InventoryChangeTracker::default();
        assert!(
            update(&mut run, &mut player, &kills(3), &mut tracker).is_none(),
            "30 points is below the first rung"
        );
        let paid = update(&mut run, &mut player, &kills(1), &mut tracker)
            .expect("the kill that reaches 40 crosses rung 35");
        assert_eq!(paid, promised, "the rung pays exactly what it advertised");
        assert_eq!(
            run.granted_future_rewards,
            std::collections::BTreeSet::from([35])
        );
        assert_eq!(advertised(&run, 40).unwrap().0, 50, "next rung advertised");

        let after_grant = stack_counts(&player.inventory, &paid);
        for (template, count) in &after_grant {
            assert_eq!(*count, promised.stackable_items[template]);
        }
        let update_wire = player.inventory.generate_client_update(&tracker);
        assert!(
            !update_wire.backpack.stackable_items.is_empty(),
            "the backpack delta carries the granted stack"
        );

        // A retried request — the same state handed to the grant again — pays nothing.
        let mut retry_tracker = InventoryChangeTracker::default();
        assert!(
            grant_reached_rungs(
                &mut run,
                40,
                &mut player.wallet,
                &mut player.inventory,
                &mut player.character,
                &mut retry_tracker,
            )
            .is_none(),
            "a rung is paid once per run"
        );
        assert_eq!(stack_counts(&player.inventory, &paid), after_grant);
    }

    /// A run persisted between the two requests (the server_state round trip) still
    /// remembers what it paid — the idempotency survives the database.
    #[test]
    fn the_paid_set_survives_persistence() {
        let (mut run, mut player) = gauge_fixture();
        let mut tracker = InventoryChangeTracker::default();
        update(&mut run, &mut player, &kills(4), &mut tracker).unwrap();
        let stored = serde_json::to_value(&run).unwrap();
        assert_eq!(stored["grantedFutureRewards"], serde_json::json!([35]));
        let mut reloaded: AbyssRun = serde_json::from_value(stored).unwrap();
        assert!(
            grant_reached_rungs(
                &mut reloaded,
                40,
                &mut player.wallet,
                &mut player.inventory,
                &mut player.character,
                &mut tracker,
            )
            .is_none()
        );

        // A run stored before this field existed deserializes with nothing paid.
        let mut legacy = serde_json::to_value(&gauge_fixture().0).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("grantedFutureRewards");
        let legacy: AbyssRun = serde_json::from_value(legacy).unwrap();
        assert!(legacy.granted_future_rewards.is_empty());
    }

    /// #312: one kill per request, all the way up — the ladder has ten rungs and the
    /// handler pays every one of them; nothing stops after the fifth.
    #[test]
    fn a_run_climbs_all_ten_rungs() {
        let (mut run, mut player) = gauge_fixture();
        let mut paid = Vec::new();
        for _ in 0..80 {
            let mut tracker = InventoryChangeTracker::default();
            let showing = advertised(&run, 40).map(|(rung, _)| rung);
            if update(&mut run, &mut player, &kills(1), &mut tracker).is_some() {
                paid.push(showing.expect("a paid rung was advertised first"));
            }
        }
        assert_eq!(paid, abyss_rewards::ABYSS_LADDER.to_vec());
        assert!(advertised(&run, 40).is_none(), "nothing left to advertise past 650");
    }

    /// A captured retail run (`initialPlayerLevel` 10, from floor 1): each floor's
    /// dungeon, its difficulty, and the `enemy_killed` actions sent on it, in order.
    const RETAIL_RUN_FLOORS: [(&str, u32, u64); 16] = [
        ("663053f0-3a46-4012-b004-6cb2e907f33c", 1, 3),
        ("4de80b69-fe4e-4ed5-a556-71e5b7c82ed0", 2, 3),
        ("1396d90c-c38e-47c6-a9da-6e98c49788e5", 3, 2),
        ("fe22c3c8-e4c9-491c-bd85-7c0ba9dc6b31", 4, 5),
        ("44e0d7cd-26a1-4183-996b-056a484a5e2e", 5, 3),
        ("5cc26070-12ac-4adc-b55d-da6fed6934ee", 6, 6),
        ("cc9c27fd-1917-4b0a-a62e-87cfb07e7c6d", 7, 2),
        ("d30de853-68ca-4360-b060-81aaf182d940", 8, 2),
        ("84e70169-4e8c-4820-936d-18976b871c8d", 9, 6),
        ("7924c82d-f6b3-4eb6-9edb-9b1349a1da84", 10, 3),
        ("ad7a91a8-c47e-47ab-b78f-f7559123a879", 12, 6),
        ("73aa75cb-1109-4e96-b7a2-73e55aaa99a2", 14, 3),
        ("0ec5e913-6074-4490-abaa-f1802827b007", 16, 6),
        ("2c0e2b66-2642-409c-a7c9-2184c6c51c6c", 20, 7),
        ("28ffdb77-161e-49a4-897e-7a8f679264d6", 24, 7),
        ("4c793f15-aa4b-406f-be88-9689085fdc05", 28, 1),
    ];

    /// The kill (1-based, over the whole run) on whose response retail paid each rung,
    /// 35 through 490. Retail's server and the client agree by construction, so these
    /// are where the client's own gauge crossed.
    const RETAIL_PAID_AT_KILL: [usize; 9] = [24, 26, 28, 33, 38, 43, 47, 51, 61];

    /// Replays the captured run through the real handler steps, one kill per request,
    /// and returns the kill each rung was paid on.
    fn replay_retail_run() -> Vec<usize> {
        let (_, mut player) = gauge_fixture();
        let mut run = run_from(&[], 10);
        run.slices = RETAIL_RUN_FLOORS
            .iter()
            .enumerate()
            .map(|(i, (dungeon, difficulty, _))| slice_for(dungeon, *difficulty, i as u32 + 1))
            .collect();
        run.current_floor_index = 0;
        let completed = parse_actions(serde_json::json!([
            {"type": "abyss_slice_completed", "time": 1}
        ]));
        let mut paid_at = Vec::new();
        let mut kill_no = 0;
        for (_, _, floor_kills) in RETAIL_RUN_FLOORS {
            for _ in 0..floor_kills {
                kill_no += 1;
                let mut tracker = InventoryChangeTracker::default();
                let before = run.granted_future_rewards.len();
                update(&mut run, &mut player, &kills(1), &mut tracker);
                paid_at.extend(std::iter::repeat(kill_no).take(run.granted_future_rewards.len() - before));
            }
            update(&mut run, &mut player, &completed, &mut InventoryChangeTracker::default());
        }
        paid_at
    }

    /// #312, the defect. The server must never pay a rung before the client's gauge gets
    /// there: the client then takes that rung as its new floor, its own score sits below
    /// it, and the gauge stops filling for the rest of the run. Scored at a flat 1.0 the
    /// critter floors of this captured run put the server ahead from the first rung (35
    /// paid on kill 18, the client got there on kill 24).
    #[test]
    fn the_server_never_pays_a_rung_before_the_clients_gauge_reaches_it() {
        let paid_at = replay_retail_run();
        for (rung, (server, client)) in abyss_rewards::ABYSS_LADDER
            .iter()
            .zip(paid_at.iter().zip(RETAIL_PAID_AT_KILL))
        {
            assert!(
                *server >= client,
                "rung {rung} paid on kill {server}, before the client's gauge reached it on kill {client} (all: {paid_at:?})"
            );
        }
        assert_eq!(&paid_at[..2], &RETAIL_PAID_AT_KILL[..2], "the shallow rungs land on retail's kill");
    }

    /// NEGATIVE CONTROL: the same run scored at a flat 1.0, as the server did, pays the
    /// first rung six kills early — the replay above can tell the two apart.
    #[test]
    fn a_flat_multiplier_pays_the_first_rung_early() {
        let sd = real_static_abyss();
        let mut score = 0.0;
        let mut kill_no = 0;
        'run: for (_, difficulty, floor_kills) in RETAIL_RUN_FLOORS {
            for _ in 0..floor_kills {
                kill_no += 1;
                score += sd.kill_score(difficulty as i32 - 10) as f64;
                if score >= 35.0 {
                    break 'run;
                }
            }
        }
        assert_eq!(kill_no, 18);
        assert!(kill_no < RETAIL_PAID_AT_KILL[0]);
    }

    /// One request that crosses two rungs pays both, merged into one `reward` —
    /// the captured retail shape (a level-79 kill crossing 35 and 50 came back as
    /// `{"stackableItems":{…},"chests":[{"id":"875","tier":1,"level":79}]}`).
    #[test]
    fn crossing_two_rungs_in_one_update_pays_both() {
        let (mut run, mut player) = gauge_fixture();
        run.score = 34.0;
        let rung35 = abyss_rewards::future_reward_for_rung(35, 40, run.seed).unwrap();

        let mut tracker = InventoryChangeTracker::default();
        let paid = update(&mut run, &mut player, &kills(2), &mut tracker)
            .expect("34 + 20 crosses 35 and 50");
        assert_eq!(run.score, 54.0);
        assert_eq!(
            run.granted_future_rewards,
            std::collections::BTreeSet::from([35, 50])
        );
        assert_eq!(
            paid.stackable_items, rung35.stackable_items,
            "rung 35's stack"
        );
        assert_eq!(paid.chests.len(), 1, "rung 50's chest");
        assert_eq!((paid.chests[0].tier, paid.chests[0].level), (1, 40));
        let chest_id = paid.chests[0]
            .id
            .clone()
            .expect("the treasury id is echoed");
        assert!(player.inventory.treasury.get_chest(&chest_id).is_some());
        assert_eq!(tracker.modified_treasury.added, vec![chest_id]);
        assert_eq!(advertised(&run, 40).unwrap().0, 70, "next rung advertised");

        let wire = serde_json::to_value(&paid).unwrap();
        let mut keys: Vec<_> = wire.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["chests", "stackableItems"]);
        assert_eq!(wire["chests"][0]["tier"], 1);
        assert!(wire["chests"][0]["id"].is_string());
    }

    /// CONTROL: a run that never reaches the first rung is paid nothing, and a run
    /// that ends there leaves nothing behind for the next run.
    #[test]
    fn a_run_below_the_first_rung_pays_nothing() {
        let (mut run, mut player) = gauge_fixture();
        let mut tracker = InventoryChangeTracker::default();
        assert!(update(&mut run, &mut player, &kills(3), &mut tracker).is_none());
        assert_eq!(run.score, 30.0);
        assert!(run.granted_future_rewards.is_empty());
        assert!(player.inventory.backpack.stackable_items.is_empty());
        assert!(player.inventory.treasury.chests().is_empty());
        assert_eq!(advertised(&run, 40).unwrap().0, 35, "still climbing to 35");
    }

    /// The gear rung's instance id is a function of the per-character seed, so a
    /// second run advertises the same id. Paying it must not overwrite the copy the
    /// player already holds.
    #[test]
    fn a_held_gear_id_is_reminted_not_overwritten() {
        let (mut run, mut player) = gauge_fixture();
        let gear = abyss_rewards::future_reward_for_rung(260, 40, run.seed).unwrap();
        assert_eq!(gear.items.len(), 1, "rung 260 grants one piece of gear");
        let advertised_id = gear.items[0].id;
        let mut held = gear.items[0].item.clone();
        held.tempering_level = 5;
        player
            .inventory
            .backpack
            .items
            .0
            .insert(advertised_id, held);

        run.granted_future_rewards = abyss_rewards::ABYSS_LADDER
            .iter()
            .copied()
            .filter(|r| *r < 260)
            .collect();
        run.score = 255.0;
        let mut tracker = InventoryChangeTracker::default();
        let paid = update(&mut run, &mut player, &kills(1), &mut tracker).unwrap();
        assert_eq!(paid.items.len(), 1);
        assert_ne!(paid.items[0].id, advertised_id, "a held id is re-minted");
        assert_eq!(
            player.inventory.backpack.items.0[&advertised_id].tempering_level, 5,
            "the copy already held is untouched"
        );
        assert!(
            player
                .inventory
                .backpack
                .items
                .0
                .contains_key(&paid.items[0].id)
        );
    }

    // ── The gauge against the captured retail runs (#312) ──────────────────

    /// Every kill of the captured retail runs that paid gauge rungs, floor by floor:
    /// `(floorIndex, dungeon, difficulty, the spawnGroupId of each kill in order)`.
    /// The first is character 78f2b668 at initialPlayerLevel 10 from floor 1 (65 kills,
    /// `/start` capture 12011); the second 97cf5fa6 at 75 from floor 90 (35087).
    const RETAIL_RUN_IPL10: [(u32, &str, u32, &[&str]); 16] = [
        (1, "663053f0-3a46-4012-b004-6cb2e907f33c", 1, &["c41668b3-ad8b-42b4-ba5d-a0574039a3cc", "c41668b3-ad8b-42b4-ba5d-a0574039a3cc", "9a057ca6-5f8d-4700-8665-6c56de0e1103"]),
        (2, "4de80b69-fe4e-4ed5-a556-71e5b7c82ed0", 2, &["421ede71-4cf3-499c-aee3-ef83db34f22d", "d0ec4eee-089c-4909-ba6e-6cadcc453121", "d0ec4eee-089c-4909-ba6e-6cadcc453121"]),
        (3, "1396d90c-c38e-47c6-a9da-6e98c49788e5", 3, &["e885f3eb-a828-47a6-b4a8-96eec96d17c7", "e885f3eb-a828-47a6-b4a8-96eec96d17c7"]),
        (4, "fe22c3c8-e4c9-491c-bd85-7c0ba9dc6b31", 4, &["0e302177-29b0-4808-8bb6-f429d1812102", "0e302177-29b0-4808-8bb6-f429d1812102", "0e302177-29b0-4808-8bb6-f429d1812102", "0e302177-29b0-4808-8bb6-f429d1812102", "0e302177-29b0-4808-8bb6-f429d1812102"]),
        (5, "44e0d7cd-26a1-4183-996b-056a484a5e2e", 5, &["7597ec37-ca26-43ea-8aef-50cb2aed320b", "7597ec37-ca26-43ea-8aef-50cb2aed320b", "7597ec37-ca26-43ea-8aef-50cb2aed320b"]),
        (6, "5cc26070-12ac-4adc-b55d-da6fed6934ee", 6, &["2d2a213f-dd06-49bb-8925-c370a2b2d766", "2d2a213f-dd06-49bb-8925-c370a2b2d766", "2d2a213f-dd06-49bb-8925-c370a2b2d766", "2d2a213f-dd06-49bb-8925-c370a2b2d766", "2d2a213f-dd06-49bb-8925-c370a2b2d766", "2d2a213f-dd06-49bb-8925-c370a2b2d766"]),
        (7, "cc9c27fd-1917-4b0a-a62e-87cfb07e7c6d", 7, &["832b5f9a-6ce1-4391-829b-8457ad3172ff", "832b5f9a-6ce1-4391-829b-8457ad3172ff"]),
        (8, "d30de853-68ca-4360-b060-81aaf182d940", 8, &["c45159d4-9630-457c-a703-9f74fdf4be7e", "c45159d4-9630-457c-a703-9f74fdf4be7e"]),
        (9, "84e70169-4e8c-4820-936d-18976b871c8d", 9, &["d3f5848e-ba7f-4484-93be-52e15b2407ad", "89f7c6a9-bfaa-4191-8a3b-ffff433f1766", "cc150364-705d-4ae5-b5f4-9c0e46128f6a", "cc150364-705d-4ae5-b5f4-9c0e46128f6a", "cc150364-705d-4ae5-b5f4-9c0e46128f6a", "d3f5848e-ba7f-4484-93be-52e15b2407ad"]),
        (10, "7924c82d-f6b3-4eb6-9edb-9b1349a1da84", 10, &["da988347-e455-43ce-b205-f48724631301", "da988347-e455-43ce-b205-f48724631301", "da988347-e455-43ce-b205-f48724631301"]),
        (11, "ad7a91a8-c47e-47ab-b78f-f7559123a879", 12, &["409178a6-44a7-462b-aa96-bfd0b83e8df0", "409178a6-44a7-462b-aa96-bfd0b83e8df0", "409178a6-44a7-462b-aa96-bfd0b83e8df0", "409178a6-44a7-462b-aa96-bfd0b83e8df0", "409178a6-44a7-462b-aa96-bfd0b83e8df0", "409178a6-44a7-462b-aa96-bfd0b83e8df0"]),
        (12, "73aa75cb-1109-4e96-b7a2-73e55aaa99a2", 14, &["cd20d169-2cf7-4490-825f-3342fbfa79ae", "cd20d169-2cf7-4490-825f-3342fbfa79ae", "cd20d169-2cf7-4490-825f-3342fbfa79ae"]),
        (13, "0ec5e913-6074-4490-abaa-f1802827b007", 16, &["998b8cd8-75e2-4826-af97-253bd1ad378a", "998b8cd8-75e2-4826-af97-253bd1ad378a", "17fca83f-dfe0-484c-a1e8-6a96c104f621", "998b8cd8-75e2-4826-af97-253bd1ad378a", "17fca83f-dfe0-484c-a1e8-6a96c104f621", "998b8cd8-75e2-4826-af97-253bd1ad378a"]),
        (14, "2c0e2b66-2642-409c-a7c9-2184c6c51c6c", 20, &["7d4e8432-9477-4642-a56d-80f88c62f817", "7d4e8432-9477-4642-a56d-80f88c62f817", "7d4e8432-9477-4642-a56d-80f88c62f817", "d6a0f996-661a-4a15-b0b7-9a9add22aa22", "7d4e8432-9477-4642-a56d-80f88c62f817", "7d4e8432-9477-4642-a56d-80f88c62f817", "7d4e8432-9477-4642-a56d-80f88c62f817"]),
        (15, "28ffdb77-161e-49a4-897e-7a8f679264d6", 24, &["c39c5257-ce4f-4160-a992-1e3a523a253e", "c39c5257-ce4f-4160-a992-1e3a523a253e", "c39c5257-ce4f-4160-a992-1e3a523a253e", "6ad2e6c0-40ac-456c-b294-7422dafe0fb0", "c39c5257-ce4f-4160-a992-1e3a523a253e", "c39c5257-ce4f-4160-a992-1e3a523a253e", "6ad2e6c0-40ac-456c-b294-7422dafe0fb0"]),
        (16, "4c793f15-aa4b-406f-be88-9689085fdc05", 28, &["eaf9d5f0-3ee3-4e9e-bac6-0f866a704d2a"]),
    ];
    const RETAIL_RUN_IPL75: [(u32, &str, u32, &[&str]); 1] = [
        (90, "f00dea4c-025d-4233-a503-8ab28ef53c80", 100, &["16d2aa0a-70e4-4a6e-b423-a188b163beb9", "473b13bd-0e2a-4efe-9c72-be5746d1ba33", "473b13bd-0e2a-4efe-9c72-be5746d1ba33"]),
    ];

    /// Replay a captured run kill by kill, one `/update` per kill as retail sent them,
    /// and return `(kill number, rungs that update paid)` for every paying update.
    fn replay(ipl: u32, floors: &[(u32, &str, u32, &[&str])]) -> Vec<(usize, Vec<u32>)> {
        let mut run = run_from(&floors.iter().map(|f| (f.0, f.2)).collect::<Vec<_>>(), ipl);
        run.current_floor_index = 0;
        for (slice, floor) in run.slices.iter_mut().zip(floors) {
            slice.dungeon_settings_id = Uuid::parse_str(floor.1).unwrap();
            slice.completed = false;
            slice.enemy_killed = false;
        }
        let (_, mut player) = gauge_fixture();
        let mut tracker = InventoryChangeTracker::default();
        let mut paid = Vec::new();
        let mut kill_no = 0;
        for (_, _, _, groups) in floors {
            for group in *groups {
                kill_no += 1;
                let before = run.granted_future_rewards.clone();
                let action = parse_actions(serde_json::json!([{
                    "type": "enemy_killed", "spawnGroupId": group, "spawnerIndex": 0,
                    "enemyIndex": 0, "xpReward": 0.0, "time": kill_no
                }]));
                if update(&mut run, &mut player, &action, &mut tracker).is_some() {
                    let mut rungs: Vec<u32> =
                        run.granted_future_rewards.difference(&before).copied().collect();
                    rungs.sort();
                    paid.push((kill_no, rungs));
                }
            }
            let completed =
                parse_actions(serde_json::json!([{"type": "abyss_slice_completed", "time": 0}]));
            update(&mut run, &mut player, &completed, &mut tracker);
        }
        paid
    }

    /// The report: "the gauge isn't showing the progress after the first one". The
    /// client fills the gauge from its own score and moves it on only when a response
    /// carries `reward`, so every rung has to be paid on the kill that crosses it on
    /// the client. Retail did exactly that; replaying its kills must pay each rung on
    /// the same kill. The per-floor table (lowest multiplier of the floor) paid 2 of 9
    /// on time and the rest up to 7 kills late — by kill 58, when it paid 360, the
    /// client had passed 490 — leaving the gauge pinned full.
    #[test]
    fn replaying_the_retail_run_pays_every_rung_on_retails_kill() {
        assert_eq!(
            replay(10, &RETAIL_RUN_IPL10),
            vec![
                (24, vec![35]),
                (26, vec![50]),
                (28, vec![70]),
                (33, vec![95]),
                (38, vec![135]),
                (43, vec![190]),
                (47, vec![260]),
                (51, vec![360]),
                (61, vec![490]),
            ],
            "retail paid these rungs on these kills (captures 12117 ... 12222), and no others"
        );
    }

    /// The deep run: one kill crossed 35 and 50 at once (retail merged both into one
    /// `reward`), the next crossed 70.
    #[test]
    fn replaying_the_deep_retail_run_pays_its_two_updates() {
        assert_eq!(
            replay(75, &RETAIL_RUN_IPL75),
            vec![(2, vec![35, 50]), (3, vec![70])]
        );
    }
}

#[cfg(test)]
mod update_body_shape_tests {
    use super::*;

    /// THE regression. `durability` arrives as a JSON string.
    ///
    /// All 838 durability values in the captured corpus are strings —
    /// `"durability": "673.2878"` — and not one is a number. `f64` rejected
    /// them, and because serde fails the whole body that 400s the entire
    /// request, not just the field. Report #156: the first post-fight update a
    /// minute into the Abyss 400s, the client retries the same action forever,
    /// and the player sees "Network Not Reachable".
    #[test]
    fn a_string_durability_is_accepted() {
        let body: UpdateAbyssRequest = serde_json::from_value(serde_json::json!({
            "currentState": {"b64": ""},
            "actions": [{
                "type": "combat_completed",
                "time": 1787617384801u64,
                "items": [
                    {"id": "11111111-2222-4333-8444-555555555555", "durability": "673.2878"},
                    {"id": "66666666-7777-4888-8999-aaaaaaaaaaaa", "durability": "6.074898"}
                ]
            }]
        }))
        .expect("a captured combat_completed body must deserialize");
        let AbyssUpdateAction::CombatCompleted(action) = &body.actions[0] else {
            panic!("expected combat_completed, got {:?}", body.actions[0]);
        };
        assert_eq!(action.items.len(), 2);
        assert!((action.items[0].durability - 673.2878).abs() < 1e-9);
        assert!((action.items[1].durability - 6.074898).abs() < 1e-9);
    }

    /// The control: a NUMBER must still work. Swapping one wire type for the
    /// other would have passed the test above and broken every other client.
    #[test]
    fn a_numeric_durability_still_works() {
        let body: UpdateAbyssRequest = serde_json::from_value(serde_json::json!({
            "actions": [{
                "type": "combat_completed",
                "items": [{"id": "11111111-2222-4333-8444-555555555555", "durability": 42.5}]
            }]
        }))
        .expect("a numeric durability must still deserialize");
        let AbyssUpdateAction::CombatCompleted(action) = &body.actions[0] else {
            panic!("expected combat_completed");
        };
        assert!((action.items[0].durability - 42.5).abs() < 1e-9);
    }

    /// `gemsPayment` is a BOOL in all 14 captured revives, not a count. `u64`
    /// rejected every one, killing the whole update the same way.
    #[test]
    fn a_boolean_gems_payment_is_accepted() {
        let body: UpdateAbyssRequest = serde_json::from_value(serde_json::json!({
            "actions": [
                {"type": "revive", "gemsPayment": true,  "time": 1782858208727u64},
                {"type": "revive", "gemsPayment": false, "time": 1782858208728u64}
            ]
        }))
        .expect("a captured revive body must deserialize");
        assert_eq!(body.actions.len(), 2);
        assert!(matches!(body.actions[0], AbyssUpdateAction::Revive(_)));
    }

    /// Garbage must still be rejected — the field is permissive about the wire
    /// type, not about the value. Without this the deserializer could silently
    /// default and hide a real client change.
    #[test]
    fn a_non_numeric_durability_string_is_still_an_error() {
        let err = serde_json::from_value::<UpdateAbyssRequest>(serde_json::json!({
            "actions": [{
                "type": "combat_completed",
                "items": [{"id": "11111111-2222-4333-8444-555555555555", "durability": "banana"}]
            }]
        }));
        assert!(err.is_err(), "a non-numeric string must not be accepted");
    }

    /// One whole captured update body, every action type in it at once, with the
    /// exact key sets the corpus shows. This is what the handler really receives.
    #[test]
    fn a_full_captured_body_with_every_action_type_deserializes() {
        let body: UpdateAbyssRequest = serde_json::from_value(serde_json::json!({
            "currentState": {"b64": ""},
            "actions": [
                {"type": "enemy_killed", "spawnGroupId": "11111111-2222-4333-8444-555555555555",
                 "spawnerIndex": 3, "enemyIndex": 2, "xpReward": 214.0, "time": 1787617384801u64},
                {"type": "enemy_loot_collected", "spawnGroupId": "11111111-2222-4333-8444-555555555555",
                 "spawnerIndex": 0, "enemyIndex": 0, "loot": [], "time": 1787617384802u64},
                {"type": "combat_completed", "time": 1787617384803u64,
                 "items": [{"id": "66666666-7777-4888-8999-aaaaaaaaaaaa", "durability": "355.0"}]},
                {"type": "item_consumed", "itemTemplateId": "22222222-3333-4444-8555-666666666666",
                 "time": 1787617384804u64},
                {"type": "revive", "gemsPayment": true, "time": 1787617384805u64},
                {"type": "abyss_slice_completed", "time": 1787617384806u64}
            ]
        }))
        .expect("the full captured action set must deserialize");
        assert_eq!(body.actions.len(), 6);
        assert!(
            !body.actions.iter().any(|a| matches!(a, AbyssUpdateAction::Unknown)),
            "no captured action may fall into Unknown: {:?}",
            body.actions
        );
    }
}
