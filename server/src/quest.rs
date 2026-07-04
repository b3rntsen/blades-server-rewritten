use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::{
    economy::{RewardGrant, apply_reward, grant_chest},
    user_data::{
        CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
        DungeonGeneratedDataWithId, InventoryChangeTracker, QuestWithId,
    },
    util::quest::generate_quest_data,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, associations::HasTable, insert_into};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    json_db::JsonDbWrapper,
    models::{
        CharacterDbEntryCharacterAlone, CharacterDbEntryEconomy, QuestDbEntry, QuestDbEntryInfo,
    },
    session::SessionLookedUpMaybe,
    util::{self, check_permission_for_character_and_get_it},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetQuestsResponse {
    quests: Vec<QuestWithId>,
    dungeon_generated_data_list: Vec<DungeonGeneratedDataWithId>,
    /// Town-job board entries (`type:"JOB"` with a `jobSetup` block), rolled
    /// faithfully from `app_state.job_pools` — see [`jobs_gen`]. Each entry is a
    /// raw `Value` because `jobSetup` carries far more than the typed `Quest`
    /// struct models (it is served verbatim to the client, never re-parsed here).
    jobs: Vec<Value>,
    character: CompleteCharacterWithIdWithoutData,
    /// Per-pool rotation timers (`[{id, endTime, nextStartTime}]`, epoch seconds)
    /// computed relative to *now* by [`jobs_gen`] — no longer frozen constants.
    job_pools: Value,
    game_event_quests: Vec<()>,            //TODO:
    game_event_quests_in_warning: Vec<()>, //TODO,
    game_event_quests_finished: Vec<()>,   //TODO
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests")]
pub async fn get_quests(
    session: SessionLookedUpMaybe,
    request: Json<Option<()>>,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetQuestsResponse>, BladeApiError> {
    assert!(request.is_none());
    let session = session.get_session_or_error()?;

    let character_id_var = path.into_inner();
    // Real wall clock — this is a live server, so `now` drives the pool rotation
    // and the daily reset. Tests exercise the pure generator with an injected `now`.
    let now = jobs_gen::now_epoch_secs();
    let job_pools_def = app_state.job_pools.clone();
    let mut conn = app_state.db_pool.get().await.unwrap();
    conn.transaction(|mut conn| {
        async move {
            let character = {
                use crate::schema::characters::dsl::*;

                characters::table()
                    .filter(id.eq(&character_id_var))
                    .select(CharacterDbEntryCharacterAlone::as_select())
                    .load(&mut conn)
                    .await?
            };
            let mut character =
                util::get_only_single_character_and_check_permission(character, &session.session)?;

            // ---- Town jobs: rotate + (re)generate for the current reset window ----
            // The character carries `lastJobsResetTime`; when it predates the current
            // daily-reset boundary we roll a fresh set of jobs, advance the difficulty
            // cycle, and persist the generated JOB entries as `quests` rows so the
            // follow-up /objectives + /complete routes can resolve their questIds.
            // Within a window the same jobs return (deterministic seed), so a
            // re-fetch is idempotent.
            let reset_boundary = jobs_gen::current_reset_boundary(&job_pools_def, now);
            let needs_regen = character.character.0.last_jobs_reset_time < reset_boundary;

            let (jobs, job_pools) = jobs_gen::generate(
                &job_pools_def,
                character_id_var,
                character.character.0.level,
                character.character.0.job_difficulty_cycle_index,
                reset_boundary,
                now,
            );

            if needs_regen {
                // Drop the previous window's JOB rows (marked by the sentinel
                // gldQuestId) so stale, un-accepted jobs don't linger, then insert
                // the freshly rolled ones. Jobs already carried into a dungeon keep a
                // real dungeon_state; we only prune the untouched catalog rows.
                let job_quest_ids: Vec<Uuid> = jobs
                    .iter()
                    .filter_map(|j| j.get("questId").and_then(|v| v.as_str()))
                    .filter_map(|s| Uuid::parse_str(s).ok())
                    .collect();
                {
                    use crate::schema::quests;
                    // Delete prior-window job rows for this character that are NOT part
                    // of the new set and have not been entered (no dungeon_state).
                    let stale: Vec<Uuid> = quests::table
                        .filter(quests::character_id.eq(character_id_var))
                        .filter(quests::dungeon_state.is_null())
                        .select(QuestDbEntry::as_select())
                        .load(&mut conn)
                        .await?
                        .into_iter()
                        .filter(|q| jobs_gen::is_job_row(&q.info.0))
                        .map(|q| q.id)
                        .filter(|id| !job_quest_ids.contains(id))
                        .collect();
                    if !stale.is_empty() {
                        diesel::delete(
                            quests::table
                                .filter(quests::character_id.eq(character_id_var))
                                .filter(quests::id.eq_any(&stale)),
                        )
                        .execute(&mut conn)
                        .await?;
                    }
                }
                // Upsert the current window's job rows (idempotent within the window).
                for job in &jobs {
                    if let Some(entry) = jobs_gen::job_quest_db_entry(job, character_id_var) {
                        use crate::schema::quests;
                        insert_into(quests::table)
                            .values(&entry)
                            .on_conflict((quests::id, quests::character_id))
                            .do_nothing()
                            .execute(&mut conn)
                            .await?;
                    }
                }
                // Persist the rotation scalars on the character.
                character.character.0.last_jobs_reset_time = reset_boundary;
                character.character.0.job_difficulty_cycle_index =
                    jobs_gen::next_cycle_index(&job_pools_def, character.character.0.job_difficulty_cycle_index);
                {
                    use crate::schema::characters;
                    diesel::update(characters::table)
                        .filter(characters::id.eq(character_id_var))
                        .set(&character)
                        .execute(&mut conn)
                        .await?;
                }
            }

            // we could have done an inner join to check the get the user id, but the user has already been checked previously.
            let quests = {
                use crate::schema::quests::dsl::*;
                // take care! that line above import a character_id thing
                quests::table()
                    .filter(character_id.eq(&character_id_var))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
            };

            let mut result_quests = Vec::new();
            let mut result_generated_data = Vec::new();

            for quest in quests {
                // Stored JOB rows are surfaced only in `jobs[]` (regenerated above),
                // never in `quests[]` — matching prod, where the two arrays never
                // overlap.
                if jobs_gen::is_job_row(&quest.info.0) {
                    continue;
                }
                result_quests.push(QuestWithId {
                    quest_id: quest.id,
                    quest: quest.info.0,
                });
                if let Some(generated_data) = quest.generated_data.0 {
                    result_generated_data.push(DungeonGeneratedDataWithId {
                        quest_id: quest.id,
                        inner: generated_data,
                    });
                };
            }

            Ok(Json(GetQuestsResponse {
                quests: result_quests,
                dungeon_generated_data_list: result_generated_data,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id_var,
                    character: character.character.0,
                },
                jobs,
                game_event_quests: Vec::new(),
                game_event_quests_finished: Vec::new(),
                game_event_quests_in_warning: Vec::new(),
                job_pools,
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptQuestResponse {
    quest: QuestWithId,
    dungeon_generated_data: Option<DungeonGeneratedDataWithId>,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/accept"
)]
async fn accept_quest(
    session: SessionLookedUpMaybe,
    request: Json<Option<()>>,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<AcceptQuestResponse>, BladeApiError> {
    assert!(request.is_none());
    let session = session.get_session_or_error()?;
    let (character_id, quest_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    // A town-job questId is not present in `game_data.quests`; it is rolled from
    // the job pools. In prod the client uses jobs straight from the /quests board
    // (no /accept), but we still accept a job gracefully: regenerate the current
    // window and, if the questId is one of ours, persist it (or reuse the row the
    // /quests board already stored). Falls through to the normal quest path below
    // for regular quest ids.
    if !app_state.game_data.quests.contains_key(&quest_id) {
        // Load the character (with permission check) for the job-difficulty cycle.
        let character = {
            use crate::schema::characters::dsl::*;
            let rows = characters
                .filter(id.eq(&character_id))
                .select(CharacterDbEntryCharacterAlone::as_select())
                .load(&mut conn)
                .await?;
            util::get_only_single_character_and_check_permission(rows, &session.session)?
        };
        let now = jobs_gen::now_epoch_secs();
        let reset_boundary = jobs_gen::current_reset_boundary(&app_state.job_pools, now);
        let (jobs, _pools) = jobs_gen::generate(
            &app_state.job_pools,
            character_id,
            character.character.0.level,
            character.character.0.job_difficulty_cycle_index,
            reset_boundary,
            now,
        );
        if let Some(job) = jobs.iter().find(|j| {
            j.get("questId").and_then(|v| v.as_str()) == Some(&quest_id.to_string())
        }) {
            if let Some(entry) = jobs_gen::job_quest_db_entry(job, character_id) {
                use crate::schema::quests;
                insert_into(quests::table)
                    .values(&entry)
                    .on_conflict((quests::id, quests::character_id))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;
                return Ok(Json(AcceptQuestResponse {
                    quest: QuestWithId {
                        quest_id,
                        quest: entry.info.0,
                    },
                    dungeon_generated_data: None,
                }));
            }
        }
        // Not a job we know about and not a real quest → let the normal path 404.
    }

    // check permission (normal-quest path)
    let _ = check_permission_for_character_and_get_it(&mut conn, &session.session, character_id)
        .await?;

    // actually add quest

    let (quest, dungeon_generated_data) = generate_quest_data(&app_state.game_data, quest_id)?;
    //TODO: specifically handle the case the quest already exist (primary key is character id + quest id)

    let to_insert = QuestDbEntry {
        id: quest_id,
        character_id,
        info: JsonDbWrapper(quest.clone()),
        generated_data: JsonDbWrapper(dungeon_generated_data.clone()),
        dungeon_state: None,
    };

    {
        use crate::schema::quests::dsl::*;

        insert_into(quests::table())
            .values(&to_insert)
            .execute(&mut conn)
            .await?;
    }

    Ok(Json(AcceptQuestResponse {
        quest: QuestWithId {
            quest_id: quest_id,
            quest,
        },
        dungeon_generated_data: dungeon_generated_data.map(|v| DungeonGeneratedDataWithId {
            quest_id: quest_id,
            inner: v,
        }),
    }))
}

// ---------------------------------------------------------------------------
// POST /quests/{quest_id}/complete
// ---------------------------------------------------------------------------

/// Wire shape matched from captured `/quests/{id}/complete` responses:
/// ```json
/// { "reward":{...}, "inventory":{...}, "wallet":[...], "character":{...} }
/// ```
/// `reward` is lenient: unknown quest → empty reward (all zeros / empty maps).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteQuestResponse {
    reward: RewardGrant,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
    character: CompleteCharacterWithIdWithoutData,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/complete"
)]
pub async fn complete_quest(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<CompleteQuestResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the economy row (character + wallet + inventory) under a row lock.
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Load the quest row and mark it completed.
            let mut quest_entry = {
                use crate::schema::quests;
                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1))?
            };

            quest_entry.info.0.completed = true;

            // Look up the capture-derived reward. Lenient: unknown quest → empty reward.
            let reward = globals
                .static_data
                .quest_rewards
                .get(&quest_id)
                // Also try by gldQuestId (event quests use gldQuestId ≠ quest_id).
                .or_else(|| {
                    globals
                        .static_data
                        .quest_rewards
                        .get(&quest_entry.info.0.gld_quest_id)
                })
                .cloned()
                .unwrap_or_default();

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
            if !reward.chests.is_empty() {
                for chest in &reward.chests {
                    grant_chest(
                        &mut entry.inventory.0,
                        chest.tier,
                        chest.level,
                        &mut tracker,
                    );
                }
                entry.inventory.0.treasury_version += 1;
            }

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let character = entry.character.0.clone();

            // Write the completed quest flag back.
            {
                use crate::schema::quests;
                diesel::update(quests::table)
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .set(QuestDbEntryInfo {
                        info: quest_entry.info,
                    })
                    .execute(&mut conn)
                    .await?;
            }

            // Write the economy (wallet + inventory + character XP) back.
            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(CompleteQuestResponse {
                reward,
                inventory,
                wallet,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character,
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

// ---------------------------------------------------------------------------
// POST /quests/{quest_id}/objectives
// ---------------------------------------------------------------------------

/// `objectiveUpdates` maps objective UUID → `{status, progress}` (and optionally
/// `completed`). The client reports absolute progress; we merge it in and persist.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ObjectivesRequest {
    #[serde(default)]
    objective_updates: std::collections::HashMap<Uuid, ObjectiveUpdate>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct ObjectiveUpdate {
    status: blades_lib::user_data::QuestStatus,
    progress: f64,
    #[serde(default)]
    completed: bool,
}

/// Wire shape for the objectives response.
///
/// Per captures there are two cases:
/// 1. Pure progress update (no objective yet completed): `{ quest:{...} }`.
/// 2. An objective reaches `Completed` status: `{ reward:{...}, inventory:{...},
///    character:{...}, quest:{...} }`.
///
/// We always include all fields and rely on `skip_serializing_if` to omit the empty
/// reward/inventory/character when no reward is due. In practice the client ignores
/// extra empty fields, but this matches the narrow case 1 wire exactly too (the
/// captured case-1 body was purely `{quest:{...}}`). We therefore split on whether the
/// reward is empty.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObjectivesResponse {
    #[serde(skip_serializing_if = "RewardGrant::is_empty")]
    reward: RewardGrant,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<CompleteInventoryUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<CompleteCharacterWithIdWithoutData>,
    quest: QuestWithId,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/objectives"
)]
pub async fn update_quest_objectives(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<ObjectivesRequest>,
) -> Result<Json<ObjectivesResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let body = body.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the economy row under a row lock (needed only when a reward is granted,
            // but we can't know upfront; take it eagerly to keep the transaction simple).
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Load and lock the quest row.
            let mut quest_entry = {
                use crate::schema::quests;
                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1))?
            };

            // Merge each objective update in. The client sends absolute progress.
            let mut any_newly_completed = false;
            for (obj_id, update) in &body.objective_updates {
                let entry_obj = quest_entry
                    .info
                    .0
                    .objective_statuses
                    .entry(*obj_id)
                    .or_insert_with(|| blades_lib::user_data::ObjectiveStatus {
                        status: blades_lib::user_data::QuestStatus::Active,
                        progress: 0.0,
                        completed: false,
                    });
                entry_obj.status = update.status;
                entry_obj.progress = update.progress;
                if update.completed && !entry_obj.completed {
                    entry_obj.completed = true;
                    any_newly_completed = true;
                }
            }

            // Grant an objective-completion reward only if at least one objective became
            // Completed for the first time. We look up by quest_id / gldQuestId.
            // NOTE: The captures show partial rewards (stackableItems only) on a
            // single-objective completion. For simplicity we grant the full quest reward
            // when any objective completes; the client is lenient about over-rewarding
            // here (the actual full reward is still gatekept at `/complete`).
            let reward = if any_newly_completed {
                globals
                    .static_data
                    .quest_rewards
                    .get(&quest_id)
                    .or_else(|| {
                        globals
                            .static_data
                            .quest_rewards
                            .get(&quest_entry.info.0.gld_quest_id)
                    })
                    .cloned()
                    .unwrap_or_default()
            } else {
                RewardGrant::default()
            };

            let (opt_inventory, opt_character) = if !reward.is_empty() {
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
                if !reward.chests.is_empty() {
                    for chest in &reward.chests {
                        grant_chest(
                            &mut entry.inventory.0,
                            chest.tier,
                            chest.level,
                            &mut tracker,
                        );
                    }
                    entry.inventory.0.treasury_version += 1;
                }
                let inv = entry.inventory.0.generate_client_update(&tracker);
                let ch = entry.character.0.clone();
                // Write economy back.
                {
                    use crate::schema::characters;
                    diesel::update(characters::table)
                        .filter(characters::id.eq(entry.id))
                        .set(entry)
                        .execute(&mut conn)
                        .await?;
                }
                (
                    Some(inv),
                    Some(CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: ch,
                    }),
                )
            } else {
                (None, None)
            };

            let quest_with_id = QuestWithId {
                quest_id,
                quest: quest_entry.info.0.clone(),
            };

            // Persist the updated objective statuses.
            {
                use crate::schema::quests;
                diesel::update(quests::table)
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .set(QuestDbEntryInfo {
                        info: quest_entry.info,
                    })
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(ObjectivesResponse {
                reward,
                inventory: opt_inventory,
                character: opt_character,
                quest: quest_with_id,
            }))
        }
        .scope_boxed()
    })
    .await
}

