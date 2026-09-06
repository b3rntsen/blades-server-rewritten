use std::{collections::{HashMap, HashSet}, sync::Arc};
use actix_web::{
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::game_data::GameData;
use blades_lib::util::dungeon::generate_for_dungeon;
use blades_lib::user_data::{
    B64EncodedData, CompleteCharacterWithIdWithoutData, DungeonGeneratedData, DungeonState,
    DungeonStatus, InventoryChangeTracker, Quest,
};
use diesel;
use diesel::{
    prelude::*,
    associations::HasTable,
    BoolExpressionMethods, ExpressionMethods, QueryDsl, SelectableHelper,
};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt, AsyncPgConnection};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use crate::{
    event_quests::{EventCompletion, apply_event_rewards},
    BladeApiError, ServerGlobal,
    json_db::JsonDbWrapper,
    models::{QuestDbEntry, QuestDbEntryDungeonStateAndInitialState},
    quest::jobs_gen,
    session::{Session, SessionLookedUpMaybe},
    util::check_permission_for_character_and_get_it,
};
use rand;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DungeonResponseEntry {
    quest_id: Uuid,
    initial_state: B64EncodedData,
    status: DungeonStatus,
    remaining_entries: Option<i32>,
    max_entries: Option<i32>,
}

#[derive(Serialize)]
pub struct DungeonResponse {
    dungeons: Vec<DungeonResponseEntry>,
}

#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = crate::schema::event_dungeons)]
pub struct EventDungeonEntryInfo {
    pub id: Uuid,
    pub dungeon_state: Option<serde_json::Value>,
    pub initial_state: Option<serde_json::Value>,
    pub entry_count: i32,
    pub expires_at: Option<chrono::NaiveDateTime>,
    pub max_entries: i32,
}

