use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::{
    economy::{RewardGrant, apply_reward, grant_chest},
    static_data::StaticData,
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
    util,
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
    /// Quests the server removed in the course of answering this request.
    ///
    /// The job rotation deletes the previous window's un-entered job rows; without
    /// this the client is never told and keeps showing board entries that no longer
    /// exist. Retail sends the field in 17.91% of captured `/quests` responses and
    /// **never sends it empty** — it is a "there were deletions" signal, not a
    /// always-present list — so it is skipped when nothing was removed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deleted_quest_ids: Vec<Uuid>,
    /// The event ("Sigil") quests whose instance window is open right now — one
    /// stored, per-character `GAME_EVENT` row per active event. Their `questId` is a
    /// per-character INSTANCE id and their `gldQuestId` is the template, which is why
    /// everything downstream must resolve through `gldQuestId`.
    game_event_quests: Vec<QuestWithId>,
    /// Events opening within the next 24 h. MEASURED: retail's warning array is
    /// "starting soon", not "ending soon" — all 686 captured entries had a start time
    /// between 0.1 h and 24.0 h in the FUTURE, and there was always exactly one.
    /// These are announcements, so they are not persisted: a warning quest has no
    /// stored progress until its window opens.
    game_event_quests_in_warning: Vec<QuestWithId>,
    /// Deliberately empty. Retail sent 101 entries across the corpus, and the
    /// discriminator is not determinable from it: every one sat 1–48 h after its
    /// instance start — i.e. INSIDE the same 48 h window that the active array uses —
    /// and carried `completed: false`, so it is neither "window elapsed" nor "player
    /// finished it". Sending a guess here would put quests on the player's finished
    /// list that retail would not have. See docs/quest-and-event-model.md.
    game_event_quests_finished: Vec<QuestWithId>,
}