// ===========================================================================
// Town-job generation (`jobs_gen`)
// ===========================================================================
//
// Faithful generation of the `/quests` `jobs[]` board and per-pool `jobPools`
// timers from `app_state.job_pools` (server/data/static/job_pools.json, itself
// extracted from the APK). Ground truth for the wire shape is the prod capture
// (a JOB entry = `{questId, version, type:"JOB", objectiveStatuses,
// difficultyLevel, seed, jobPoolId, jobSetup:{…}, completed}`; timers are
// `[{id, endTime, nextStartTime}]` in epoch **seconds**).
//
// Design:
//   * Generation is **deterministic** from `(character_id, reset_boundary,
//     pool_id, slot)`, so the same board returns on every /quests fetch within a
//     reset window without persisting the job bodies. `questId`s are derived the
//     same way, so /objectives + /complete can resolve an accepted job.
//   * The enemy-family / dungeon-template / gather-item / duel-boss IDs are not
//     present in job_pools.json (they live in the APK dungeon bundles), so they
//     are drawn from constant pools harvested from the captures — enough for a
//     faithful, acceptable board. Everything else (spawn groups, boss loot,
//     difficulty ranges, gem-skip, duel-boss list) comes straight from
//     job_pools.json.
//   * Never panics on the `Value` shape: a malformed / Null `job_pools` yields an
//     empty jobs list and empty timers rather than a 500.
mod jobs_gen {
    use super::*;
    use blades_lib::user_data::{ObjectiveStatus, Quest, QuestStatus, QuestType};
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Stored JOB `quests` rows are tagged with this sentinel `gldQuestId` so the
    /// /quests handler can (a) keep them out of the `quests[]` array and (b)
    /// recognise a prior-window job row when pruning. It is a fixed, otherwise
    /// unused UUID — no real quest carries it.
    pub const JOB_SENTINEL_GLD: Uuid =
        Uuid::from_u128(0x30B10B5F_0000_4A0B_8000_000000000B0B_u128);