#[derive(Debug, Clone, Insertable, Queryable, Selectable)]
#[diesel(table_name = crate::schema::event_dungeons)]
pub struct EventDungeonDbEntry {
    pub id: Uuid,
    pub character_id: Uuid,
    pub event_id: Uuid,
    pub dungeon_id: Uuid,
    pub dungeon_state: Option<serde_json::Value>,
    pub initial_state: Option<serde_json::Value>,
    pub generated_data: serde_json::Value,
    pub entered_at: chrono::NaiveDateTime,
    pub expires_at: Option<chrono::NaiveDateTime>,
    pub entry_count: i32,
    pub max_entries: i32,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/dungeons")]
pub async fn get_dungeons(
    path: web::Path<Uuid>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<DungeonResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id_normal = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    let _ = check_permission_for_character_and_get_it(&mut *conn, &session.session, character_id_normal)
        .await?;

    // Get quest dungeons
    let ongoing_quest_query = {
        use crate::schema::quests::dsl::*;
        quests::table()
            .filter(
                character_id
                    .eq(character_id_normal)
                    .and(dungeon_state.is_not_null())
                    .and(initial_state.is_not_null()),
            )
            .select(QuestDbEntryDungeonStateAndInitialState::as_select())
            .load(&mut conn)
            .await?
    };

    let mut dungeons: Vec<DungeonResponseEntry> = ongoing_quest_query
        .into_iter()
        .map(|entry| DungeonResponseEntry {
            quest_id: entry.id,
            initial_state: entry.initial_state.unwrap().0,
            status: entry.dungeon_state.unwrap().0.dungeon_status,
            // Regular quests don't have entry limits, so set to None
            remaining_entries: None,
            max_entries: None,
        })
        .collect();

    // Also get event dungeons
    let event_dungeons_query = {
        use crate::schema::event_dungeons::dsl::*;
        event_dungeons::table()
            .filter(
                character_id
                    .eq(character_id_normal)
                    .and(dungeon_state.is_not_null())
                    .and(initial_state.is_not_null()),
            )
            .select(EventDungeonEntryInfo::as_select())
            .load(&mut conn)
            .await?
    };

    // Convert event dungeons to the same response format
    for entry in event_dungeons_query {
        if let (Some(state_value), Some(initial_value)) = (entry.dungeon_state, entry.initial_state) {
            let state: DungeonState = serde_json::from_value(state_value)
                .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;
            let initial: B64EncodedData = serde_json::from_value(initial_value)
                .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;
            dungeons.push(DungeonResponseEntry {
                quest_id: entry.id, // This is the event dungeon ID
                initial_state: initial,
                status: state.dungeon_status,
                remaining_entries: Some(entry.max_entries - entry.entry_count),
                max_entries: Some(entry.max_entries),
            });
        }
    }

    Ok(Json(DungeonResponse { dungeons }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnterDungeonRequest {
    dungeon_instance: Option<B64EncodedData>,
    current_state: B64EncodedData,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnterDungeonResponse {
    dungeon_status: DungeonStatus,
}

/// `{"character": …}` — the exact envelope retail's exit returns.
///
/// NOT `CompleteCharacterWithIdAndData`: retail's body carries the character's own
/// fields and `id`, and NO `data` key. Verified against the smallest captured
/// response (982 B), which ends `…"nameValidated":true}}` with nothing after the
/// character object.
#[derive(Serialize)]
struct ExitDungeonResponse {
    character: CompleteCharacterWithIdWithoutData,
}

/// Leave the current quest dungeon.
///
/// Reported as tracker #83. We served `dungeons/current/enter` and
/// `dungeons/current/update` but not `exit`, so the client got a 404 where retail
/// answered — 592 retail 200s exist for this route in the capture DB (982 B to
/// 60 KB) against our own 404s.
///
/// Retail's response is the character with **`currentQuestDungeon: null`** — exit
/// clears the active dungeon and hands the updated character back, which is what
/// lets the client leave the dungeon UI. So both halves of the state have to go in
/// one transaction: the character's `current_quest_dungeon`, and the quest row's
/// `dungeon_state`. Clearing one without the other strands the player — the client
/// would think it had left while the server still held a live dungeon, or the
/// reverse.
///
/// Idempotent on purpose: exiting a dungeon that is already gone returns the
/// character rather than erroring. The client retries this on a dropped connection,
/// and a second 4xx would strand the very player the retry is meant to rescue.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/dungeons/current/exit"
)]
pub async fn exit_quest_dungeon(
    path: web::Path<(Uuid, Uuid)>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<ExitDungeonResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (char_id, quest_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await?;

    check_permission_for_character_and_get_it(&mut conn, &session.session, char_id).await?;

    // Load as tuple and construct manually:
    let row_info: JsonDbWrapper<serde_json::Value> = match crate::schema::quests::table
        .filter(crate::schema::quests::id.eq(quest_id))
        .filter(crate::schema::quests::character_id.eq(char_id))
        .select(crate::schema::quests::info)
        .first(&mut *conn)
        .await
    {
        Ok(info) => info,
        Err(_) => return Err(BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2)),
    };

    // Extract gldQuestId from the quest's info field
    let gld_quest_id: Uuid = row_info.0["gldQuestId"]
    .as_str()
    .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?
    .parse()
    .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;

    // Now check if this is an event quest by looking up the template
    let is_event = app_state.event_quests.templates.contains_key(&gld_quest_id);

    if is_event {
        // Clear event dungeon state instead
        return handle_event_dungeon_exit(&mut conn, char_id, gld_quest_id, &app_state).await;
    }

    conn.transaction(move |mut conn| {
        async move {
            // 1. Clear the quest's dungeon state, if it still holds one.
            {
                use crate::schema::quests::dsl::*;
                // BOTH halves of the primary key. `quests.id` alone is NOT unique:
                // an ordinary story quest is stored under the template id, so every
                // character on that quest has a row with the same `id`, and exiting
                // filtered on `id` would clear every one of those players' dungeons.
                let owner = character_id;
                diesel::update(quests.filter(id.eq(&quest_id).and(character_id.eq(&owner))))
                    .set(dungeon_state.eq(None::<serde_json::Value>))
                    .execute(&mut conn)
                    .await?;
            }

            // 2. Clear the character's pointer to it and read the row back, so the
            //    response is the state we just committed rather than a copy made
            //    before the write.
            let updated = {
                use crate::schema::characters::dsl::*;
                // Only the `character` column: this handler touches one field of
                // it, and selecting a wider model would pull the whole save for no
                // reason. `for_update` so a concurrent write cannot interleave
                // between the read and the clear.
                let mut current: JsonDbWrapper<blades_lib::user_data::CompleteCharacter> =
                    characters
                        .filter(id.eq(char_id))
                        .select(character)
                        .for_update()
                        .load(&mut conn)
                        .await?
                        .into_iter()
                        .next()
                        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 3))?;

                current.0.current_quest_dungeon = serde_json::Value::Null;

                diesel::update(characters)
                    .filter(id.eq(char_id))
                    .set(character.eq(&current))
                    .execute(&mut conn)
                    .await?;
                current
            };

            Ok::<_, BladeApiError>(Json(ExitDungeonResponse {
                character: CompleteCharacterWithIdWithoutData {
                    id: char_id,
                    character: updated.0,
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

/// Which dungeon the client should load for a run of `quest_id`.
///
/// **The URL carries an INSTANCE id, not a template id.** Only ordinary story
/// quests happen to reuse the template id as the instance id; anything minted per
/// character does not. An event quest's `questId` resolves to nothing in
/// `parsed.json` — its template is the row's `gldQuestId` (in the retail corpus
/// 1271 event rows had `questId != gldQuestId` and not one had them equal, pinned
/// by `an_event_quest_instance_id_is_not_its_template_id` in `quest.rs`). Looking
/// the template up by the URL id therefore missed for *every* event quest, so the
/// door to every event was a 404: tracker #90.
///
/// Three cases, tried in this order:
///
/// 1. the URL id IS a template — ordinary quests, unchanged;
/// 2. otherwise the stored row's `gldQuestId` is the template — event quests;
/// 3. a town JOB carries the sentinel `gldQuestId`, which resolves to no template
///    *by design*. Its dungeon is not missing, it is simply not reached through
///    `game_data.quests`: every job is rolled from the one shared reference
///    dungeon ([`jobs_gen::JOB_SPAWN_GROUPS_REFERENCE`]), which is also what the
///    job row's own `dungeonGeneratedData` was generated from, so handing the
///    client that id makes the two describe the same dungeon by construction.
///    Serving a different id is the Abyss failure documented in
///    `blades_lib::util::dungeon`: nothing resolves, no enemy spawns, the run
///    cannot progress.
///
/// `row` is `None` when no quest row exists; resolution still runs in that case so
/// that a wholly unknown `quest_id` keeps producing the 404 it always has instead
/// of turning into the "no such row" 400 further down.
fn resolve_dungeon_settings_id(
    game_data: &GameData,
    quest_id: Uuid,
    row: Option<&Quest>,
) -> Result<Uuid, BladeApiError> {
    // A town job is not a template lookup at all.
    if row.is_some_and(jobs_gen::is_job_row) {
        return if game_data
            .dungeons
            .contains_key(&jobs_gen::JOB_SPAWN_GROUPS_REFERENCE)
        {
            Ok(jobs_gen::JOB_SPAWN_GROUPS_REFERENCE)
        } else {
            // A parsed.json without the reference dungeon also gives the job a `None`
            // generatedData, so there is genuinely nothing to enter. 404 beats a
            // dungeon id the client cannot resolve.
            Err(BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))
        };
    }

    let template = game_data
        .quests
        .get(&quest_id)
        .or_else(|| game_data.quests.get(&row?.gld_quest_id));

    let Some(template) = template else {
        // Genuinely unknown: neither the instance id nor the row's template id is
        // anything we ship. Same 404 as before.
        return Err(BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2));
    };

    match template.dungeon_info.as_ref() {
        Some(info) => Ok(info.dungeon_uuid),
        // A dialogue-only quest has no dungeon to enter.
        None => Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2)),
    }
}

async fn handle_event_dungeon_exit(
    conn: &mut AsyncPgConnection,
    char_id: Uuid,
    quest_id: Uuid,
    app_state: &ServerGlobal,	
) -> Result<Json<ExitDungeonResponse>, BladeApiError> {
    use crate::schema::event_dungeons::dsl::*;

    let event_template = app_state.event_quests.templates.get(&quest_id)
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?;

    // Get character data
    let mut character_data = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(id.eq(char_id))
            .select(crate::models::CharacterDbEntryCharacterWalletInventory::as_select())
            .for_update()
            .first::<crate::models::CharacterDbEntryCharacterWalletInventory>(conn)
            .await?
    };

    let mut completion = EventCompletion::get_or_create(conn, char_id, quest_id).await?;
    let completion_index = completion.completion_count as usize;
    let tier_count = event_template.rewards.len();

    // The reward model, verbatim from the data's own _meta (capture-derived and
    // checked against 93 retail instances / 300+ completions):
    //
    //   "The Nth completion of an event-quest instance pays rewards[N];
    //    the last one pays rewards[last] + finalReward."
    //
    // So finalReward is a BONUS ON the final tier, not a tier of its own, and
    // there is nothing to pay once the tiers are exhausted. Paying finalReward
    // by itself on every later exit made the event farmable without limit
    // (tracker #98: "I can do it over and over").
    let payout = event_template.payout_for_completion(completion_index);
    if payout.is_none() {
        // Event finished. Deliberately NOT an error: the player still has to be
        // able to walk out of the dungeon. They just leave with nothing.
        log::info!(
            "event exit: character {} has completed all {} tiers of event quest {} - no reward",
            char_id,
            tier_count,
            quest_id
        );
    }

    let mut wallet = std::mem::take(&mut character_data.wallet.0);
    let mut inventory_modification_tracker = InventoryChangeTracker::default();

    if let Some(rewards) = payout.as_ref() {
        apply_event_rewards(
            rewards,
            &mut character_data,
            &mut wallet,
            &mut inventory_modification_tracker,
        )?;
    }

    character_data.wallet.0 = wallet;

    // Advance only when a tier was actually consumed. Incrementing on a finished
    // event would run the counter up forever and desync the completedQuests
    // mirror written just below from the number of tiers that exist.
    if payout.is_some() {
        completion.increment_completion(conn).await?;
    }

    // Mirror complete_quest's write into the character's completedQuests JSON
    // (`{ "<gldQuestId>": <completion_count> }`) — this is what the client's
    // quest-list checkboxes actually read. increment_completion only updates the
    // separate event_completions table, which gates rewards/tiers server-side but
    // is invisible to that UI, so without this the tier pays out correctly but
    // never shows as completed.
    if !character_data.character.0.completed_quests.is_object() {
        character_data.character.0.completed_quests = json!({});
    }
    character_data
        .character
        .0
        .completed_quests
        .as_object_mut()
        .unwrap()
        .insert(
            quest_id.to_string(),
            json!(completion.completion_count),
    );

    // Save the character data:
    {
        use crate::schema::characters;
        diesel::update(characters::table)
            .filter(characters::id.eq(char_id))
            .set(character_data)
            .execute(conn)
            .await?;
    }

    // Clear the event dungeon state
    diesel::update(event_dungeons)
        .filter(character_id.eq(char_id))
        .filter(dungeon_id.eq(quest_id))
        .set(dungeon_state.eq(None::<serde_json::Value>))
        .execute(conn)
        .await?;
    
    // Read the character back
    let updated = {
        use crate::schema::characters::dsl::*;
        let mut current: JsonDbWrapper<blades_lib::user_data::CompleteCharacter> = 
        characters
            .filter(id.eq(char_id))
            .select(character)
            .for_update()
            .load(conn)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 3))?;
        
        current.0.current_quest_dungeon = serde_json::Value::Null;

        diesel::update(characters)
            .filter(id.eq(char_id))
            .set(character.eq(&current))
            .execute(conn)
            .await?;
        current
    };
    
    Ok(Json(ExitDungeonResponse {
        character: CompleteCharacterWithIdWithoutData {
            id: char_id,
            character: updated.0,
        },
    }))
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/dungeons/current/enter"
)]
pub async fn enter_quest_dungeon(
    path: web::Path<(Uuid, Uuid)>,
    body: Json<EnterDungeonRequest>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<EnterDungeonResponse>, BladeApiError> {
    let session_lookup = session.get_session_or_error()?;
    let validated_session = &session_lookup.session; 
    let body = body.0;
    let (character_id_normal, quest_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    // Route event quests to their own handler before the ordinary path. An event
    // quest's URL id is an INSTANCE id; its template is the row's `gldQuestId`,
    // which is also the key `event_quests.json` is indexed by.
    let row_info: JsonDbWrapper<serde_json::Value> = match crate::schema::quests::table
        .filter(crate::schema::quests::id.eq(quest_id))
        .filter(crate::schema::quests::character_id.eq(character_id_normal))
        .select(crate::schema::quests::info)
        .first(&mut *conn)
        .await
    {
        Ok(info) => info,
        Err(_) => return Err(BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2)),
    };

    let gld_quest_id: Uuid = row_info.0["gldQuestId"]
        .as_str()
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?
        .parse()
        .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;

    if app_state.event_quests.templates.contains_key(&gld_quest_id) {
        return handle_event_dungeon_entry(
            &mut conn,
            &app_state,
            character_id_normal,
            gld_quest_id,
            body,
            validated_session,
        )
        .await;
    }

    let _ =
        check_permission_for_character_and_get_it(&mut conn, validated_session, character_id_normal)
            .await?;

    conn.transaction(move |mut conn| {
        async move {
            let quest_query = {
                use crate::schema::quests::dsl::*;

                quests::table()
                    // Both halves of the primary key. Filtering on `id` alone would
                    // lock — and, below, WRITE — another character's row, and this
                    // handler now reads that row's `gldQuestId` to pick the dungeon.
                    .filter(id.eq(&quest_id).and(character_id.eq(&character_id_normal)))
                    .select(QuestDbEntry::as_select())
                    .for_update()
                    .load(&mut conn)
                    .await?
            };

            let quest_row = quest_query.into_iter().next();

            // Before the row-existence check on purpose — see the doc comment.
            let dungeon_settings_id = resolve_dungeon_settings_id(
                &globals.game_data,
                quest_id,
                quest_row.as_ref().map(|q| &q.info.0),
            )?;

            let quest = match quest_row {
                Some(v) => v,
                None => return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20002, 2)),
            };

            if let Some(dungeon_instance) = body.dungeon_instance {
                // first time entering
                if quest.dungeon_state.is_some() {
                    return Err(BladeApiError::new(StatusCode::CONFLICT, 20003, 1));
                }
                let status = DungeonStatus {
                    dungeon_settings_ids: vec![dungeon_settings_id],
                    revive_count: 0,
                    algorithm_version: 1,
                    current_state: body.current_state,
                    enemy_status: HashMap::default(),
                    seed: 54321,
                    level: 1,
                    version: 1, //TODO: figure out where this version come from.
                };

                {
                    use crate::schema::quests::dsl::*;

                    diesel::update(quests)
                        .filter(id.eq(quest_id).and(character_id.eq(character_id_normal)))
                        .set((
                            dungeon_state.eq(Some(JsonDbWrapper(DungeonState {
                                dungeon_status: status.clone(),
                            }))),
                            initial_state.eq(Some(JsonDbWrapper(dungeon_instance.clone()))),
                        ))
                        .execute(&mut conn)
                        .await
                        .unwrap();
                }

                Ok(Json(EnterDungeonResponse {
                    dungeon_status: status,
                }))
            } else {
                // we are re-entering the dungeon. Just save the progress
                let mut dungeon_state_actual = if let Some(dungeon_state) = quest.dungeon_state {
                    dungeon_state.0
                } else {
                    return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20004, 2));
                };
                dungeon_state_actual.dungeon_status.current_state = body.current_state;
                {
                    use crate::schema::quests::dsl::*;

                    diesel::update(quests)
                        .filter(id.eq(quest_id).and(character_id.eq(character_id_normal)))
                        .set(dungeon_state.eq(Some(JsonDbWrapper(dungeon_state_actual.clone()))))
                        .execute(&mut conn)
                        .await?;
                };
                Ok(Json(EnterDungeonResponse {
                    dungeon_status: dungeon_state_actual.dungeon_status,
                }))
            }
        }
        .scope_boxed()
    })
    .await
}