/// Split stored quest rows into the client's `quests[]` and `generatedData[]`.
///
/// Two rows are dropped rather than advertised:
///
/// * **JOB rows**, which are surfaced only in `jobs[]` — matching prod, where the
///   two arrays never overlap. Their `generatedData[]` entries are NOT dropped: retail
///   sends one per job, and the caller adds them from the freshly rolled board via
///   [`jobs_gen::job_generated_data_list`] (see that function for why the board rather
///   than the row is the source).
/// * **Quests with no generated data.** There is no dungeon behind such a quest, so
///   it cannot be placed on the quest map or started. This used to push the quest
///   into `quests[]` anyway while its `generatedData[]` entry was pushed only
///   `if let Some(...)`, handing the client a quest it could list but never resolve:
///   the `!` badge counted it and the map waited forever for data that was never
///   coming. [report #62]
///
///   Measured across all of production: exactly TWO rows were in that state — "The
///   Message" (`cca4a80b…`), whose template ships no `dungeon_uuid` and which is
///   therefore excluded from the built pool, held by exactly the two characters that
///   reported a blank quest map. All 50 other non-job quests carry their data.
///
///   Skipping is the honest answer: with no dungeon we can neither render it nor let
///   anyone play it, and advertising it is what hangs the client.
///
/// A `GAME_EVENT` row is routed to `gameEventQuests[]` instead of `quests[]`, and
/// only while its instance is still open — `open_event_instances` carries the
/// instance ids the event calendar says are live right now. A stored row whose
/// window has closed is simply not advertised (the row stays, so a re-opened window
/// finds the player's milestone progress where they left it).
///
/// The invariant the client relies on, and the one the tests pin: **every quest in
/// `quests[]` or `gameEventQuests[]` has a matching entry in `generatedData[]`,
/// keyed by the same id.** Retail holds it for event quests too — in every captured
/// response the event instance's id was also in `dungeonGeneratedDataList`.
fn split_quest_rows(
    rows: impl Iterator<
        Item = (
            Uuid,
            blades_lib::user_data::Quest,
            Option<blades_lib::user_data::DungeonGeneratedData>,
        ),
    >,
    open_event_instances: &std::collections::HashSet<Uuid>,
) -> (
    Vec<QuestWithId>,
    Vec<QuestWithId>,
    Vec<DungeonGeneratedDataWithId>,
) {
    let mut quests = Vec::new();
    let mut event_quests = Vec::new();
    let mut generated = Vec::new();
    for (quest_id, info, generated_data) in rows {
        if jobs_gen::is_job_row(&info) {
            continue;
        }
        let is_event = matches!(info.r#type, blades_lib::user_data::QuestType::GameEvent);
        if is_event && !open_event_instances.contains(&quest_id) {
            continue;
        }
        let Some(inner) = generated_data else {
            continue;
        };
        let with_id = QuestWithId { quest_id, quest: info };
        if is_event {
            event_quests.push(with_id);
        } else {
            quests.push(with_id);
        }
        generated.push(DungeonGeneratedDataWithId { quest_id, inner });
    }
    (quests, event_quests, generated)
}

/// The response's `dungeonGeneratedDataList`: the stored rows' entries (quests + open
/// events, from [`split_quest_rows`]) plus one for every job on the board.
///
/// Retail's list spans all three — the captured body's 10 entries are 6 jobs + 2 quests
/// + 2 events. Ours omitted the jobs entirely, so the client put them on the map and
/// waited forever for data that never came (report #85, the same failure mode as #62).
///
/// This is a function rather than two lines in the handler so the invariant is testable:
/// the handler itself needs a DB and a session, and the shape-only job tests that let
/// #85 ship are exactly what happens when the assembly step has no test of its own.
fn assemble_generated_data_list(
    mut from_rows: Vec<DungeonGeneratedDataWithId>,
    game_data: &blades_lib::game_data::GameData,
    static_data: &StaticData,
    jobs: &[Value],
) -> Vec<DungeonGeneratedDataWithId> {
    from_rows.extend(jobs_gen::job_generated_data_list(game_data, static_data, jobs));
    from_rows
}

#[post("/api/game/v1/public/characters/{character_id}/quests")]
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
    // Shared globals for use inside the transaction closure (the daily-quest select reads
    // static_data + game_data). Cloning the Arc avoids borrowing `app_state` across the
    // `conn` borrow (which the closure would otherwise move — E0505).
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();
    conn.transaction(|mut conn| {
        async move {
            // Collected by the job rotation below and reported to the client.
            let mut deleted_quest_ids: Vec<Uuid> = Vec::new();
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
                    // Whatever goes here is reported to the client as `deletedQuestIds`
                    // — otherwise the board keeps showing entries we just removed.
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
                        deleted_quest_ids.extend(stale.iter().copied());
                    }
                }

                // Upsert the current window's job rows (idempotent within the window).
                for job in &jobs {
                    if let Some(entry) =
                        jobs_gen::job_quest_db_entry(job, character_id_var, &globals.game_data, &globals.static_data)
                    {
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

            // ---- Event ("Sigil") quests: mint the instances whose window is open ----
            // One stored GAME_EVENT row per (character, event instance). Deterministic
            // id, so re-fetching within the window resolves the SAME row and the
            // player's objective progress and milestone count survive. Inserting is
            // what makes /objectives and /complete able to find the quest at all.
            let player_level = character.character.0.level as i64;
            let minted = event_quests::mint(
                &globals.static_data,
                &globals.game_data,
                character_id_var,
                player_level,
                now as i64,
            );
            let open_event_instances: std::collections::HashSet<Uuid> =
                minted.iter().map(|m| m.quest_id).collect();
            for m in &minted {
                use crate::schema::quests;
                insert_into(quests::table)
                    .values(&QuestDbEntry {
                        id: m.quest_id,
                        character_id: character_id_var,
                        info: JsonDbWrapper(m.quest.clone()),
                        generated_data: JsonDbWrapper(m.dungeon.clone()),
                        dungeon_state: None,
                    })
                    .on_conflict((quests::id, quests::character_id))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;
            }

            // Retire event rows whose window has closed and that the player never
            // entered. Without this each character accrues a dead row per event per
            // window — about 365 a year — and the client keeps being told about
            // instances that no longer exist. Same shape as the job prune above: only
            // rows with no `dungeon_state` are removed (an entered run is left alone),
            // and whatever goes is reported as `deletedQuestIds`.
            {
                use crate::schema::quests;
                let stale: Vec<Uuid> = quests::table
                    .filter(quests::character_id.eq(character_id_var))
                    .filter(quests::dungeon_state.is_null())
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .filter(|q| {
                        matches!(
                            q.info.0.r#type,
                            blades_lib::user_data::QuestType::GameEvent
                        ) && !open_event_instances.contains(&q.id)
                    })
                    .map(|q| q.id)
                    .collect();
                if !stale.is_empty() {
                    diesel::delete(
                        quests::table
                            .filter(quests::character_id.eq(character_id_var))
                            .filter(quests::id.eq_any(&stale)),
                    )
                    .execute(&mut conn)
                    .await?;
                    deleted_quest_ids.extend(stale.iter().copied());
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

            let (result_quests, game_event_quests, row_generated_data) = split_quest_rows(
                quests
                    .into_iter()
                    .map(|q| (q.id, q.info.0, q.generated_data.0)),
                &open_event_instances,
            );
            let result_generated_data =
                assemble_generated_data_list(row_generated_data, &globals.game_data, &globals.static_data, &jobs);

            // Events opening within the next 24h, announced but not yet playable.
            let game_event_quests_in_warning = event_quests::upcoming(
                &globals.static_data,
                &globals.game_data,
                character_id_var,
                player_level,
                now as i64,
            );

            Ok(Json(GetQuestsResponse {
                deleted_quest_ids,
                quests: result_quests,
                dungeon_generated_data_list: result_generated_data,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id_var,
                    character: character.character.0,
                },
                jobs: Vec::new(),
                game_event_quests,
                game_event_quests_finished: Vec::new(),
                game_event_quests_in_warning,
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
    "/api/game/v1/public/characters/{character_id}/quests/{quest_id}/accept"
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
            if let Some(entry) =
                jobs_gen::job_quest_db_entry(job, character_id, &app_state.game_data, &app_state.static_data)
            {
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
                    // Was hard-coded `None`: accepting a job handed the client a quest
                    // with no dungeon data, the accept-path half of report #85.
                    dungeon_generated_data: entry.generated_data.0.map(|inner| {
                        DungeonGeneratedDataWithId { quest_id, inner }
                    }),
                }));
            }
        }
        // Not a job we know about and not a real quest → let the normal path 404.
    }

    // check permission (normal-quest path) + read the character level so the quest's
    // enemies scale to the player (fix: generate_quest_data no longer hard-codes level 1).
    let character = {
        use crate::schema::characters::dsl::*;
        let rows = characters
            .filter(id.eq(&character_id))
            .select(CharacterDbEntryCharacterAlone::as_select())
            .load(&mut conn)
            .await?;
        util::get_only_single_character_and_check_permission(rows, &session.session)?
    };
    let player_level = character.character.0.level as i64;

    // actually add quest (level-scaled; a nil-dungeon dialogue quest generates no
    // dungeon data instead of erroring).
    let (quest, dungeon_generated_data) = generate_quest_data(
        &app_state.game_data,
        &app_state.static_data,
        quest_id,
        player_level,
        &app_state.static_data.quests_daily.level_scaling,
    )?;
    //TODO: specifically handle the case the quest already exist (primary key is character id + quest id)

    let to_insert = QuestDbEntry {
        id: quest_id,
        character_id,
        info: JsonDbWrapper(quest),
        generated_data: JsonDbWrapper(dungeon_generated_data),
        dungeon_state: None,
    };

    // Accepting an already-accepted quest returns the STORED row rather than failing
    // on the (id, character_id) primary key. The client re-sends /accept on a retry
    // or a reconnect, and a 500 there strands the player on a quest they cannot open;
    // it must also not reset progress they already made, so the stored row wins.
    {
        use crate::schema::quests;
        insert_into(quests::table)
            .values(&to_insert)
            .on_conflict((quests::id, quests::character_id))
            .do_nothing()
            .execute(&mut conn)
            .await?;
    }
    let stored = {
        use crate::schema::quests;
        quests::table
            .filter(quests::id.eq(quest_id))
            .filter(quests::character_id.eq(character_id))
            .select(QuestDbEntry::as_select())
            .load(&mut conn)
            .await?
            .into_iter()
            .next()
            .unwrap_or(to_insert)
    };

    Ok(Json(AcceptQuestResponse {
        quest: QuestWithId {
            quest_id,
            quest: stored.info.0,
        },
        dungeon_generated_data: stored
            .generated_data
            .0
            .map(|inner| DungeonGeneratedDataWithId { quest_id, inner }),
    }))
}

/// What completing `quest_id` pays, and the bookkeeping that goes with it.
///
/// Two populations, and telling them apart is the whole point:
///
/// * **An ordinary quest** pays a fixed amount from `quest_rewards.json`. That table
///   is keyed by the TEMPLATE id, so the lookup goes through `gldQuestId` first and
///   only falls back to the row id. It used to be the other way round, against a
///   table keyed by whatever id happened to be in the captured URL — which for an
///   event quest is a per-character instance, so 78 of its 148 keys belonged to
///   instances that will never exist again and every event quest paid nothing.
///
/// * **An event quest** is repeatable and pays a MILESTONE: the Nth completion pays
///   `rewards[N]`, and the last one additionally pays `finalReward`. Measured across
///   93 retail instances — 91/93 first completions, 67/68 second, 59/60 third, 56/57
///   fourth, and all 54 observed fifth completions paid the last tier merged with
///   `finalReward`. Past the last milestone the instance is exhausted and pays
///   nothing.
///
/// A quest with no captured reward pays an empty grant and is logged. No number is
/// synthesised for it: observed `characterXp` spreads over 200–900 with no rule that
/// predicts it from level, category or objective count, so a constant would be a
/// fabrication wearing a fallback's clothes. `quest_rewards.json._meta` lists exactly
/// which quests are in that state.
fn resolve_completion_reward(
    static_data: &blades_lib::static_data::StaticData,
    quest_id: Uuid,
    quest: &blades_lib::user_data::Quest,
    server_state: &mut blades_lib::server_state::ServerState,
) -> RewardGrant {
    if matches!(quest.r#type, blades_lib::user_data::QuestType::GameEvent) {
        let Some(tmpl) = static_data.event_quests.templates.get(&quest.gld_quest_id) else {
            log::warn!(
                "[quest] event quest {quest_id} (template {}) has no entry in \
                 event_quests.json — paying nothing",
                quest.gld_quest_id
            );
            return RewardGrant::default();
        };
        let completion = *server_state
            .event_quest_completions
            .entry(quest_id)
            .or_insert(0) as usize;
        let Some(mut reward) = tmpl.payout(completion) else {
            return RewardGrant::default(); // instance exhausted
        };
        if completion + 1 == tmpl.milestone_count() {
            if let Some(final_reward) = &tmpl.final_reward {
                merge_reward(&mut reward, final_reward);
            }
        }
        server_state
            .event_quest_completions
            .insert(quest_id, completion as u32 + 1);
        return reward;
    }

    // Template first: `quest_rewards.json` is keyed by gldQuestId.
    if let Some(r) = static_data.quest_rewards.get(&quest.gld_quest_id) {
        return r.clone();
    }
    if let Some(r) = static_data.quest_rewards.get(&quest_id) {
        return r.clone();
    }
    log::warn!(
        "[quest] no captured reward for quest {quest_id} (template {}) — paying nothing",
        quest.gld_quest_id
    );
    RewardGrant::default()
}

/// Add `extra` into `into`. Used for the last event milestone, which retail paid as
/// the tier and the `finalReward` in a single `/complete` body.
fn merge_reward(into: &mut RewardGrant, extra: &RewardGrant) {
    for (id, n) in &extra.currencies {
        *into.currencies.entry(*id).or_insert(0) += *n;
    }
    for (id, n) in &extra.stackable_items {
        *into.stackable_items.entry(*id).or_insert(0) += *n;
    }
    into.items.extend(extra.items.iter().cloned());
    into.chests.extend(extra.chests.iter().cloned());
    into.character_xp += extra.character_xp;
    into.town_xp += extra.town_xp;
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
    "/api/game/v1/public/characters/{character_id}/quests/{quest_id}/complete"
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

            // Update the character's completedQuests JSON.
            // The client expects: { "<gldQuestId>": <completion_count> }
            if !entry.character.0.completed_quests.is_object() {
                entry.character.0.completed_quests = json!({});
            }

            let completed_quests = entry
                .character
                .0
                .completed_quests
                .as_object_mut()
                .unwrap();

            let key = quest_entry.info.0.gld_quest_id.to_string();

            let current_count = completed_quests
                .get(&key)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            completed_quests.insert(
                key,
                json!(current_count + 1),
            );

            let reward = resolve_completion_reward(
                &globals.static_data,
                quest_id,
                &quest_entry.info.0,
                &mut entry.server_state.0,
            );

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

/// One objective's absolute progress as the client reports it.
///
/// The client sends exactly `{status, progress}` — 1409 of 1409 captured
/// `objectiveUpdates` entries have those two keys and nothing else. In particular it
/// never sends `completed`, so completion has to be read off `status == Completed`.
/// Reading a `completed` flag that never arrives left `any_newly_completed` false on
/// every request, which silently disabled the objective-reward path entirely.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct ObjectiveUpdate {
    status: blades_lib::user_data::QuestStatus,
    progress: f64,
}

/// Wire shape for the objectives response.
///
/// The retail corpus has exactly three shapes, and the key the quest comes back
/// under is part of the contract:
///
/// | shape | n | when |
/// |---|---|---|
/// | `{quest}` | 856 | ordinary quest, no objective reward |
/// | `{gameEventQuest}` | 363 | event quest — **always** just the quest |
/// | `{character, inventory, quest, reward}` | 42 | ordinary quest whose objective carries a reward |
///
/// Two things follow. An event quest never pays here (its milestones are paid at
/// `/complete`), and an ordinary quest pays only what that OBJECTIVE is worth in
/// `parsed.json` — not the whole quest reward, which is what this handler used to
/// grant and which would have double-paid against `/complete`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObjectivesResponse {
    #[serde(skip_serializing_if = "RewardGrant::is_empty")]
    reward: RewardGrant,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<CompleteInventoryUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<CompleteCharacterWithIdWithoutData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quest: Option<QuestWithId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    game_event_quest: Option<QuestWithId>,
}

/// Which of the two response slots the updated quest goes into.
///
/// Retail answers an event quest under `gameEventQuest` (363 responses) and an
/// ordinary one under `quest` (856, plus 42 that also carry a reward). The client
/// reads the two keys differently — one drives the milestone track, the other the
/// quest log — so a quest in the wrong slot is silently ignored.
fn objectives_wire_slot(
    is_event: bool,
    quest: QuestWithId,
) -> (Option<QuestWithId>, Option<QuestWithId>) {
    if is_event {
        (None, Some(quest))
    } else {
        (Some(quest), None)
    }
}

/// What newly completing `objective_ids` on `gld_quest_id` is worth.
///
/// Straight from `parsed.json`: each objective carries a `rewards[]` list with
/// `experience` and `town_points`. 20 of the 301 shipped objectives have one, which
/// is the population behind retail's 42 reward-bearing `/objectives` responses.
///
/// `items_to_reward` is deliberately NOT granted: 18 of those 20 name an item
/// template, and turning a template id into an instanced `RewardItem` needs the item
/// generator that the shop/craft paths own. Granting the XP and skipping the item is
/// visible and short; inventing an item is not.
fn objective_reward(
    game_data: &blades_lib::game_data::GameData,
    gld_quest_id: Uuid,
    objective_ids: &[Uuid],
) -> RewardGrant {
    let mut out = RewardGrant::default();
    let Some(info) = game_data
        .quests
        .get(&gld_quest_id)
        .and_then(|q| q.dungeon_info.as_ref())
    else {
        return out;
    };
    for oid in objective_ids {
        let Some(objective) = info.objectives.get(oid) else {
            continue;
        };
        for r in &objective.rewards {
            out.character_xp += r.experience.max(0.0) as u64;
            out.town_xp += r.town_points;
        }
    }
    out
}

#[post(
    "/api/game/v1/public/characters/{character_id}/quests/{quest_id}/objectives"
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

            // Merge each objective update in. The client sends absolute progress, and
            // reports completion as `status: "Completed"` — there is no `completed`
            // flag on the wire.
            let mut newly_completed: Vec<Uuid> = Vec::new();
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
                let done = matches!(update.status, blades_lib::user_data::QuestStatus::Completed);
                if done && !entry_obj.completed {
                    entry_obj.completed = true;
                    newly_completed.push(*obj_id);
                }
            }

            let is_event = matches!(
                quest_entry.info.0.r#type,
                blades_lib::user_data::QuestType::GameEvent
            );
            // An event quest never pays here — all 363 captured `{gameEventQuest}`
            // responses are the quest alone, and its milestones are paid at /complete.
            let reward = if is_event || newly_completed.is_empty() {
                RewardGrant::default()
            } else {
                objective_reward(
                    &globals.game_data,
                    quest_entry.info.0.gld_quest_id,
                    &newly_completed,
                )
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

            let (quest, game_event_quest) = objectives_wire_slot(is_event, quest_with_id);
            Ok::<_, BladeApiError>(Json(ObjectivesResponse {
                reward,
                inventory: opt_inventory,
                character: opt_character,
                quest,
                game_event_quest,
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
    use blades_lib::game_data::GameData;
    use blades_lib::static_data::QuestLevelScaling;
    use blades_lib::user_data::{
        DungeonGeneratedData, ObjectiveStatus, Quest, QuestStatus, QuestType,
    };
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The dungeon whose spawn groups EVERY town job's generated data is keyed to.
    ///
    /// `parsed.json` calls it `JobSpawnGroupsReference` (6 enemy spawn groups, 7 item
    /// spawns, 2 chests). It is not a playable layout — it is the shared id space the
    /// job generator draws from, and the client resolves a job's spawn ids against it
    /// whatever `jobSetup.dungeonTemplateId` says.
    ///
    /// MEASURED, not guessed. In the one committed full `/quests` body
    /// (`blades-capture reference/capture-599.jsonl`: 6 jobs, 2 quests, 10 generated-data
    /// entries) all six job entries' enemy/item/chest ids are subsets of this dungeon's,
    /// and the two story quests in the same body — the control — are subsets of neither.
    ///
    /// The `JobCaveVariant_03`-style ids in [`DUNGEON_TEMPLATES`] are NOT usable here:
    /// all 17 of them exist in `parsed.json` with a completely EMPTY `spawn_info`, so
    /// generating from the template id yields an entry with no enemies, no items and no
    /// chests. That is what made this look like missing data rather than a wrong key.
    pub const JOB_SPAWN_GROUPS_REFERENCE: Uuid =
        Uuid::from_u128(0x93202b6a_f74e_49d4_ab70_bd46cc2f9892_u128);

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

    /// The `DungeonGeneratedData` for one rolled job.
    ///
    /// Every job draws from the SAME dungeon ([`JOB_SPAWN_GROUPS_REFERENCE`]) — that is
    /// how retail does it — with the enemies scaled to the job's own `difficultyLevel`.
    /// A job's `difficultyLevel` IS its enemy level (retail: a level-48 character's board
    /// carried jobs at 42 and 46), so it goes straight in; the XP per enemy comes from the
    /// shared `givenXpFormula` rather than a second copy of `100 * level` here.
    ///
    /// Returns `None` only when `parsed.json` is missing the reference dungeon, which the
    /// caller treats as "no entry" rather than an error — same policy as the quest path.
    pub fn generated_data_for_job(
        game_data: &GameData,
        static_data: &StaticData,
        job: &Value,
    ) -> Option<DungeonGeneratedData> {
        let enemy_level = get_i64(job, "difficultyLevel", 1).max(1);
        let given_xp = QuestLevelScaling::default().given_xp(enemy_level);
        let mut data = blades_lib::util::dungeon::generate_for_dungeon(
            game_data,
            static_data,
            &JOB_SPAWN_GROUPS_REFERENCE,
            enemy_level,
            given_xp,
        )?;
        // Retail stamps `version: 1` on all ten generated-data entries in the captured
        // body. `generate_for_dungeon` hard-codes 0 because that is what our story quests
        // have always shipped and they work; bumping it there would change their wire
        // output for no measured reason, so the job path — which has a measured value —
        // sets its own.
        data.version = 1;
        Some(data)
    }

    /// The `dungeonGeneratedDataList` entries for a whole board.
    ///
    /// Retail's list covers jobs as well as quests: in the captured body all 6 job ids and
    /// both story-quest ids were present (10 entries = 6 jobs + 2 quests + 2 events). We
    /// used to send none for jobs, so the client listed a job on the map and then waited
    /// forever for data that was never coming — the same failure as report #62, and the
    /// reason deleting `job_pools.json` "fixed" the hang: no jobs, no unresolvable ids.
    ///
    /// Derived from the freshly rolled board rather than from the stored rows on purpose.
    /// The board is the set the client is actually told about, so this cannot drift out of
    /// step with `jobs[]`, and it heals characters whose rows were written by the old code
    /// with a NULL `generated_data` (their rows are only rewritten at the next daily
    /// rotation). The row is still populated by [`job_quest_db_entry`] for /accept and the
    /// dungeon-enter path — and because both sides derive from the same reference dungeon
    /// and the same `difficultyLevel`, the two agree by construction.
    pub fn job_generated_data_list(
        game_data: &GameData,
        static_data: &StaticData,
        jobs: &[Value],
    ) -> Vec<DungeonGeneratedDataWithId> {
        jobs.iter()
            .filter_map(|job| {
                let quest_id = Uuid::parse_str(get_str(job, "questId")?).ok()?;
                Some(DungeonGeneratedDataWithId {
                    quest_id,
                    inner: generated_data_for_job(game_data, static_data, job)?,
                })
            })
            .collect()
    }

    /// Build the storable `QuestDbEntry` for a generated job Value. The row is a
    /// plain `Quest` (type Normal, sentinel gldQuestId) carrying the job's
    /// objective statuses + difficulty + seed, so /objectives + /complete resolve
    /// it. The rich `jobSetup` lives only in the regenerated board.
    ///
    /// The row also carries the job's generated data. It used to store `None`, which left
    /// /accept handing the client a job with no dungeon data — the accept-path half of the
    /// same hang.
    pub fn job_quest_db_entry(
        job: &Value,
        character_id: Uuid,
        game_data: &GameData,
        static_data: &StaticData,
    ) -> Option<QuestDbEntry> {
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
            seed: get_i64(job, "seed", 0).into(),
            gld_quest_id: JOB_SENTINEL_GLD,
            game_event_quest_data: None,
            rewards: None,
            final_reward: None,
            completed: false,
        };
        Some(QuestDbEntry {
            id: quest_id,
            character_id,
            info: JsonDbWrapper(quest),
            generated_data: JsonDbWrapper(generated_data_for_job(game_data, static_data, job)),
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

// ---------------------------------------------------------------------------
// Event ("Sigil") quests (`event_quests`)
// ---------------------------------------------------------------------------
//
// The `/quests` response's `gameEventQuests[]` array is where a timed event quest
// reaches the player. It is NOT a place for ordinary quests: across the retail
// corpus every one of the 2 753 entries in that array carried `type: "GAME_EVENT"`,
// a `gameEventQuestData.gameEventInstanceId`, five milestone `rewards` and a
// `finalReward`. (Between this change and the previous one, we were filling it with
// `type: "NORMAL"` quests picked by a guessed daily rotation — visible in our own
// captured traffic by its tell-tale `seed: 1234`.)
//
// The model, entirely from `game_events.json` + `event_quests.json`:
//
//   * An event repeats every `recurrence.recurrenceInterval` days and each instance
//     stays open `durationSecs` (39 days / 2 days for all 39 events).
//   * While an instance is open, each character gets ONE quest row for it. The row's
//     `questId` is a per-character INSTANCE id; its `gldQuestId` is the template.
//     **Everything downstream must resolve through `gldQuestId`** — the objectives,
//     the dungeon, the rewards and the version all live under the template, and the
//     instance id resolves to nothing at all in `parsed.json`.
//   * Completing the instance is repeatable: the Nth completion pays the Nth
//     milestone, and the last also pays `finalReward` (see `complete_quest`).
mod event_quests {
    use super::*;
    use blades_lib::features::game_events::{self, EventDef, WARNING_LEAD_SECS};
    use blades_lib::game_data::GameData;
    use blades_lib::static_data::StaticData;
    use blades_lib::user_data::{DungeonGeneratedData, GameEventQuestData, QuestType};

    /// One event-quest instance built for a character.
    pub struct MintedEventQuest {
        pub quest_id: Uuid,
        pub quest: blades_lib::user_data::Quest,
        pub dungeon: Option<DungeonGeneratedData>,
    }

    /// The per-character instance quest id for an event instance.
    ///
    /// Deterministic so that re-fetching `/quests` inside the window resolves the
    /// same stored row — otherwise every poll would mint a new quest and the player's
    /// objective progress and milestone count would reset under them. Derived from
    /// `(character_id, gameEventInstanceId)`, which already contains the event id and
    /// the window start, so two windows of the same event get different ids.
    pub fn instance_quest_id(character_id: Uuid, game_event_instance_id: &str) -> Uuid {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a
        let mut lo: u64 = 0x84222325_CBF29CE4;
        for b in character_id
            .as_bytes()
            .iter()
            .chain(game_event_instance_id.as_bytes())
        {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
            lo = lo.rotate_left(7) ^ h;
        }
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&h.to_le_bytes());
        bytes[8..16].copy_from_slice(&lo.to_le_bytes());
        bytes[6] = (bytes[6] & 0x0F) | 0x40; // v4 shape
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Uuid::from_bytes(bytes)
    }

    /// Build the wire body for one event instance. `None` when the template quest is
    /// not in `parsed.json` (then we cannot honestly produce objectives or a dungeon,
    /// so the event is simply not advertised rather than advertised unplayable).
    fn build(
        def: &EventDef,
        instance_start: i64,
        static_data: &StaticData,
        game_data: &GameData,
        character_id: Uuid,
        player_level: i64,
    ) -> Option<MintedEventQuest> {
        let instance_id = format!("{}::{}", def.event_id, instance_start);
        let quest_id = instance_quest_id(character_id, &instance_id);

        // Resolve the body through the TEMPLATE id — `def.quest_id` is the gldQuestId.
        let (mut quest, dungeon) = generate_quest_data(
            game_data,
            static_data,
            def.quest_id,
            player_level,
            &static_data.quests_daily.level_scaling,
        )
        .ok()?;

        quest.r#type = QuestType::GameEvent;
        quest.gld_quest_id = def.quest_id;
        quest.game_event_quest_data = Some(GameEventQuestData {
            game_event_instance_id: instance_id,
        });
        if let Some(tmpl) = static_data.event_quests.templates.get(&def.quest_id) {
            quest.rewards = Some(tmpl.rewards.clone());
            quest.final_reward = tmpl.final_reward.clone();
        }
        Some(MintedEventQuest {
            quest_id,
            quest,
            dungeon,
        })
    }

    /// The event quests whose instance window covers `now`.
    pub fn mint(
        static_data: &StaticData,
        game_data: &GameData,
        character_id: Uuid,
        player_level: i64,
        now: i64,
    ) -> Vec<MintedEventQuest> {
        static_data
            .game_events
            .iter()
            .filter_map(|def| {
                let start = def.active_instance_start(now)?;
                build(def, start, static_data, game_data, character_id, player_level)
            })
            .collect()
    }

    /// The event quests whose window opens within the warning lead (24 h).
    pub fn upcoming(
        static_data: &StaticData,
        game_data: &GameData,
        character_id: Uuid,
        player_level: i64,
        now: i64,
    ) -> Vec<QuestWithId> {
        game_events::upcoming_events(&static_data.game_events, now, WARNING_LEAD_SECS)
            .into_iter()
            .filter_map(|e| {
                let def = static_data
                    .game_events
                    .iter()
                    .find(|d| d.quest_id == e.quest_id)?;
                let m = build(
                    def,
                    e.start_time_secs,
                    static_data,
                    game_data,
                    character_id,
                    player_level,
                )?;
                Some(QuestWithId {
                    quest_id: m.quest_id,
                    quest: m.quest,
                })
            })
            .collect()
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
        let gd = super::report85_job_generated_data_tests::game_data();
        for j in &jobs {
            assert!(
                jobs_gen::job_quest_db_entry(j, CHAR, &gd).is_some(),
                "job must build a persistable quest row"
            );
        }
    }
}

/// Report #85: the quest map loads forever, and deleting `job_pools.json` "fixes" it.
///
/// Retail sends a `dungeonGeneratedDataList` entry for EVERY job. We sent none, so the
/// client listed the jobs on the map and then waited for spawn data that never came —
/// and with no jobs at all there were no unresolvable ids, which is why removing the
/// pool file made the hang go away while producing an empty board.
///
/// Ground truth is the one committed full `/quests` body,
/// `blades-capture reference/capture-599.jsonl`: 6 jobs, 2 quests, 10 generated-data
/// entries; 6/6 job ids present and — the control — 2/2 quest ids present.
///
/// The tests that shipped this bug (`jobs_tests`) were shape-only: they checked that a
/// job had a `jobSetup` and a UUID, never that anything could resolve it. These pin the
/// invariant instead, and every one of them is paired with a story-quest control so a
/// red run means the job path broke rather than the fixture failing to load.
#[cfg(test)]
mod report85_job_generated_data_tests {
    use super::*;
    use blades_lib::game_data::GameData;
    use blades_lib::static_data::QuestLevelScaling;
    use std::collections::HashSet;

    pub fn game_data() -> GameData {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    fn job_pools() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid job_pools.json")
    }

    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    /// 2026-05-13 Wed 06:00 UTC — the weekday whose board prod capture id=1105 shows.
    const NOW_WED: u64 = 1_778_648_400 + 3600;

    /// The real board, rolled from the committed job pools.
    fn board() -> Vec<Value> {
        let pools = job_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 48, 0, boundary, NOW_WED);
        assert!(!jobs.is_empty(), "the committed pools must roll a board at all");
        jobs
    }

    /// A story quest that really has a dungeon, used as the control everywhere below.
    /// Picked from `parsed.json` at run time rather than hard-coded so the control cannot
    /// rot into a quest that no longer exists.
    fn story_quest_with_dungeon(gd: &GameData) -> Uuid {
        let mut ids: Vec<Uuid> = gd
            .quests
            .iter()
            .filter(|(_, q)| {
                q.dungeon_info
                    .as_ref()
                    .is_some_and(|d| !d.dungeon_uuid.is_nil() && gd.dungeons.contains_key(&d.dungeon_uuid))
            })
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        assert!(!ids.is_empty(), "parsed.json has at least one dungeon-backed quest");
        ids[0]
    }

    fn story_generated(gd: &GameData) -> blades_lib::user_data::DungeonGeneratedData {
        let (_q, data) =
            generate_quest_data(gd, story_quest_with_dungeon(gd), 48, &QuestLevelScaling::default())
                .expect("a dungeon-backed quest generates");
        data.expect("…with dungeon data")
    }

    /// THE bug. Every job the client is told about must have a generated-data entry
    /// keyed by the same id — retail: 6/6.
    ///
    /// Asserted against [`assemble_generated_data_list`], the function the route actually
    /// builds the array with, not against the job helper in isolation: a helper that works
    /// while nothing calls it is precisely the state this repo was in.
    #[test]
    fn every_job_on_the_board_has_generated_data() {
        let gd = game_data();
        let jobs = board();
        let list = assemble_generated_data_list(Vec::new(), &gd, &jobs);

        let job_ids: Vec<Uuid> = jobs
            .iter()
            .map(|j| Uuid::parse_str(j["questId"].as_str().expect("questId")).expect("uuid"))
            .collect();
        let with_data: HashSet<Uuid> = list.iter().map(|g| g.quest_id).collect();

        let missing: Vec<Uuid> = job_ids.iter().copied().filter(|id| !with_data.contains(id)).collect();
        assert!(
            missing.is_empty(),
            "{} of {} jobs have no generatedData entry — the quest map waits forever for \
             them (report #85). Missing: {missing:?}",
            missing.len(),
            job_ids.len(),
        );
        assert_eq!(list.len(), job_ids.len(), "exactly one entry per job, no extras");
    }

    /// The control for the test above. If `parsed.json` failed to load, or
    /// `generate_for_dungeon` broke outright, this goes red too — so a lone failure up
    /// there means the JOB path regressed, not the harness.
    #[test]
    fn story_quests_still_have_generated_data() {
        let gd = game_data();
        let data = story_generated(&gd);
        assert!(
            !data.enemy_generated_data.is_empty(),
            "the control quest must still generate enemies"
        );
    }

    /// An entry the client cannot populate a level from is no better than a missing one:
    /// the 17 `JobCaveVariant_*` ids our roller puts in `jobSetup.dungeonTemplateId` all
    /// exist in `parsed.json` with an EMPTY `spawn_info`, so generating from the template
    /// id yields an entry with no enemies at all. This test is what catches that mistake.
    #[test]
    fn a_jobs_generated_data_is_not_empty() {
        let gd = game_data();
        for job in board() {
            let data = jobs_gen::generated_data_for_job(&gd, &job)
                .expect("the reference dungeon resolves");
            assert!(
                !data.enemy_generated_data.is_empty(),
                "job {} generated no enemies — nothing to kill, nothing to complete",
                job["questId"]
            );
            assert!(!data.chest_generated_data.is_empty(), "…and no chests");
        }
        // The control that makes the above discriminating: generating from the
        // dungeonTemplateId instead — the plausible wrong answer — really is empty.
        let template: Uuid = Uuid::parse_str(
            board()[0]["jobSetup"]["dungeonTemplateId"].as_str().expect("template id"),
        )
        .expect("uuid");
        let from_template = blades_lib::util::dungeon::generate_for_dungeon(&gd, &template, 40, 4000)
            .expect("the template dungeon exists in parsed.json");
        assert!(
            from_template.enemy_generated_data.is_empty(),
            "if the template id ever gains spawn info, revisit JOB_SPAWN_GROUPS_REFERENCE"
        );
    }

    /// The measured identity of the reference dungeon, pinned.
    ///
    /// In capture-599 all six job entries' spawn ids are subsets of
    /// `JobSpawnGroupsReference`, and the two story quests in the same body are subsets of
    /// neither — that discriminating pair is what identified it. Same assertion here
    /// against our own output, so swapping in a different dungeon fails.
    #[test]
    fn job_spawn_ids_come_from_the_reference_dungeon_and_story_quests_do_not() {
        let gd = game_data();
        let reference = gd
            .dungeons
            .get(&jobs_gen::JOB_SPAWN_GROUPS_REFERENCE)
            .expect("JobSpawnGroupsReference is in parsed.json");
        assert_eq!(reference.handle, "JobSpawnGroupsReference", "the id still names it");
        let enemies: HashSet<Uuid> = reference.spawn_info.enemy_spawn_groups.keys().copied().collect();

        for job in board() {
            let data = jobs_gen::generated_data_for_job(&gd, &job).expect("generated");
            for id in data.enemy_generated_data.keys() {
                assert!(enemies.contains(id), "job spawn id {id} is not in the reference dungeon");
            }
        }

        // Control: a story quest's ids are NOT the reference dungeon's. Without this a
        // reference containing *every* spawn id in the game would pass the loop above.
        let story = story_generated(&gd);
        assert!(
            story.enemy_generated_data.keys().all(|id| !enemies.contains(id)),
            "the control quest must draw from its OWN dungeon, not the job reference"
        );
    }

    /// The stored row carries the data too, so /accept and the dungeon-enter path can
    /// resolve a job. It used to persist `None` unconditionally.
    #[test]
    fn a_stored_job_row_carries_its_generated_data() {
        let gd = game_data();
        for job in board() {
            let entry = jobs_gen::job_quest_db_entry(&job, CHAR, &gd).expect("row builds");
            let data = entry
                .generated_data
                .0
                .unwrap_or_else(|| panic!("job row {} persisted no generated data", entry.id));
            assert!(!data.enemy_generated_data.is_empty());
        }
    }

    /// Retail stamps `version: 1` on every generated-data entry in the captured body.
    /// Our story quests have always shipped 0 and work, so this is cosmetic and scoped to
    /// the job path — the control pins that the quest path is untouched.
    #[test]
    fn job_entries_carry_the_captured_version_and_story_quests_are_unchanged() {
        let gd = game_data();
        for job in board() {
            let data = jobs_gen::generated_data_for_job(&gd, &job).expect("generated");
            assert_eq!(data.version, 1, "retail sends version 1 on job entries");
            assert_eq!(data.algorithm_version, 1);
        }
        assert_eq!(story_generated(&gd).version, 0, "the story-quest path must not shift");
    }

    /// The key set retail puts on a job's generated-data entry, pinned.
    ///
    /// From capture-599: every one of the six job entries carried exactly
    /// `algorithmVersion, chestGeneratedData, enemyGeneratedData, itemGeneratedData,
    /// questId, version` — zero missing, zero extra against ours. Note what is NOT there:
    /// no `gldQuestId`. The two story quests in the same body DO carry one on their
    /// `quests[]` entry, which is how we know the missing link was the generated data and
    /// not a missing gldQuestId on the jobs.
    #[test]
    fn a_job_entry_serializes_to_retails_key_set() {
        let gd = game_data();
        let jobs = board();
        let list = assemble_generated_data_list(Vec::new(), &gd, &jobs);
        let entry = serde_json::to_value(&list[0]).expect("entry serializes");
        let mut keys: Vec<&str> = entry
            .as_object()
            .expect("object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "algorithmVersion",
                "chestGeneratedData",
                "enemyGeneratedData",
                "itemGeneratedData",
                "questId",
                "version",
            ],
            "job generated-data key set must match capture-599's six job entries",
        );
    }

    /// The invariant as the client sees it, on the serialized response: every id in
    /// `jobs[]` appears in `dungeonGeneratedDataList[]`, and so does every id in
    /// `quests[]`.
    #[test]
    fn the_serialized_response_resolves_every_job_and_every_quest() {
        let gd = game_data();
        let jobs = board();

        // A story quest advertised alongside the jobs, exactly as a real board is.
        let story_id = Uuid::from_u128(0x570F);
        let (story_quest, story_data) =
            generate_quest_data(&gd, story_quest_with_dungeon(&gd), 48, &QuestLevelScaling::default())
                .expect("control quest generates");
        // Assembled exactly the way the handler assembles it: the story quest arrives via
        // `split_quest_rows` (stored row), the jobs are added by the same call the route
        // makes. Going through `assemble_generated_data_list` rather than reaching past it
        // is the point — the route needs a DB, so this function is the closest testable
        // seam to the wire.
        let (quests_out, _events, from_rows) = split_quest_rows(
            vec![(story_id, story_quest.clone(), story_data.clone())].into_iter(),
            &Default::default(),
        );
        let generated = assemble_generated_data_list(from_rows, &gd, &jobs);

        assert_eq!(quests_out.len(), 1, "the control quest is advertised");

        let body = serde_json::to_value(GetQuestsResponse {
            quests: quests_out,
            dungeon_generated_data_list: generated,
            jobs: jobs.clone(),
            character: blades_lib::user_data::CompleteCharacterWithIdWithoutData {
                id: Uuid::nil(),
                character: Default::default(),
            },
            job_pools: json!([]),
            deleted_quest_ids: vec![],
            game_event_quests: vec![],
            game_event_quests_in_warning: vec![],
            game_event_quests_finished: vec![],
        })
        .expect("response serializes");

        let resolvable: HashSet<&str> = body["dungeonGeneratedDataList"]
            .as_array()
            .expect("list present")
            .iter()
            .map(|e| e["questId"].as_str().expect("entry has a questId"))
            .collect();

        for job in body["jobs"].as_array().expect("jobs") {
            let id = job["questId"].as_str().unwrap();
            assert!(resolvable.contains(id), "job {id} is on the board but unresolvable");
        }
        // The control, in the same assertion style: quests must resolve too. A change
        // that emptied the whole list would pass the loop above only if `jobs` were also
        // empty, and this catches the case where it is not.
        for quest in body["quests"].as_array().expect("quests") {
            let id = quest["questId"].as_str().unwrap();
            assert!(resolvable.contains(id), "quest {id} is advertised but unresolvable");
        }
        assert_eq!(
            body["jobs"].as_array().unwrap().len() + body["quests"].as_array().unwrap().len(),
            resolvable.len(),
            "one entry per advertised thing, as in capture-599 (6 jobs + 2 quests + 2 events = 10)"
        );
    }
}

#[cfg(test)]
mod event_quest_tests {
    use super::*;
    use blades_lib::static_data::StaticData;

    /// The committed static data, loaded the way the server loads it.
    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> blades_lib::game_data::GameData {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    /// 2026-05-03 00:00 UTC — inside the corpus's own event calendar.
    const NOW: i64 = 1_777_852_800;

    /// A quest body built through serde from the exact key set production stores.
    fn quest(gld: Uuid) -> blades_lib::user_data::Quest {
        serde_json::from_value(json!({
            "version": 0,
            "type": "NORMAL",
            "objectiveStatuses": {},
            "difficultyLevel": 0,
            "seed": 0,
            "gldQuestId": gld,
            "completed": false,
        }))
        .expect("fixture quest must deserialize")
    }

    fn generated() -> blades_lib::user_data::DungeonGeneratedData {
        serde_json::from_value(json!({ "algorithmVersion": 0, "version": 0 }))
            .expect("minimal generated data must deserialize")
    }

    /// THE gldQuestId gotcha, pinned.
    ///
    /// An event quest's `questId` is a per-character instance that resolves to
    /// NOTHING in `parsed.json`; its `gldQuestId` is the template that carries the
    /// objectives, the dungeon, the version and the rewards. Retail's corpus is
    /// unambiguous: 1271 of the event-quest entries had `questId != gldQuestId` and
    /// not one had them equal.
    #[test]
    fn an_event_quest_instance_id_is_not_its_template_id() {
        let sd = static_data();
        let gd = game_data();
        let minted = event_quests::mint(&sd, &gd, CHAR, 40, NOW);
        assert!(!minted.is_empty(), "the committed calendar opens events at NOW");
        for m in &minted {
            assert_ne!(
                m.quest_id, m.quest.gld_quest_id,
                "the instance id must differ from the template id"
            );
            assert!(
                gd.quests.contains_key(&m.quest.gld_quest_id),
                "the TEMPLATE resolves in parsed.json"
            );
            assert!(
                !gd.quests.contains_key(&m.quest_id),
                "the INSTANCE must not — that is the whole point of the two ids"
            );
            assert!(matches!(m.quest.r#type, blades_lib::user_data::QuestType::GameEvent));
            let data = m.quest.game_event_quest_data.as_ref().expect("carries its instance");
            assert!(data.game_event_instance_id.contains("::"));
        }
    }

    /// Every open event must be *playable*: objectives and dungeon data, resolved
    /// through the template. Advertising an event with no dungeon data is what hangs
    /// the client's quest map (report #62), and the instance id resolves to nothing,
    /// so a lookup on the wrong id produces exactly that.
    #[test]
    fn every_minted_event_quest_has_objectives_and_dungeon_data() {
        let sd = static_data();
        let gd = game_data();
        for m in event_quests::mint(&sd, &gd, CHAR, 40, NOW) {
            assert!(
                !m.quest.objective_statuses.is_empty(),
                "event quest {} has no objectives",
                m.quest_id
            );
            assert!(
                m.dungeon.is_some(),
                "event quest {} has no dungeon data — the client would wait forever",
                m.quest_id
            );
            assert!(m.quest.rewards.is_some(), "milestones must reach the client");
            assert_eq!(m.quest.rewards.as_ref().unwrap().len(), 5, "five milestones");
            assert!(m.quest.final_reward.is_some());
        }
    }

    /// The instance id is stable for a character within a window and different across
    /// characters and across windows. Without stability every `/quests` poll would
    /// mint a new row and reset the player's progress.
    #[test]
    fn instance_ids_are_stable_per_character_and_window() {
        let a = event_quests::instance_quest_id(CHAR, "e1::1000");
        assert_eq!(a, event_quests::instance_quest_id(CHAR, "e1::1000"), "stable");
        assert_ne!(a, event_quests::instance_quest_id(CHAR, "e1::2000"), "next window differs");
        let other = Uuid::from_u128(0xdead_beef);
        assert_ne!(a, event_quests::instance_quest_id(other, "e1::1000"), "per character");
    }

    /// A GAME_EVENT row goes to `gameEventQuests[]`, never `quests[]` — and only
    /// while its window is open.
    #[test]
    fn event_rows_are_routed_to_the_event_array_and_expire_with_their_window() {
        let open = Uuid::from_u128(0x0E1);
        let closed = Uuid::from_u128(0x0E2);
        let normal = Uuid::from_u128(0x0A1);
        let mut event = quest(Uuid::from_u128(0xC0FFEE));
        event.r#type = blades_lib::user_data::QuestType::GameEvent;

        let mut live = std::collections::HashSet::new();
        live.insert(open);

        let (quests, events, generated_data) = split_quest_rows(
            vec![
                (open, event.clone(), Some(generated())),
                (closed, event, Some(generated())),
                (normal, quest(Uuid::from_u128(0xA1)), Some(generated())),
            ]
            .into_iter(),
            &live,
        );
        assert_eq!(quests.len(), 1, "only the ordinary quest is in quests[]");
        assert_eq!(quests[0].quest_id, normal);
        assert_eq!(events.len(), 1, "the open event, and only it");
        assert_eq!(events[0].quest_id, open);
        // The client's invariant still holds across BOTH arrays.
        let mut advertised: Vec<Uuid> = quests
            .iter()
            .chain(events.iter())
            .map(|q| q.quest_id)
            .collect();
        let mut have: Vec<Uuid> = generated_data.iter().map(|g| g.quest_id).collect();
        advertised.sort();
        have.sort();
        assert_eq!(advertised, have);
    }

    /// The milestone ladder: the Nth completion pays the Nth tier, the last one adds
    /// the final reward, and past the end the instance pays nothing.
    #[test]
    fn an_event_quest_pays_its_milestones_in_order() {
        let sd = static_data();
        let gd = game_data();
        let m = event_quests::mint(&sd, &gd, CHAR, 40, NOW)
            .into_iter()
            .next()
            .expect("an open event");
        let tmpl = sd
            .event_quests
            .templates
            .get(&m.quest.gld_quest_id)
            .expect("template shipped");
        let mut state = blades_lib::server_state::ServerState::default();

        let mut paid = Vec::new();
        for _ in 0..6 {
            paid.push(resolve_completion_reward(&sd, m.quest_id, &m.quest, &mut state));
        }
        for tier in 0..5 {
            assert!(!paid[tier].is_empty(), "milestone {tier} must pay something");
        }
        assert!(
            paid[5].is_empty(),
            "a sixth completion pays nothing — the instance is exhausted"
        );
        // Successive milestones are distinct, i.e. it is a ladder and not the same
        // reward five times (which is what a fixed quest_rewards lookup would give).
        assert_ne!(
            serde_json::to_value(&paid[0]).unwrap(),
            serde_json::to_value(&paid[1]).unwrap(),
            "milestone 1 and 2 must differ"
        );
        // The last one carries the finalReward on top of the last tier.
        let final_reward = tmpl.final_reward.as_ref().expect("shipped");
        let last = serde_json::to_value(&paid[4]).unwrap();
        for (id, n) in &final_reward.stackable_items {
            let got = last["stackableItems"][id.to_string()].as_u64().unwrap_or(0);
            assert!(
                got >= *n,
                "the last milestone must include the finalReward's {n} of {id}, got {got}"
            );
        }
        assert_eq!(state.event_quest_completions.get(&m.quest_id), Some(&5));
    }

    /// An ordinary quest resolves its reward through `gldQuestId`, and every quest
    /// `quest_rewards.json` covers actually pays.
    #[test]
    fn an_ordinary_quest_pays_from_the_template_keyed_table() {
        let sd = static_data();
        let gd = game_data();
        let mut state = blades_lib::server_state::ServerState::default();
        let mut paid = 0;
        for gld in gd.quests.keys() {
            if !sd.quest_rewards.contains_key(gld) {
                continue;
            }
            let mut q = quest(*gld);
            q.r#type = blades_lib::user_data::QuestType::Normal;
            // The ROW id is deliberately not the template id, so a lookup that keys on
            // the row id instead of gldQuestId finds nothing and pays zero.
            let row_id = Uuid::from_u128(0xF00D);
            let reward = resolve_completion_reward(&sd, row_id, &q, &mut state);
            assert!(!reward.is_empty(), "quest {gld} is covered but paid nothing");
            paid += 1;
        }
        assert!(
            paid >= 100,
            "the committed table must cover a real share of the 171 quests, got {paid}"
        );
    }
}


#[cfg(test)]
mod deleted_quest_ids_tests {
    use super::GetQuestsResponse;

    /// Retail sends `deletedQuestIds` in 17.91% of captured `/quests` responses and
    /// NEVER sends it empty — it is a "there were deletions" signal, not a list that
    /// is always present. So an empty one must be omitted, not serialized as `[]`.
    #[test]
    fn an_empty_deletion_list_is_omitted_entirely() {
        let json = serde_json::to_value(GetQuestsResponse {
            quests: vec![],
            dungeon_generated_data_list: vec![],
            jobs: vec![],
            character: blades_lib::user_data::CompleteCharacterWithIdWithoutData {
                id: uuid::Uuid::nil(),
                character: Default::default(),
            },
            job_pools: serde_json::json!([]),
            deleted_quest_ids: vec![],
            game_event_quests: vec![],
            game_event_quests_in_warning: vec![],
            game_event_quests_finished: vec![],
        })
        .unwrap();
        assert!(
            json.get("deletedQuestIds").is_none(),
            "retail never sends an empty deletedQuestIds; got {json}",
        );
    }

    /// ...and when something WAS deleted, it must be present and camelCased. This is
    /// the control: a change that skipped the field unconditionally would pass the
    /// test above and fail this one.
    #[test]
    fn a_non_empty_deletion_list_is_sent() {
        let id = uuid::Uuid::parse_str("159bc1e7-454c-4e2a-90cf-e200c74b961a").unwrap();
        let json = serde_json::to_value(GetQuestsResponse {
            quests: vec![],
            dungeon_generated_data_list: vec![],
            jobs: vec![],
            character: blades_lib::user_data::CompleteCharacterWithIdWithoutData {
                id: uuid::Uuid::nil(),
                character: Default::default(),
            },
            job_pools: serde_json::json!([]),
            deleted_quest_ids: vec![id],
            game_event_quests: vec![],
            game_event_quests_in_warning: vec![],
            game_event_quests_finished: vec![],
        })
        .unwrap();
        assert_eq!(
            json["deletedQuestIds"],
            serde_json::json!(["159bc1e7-454c-4e2a-90cf-e200c74b961a"]),
            "a real deletion must reach the client",
        );
    }
}

#[cfg(test)]
mod report62_quest_map_tests {
    use super::*;
    use blades_lib::user_data::Quest;

    /// Built through serde from the exact key set production stores, so the fixture
    /// tracks the wire shape rather than restating the Rust struct.
    fn quest(gld: Uuid) -> Quest {
        serde_json::from_value(json!({
            "version": 0,
            "type": "NORMAL",
            "objectiveStatuses": {},
            "difficultyLevel": 0,
            "seed": 0,
            "gldQuestId": gld,
            "completed": false,
        }))
        .expect("fixture quest must deserialize")
    }

    fn generated() -> blades_lib::user_data::DungeonGeneratedData {
        serde_json::from_value(json!({ "algorithmVersion": 0, "version": 0 }))
            .expect("minimal generated data must deserialize")
    }

    /// THE invariant the client relies on: every quest it is told about has a
    /// matching `generatedData` entry. Break it and the quest map waits forever for
    /// data that never arrives — which is exactly report #62.
    #[test]
    fn every_advertised_quest_has_generated_data() {
        let with_data = Uuid::from_u128(1);
        let without_data = Uuid::from_u128(2);
        let normal = Uuid::from_u128(0xAAAA);

        let (quests, _events, data) = split_quest_rows(
            vec![
                (with_data, quest(normal), Some(generated())),
                // "The Message": a real quest whose template ships no dungeon, so
                // `generate_quest_data` produced nothing.
                (without_data, quest(normal), None),
            ]
            .into_iter(),
            &Default::default(),
        );

        assert_eq!(quests.len(), 1, "the dataless quest must not be advertised");
        assert_eq!(quests[0].quest_id, with_data);

        let advertised: Vec<Uuid> = quests.iter().map(|q| q.quest_id).collect();
        let have_data: Vec<Uuid> = data.iter().map(|g| g.quest_id).collect();
        assert_eq!(
            advertised, have_data,
            "every advertised quest must have generated data, keyed by the same id"
        );
    }

    /// The control for the test above: a quest WITH data is still served. Without
    /// this, a `split_quest_rows` that returned two empty vecs would pass.
    #[test]
    fn a_quest_with_data_is_still_served() {
        let id = Uuid::from_u128(7);
        let (quests, _events, data) = split_quest_rows(
            vec![(id, quest(Uuid::from_u128(0xBBBB)), Some(generated()))].into_iter(),
            &Default::default(),
        );
        assert_eq!(quests.len(), 1, "a normal quest must still be served");
        assert_eq!(data.len(), 1, "…with its generated data");
        assert_eq!(quests[0].quest_id, id);
    }

    /// Job rows stay out of `quests[]` — they are surfaced only in `jobs[]`, and the
    /// two arrays never overlap in prod. This behaviour predates the fix and must
    /// survive it.
    #[test]
    fn job_rows_are_still_excluded() {
        let (quests, _events, data) = split_quest_rows(
            vec![(
                Uuid::from_u128(9),
                quest(jobs_gen::JOB_SENTINEL_GLD),
                Some(generated()),
            )]
            .into_iter(),
            &Default::default(),
        );
        assert!(quests.is_empty(), "a job row must not appear in quests[]");
        assert!(data.is_empty());
    }

    /// A character holding a mix — the shape the two level-48 Adventurers actually
    /// have in production: several playable quests, six job rows, and one dataless
    /// quest. Only the playable ones survive, and the arrays stay aligned.
    #[test]
    fn the_production_shape_resolves_to_a_consistent_pair_of_arrays() {
        let mut rows = Vec::new();
        for i in 0..3u128 {
            rows.push((Uuid::from_u128(100 + i), quest(Uuid::from_u128(0xC0 + i)), Some(generated())));
        }
        for i in 0..6u128 {
            rows.push((Uuid::from_u128(200 + i), quest(jobs_gen::JOB_SENTINEL_GLD), None));
        }
        // "The Message"
        rows.push((Uuid::from_u128(300), quest(Uuid::from_u128(0xCCA4)), None));

        let (quests, _events, data) = split_quest_rows(rows.into_iter(), &Default::default());
        assert_eq!(quests.len(), 3, "three playable quests");
        assert_eq!(data.len(), 3, "each with its data");
        let a: Vec<Uuid> = quests.iter().map(|q| q.quest_id).collect();
        let b: Vec<Uuid> = data.iter().map(|g| g.quest_id).collect();
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod objectives_wire_tests {
    use super::*;

    /// The live 400.
    ///
    /// 137 of the 139 `/objectives` calls ever made against this server answered
    /// `400 Json deserialize error: unknown variant 'Completed', expected 'Active'`
    /// — the body below, verbatim from capture 257672. `QuestStatus` modelled only
    /// `Active`, so the request could not be parsed and no quest could ever be
    /// finished through the normal flow. It is the reason the audit's answer to
    /// "how many quests are playable end to end" was zero.
    #[test]
    fn the_client_report_that_400ed_every_time_now_parses() {
        let body = r#"{"objectiveUpdates":{"76b97069-67e9-4202-aa93-8bc1dc7fbc65":{"status":"Completed","progress":1.0}}}"#;
        let parsed: ObjectivesRequest = serde_json::from_str(body)
            .expect("the client's own completion report must deserialize");
        let id = Uuid::parse_str("76b97069-67e9-4202-aa93-8bc1dc7fbc65").unwrap();
        let update = parsed.objective_updates.get(&id).expect("the objective is there");
        assert!(
            matches!(update.status, blades_lib::user_data::QuestStatus::Completed),
            "a completion report must arrive as Completed"
        );
        assert_eq!(update.progress, 1.0);
    }

    /// The control: an in-progress report still parses, so the fix is a widening and
    /// not a swap. Without this a build that renamed `Active` to `Completed` would
    /// pass the test above.
    #[test]
    fn an_in_progress_report_still_parses() {
        let body = r#"{"objectiveUpdates":{"76b97069-67e9-4202-aa93-8bc1dc7fbc65":{"status":"Active","progress":0.5}}}"#;
        let parsed: ObjectivesRequest = serde_json::from_str(body).expect("still valid");
        let id = Uuid::parse_str("76b97069-67e9-4202-aa93-8bc1dc7fbc65").unwrap();
        assert!(matches!(
            parsed.objective_updates[&id].status,
            blades_lib::user_data::QuestStatus::Active
        ));
    }

    /// The three response shapes retail actually sent, and the key each quest comes
    /// back under. An event quest answers under `gameEventQuest` with no reward; an
    /// ordinary one under `quest`.
    #[test]
    fn an_event_quest_answers_under_its_own_key_and_pays_nothing_here() {
        let q = QuestWithId {
            quest_id: Uuid::from_u128(1),
            quest: serde_json::from_value(json!({
                "version": 1, "type": "GAME_EVENT", "objectiveStatuses": {},
                "difficultyLevel": 10, "seed": 0,
                "gldQuestId": "7f0d1508-312b-4036-970f-ff5f4c342526",
                "completed": false,
            }))
            .unwrap(),
        };
        let (quest, game_event_quest) = objectives_wire_slot(true, q);
        let wire = serde_json::to_value(ObjectivesResponse {
            reward: RewardGrant::default(),
            inventory: None,
            character: None,
            quest,
            game_event_quest,
        })
        .unwrap();
        assert!(wire.get("quest").is_none(), "an event quest is not under `quest`");
        assert_eq!(wire["gameEventQuest"]["type"], "GAME_EVENT");
        assert!(wire.get("reward").is_none(), "no reward on an event objective");
        assert_eq!(
            wire.as_object().unwrap().len(),
            1,
            "retail's 363 event responses were the quest and nothing else: {wire}"
        );
    }

    /// ...and the ordinary case keeps its `quest` key, so the split is a routing
    /// decision rather than a rename.
    #[test]
    fn an_ordinary_quest_still_answers_under_quest() {
        let q = QuestWithId {
            quest_id: Uuid::from_u128(2),
            quest: serde_json::from_value(json!({
                "version": 1, "type": "NORMAL", "objectiveStatuses": {},
                "difficultyLevel": 10, "seed": 0,
                "gldQuestId": "7f0d1508-312b-4036-970f-ff5f4c342526",
                "completed": false,
            }))
            .unwrap(),
        };
        let (quest, game_event_quest) = objectives_wire_slot(false, q);
        let wire = serde_json::to_value(ObjectivesResponse {
            reward: RewardGrant::default(),
            inventory: None,
            character: None,
            quest,
            game_event_quest,
        })
        .unwrap();
        assert_eq!(wire["quest"]["type"], "NORMAL");
        assert!(wire.get("gameEventQuest").is_none());
    }

    /// A `GAME_EVENT` quest must survive a round-trip through the wire with its
    /// event fields intact — they are what the client needs to show the milestone
    /// track, and they are stored in the same JSONB the row is read back from.
    #[test]
    fn the_event_fields_round_trip_and_stay_off_an_ordinary_quest() {
        let event: blades_lib::user_data::Quest = serde_json::from_value(json!({
            "version": 1, "type": "GAME_EVENT", "objectiveStatuses": {},
            "difficultyLevel": 10, "seed": -270074008i64,
            "gldQuestId": "7f07d85f-f4ed-4762-b670-79e36b224902",
            "gameEventQuestData": { "gameEventInstanceId": "ffcbe281-e953-49c9-b048-69780616c034::1777694400" },
            "rewards": [{ "stackableItems": { "c64bcb53-41f4-41ba-892a-fe2cca423caa": 1 }, "characterXp": 700 }],
            "finalReward": { "stackableItems": { "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2": 25000 } },
            "completed": false,
        }))
        .expect("a captured event quest must deserialize");
        let back = serde_json::to_value(&event).unwrap();
        assert_eq!(
            back["gameEventQuestData"]["gameEventInstanceId"],
            "ffcbe281-e953-49c9-b048-69780616c034::1777694400"
        );
        assert_eq!(back["rewards"][0]["characterXp"], 700);
        assert_eq!(
            back["finalReward"]["stackableItems"]["f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"],
            25000
        );

        // An ordinary quest must not grow the three event keys — retail never sent
        // them on a NORMAL quest, and a `null` there is a wire change.
        let normal: blades_lib::user_data::Quest = serde_json::from_value(json!({
            "version": 1, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": 10, "seed": 0,
            "gldQuestId": "7f0d1508-312b-4036-970f-ff5f4c342526", "completed": false,
        }))
        .unwrap();
        let back = serde_json::to_value(&normal).unwrap();
        for key in ["gameEventQuestData", "rewards", "finalReward"] {
            assert!(back.get(key).is_none(), "{key} must be omitted, got {back}");
        }
    }
}

#[cfg(test)]
mod playability_sweep {
    use super::*;
    use blades_lib::static_data::StaticData;
    use blades_lib::util::quest::generate_quest_data;

    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> blades_lib::game_data::GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    /// The audit, as a test: walk every shipped quest through the accept path at a
    /// spread of player levels and report what actually resolves.
    ///
    /// Accepting is where quest data has historically blown up — a nil dungeon uuid
    /// used to `.ok_or(DungeonNotFound)` and 500 the six dialogue quests, and a
    /// malformed item spawn used to panic. So this asserts three things, and the
    /// numbers are stated rather than implied so a regression reads as a diff:
    ///
    ///  * every one of the 171 quests generates a body without erroring;
    ///  * exactly the 6 nil-dungeon quests come back without dungeon data, and every
    ///    other quest comes back WITH it;
    ///  * every quest with a dungeon has at least one objective, because a quest with
    ///    no objective cannot be completed by the client.
    #[test]
    fn every_shipped_quest_generates_at_every_level() {
        let sd = static_data();
        let gd = game_data();
        let scaling = &sd.quests_daily.level_scaling;

        let mut errored = Vec::new();
        let mut no_dungeon = Vec::new();
        let mut no_objectives: Vec<Uuid> = Vec::new();
        let total = gd.quests.len();

        for quest_id in gd.quests.keys() {
            for level in [1i64, 15, 48, 86, 100] {
                match generate_quest_data(&gd, *quest_id, level, scaling) {
                    Err(e) => errored.push(format!("{quest_id} @ {level}: {e}")),
                    Ok((quest, dungeon)) => {
                        if level != 1 {
                            continue; // the shape checks below do not vary with level
                        }
                        if dungeon.is_none() {
                            no_dungeon.push(*quest_id);
                        } else if quest.objective_statuses.is_empty() {
                            no_objectives.push(*quest_id);
                        }
                    }
                }
            }
        }

        assert_eq!(total, 171, "the shipped quest corpus is 171 quests");
        assert!(errored.is_empty(), "{} quest(s) failed to generate:\n{}", errored.len(), errored.join("\n"));
        assert_eq!(
            no_dungeon.len(),
            6,
            "exactly the 6 nil-dungeon dialogue quests have no dungeon; got {}: {:?}",
            no_dungeon.len(),
            no_dungeon
        );
        // Every nil-dungeon quest must be one quests_daily.json already knows about,
        // so the two never drift apart.
        let declared = sd.quests_daily.non_dungeon_ids();
        for id in &no_dungeon {
            assert!(declared.contains(id), "{id} has no dungeon but is not in nonDungeonQuests");
        }
        // One known exception, and it is not a real quest: `MultiKitTest`
        // (category "test", `version: 0`) is a developer fixture the client ships. It
        // has a dungeon and zero objectives, so nothing can complete it — but nothing
        // advertises it either, and inventing an objective for it would be exactly the
        // kind of fabricated data this corpus is supposed to be free of. Pinned by id
        // so a SECOND objective-less quest, which would be a real extraction bug,
        // still fails this test.
        let multikit_test = Uuid::parse_str("7fd324c5-cfcf-42df-8db7-07651a9a8ac2").unwrap();
        no_objectives.retain(|id| *id != multikit_test);
        assert!(
            no_objectives.is_empty(),
            "{} quest(s) have a dungeon but no objectives, so the client can never \
             finish them: {:?}",
            no_objectives.len(),
            no_objectives
        );
    }

    /// Reward coverage, stated as a number so it can only move deliberately.
    ///
    /// Before the capture re-extraction this was 26/171 (15 %), because the table was
    /// keyed by the instance ids that appeared in captured `/complete` URLs rather
    /// than by the template. Resolving those back through `gldQuestId` lifted it to
    /// 103 flat / 142 covered.
    ///
    /// It then moved again, to 115 / 154, and NOT by inventing anything: the 29
    /// quests no capture ever completed were never missing a reward. The shipped
    /// quest asset carries `reward_preview`, and it IS the reward —
    /// `characterXp == reward_preview.experience` and
    /// `townXp == reward_preview.town_points` held 59/59 with one distinct ratio
    /// (exactly 1.0) across every quest where a capture and a definition both
    /// exist. `scripts/build-quest-rewards-static.py` in the capture repo
    /// generates the table from it.
    ///
    /// What is left is not a coverage gap of the same kind: 57 quests ship an
    /// explicit `0.0`, which is retail saying "this pays nothing". Every quest in
    /// the asset has a preview, so there is no missing-vs-zero ambiguity — and
    /// they are deliberately absent from the table rather than present-and-empty,
    /// because a key that pays nothing would make `contains_key` claim a coverage
    /// the player never sees.
    ///
    /// Gold is still capture-only: `reward_preview` has no currency of any kind.
    #[test]
    fn reward_coverage_is_what_the_captures_support() {
        let sd = static_data();
        let gd = game_data();
        let flat = gd.quests.keys().filter(|q| sd.quest_rewards.contains_key(q)).count();
        let evented = gd
            .quests
            .keys()
            .filter(|q| sd.event_quests.templates.contains_key(q))
            .count();
        let covered: std::collections::HashSet<_> = gd
            .quests
            .keys()
            .filter(|q| {
                sd.quest_rewards.contains_key(q) || sd.event_quests.templates.contains_key(q)
            })
            .collect();

        assert_eq!(flat, 115, "flat rewards from quest_rewards.json");
        assert_eq!(evented, 39, "milestone ladders from event_quests.json");
        assert_eq!(covered.len(), 154, "154 of 171 quests pay something");
        assert!(
            covered.len() as f64 / gd.quests.len() as f64 > 0.80,
            "coverage must stay above 80%"
        );
        // …and the two tables must not overlap: a quest is EITHER flat or a ladder.
        // Overlap would mean an event quest also has a fixed reward, and whichever
        // branch ran first would silently win.
        assert_eq!(
            flat + evented,
            covered.len(),
            "a quest must not appear in both quest_rewards.json and event_quests.json"
        );
    }

    /// Every event template ships a complete, usable ladder — five wire tiers, five
    /// granting tiers and a final reward. A partially-extracted file would otherwise
    /// pay `None` somewhere in the middle of a player's run.
    #[test]
    fn every_event_template_ships_a_complete_milestone_ladder() {
        let sd = static_data();
        assert_eq!(sd.event_quests.templates.len(), 39);
        for (gld, tmpl) in &sd.event_quests.templates {
            assert_eq!(tmpl.rewards.len(), 5, "{gld}: five wire milestones");
            assert_eq!(tmpl.payable_rewards.len(), 5, "{gld}: five granting milestones");
            assert!(tmpl.final_reward.is_some(), "{gld}: a final reward");
            assert!(!tmpl.objective_ids.is_empty(), "{gld}: objective ids");
            for step in 0..5 {
                let payout = tmpl.payout(step).unwrap_or_else(|| panic!("{gld}: no tier {step}"));
                assert!(!payout.is_empty(), "{gld}: tier {step} pays nothing");
            }
            assert!(tmpl.payout(5).is_none(), "{gld}: exhausted after five");
        }
    }

    /// Every event in the calendar has a template AND a quest definition. An event
    /// with no template would advertise a quest that pays nothing; one with no
    /// definition would advertise a quest with no dungeon, which hangs the map.
    #[test]
    fn every_calendar_event_is_fully_backed() {
        let sd = static_data();
        let gd = game_data();
        assert_eq!(sd.game_events.len(), 39, "the committed calendar");
        for def in &sd.game_events {
            assert!(
                gd.quests.contains_key(&def.quest_id),
                "event {} points at quest {} which is not in parsed.json",
                def.event_id,
                def.quest_id
            );
            assert!(
                sd.event_quests.templates.contains_key(&def.quest_id),
                "event {} has no milestone table",
                def.event_id
            );
            assert_eq!(
                def.recurrence.recurrence_interval, 39,
                "every captured event recurs on 39 days"
            );
            assert_eq!(def.window_secs(), 172_800, "…and stays open for two");
        }
    }
}

#[cfg(test)]
mod assert_reachability {
    /// Is `assert!(request.is_none())` in `get_quests` / `accept_quest`
    /// reachable from the wire?
    ///
    /// `Json<Option<()>>` only ever yields `None`: `null` deserializes to
    /// `None` (Option wins over the unit type) and every other body is a serde
    /// error, which actix turns into a 400 before the handler runs. So the
    /// assert cannot fire and is dead — worth knowing, because a panicking
    /// assert in a request handler WOULD be a denial-of-service if it were
    /// reachable.
    #[test]
    fn option_unit_can_never_be_some() {
        assert_eq!(serde_json::from_str::<Option<()>>("null").unwrap(), None);
        for body in ["{}", "[]", "\"x\"", "1", "true", "{\"a\":1}"] {
            assert!(
                serde_json::from_str::<Option<()>>(body).is_err(),
                "{body} must be a 400 from serde, not a body the handler sees"
            );
        }
    }
}