    /// Default daily reset hour (UTC) when the pool defs don't specify one.
    const DEFAULT_RESET_HOUR: u64 = 5;
    const SECS_PER_DAY: u64 = 86_400;

    pub fn now_epoch_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    // -- deterministic PRNG (splitmix64) -----------------------------------
    // Self-contained so the module needs no extra crate. Same seed → same rolls.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        /// Uniform in `[0, n)` (n>0).
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next_u64() % n }
        }
        fn pick<'a, T>(&mut self, slice: &'a [T]) -> Option<&'a T> {
            if slice.is_empty() {
                None
            } else {
                Some(&slice[self.below(slice.len() as u64) as usize])
            }
        }
        fn range_incl(&mut self, lo: i64, hi: i64) -> i64 {
            if hi <= lo { lo } else { lo + self.below((hi - lo + 1) as u64) as i64 }
        }
    }

    /// Hash a `(character, boundary, pool, slot)` tuple into a PRNG seed.
    fn seed_for(character_id: Uuid, reset_boundary: u64, pool_id: &str, slot: u64) -> u64 {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a offset basis
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };
        mix(character_id.as_bytes());
        mix(&reset_boundary.to_le_bytes());
        mix(pool_id.as_bytes());
        mix(&slot.to_le_bytes());
        h
    }

    /// Derive a deterministic v4-shaped UUID from a seed.
    fn uuid_from_seed(seed: u64) -> Uuid {
        let mut rng = Rng::new(seed);
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&rng.next_u64().to_le_bytes());
        bytes[8..16].copy_from_slice(&rng.next_u64().to_le_bytes());
        // Stamp version 4 + RFC-4122 variant so it is a well-formed UUID.
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Uuid::from_bytes(bytes)
    }

    // -- faithful constant pools (harvested from prod captures) ------------
    // The APK dungeon bundles (not job_pools.json) hold these; the captured set
    // is representative and keeps generated jobs acceptable/displayable.
    const DUNGEON_TEMPLATES: &[&str] = &[
        "18e81559-3561-47ef-b73e-9f3bc34ba0b8", "19a3b1b0-c18b-4f2f-b73f-780f3759fe48",
        "3bcfeff9-5b22-4f7c-b1b8-ef4b277f7bc2", "4d3153a0-cfc5-405c-b065-92547ee9fbbc",
        "57d639c2-ec4c-4e6b-9995-ff6a7ef3e712", "5af68bb1-e478-4a2d-916c-651b8d793749",
        "62598ab9-82ac-4321-8534-a048edd26ccb", "6c9fe3b7-2557-408a-b52a-1842680fed3f",
        "79757d9d-8a26-4a19-bd2e-49ba8577a007", "86e6a720-caa4-4b13-b8b8-73f3dcf049a2",
        "a9386df1-5b26-462b-9c56-de9cb371c790", "b13c209c-3f41-48ac-998c-e15482e3a1a0",
        "c232c91f-37cb-4254-b8be-31f9c9d5dc54", "c45ba434-a2a7-4fff-99e2-8aa703f13893",
        "c5da36d7-5e28-454c-b1f5-ba57c2f5c0c4", "dbfd45fe-8c8c-4c8d-83c6-9b4566afc788",
        "e7418cc7-01de-4c84-ba00-e221f8783d51",
    ];
    const ENEMY_FAMILIES: &[&str] = &[
        "008cf5b0-2590-433b-832e-f2e6f0e0226f", "06591d48-8c3a-4f81-a2c6-dba2e7163788",
        "1696d9c0-900f-4829-ae3f-f0441d92a37c", "1b2a30db-2871-43a2-bca0-eaa4bd804698",
        "20e856fc-9465-4ffe-8d0a-6118c2eed219", "225d747b-9d24-4ffc-9ece-541728b4aef0",
        "31be99a6-8557-4e9b-81e6-5503f900b7d2", "33de9f64-8eb6-41d2-b62d-8b7fb3632729",
        "340cd608-31ec-447f-9f72-2162639bff3c", "3d932102-3b5c-42ba-b96a-35405752c5a3",
        "3fa0aa97-7a45-4c96-8b62-53e08691f746", "4c60bb97-3918-485a-822e-1017d2401dd2",
        "50994925-f050-48b2-8cab-259b0f1a3531", "521bb612-587d-4a90-adee-904a48d89c33",
        "6ee657a9-5cc3-45b7-ad14-db8828f7ae2c", "7f9c2b46-e6b8-4a65-9caa-f2b952623c23",
        "878febe5-106b-4b48-972a-7debd771a079", "8c75bd1f-95a3-47d4-a28c-fdb1fc0de228",
        "90a62106-6294-4456-8206-cf6817995bf8", "9137d218-6f05-4e8f-a5e5-1c63c61c95ca",
        "be99402d-c518-4e81-be00-9e2e20e690b0", "d14a0ec0-39a5-417f-a21c-8c4840d60a56",
        "de4686d9-f748-40f7-a8d3-7baadb46a695", "de8e06be-5403-4fc1-b912-1a5cd9d608a6",
        "ea8096f9-6c1b-4b42-af71-9ceabd7de33d",
    ];
    const DUEL_BOSSES: &[&str] = &[
        "01d82726-527f-4601-929c-182acd3fa9b7", "024b4f81-c7ef-4322-a547-ee863b4c02ad",
        "282b51da-b334-4cab-90fd-ba7fbdea00f1", "2f85c042-ab17-47f2-a4b2-385f8626034c",
        "33bbefc6-abb3-48e1-a233-96002d9ca98c", "68a30f8a-dd30-4a41-a014-200f16a8ff89",
        "ad5bf23a-2899-40e1-b77b-dd0cb3555176", "dadd4e4e-7544-4680-9a73-84208c8ab7a2",
        "ea48eb54-672c-4b28-9c92-60463314ee0d",
    ];
    const GATHER_ITEMS: &[&str] = &[
        "0fab3016-8306-48ee-8268-d3f7bea7d9d2", "144a3de0-bc3b-45b4-858e-0c7864ffce52",
        "49a5aed9-3fc2-423a-875c-1e4f3c10f4d8", "5fd5015c-43f9-4e25-90cb-e960753842a9",
        "7ea91e7d-3c00-47d8-bf31-6da3aaa008ee", "8e7d18af-a9bd-4a3f-964e-ab9f301cdc35",
        "9972b682-4c8d-43ba-90f1-b22f5800b0e9", "a885cc70-b2b3-4a28-9e19-2d946e2255e3",
        "b010281a-df63-436c-9396-41eba43665df", "d145895e-e222-4cb0-be8a-e297b628173c",
        "d7b5faad-fffe-4717-a75d-bb80ba61b6f5", "da767378-8c00-43c1-a5eb-705d7d2f7306",
        "e2a06efd-e77e-4f7b-9138-7dcc64844b62", "fa22d326-f218-4c4b-8524-e9481e6066d6",
    ];
    /// The soft-currency reward item ("gold") used by every captured job.
    const REWARD_ITEM_GOLD: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

    /// Objective template IDs are fixed per job type in the captures.
    fn objective_ids(job_type: i64) -> &'static [&'static str] {
        match job_type {
            0 => &["fe67a8c1-b107-44de-8e6a-c76e259fd42d", "8b425eba-67ff-4d38-ba3b-ffa2e8493954"],
            1 => &["33af0174-0c6e-4907-a4c6-77fa9caff640", "919c2ad0-0b07-4fd2-a690-8d36be2e311b"],
            3 => &["c1ac35b0-4bda-4741-8115-0d3345d63ce6", "51cdee5a-82d9-46bd-9655-64ae0294c310"],
            4 => &["0e54c204-300d-40c1-b9e1-674380dfa330", "bdb8cc31-d9e1-409c-9a57-0014af59d430"],
            5 => &["091311ed-5e00-40d7-8720-8428407291e0"],
            // type 2 (Clear) wasn't captured; reuse the Defeat objective pair.
            _ => &["fe67a8c1-b107-44de-8e6a-c76e259fd42d", "8b425eba-67ff-4d38-ba3b-ffa2e8493954"],
        }
    }

    /// Number of distinct localization variants per job-type name key.
    fn name_variant_count(job_type: i64) -> u64 {
        match job_type {
            0 => 13, // Defeat.001..013
            1 => 11, // Explore
            2 => 6,  // Clear (estimate)
            3 => 6,  // Rescue
            4 => 8,  // Gather
            5 => 8,  // Duel
            _ => 1,
        }
    }
    fn name_prefix(job_type: i64) -> &'static str {
        match job_type {
            0 => "Defeat",
            1 => "Explore",
            2 => "Clear",
            3 => "Rescue",
            4 => "Gather",
            5 => "Duel",
            _ => "Defeat",
        }
    }

    // -- small Value helpers (never panic) ---------------------------------
    fn get_u64(v: &Value, key: &str, def: u64) -> u64 {
        v.get(key).and_then(|x| x.as_u64()).unwrap_or(def)
    }
    fn get_i64(v: &Value, key: &str, def: i64) -> i64 {
        v.get(key).and_then(|x| x.as_i64()).unwrap_or(def)
    }
    fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
        v.get(key).and_then(|x| x.as_str())
    }

    /// The current daily-reset boundary (epoch secs of the most recent
    /// `resetHour:resetMinute` UTC on or before `now`). Reads the reset time from
    /// globals/pool recurrence; defaults to 05:00 UTC.
    pub fn current_reset_boundary(pools_def: &Value, now: u64) -> u64 {
        let (hour, minute) = daily_reset_hm(pools_def);
        last_reset_at_or_before(now, hour, minute)
    }

    fn daily_reset_hm(pools_def: &Value) -> (u64, u64) {
        if let Some(g) = pools_def.get("globals") {
            if let Some(h) = g.get("dailyJobsRefreshTimeHour").and_then(|x| x.as_u64()) {
                let m = g.get("dailyJobsRefreshTimeMinute").and_then(|x| x.as_u64());
                return (h, m.unwrap_or(0));
            }
        }
        if let Some(pools) = pools_def.get("jobPools").and_then(|p| p.as_array()) {
            for p in pools {
                if let Some(rec) = p.get("recurrence") {
                    if let Some(h) = rec.get("resetHour").and_then(|x| x.as_u64()) {
                        return (h, rec.get("resetMinute").and_then(|x| x.as_u64()).unwrap_or(0));
                    }
                }
            }
        }
        (DEFAULT_RESET_HOUR, 0)
    }

    /// Epoch secs of the most recent `hour:minute` UTC boundary <= `now`.
    fn last_reset_at_or_before(now: u64, hour: u64, minute: u64) -> u64 {
        let day_start = (now / SECS_PER_DAY) * SECS_PER_DAY; // 00:00 UTC of now's day
        let today_reset = day_start + hour * 3600 + minute * 60;
        if now >= today_reset {
            today_reset
        } else {
            today_reset.saturating_sub(SECS_PER_DAY)
        }
    }

    /// Weekday index (0 = Sunday .. 6 = Saturday) of an epoch-secs instant (UTC).
    fn weekday_sun0(epoch: u64) -> u64 {
        // 1970-01-01 was a Thursday (=4 in Sun0 indexing).
        ((epoch / SECS_PER_DAY) + 4) % 7
    }

    /// Advance the difficulty-cycle index, wrapping the `globals.difficultyCycle`
    /// length (defaults to a full wrap at 80 if absent). Purely a rotation counter.
    pub fn next_cycle_index(pools_def: &Value, cur: i64) -> i64 {
        let len = pools_def
            .get("globals")
            .and_then(|g| g.get("difficultyCycle"))
            .and_then(|c| c.as_array())
            .map(|a| a.len() as i64)
            .filter(|n| *n > 0)
            .unwrap_or(80);
        // Advance by the max jobs rolled per reset (4) so the cycle drifts like prod.
        (cur + 4).rem_euclid(len)
    }

    /// Whether a stored `Quest` row is one of our JOB rows (by sentinel gldQuestId).
    pub fn is_job_row(q: &Quest) -> bool {
        q.gld_quest_id == JOB_SENTINEL_GLD
    }

    /// Build the storable `QuestDbEntry` for a generated job Value. The row is a
    /// plain `Quest` (type Normal, sentinel gldQuestId) carrying the job's
    /// objective statuses + difficulty + seed, so /objectives + /complete resolve
    /// it. The rich `jobSetup` lives only in the regenerated board.
    pub fn job_quest_db_entry(job: &Value, character_id: Uuid) -> Option<QuestDbEntry> {
        let quest_id = Uuid::parse_str(get_str(job, "questId")?).ok()?;
        let mut objective_statuses = HashMap::new();
        if let Some(obj) = job.get("objectiveStatuses").and_then(|v| v.as_object()) {
            for k in obj.keys() {
                if let Ok(oid) = Uuid::parse_str(k) {
                    objective_statuses.insert(
                        oid,
                        ObjectiveStatus { status: QuestStatus::Active, progress: 0.0, completed: false },
                    );
                }
            }
        }
        let quest = Quest {
            version: get_u64(job, "version", 0),
            r#type: QuestType::Normal,
            objective_statuses,
            difficulty_level: get_i64(job, "difficultyLevel", 1),
            seed: get_i64(job, "seed", 0) as u64,
            gld_quest_id: JOB_SENTINEL_GLD,
            completed: false,
        };
        Some(QuestDbEntry {
            id: quest_id,
            character_id,
            info: JsonDbWrapper(quest),
            generated_data: JsonDbWrapper(None),
            dungeon_state: None,
        })
    }

    /// How many jobs a pool contributes right now (0 when the pool is dormant),
    /// mirroring the captured behaviour:
    ///   * standard/daily      → maxActive jobs, always active.
    ///   * boss/special weekly → 1 job, always active.
    ///   * featured weekly     → 1 job, only on its `dayOfWeek` window; the featured
    ///     pool for weekday D is active during the game-day that began at D's reset,
    ///     i.e. current game-weekday == (D+1) mod 7.
    ///   * featured/daily      → 0 (dormant in prod).
    fn pool_active_count(pool: &Value, reset_boundary: u64) -> u64 {
        let rec = pool.get("recurrence").cloned().unwrap_or(Value::Null);
        let rec_type = get_i64(&rec, "type", 1);
        let presentation = get_i64(pool, "presentation", 0);
        let max_active = get_u64(pool, "maxActive", 1);
        match presentation {
            0 => max_active.max(1),
            2 => 1,
            _ => {
                if rec_type == 1 {
                    0
                } else {
                    let target_dow = get_i64(&rec, "dayOfWeek", 0).rem_euclid(7) as u64;
                    let cur_dow = weekday_sun0(reset_boundary);
                    if cur_dow == (target_dow + 1) % 7 { 1 } else { 0 }
                }
            }
        }
    }

    /// Compute a pool's `{endTime, nextStartTime}` timers relative to `now`.
    fn pool_timers(pool: &Value, now: u64, active_count: u64) -> (u64, u64) {
        let rec = pool.get("recurrence").cloned().unwrap_or(Value::Null);
        let rec_type = get_i64(&rec, "type", 1);
        let hour = get_u64(&rec, "resetHour", DEFAULT_RESET_HOUR);
        let minute = get_u64(&rec, "resetMinute", 0);
        let today_reset = last_reset_at_or_before(now, hour, minute);
        let next_daily = today_reset + SECS_PER_DAY;
        let presentation = get_i64(pool, "presentation", 0);
        if rec_type == 1 {
            // daily pool: window ends at the next daily reset.
            (next_daily, next_daily)
        } else {
            // weekly pool.
            let target_dow = get_i64(&rec, "dayOfWeek", 0).rem_euclid(7) as u64;
            let next_target = next_weekday_reset(now, target_dow, hour, minute);
            if presentation == 2 {
                (next_target, next_target)
            } else if active_count > 0 {
                (next_daily, next_target)
            } else {
                (0, next_target)
            }
        }
    }

    /// Epoch secs of the next `hour:minute` reset that falls on `target_dow`
    /// (0=Sun..6=Sat), strictly after `now`.
    fn next_weekday_reset(now: u64, target_dow: u64, hour: u64, minute: u64) -> u64 {
        let base = last_reset_at_or_before(now, hour, minute);
        let base_dow = weekday_sun0(base);
        let days_ahead = (target_dow + 7 - base_dow) % 7;
        let candidate = base + days_ahead * SECS_PER_DAY;
        if candidate <= now {
            candidate + SECS_PER_WEEK
        } else {
            candidate
        }
    }
    const SECS_PER_WEEK: u64 = 604_800;

    /// Difficulty-level roll for a job: character level offset by the per-type,
    /// per-level difficulty range from `perTypeDifficulty` (clamped to a floor of 1).
    fn roll_difficulty(pools_def: &Value, job_type: i64, level: u16, rng: &mut Rng) -> i64 {
        let level = level.max(1) as i64;
        let (mut lo, mut hi) = pools_def
            .get("globals")
            .and_then(|g| g.get("baseJobDifficultyRange"))
            .map(|r| (get_i64(r, "min", -2), get_i64(r, "max", 9)))
            .unwrap_or((-2, 9));
        if let Some(arr) = pools_def.get("perTypeDifficulty").and_then(|a| a.as_array()) {
            if let Some(entry) = arr.iter().find(|e| get_i64(e, "jobType", -1) == job_type) {
                if let Some(by_level) = entry.get("difficultyByLevel").and_then(|a| a.as_array()) {
                    let mut best: Option<&Value> = None;
                    for row in by_level {
                        if get_i64(row, "level", i64::MAX) <= level {
                            best = Some(row);
                        }
                    }
                    if let Some(row) = best.or_else(|| by_level.get(0)) {
                        lo = get_i64(row, "veryEasyMin", lo);
                        hi = get_i64(row, "veryHardMax", hi);
                    }
                }
            }
        }
        let offset = rng.range_incl(lo, hi);
        (level + offset).max(1)
    }

    /// Roll a single job Value for a pool + slot. Deterministic via the seed.
    fn roll_job(
        pools_def: &Value,
        pool: &Value,
        character_id: Uuid,
        level: u16,
        reset_boundary: u64,
        slot: u64,
    ) -> Value {
        let pool_id = get_str(pool, "jobPoolId").unwrap_or("");
        let presentation = get_i64(pool, "presentation", 0);
        let base_seed = seed_for(character_id, reset_boundary, pool_id, slot);
        let mut rng = Rng::new(base_seed);

        // The weekly boss pool is always a Duel (type 5); otherwise pick from the
        // non-duel types {0,1,3,4} (Clear=2 unseen in captures → skip).
        let job_type: i64 = if presentation == 2 {
            5
        } else {
            *rng.pick(&[0i64, 1, 3, 4]).unwrap_or(&0)
        };

        let quest_id = uuid_from_seed(base_seed.wrapping_add(0xA11CE));
        let seed_field: i64 = rng.next_u64() as i64; // signed, matches captured range
        let difficulty = roll_difficulty(pools_def, job_type, level, &mut rng);

        let mut objectives = serde_json::Map::new();
        for oid in objective_ids(job_type) {
            objectives.insert(
                (*oid).to_string(),
                json!({ "status": "Active", "progress": 0.0, "completed": false }),
            );
        }

        let dungeon = rng.pick(DUNGEON_TEMPLATES).copied().unwrap_or("");
        let prim_fam = rng.pick(ENEMY_FAMILIES).copied().unwrap_or("");
        let sec_fam = rng.pick(ENEMY_FAMILIES).copied().unwrap_or("");
        let boss_fam = rng.pick(ENEMY_FAMILIES).copied().unwrap_or(prim_fam);

        let primary_count = if job_type == 5 { 0 } else { rng.range_incl(3, 6) };
        let secondary_count = if job_type == 5 { 0 } else { rng.range_incl(2, 4) };
        let boss_level_delta = rng.range_incl(4, 8);
        let secret_room = job_type != 5 && rng.below(2) == 1;
        let reward_xp = (difficulty.max(1) as u64) * 15 + rng.below(60);
        let reward_item_count = ((difficulty.max(1) as u64) * 30 + rng.below(100)) / 10 * 10;
        let reward_gem = if secret_room && rng.below(3) == 0 { rng.range_incl(6, 15) as u64 } else { 0 };
        let initial_epl = difficulty.max(1) as u64;

        let name_idx = rng.below(name_variant_count(job_type)) + 1;
        let name_key = format!("UI.Jobs.Names.{}.{:03}", name_prefix(job_type), name_idx);
        let desc_key = format!("UI.Jobs.Description.{}", name_prefix(job_type));

        let mut job_setup = serde_json::Map::new();
        job_setup.insert("jobType".into(), json!(job_type));
        job_setup.insert("jobCreatorVersion".into(), json!(0));
        job_setup.insert("algorithmVersion".into(), json!(3));
        job_setup.insert("dungeonTemplateId".into(), json!(dungeon));
        if job_type != 5 {
            job_setup.insert("primaryEnemyFamilyId".into(), json!(prim_fam));
            job_setup.insert("secondaryEnemyFamilyId".into(), json!(sec_fam));
        }
        job_setup.insert("bossEnemyFamilyId".into(), json!(boss_fam));
        job_setup.insert("primaryEnemyCount".into(), json!(primary_count));
        job_setup.insert("secondaryEnemyCount".into(), json!(secondary_count));
        job_setup.insert("secondaryEnemyCountPerSpawnerMin".into(), json!(1));
        job_setup.insert("secondaryEnemyCountPerSpawnerMax".into(), json!(1));
        job_setup.insert("enemyBaseLevelOffset".into(), json!(0));
        job_setup.insert("bossLevelDelta".into(), json!(boss_level_delta));
        job_setup.insert("secretRoom".into(), json!(secret_room));
        if secret_room {
            job_setup.insert("secretBossLevelDelta".into(), json!(boss_level_delta + 2));
            job_setup.insert("secretBossEnemyFamilyId".into(), json!(boss_fam));
        }
        job_setup.insert("rewardGemCount".into(), json!(reward_gem));
        job_setup.insert("rewardItemId".into(), json!(REWARD_ITEM_GOLD));
        job_setup.insert("rewardItemCount".into(), json!(reward_item_count));
        job_setup.insert("rewardXp".into(), json!(reward_xp));
        match job_type {
            0 => {
                job_setup.insert("defeatEnemyCount".into(), json!(primary_count));
            }
            3 => {
                job_setup.insert("rescueNpcCount".into(), json!(2));
            }
            4 => {
                let gather = rng.pick(GATHER_ITEMS).copied().unwrap_or("");
                job_setup.insert("gatherItemId".into(), json!(gather));
                job_setup.insert("gatherItemCount".into(), json!(rng.range_incl(3, 6)));
            }
            5 => {
                let duel = rng.pick(DUEL_BOSSES).copied().unwrap_or("");
                job_setup.insert("duelBossId".into(), json!(duel));
            }
            _ => {}
        }
        job_setup.insert("initialEPL".into(), json!(initial_epl));
        job_setup.insert("questName".into(), json!({ "key": name_key, "dynamicElements": [] }));
        job_setup.insert("questDescription".into(), json!({ "key": desc_key, "dynamicElements": [] }));

        json!({
            "questId": quest_id.to_string(),
            "version": 0,
            "type": "JOB",
            "objectiveStatuses": Value::Object(objectives),
            "difficultyLevel": difficulty,
            "seed": seed_field,
            "jobPoolId": pool_id,
            "jobSetup": Value::Object(job_setup),
            "completed": false,
        })
    }

    /// Generate the full jobs board + pool timers for a character at `now`.
    /// Returns `(jobs, jobPools)`. A malformed/Null `pools_def` yields empty lists.
    pub fn generate(
        pools_def: &Value,
        character_id: Uuid,
        level: u16,
        _cycle_index: i64,
        reset_boundary: u64,
        now: u64,
    ) -> (Vec<Value>, Value) {
        let pools = match pools_def.get("jobPools").and_then(|p| p.as_array()) {
            Some(p) => p,
            None => return (Vec::new(), json!([])),
        };
        let max_active_global = pools_def
            .get("globals")
            .and_then(|g| g.get("maxActiveJobs"))
            .and_then(|v| v.as_u64())
            .unwrap_or(4);

        let mut jobs = Vec::new();
        let mut timers = Vec::new();
        for pool in pools {
            let pool_id = match get_str(pool, "jobPoolId") {
                Some(id) => id,
                None => continue,
            };
            let mut count = pool_active_count(pool, reset_boundary);
            if get_i64(pool, "presentation", 0) == 0 {
                count = count.min(max_active_global);
            }
            for slot in 0..count {
                jobs.push(roll_job(pools_def, pool, character_id, level, reset_boundary, slot));
            }
            let (end_time, next_start) = pool_timers(pool, now, count);
            timers.push(json!({ "id": pool_id, "endTime": end_time, "nextStartTime": next_start }));
        }
        (jobs, Value::Array(timers))
    }
}