/// Tracker #90: the enter-dungeon door resolved the quest TEMPLATE by the URL's
/// INSTANCE id, so every event quest 404'd.
///
/// These run against the committed `deploy/static` corpus — the same data the
/// server loads — and against real minted event quests, not hand-written ids.
#[cfg(test)]
mod dungeon_settings_resolution {
    use super::*;
    use actix_web::ResponseError;
    use blades_lib::static_data::StaticData;
    use serde_json::json;

    /// The committed static data, loaded the way the server loads it.
    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    /// 2026-05-03 00:00 UTC — inside the corpus's own event calendar.
    const NOW: i64 = 1_777_852_800;

    /// A quest body built through serde from the exact key set production stores.
    fn quest_row(gld: Uuid) -> Quest {
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

    /// `(status, rendered envelope)` of a rejection, so a test can pin WHICH error
    /// the client receives and not merely that it failed.
    fn envelope(e: &BladeApiError) -> (u16, String) {
        (e.status_code().as_u16(), e.to_string())
    }

    // ---------------------------------------------------------------- the defect

    /// THE reported bug. An event quest's instance id is not a template id, so the
    /// old `game_data.quests.get(&quest_id)` missed and the handler 404'd before it
    /// ever looked at the row. Resolution must go through the row's `gldQuestId`
    /// and land on the template's own dungeon.
    #[test]
    fn an_event_quest_instance_resolves_to_its_templates_dungeon() {
        let (sd, gd) = (static_data(), game_data());
        let minted = crate::quest::event_quests::mint(&sd, &gd, CHAR, 40, NOW);
        assert!(
            !minted.is_empty(),
            "the committed calendar opens events at NOW"
        );

        for m in &minted {
            // The precondition, restated here so a corpus change cannot quietly
            // turn this test into a tautology.
            assert!(
                !gd.quests.contains_key(&m.quest_id),
                "instance id must NOT be a template — otherwise this proves nothing"
            );

            let got = resolve_dungeon_settings_id(&gd, m.quest_id, Some(&m.quest))
                .unwrap_or_else(|e| panic!("event quest 404'd at the door: {e}"));

            let expected = gd
                .quests
                .get(&m.quest.gld_quest_id)
                .expect("template resolves")
                .dungeon_info
                .as_ref()
                .expect("an event template ships a dungeon")
                .dungeon_uuid;
            assert_eq!(got, expected, "must be the TEMPLATE's dungeon");
            assert!(
                gd.dungeons.contains_key(&got),
                "and it must be a dungeon the client can actually load"
            );
        }
    }

    // ---------------------------------------------------------------- the control

    /// Without this the fix could be "never 404", which is worse than the bug: the
    /// client would be walked into a dungeon that does not exist.
    #[test]
    fn an_unknown_quest_id_is_still_a_404() {
        let gd = game_data();
        let bogus = Uuid::from_u128(0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF_DEAD_BEEF);
        assert!(!gd.quests.contains_key(&bogus));

        // No row at all — a quest id off the street.
        let (status, msg) =
            envelope(&resolve_dungeon_settings_id(&gd, bogus, None).expect_err("must be rejected"));
        assert_eq!(status, 404);
        assert!(msg.contains("service_id: 20000"), "{msg}");
        assert!(msg.contains("error_code: 2"), "{msg}");

        // A row exists, but its gldQuestId is nothing we ship either. The fallback
        // must not become a way in.
        let row = quest_row(Uuid::from_u128(0xBAD_1DEA));
        let (status, msg) = envelope(
            &resolve_dungeon_settings_id(&gd, bogus, Some(&row)).expect_err("must be rejected"),
        );
        assert_eq!(status, 404);
        assert!(msg.contains("service_id: 20000"), "{msg}");
    }

    /// The other half of the control: an ordinary story quest, whose instance id IS
    /// its template id, must keep resolving exactly as it did.
    #[test]
    fn an_ordinary_quest_still_resolves_by_its_own_id() {
        let gd = game_data();
        let (template_id, template) = gd
            .quests
            .iter()
            .find(|(_, q)| {
                q.dungeon_info.as_ref().is_some_and(|d| {
                    !d.dungeon_uuid.is_nil() && gd.dungeons.contains_key(&d.dungeon_uuid)
                })
            })
            .expect("the corpus ships dungeon quests");
        let expected = template.dungeon_info.as_ref().unwrap().dungeon_uuid;

        // Story rows store gldQuestId == questId; pass a row that says so, and also
        // the no-row case, since both reach this handler.
        let row = quest_row(*template_id);
        assert_eq!(
            resolve_dungeon_settings_id(&gd, *template_id, Some(&row)).unwrap(),
            expected
        );
        assert_eq!(
            resolve_dungeon_settings_id(&gd, *template_id, None).unwrap(),
            expected
        );
    }

    /// Dialogue-only quests: the corpus expresses "no dungeon" as a NIL
    /// `dungeon_uuid`, not as an absent `dungeon_info` (the 400 branch above is
    /// defensive and unreachable against the shipped `parsed.json` — asserted
    /// below so it stays that way honestly). What matters is that they resolve to
    /// exactly the same nil id they always did: this handler must not start
    /// inventing a dungeon for them, and it must not start 404ing them either.
    #[test]
    fn a_dialogue_only_quest_still_resolves_to_its_nil_dungeon() {
        let gd = game_data();
        assert!(
            gd.quests.values().all(|q| q.dungeon_info.is_some()),
            "every shipped template has dungeon_info; the None branch is defensive"
        );
        let nil_quests: Vec<Uuid> = gd
            .quests
            .iter()
            .filter(|(_, q)| {
                q.dungeon_info
                    .as_ref()
                    .is_some_and(|d| d.dungeon_uuid.is_nil())
            })
            .map(|(id, _)| *id)
            .collect();
        assert!(
            !nil_quests.is_empty(),
            "the corpus ships the dialogue-only quests"
        );
        for id in nil_quests {
            assert!(
                resolve_dungeon_settings_id(&gd, id, None)
                    .expect("resolves, as before")
                    .is_nil(),
                "unchanged: a nil-dungeon quest resolves to the nil id"
            );
        }
    }

    // ---------------------------------------------------------------- town jobs

    /// The decision on jobs, pinned.
    ///
    /// A job's `gldQuestId` is a sentinel that resolves to no template, so the
    /// template path can never serve it. But its dungeon is not missing: every job
    /// is rolled from one shared reference dungeon, and that is the id the client
    /// needs. Jobs therefore enter — they do not 404.
    #[test]
    fn a_town_job_enters_the_shared_reference_dungeon() {
        let gd = game_data();
        let job_row = quest_row(jobs_gen::JOB_SENTINEL_GLD);
        // Job instance ids are rolled, so they are never templates either.
        let job_instance = Uuid::from_u128(0x0B0B_0001);
        assert!(!gd.quests.contains_key(&job_instance));

        let got = resolve_dungeon_settings_id(&gd, job_instance, Some(&job_row))
            .expect("a job must be enterable");
        assert_eq!(got, jobs_gen::JOB_SPAWN_GROUPS_REFERENCE);
        assert!(gd.dungeons.contains_key(&got), "and the client can load it");
    }

    /// …and the id we serve must describe the SAME dungeon as the `generatedData`
    /// the job row already carries. Hand the client a dungeon whose spawn groups are
    /// not the ones in its generated data and nothing spawns — the Abyss failure.
    /// This is the assertion that makes the job decision safe rather than merely
    /// non-404.
    #[test]
    fn the_job_dungeon_and_the_job_generated_data_agree() {
        let gd = game_data();
        let job = json!({
            "questId": "0b0b0001-0000-4000-8000-000000000001",
            "difficultyLevel": 20,
            "objectiveStatuses": {},
        });
        let generated = jobs_gen::generated_data_for_job(&gd, &job)
            .expect("the reference dungeon is in the corpus");

        let settings_id = resolve_dungeon_settings_id(
            &gd,
            Uuid::from_u128(0x0B0B_0001),
            Some(&quest_row(jobs_gen::JOB_SENTINEL_GLD)),
        )
        .expect("a job must be enterable");

        let dungeon = gd
            .dungeons
            .get(&settings_id)
            .expect("served id is a dungeon");
        let mut served: Vec<Uuid> = dungeon
            .spawn_info
            .enemy_spawn_groups
            .keys()
            .copied()
            .collect();
        let mut carried: Vec<Uuid> = generated.enemy_generated_data.keys().copied().collect();
        served.sort();
        carried.sort();
        assert!(!carried.is_empty(), "a job must have enemies to kill");
        assert_eq!(
            served, carried,
            "the dungeon we tell the client to load must be the one its data describes"
        );
    }
}

async fn handle_event_dungeon_entry(
    conn: &mut AsyncPgConnection,
    app_state: &ServerGlobal,
    character_id: Uuid,
    quest_id: Uuid,
    body: EnterDungeonRequest,
    session: &Session,
) -> Result<Json<EnterDungeonResponse>, BladeApiError> {
    // Get the event quest template
    let event_template = app_state.event_quests.templates.get(&quest_id)
        .ok_or_else(|| {
            log::error!("[event_dungeon] Quest {} not found in templates", quest_id);
            BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2)
        })?;