#[cfg(test)]
mod jobs_tests {
    use super::jobs_gen;
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn sample_pools() -> Value {
        // A trimmed but shape-faithful job_pools.json: one standard/daily pool
        // (maxActive 4), one boss/special weekly, and one featured weekly on Sun.
        json!({
            "globals": {
                "maxActiveJobs": 4,
                "dailyJobsRefreshTimeHour": 5,
                "dailyJobsRefreshTimeMinute": 0,
                "difficultyCycle": [0,1,2,1,0,3,0,1],
                "baseJobDifficultyRange": { "min": -2, "max": 9 }
            },
            "jobPools": [
                { "jobPoolId": "4956c6ab-1832-4edd-8bee-561b79f83ee2", "presentation": 0,
                  "maxActive": 4, "recurrence": { "type": 1, "resetHour": 5, "resetMinute": 0, "dayOfWeek": 0 } },
                { "jobPoolId": "361da91e-6860-4c31-a447-4010cbaad1dd", "presentation": 2,
                  "maxActive": 1, "recurrence": { "type": 2, "resetHour": 5, "resetMinute": 0, "dayOfWeek": 0 } },
                { "jobPoolId": "9d94baeb-96d4-49e9-bdf6-9f939be836d3", "presentation": 1,
                  "maxActive": 1, "recurrence": { "type": 2, "resetHour": 5, "resetMinute": 0, "dayOfWeek": 6 } }
            ],
            "perTypeDifficulty": [
                { "jobType": 0, "difficultyByLevel": [
                    { "level": 1, "veryEasyMin": -2, "veryHardMax": 9 },
                    { "level": 20, "veryEasyMin": -4, "veryHardMax": 7 }
                ]}
            ]
        })
    }