    // Get the actual event ID from the template's eventIds array
    let actual_event_id = *event_template.event_ids.get(0)
        .ok_or_else(|| {
            log::error!("[event_dungeon] No eventIds found for quest {}", quest_id);
            BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2)
        })?;

    // For event quests, we don't need game_data.events - use the quest_id as the dungeon UUID
    let dungeon_uuid = quest_id;
    let enemy_level = 1;
    let xp_reward = 100.0;
    let max_entries = 1;

    log::info!("[event_dungeon] Processing event quest {} with event_id {}", quest_id, actual_event_id);

    // Get or create completion record
    let completion = EventCompletion::get_or_create(conn, character_id, quest_id).await?;
    let completion_count = completion.completion_count as usize;

    // Check if character has already completed all tiers
    let total_rewards = event_template.rewards.len();
    let has_final_reward = event_template.final_reward.is_some();
    let max_completions = if has_final_reward { total_rewards } else { total_rewards };
    
    if completion_count >= max_completions {
        return Err(BladeApiError::new(
            StatusCode::FORBIDDEN,
            20001,
            1, // Already completed all event tiers
        ));
    }

    let _ = check_permission_for_character_and_get_it(&mut *conn, session, character_id).await?;

    let app_state_clone = app_state;
    let dungeon_id_clone = quest_id;

    conn.transaction(|mut conn| {
        async move {
            let existing_entry = {
                use crate::schema::event_dungeons::dsl::*;

                event_dungeons::table()
                    .filter(character_id.eq(character_id))
                    .filter(event_id.eq(actual_event_id))
                    .filter(dungeon_id.eq(dungeon_id_clone))
                    .select(EventDungeonEntryInfo::as_select())
                    .for_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
            };

            let current_entries = existing_entry.as_ref().map(|e| e.entry_count).unwrap_or(0);
            let existing_row_id = existing_entry.as_ref().map(|e| e.id);

            // Check expiration (use None for event quests since they don't expire)
            if let Some(entry) = &existing_entry {
                if let Some(expires) = entry.expires_at {
                    if chrono::Utc::now().naive_utc() > expires {
                        return Err(BladeApiError::new(
                            StatusCode::FORBIDDEN,
                            20001,
                            2, // Event expired
                        ));
                    }
                }
            }

            // If there's an existing row with a live dungeon_state, this is a RESUME
            // (reconnect, app relaunch, retry) of the player's one active attempt —
            // not a new attempt. It must not consume an entry or be blocked by
            // max_entries: bumping entry_count here on every resume was the bug —
            // a couple of reconnects would exhaust `max_entries: 1` and permanently
            // 403 a player who never actually re-entered the dungeon fresh.
            if let Some(existing) = existing_entry {
                if existing.dungeon_state.is_some() {
                    let dungeon_state_value = existing.dungeon_state.unwrap();
                    let mut dungeon_state_actual: DungeonState = serde_json::from_value(dungeon_state_value)
                        .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20002, 2))?;

                    dungeon_state_actual.dungeon_status.current_state = body.current_state;

                    {
                        use crate::schema::event_dungeons::dsl::*;

                        let dungeon_state_json = serde_json::to_value(&dungeon_state_actual)
                            .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

                        diesel::update(event_dungeons)
                            .filter(id.eq(existing.id))
                            .set((
                                dungeon_state.eq(Some(dungeon_state_json)),
                                // entry_count intentionally left unchanged — this is a resume.
                            ))
                            .execute(&mut conn)
                            .await?;
                    };

                    return Ok(Json(EnterDungeonResponse {
                        dungeon_status: dungeon_state_actual.dungeon_status,
                    }));
                }

                // Existing row but dungeon_state is NULL — the player exited (or never
                // finished) that attempt and is retrying. Retries are unlimited by
                // design: only `completion_count >= max_completions`, checked above
                // before this transaction even starts, is allowed to block `enter`.
                // Fall through to start a new attempt, reusing this row's id.
            }

            // Starting a new attempt — either no prior row at all, or the prior one
            // was exited/failed and the player is retrying the same tier.
            let dungeon_instance = body.dungeon_instance
                .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, 20002, 2))?;

            let enemy_level_i64 = enemy_level as i64;
            
            let dungeon_data = generate_for_dungeon(
                &app_state_clone.game_data,
                &dungeon_uuid,
                enemy_level_i64,
                xp_reward as u64,
            ).unwrap_or_else(|| DungeonGeneratedData {
                enemy_generated_data: HashMap::new(),
                item_generated_data: HashMap::new(),
                chest_generated_data: HashMap::new(),
                algorithm_version: 1,
                version: 0,
            });

            let status = DungeonStatus {
                dungeon_settings_ids: vec![dungeon_uuid],
                revive_count: 0,
                algorithm_version: 1,
                current_state: body.current_state,
                enemy_status: HashMap::default(),
                seed: rand::random::<u32>() as i64,
                level: enemy_level_i64 as u64,
                version: 1,
            };

            let dungeon_state_json = serde_json::to_value(DungeonState {
                dungeon_status: status.clone(),
            }).map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

            let initial_state_json = serde_json::to_value(dungeon_instance.clone())
                .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

            let generated_data_json = serde_json::to_value(dungeon_data)
                .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

            match existing_row_id {
                // Retry: reuse the existing (character_id, event_id, dungeon_id) row
                // instead of inserting a second one — this table has no dungeon_state
                // history worth keeping across attempts, and inserting a duplicate
                // would make the `existing_entry` lookup above pick between rows
                // arbitrarily on the next call.
                Some(row_id) => {
                    use crate::schema::event_dungeons::dsl::*;
                    diesel::update(event_dungeons)
                        .filter(id.eq(row_id))
                        .set((
                            dungeon_state.eq(Some(dungeon_state_json)),
                            initial_state.eq(Some(initial_state_json)),
                            generated_data.eq(generated_data_json),
                            entered_at.eq(chrono::Utc::now().naive_utc()),
                            entry_count.eq(current_entries + 1), // telemetry only, not a gate
                        ))
                        .execute(&mut conn)
                        .await?;
                }
                // Truly first attempt ever for this player/dungeon: insert fresh.
                None => {
                    let new_entry = EventDungeonDbEntry {
                        id: Uuid::new_v4(),
                        character_id,
                        event_id: actual_event_id,
                        dungeon_id: dungeon_id_clone,
                        dungeon_state: Some(dungeon_state_json),
                        initial_state: Some(initial_state_json),
                        generated_data: generated_data_json,
                        entered_at: chrono::Utc::now().naive_utc(),
                        expires_at: None, // No expiration for event quests
                        entry_count: 1,
                        max_entries,
                    };

                    use crate::schema::event_dungeons::table as event_dungeons_table;
                    diesel::insert_into(event_dungeons_table)
                        .values(&new_entry)
                        .execute(&mut conn)
                        .await?;
                }
            }

            Ok(Json(EnterDungeonResponse {
                dungeon_status: status,
            }))
        }
        .scope_boxed()
    })
    .await
}