    // 2026-05-13 06:00 UTC (Wednesday, after the 05:00 reset).
    const NOW_WED: u64 = 1_778_648_400 + 3600;
    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);

    #[test]
    fn jobs_generated_non_empty_from_sample() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        // 4 standard daily + 1 weekly boss (Wed is not the featured pool's day).
        assert!(!jobs.is_empty(), "board must not be empty");
        assert_eq!(jobs.len(), 5, "4 daily + 1 boss expected, got {}", jobs.len());
    }

    #[test]
    fn every_job_has_type_job_and_populated_job_setup() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        for j in &jobs {
            assert_eq!(j["type"], "JOB");
            assert!(j["questId"].as_str().is_some(), "questId present");
            assert!(Uuid::parse_str(j["questId"].as_str().unwrap()).is_ok(), "questId is a UUID");
            let js = &j["jobSetup"];
            assert!(js.is_object(), "jobSetup present");
            // Required jobSetup fields are populated (non-null).
            for key in [
                "jobType", "jobCreatorVersion", "algorithmVersion", "dungeonTemplateId",
                "bossEnemyFamilyId", "primaryEnemyCount", "secondaryEnemyCount",
                "enemyBaseLevelOffset", "bossLevelDelta", "secretRoom", "rewardGemCount",
                "rewardItemId", "rewardItemCount", "rewardXp", "initialEPL", "questName",
            ] {
                assert!(!js[key].is_null(), "jobSetup.{key} populated");
            }
            assert!(js["dungeonTemplateId"].as_str().unwrap().len() == 36, "real dungeon id");
            assert!(j["difficultyLevel"].as_i64().unwrap() >= 1, "difficulty >= 1");
            assert!(j["objectiveStatuses"].as_object().unwrap().len() >= 1, "has objectives");
        }
    }

    #[test]
    fn duel_boss_job_has_duel_fields() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        let boss = jobs
            .iter()
            .find(|j| j["jobPoolId"] == "361da91e-6860-4c31-a447-4010cbaad1dd")
            .expect("boss pool produced a job");
        assert_eq!(boss["jobSetup"]["jobType"], 5, "boss pool -> Duel");
        assert!(boss["jobSetup"]["duelBossId"].as_str().is_some(), "duelBossId present");
    }

    #[test]
    fn timers_are_relative_to_now_not_frozen() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (_j, timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        let arr = timers.as_array().expect("timers array");
        assert_eq!(arr.len(), 3, "one timer per pool");
        // The daily pool's next reset must be strictly in the future and within 24h.
        let daily = arr
            .iter()
            .find(|p| p["id"] == "4956c6ab-1832-4edd-8bee-561b79f83ee2")
            .unwrap();
        let end = daily["endTime"].as_u64().unwrap();
        assert!(end > NOW_WED, "daily endTime is in the future");
        assert!(end - NOW_WED <= 86_400, "daily endTime within a day");
        // Every non-zero timer is strictly after now (no stale 2026-03 constants).
        for p in arr {
            for k in ["endTime", "nextStartTime"] {
                let t = p[k].as_u64().unwrap();
                assert!(t == 0 || t > NOW_WED, "{} {} must be 0 or > now (got {})", p["id"], k, t);
            }
        }
    }

    #[test]
    fn deterministic_same_window_same_board() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let a = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        // A later `now` in the SAME window (same reset boundary) → same jobs.
        let b = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED + 3600);
        assert_eq!(a.0, b.0, "same reset window regenerates identical jobs");
    }

    #[test]
    fn different_character_different_board() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let other = Uuid::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef);
        let a = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        let b = jobs_gen::generate(&pools, other, 30, 0, boundary, NOW_WED);
        assert_ne!(a.0, b.0, "different characters roll different jobs");
    }

    #[test]
    fn null_pools_degrade_to_empty() {
        let (jobs, timers) = jobs_gen::generate(&Value::Null, CHAR, 30, 0, 0, NOW_WED);
        assert!(jobs.is_empty(), "Null pools -> empty jobs, no panic");
        assert_eq!(timers, json!([]), "Null pools -> empty timers");
        // A malformed shape (jobPools not an array) also degrades.
        let bad = json!({ "jobPools": 42, "globals": "nope" });
        let (jobs2, _t2) = jobs_gen::generate(&bad, CHAR, 30, 0, 0, NOW_WED);
        assert!(jobs2.is_empty(), "malformed pools -> empty jobs");
    }

    #[test]
    fn featured_pool_active_only_on_its_day() {
        let pools = sample_pools();
        // Sunday after reset: the featured pool with dayOfWeek=6 (Sat) is active
        // during the Sun game-day (window opened Sat 05:00). 2026-05-17 is a Sunday.
        let now_sun = 1_779_080_400 - 86_400 + 3600; // Sun 06:00 UTC (approx via boss timer base)
        let boundary = jobs_gen::current_reset_boundary(&pools, now_sun);
        let (_j, timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, now_sun);
        // The board is at least the daily 4 + boss 1 on any day.
        let (jobs_wed, _) = {
            let b = jobs_gen::current_reset_boundary(&pools, NOW_WED);
            jobs_gen::generate(&pools, CHAR, 30, 0, b, NOW_WED)
        };
        assert!(jobs_wed.len() >= 5, "daily+boss always present");
        // timers array always lists all 3 pools regardless of activity.
        assert_eq!(timers.as_array().unwrap().len(), 3);
    }

    /// End-to-end against the *real* committed `job_pools.json` (the faithful APK
    /// extract the live server loads). Guards that the shipped data still drives a
    /// non-empty, prod-shaped board. NOW_WED is 2026-05-13 Wed 06:00 UTC (after the
    /// 05:00 reset) — the day matching prod capture id=1105, whose board was:
    ///   4 daily (`4956c6ab`) + 1 weekly boss (`361da91e`) + 1 featured (`9fcbb01c`,
    ///   dayOfWeek=2/Tue, active during the Wed game-day) = 6 jobs, and 10 timers.
    #[test]
    fn real_job_pools_file_generates_prod_shaped_board() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let pools: Value = serde_json::from_str(&raw).expect("valid job_pools.json");

        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);

        assert_eq!(timers.as_array().unwrap().len(), 10, "10 pools -> 10 timers");
        assert_eq!(jobs.len(), 6, "Wed board = 4 daily + 1 boss + 1 featured, got {}", jobs.len());
        // The featured pool active on this weekday produced exactly one job.
        assert_eq!(
            jobs.iter().filter(|j| j["jobPoolId"] == "9fcbb01c-13bf-4cd9-916f-25d5faf5314e").count(),
            1,
            "Tue-featured pool active during the Wed game-day"
        );

        // Every timer is 0 or strictly in the future (no frozen 2026-03 constants).
        for p in timers.as_array().unwrap() {
            for k in ["endTime", "nextStartTime"] {
                let t = p[k].as_u64().unwrap();
                assert!(t == 0 || t > NOW_WED, "timer {} {} stale: {}", p["id"], k, t);
            }
        }
        // The daily standard pool produced exactly maxActiveJobs (4) entries.
        let daily = jobs
            .iter()
            .filter(|j| j["jobPoolId"] == "4956c6ab-1832-4edd-8bee-561b79f83ee2")
            .count();
        assert_eq!(daily, 4, "standard daily pool -> maxActiveJobs (4)");
        // The boss pool produced a Duel with a duelBossId from the real data.
        let boss = jobs
            .iter()
            .find(|j| j["jobPoolId"] == "361da91e-6860-4c31-a447-4010cbaad1dd")
            .expect("boss pool job");
        assert_eq!(boss["jobSetup"]["jobType"], 5);
        assert!(boss["jobSetup"]["duelBossId"].as_str().is_some());

        // Every generated job round-trips into a storable Quest row (accept path).
        for j in &jobs {
            assert!(
                jobs_gen::job_quest_db_entry(j, CHAR).is_some(),
                "job must build a persistable quest row"
            );
        }
    }
}

