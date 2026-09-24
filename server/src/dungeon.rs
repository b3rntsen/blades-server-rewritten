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
    B64EncodedData, CompleteCharacterWithIdWithoutData, DungeonGeneratedData,
    DungeonGeneratedDataWithId, DungeonState, DungeonStatus, ObjectiveStatus, Quest, QuestStatus,
    QuestWithId,
};
use diesel;
use diesel::{
    prelude::*,
    associations::HasTable,
    BoolExpressionMethods, ExpressionMethods, QueryDsl, SelectableHelper,
};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt, AsyncPgConnection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::{
    event_quests::{EventCompletion, EventQuestData},
    BladeApiError, ServerGlobal,
    json_db::JsonDbWrapper,
    models::{QuestDbEntry, QuestDbEntryDungeonStateAndInitialState, QuestDbEntryInfo},
    quest::jobs_gen,
    session::SessionLookedUpMaybe,
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

/// Retail's exit body: `{"restart": bool}`.
///
/// 215 of 215 distinct captured exits carry exactly that one key, 4 of them `true`.
/// We never read it, so a player who died and chose to restart was answered as if
/// they had walked out: the attempt was cleared (and, on an event, a tier was paid),
/// the client got no `dungeonStatus` to restart with, and its follow-up resume
/// `enter` found nothing and 400'd — "the quest spins and won't start again".
///
/// Parsed leniently: a missing or unreadable body is an ordinary exit. Rejecting it
/// would strand exactly the player an exit exists to release.
#[derive(Deserialize, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ExitDungeonRequest {
    #[serde(default)]
    restart: bool,
}

impl ExitDungeonRequest {
    fn parse(body: &[u8]) -> Self {
        serde_json::from_slice(body).unwrap_or_default()
    }
}

/// What retail's exit returns. Every shape carries `character`, with
/// `currentQuestDungeon: null`; the rest depends on how the attempt stands (see
/// [`exit_quest_dungeon`]).
///
/// `character` is NOT `CompleteCharacterWithIdAndData`: retail's body carries the
/// character's own fields and `id`, and NO `data` key. Verified against the smallest
/// captured response (982 B), which ends `…"nameValidated":true}}` with nothing after
/// the character object.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExitDungeonResponse {
    /// The attempt that is still alive: after a restart, or after leaving one that
    /// was never completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    dungeon_status: Option<DungeonStatus>,
    character: CompleteCharacterWithIdWithoutData,
    /// A finished event attempt: the instance's data for its next run.
    #[serde(skip_serializing_if = "Option::is_none")]
    dungeon_generated_data_list: Option<Vec<DungeonGeneratedDataWithId>>,
    /// A finished event attempt with tiers left: the instance, reset for its next run.
    #[serde(skip_serializing_if = "Option::is_none")]
    game_event_quest: Option<QuestWithId>,
    /// A finished event attempt that used the last tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    game_event_quest_finished: Option<QuestWithId>,
}

impl ExitDungeonResponse {
    fn character_only(character: CompleteCharacterWithIdWithoutData) -> Self {
        Self {
            dungeon_status: None,
            character,
            dungeon_generated_data_list: None,
            game_event_quest: None,
            game_event_quest_finished: None,
        }
    }
}

/// Leave the current quest dungeon.
///
/// Reported as tracker #83. We served `dungeons/current/enter` and
/// `dungeons/current/update` but not `exit`, so the client got a 404 where retail
/// answered — 592 retail 200s exist for this route in the capture DB (982 B to
/// 60 KB) against our own 404s.
///
/// **Whether the attempt ends depends on whether it was COMPLETED**, i.e. whether
/// `/complete` answered for it, not on the exit alone. The 215 distinct retail
/// exits split without exception:
///
/// | attempt | `restart` | n | response | afterwards |
/// |---|---|---|---|---|
/// | completed | false | 127 | `{character, dungeonGeneratedDataList, gameEventQuest}` | event run ended |
/// | completed | false | 28 | `{character, gameEventQuestFinished}` | event exhausted |
/// | completed | false | 53 | `{character}` | ordinary run ended |
/// | not completed | false | 3 | `{character, dungeonStatus}` | attempt KEPT |
/// | not completed | true | 4 | `{character, dungeonStatus}` | attempt KEPT, restarted |
///
/// and every one of the 7 captured resume `enter`s (no `dungeonInstance`) follows
/// one of those 7 kept attempts, 7/7 answered 200. Clearing every attempt on exit
/// — what this did — is why a restart after death came back to a 400.
///
/// Nothing is paid here. Rewards arrive on `/complete` (217/217 captured responses
/// carry `reward`) and never on exit (0/216).
///
/// Every shape clears the character's `current_quest_dungeon`, in the same
/// transaction as the attempt's own row, so the client and the server cannot
/// disagree about whether the player is still inside.
///
/// Idempotent on purpose: exiting a dungeon that is already gone returns the
/// character rather than erroring. The client retries this on a dropped connection,
/// and a second 4xx would strand the very player the retry is meant to rescue.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/dungeons/current/exit"
)]
pub async fn exit_quest_dungeon(
    path: web::Path<(Uuid, Uuid)>,
    body: web::Bytes,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<ExitDungeonResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (char_id, quest_id) = path.into_inner();
    let restart = ExitDungeonRequest::parse(&body).restart;
    let globals = app_state.get_ref().clone();
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
    let now = chrono::Utc::now().timestamp();

    conn.transaction(move |conn| {
        async move {
            if is_event {
                // Both ids: the TEMPLATE keys the event tables, the INSTANCE keys the
                // quest row and the client's completedQuests mirror.
                event_dungeon_exit(
                    conn,
                    &globals.static_data,
                    char_id,
                    gld_quest_id,
                    quest_id,
                    restart,
                    now,
                )
                .await
            } else {
                quest_dungeon_exit(conn, char_id, quest_id, restart).await
            }
        }
        .scope_boxed()
    })
    .await
    .map(Json)
}

/// Retail's restart: the same run from the top.
///
/// Measured on the 4 captured restarts against the last `update` before each: the
/// `seed` is unchanged 4/4 (the follow-up resume `enter` carries it too), the
/// `currentState` is unchanged 4/4, `reviveCount` is 0 4/4, and every enemy that
/// had been killed is standing again (2/2 — the other two had no enemy entries),
/// its entry and loot roll kept. Chests are left collected: no capture shows a
/// chest across a restart, and a chest that could be looted again after every
/// restart would be a free loop.
fn restarted_status(mut status: DungeonStatus) -> DungeonStatus {
    status.revive_count = 0;
    for enemy in status.enemy_status.values_mut() {
        enemy.killed = false;
    }
    status
}

/// Clear the character's pointer to its dungeon and hand the saved character back.
///
/// Only the `character` column: this touches one field of it, and selecting a wider
/// model would pull the whole save for no reason. `for_update` so a concurrent write
/// cannot interleave between the read and the clear. The response is the state just
/// committed rather than a copy made before the write.
async fn release_current_dungeon(
    conn: &mut AsyncPgConnection,
    char_id: Uuid,
) -> Result<CompleteCharacterWithIdWithoutData, BladeApiError> {
    use crate::schema::characters::dsl::*;
    let mut current: JsonDbWrapper<blades_lib::user_data::CompleteCharacter> = characters
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
    Ok(CompleteCharacterWithIdWithoutData {
        id: char_id,
        character: current.0,
    })
}

/// The ordinary (story quest, town job) half of [`exit_quest_dungeon`].
async fn quest_dungeon_exit(
    conn: &mut AsyncPgConnection,
    char_id: Uuid,
    quest_id: Uuid,
    restart: bool,
) -> Result<ExitDungeonResponse, BladeApiError> {
    use crate::schema::quests::dsl as q;
    // BOTH halves of the primary key. `quests.id` alone is NOT unique: an ordinary
    // story quest is stored under the template id, so every character on that quest
    // has a row with the same `id`, and exiting filtered on `id` would touch every
    // one of those players' dungeons.
    let row = q::quests
        .filter(q::id.eq(quest_id).and(q::character_id.eq(char_id)))
        .select(QuestDbEntry::as_select())
        .for_update()
        .load(conn)
        .await?
        .into_iter()
        .next();
    let completed = row.as_ref().is_some_and(|r| r.info.0.completed);

    let kept = match row.and_then(|r| r.dungeon_state) {
        Some(state) if restart || !completed => {
            let mut state = state.0;
            if restart {
                state.dungeon_status = restarted_status(state.dungeon_status);
                diesel::update(
                    q::quests.filter(q::id.eq(quest_id).and(q::character_id.eq(char_id))),
                )
                .set(q::dungeon_state.eq(Some(JsonDbWrapper(state.clone()))))
                .execute(conn)
                .await?;
            }
            Some(state.dungeon_status)
        }
        _ => {
            diesel::update(q::quests.filter(q::id.eq(quest_id).and(q::character_id.eq(char_id))))
                .set(q::dungeon_state.eq(None::<serde_json::Value>))
                .execute(conn)
                .await?;
            None
        }
    };

    let character = release_current_dungeon(conn, char_id).await?;
    Ok(ExitDungeonResponse {
        dungeon_status: kept,
        ..ExitDungeonResponse::character_only(character)
    })
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

/// Resolve an event quest template to the dungeon the client actually loads, then
/// generate data for that dungeon. Event quest ids and dungeon ids are different
/// UUIDs in the retail corpus; treating them as interchangeable silently produced
/// an empty payload and made every real spawn look stale to `dungeon_update`.
pub(crate) fn event_dungeon_data(
    game_data: &GameData,
    quest_id: Uuid,
) -> Result<(Uuid, DungeonGeneratedData), BladeApiError> {
    let dungeon_uuid = resolve_dungeon_settings_id(game_data, quest_id, None)?;
    let generated_data = generate_for_dungeon(game_data, &dungeon_uuid, 1, 100)
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?;
    Ok((dungeon_uuid, generated_data))
}

/// When the window of the event behind `quest_id` that is open at `now` began,
/// against the same themed calendar the feed serves.
///
/// `event_completions` has no window column, so this is what tells a stale
/// lifetime count from a live one — see `EventCompletion::reset_if_before`.
/// `None` when the event has no active instance right now, in which case the
/// stored count is left alone rather than guessed at.
///
/// Themed, not the untouched calendar: a themed window re-opens an event days after
/// its last opening, and reading the untouched calendar here would miss that new
/// window and carry the old window's completions into it.
fn event_window_start_at(
    sd: &blades_lib::static_data::StaticData,
    quest_id: Uuid,
    now: i64,
) -> Option<chrono::NaiveDateTime> {
    blades_lib::features::game_events::active_events_themed(
        &sd.game_events,
        sd.game_event_theme.as_ref(),
        now,
    )
    .into_iter()
    .find(|e| e.quest_id == quest_id)
    .and_then(|e| chrono::DateTime::from_timestamp(e.start_time_secs, 0))
    .map(|dt| dt.naive_utc())
}

/// The player's completion counter for event TEMPLATE `template_id`, with any count
/// left over from a PREVIOUS window of that event already dropped.
///
/// The only way to get an [`EventCompletion`]: every path that reads the count must
/// reset it first, and routing them all through here is what makes that a property
/// of the code rather than of each caller's memory
/// (`every_completion_counter_goes_through_the_window_reset` pins it).
///
/// Why it matters, from the times it was missed: the counter is a LIFETIME total
/// against per-window tiers. The path that decides WHICH TIER pays once read a row
/// still carrying a finished window's count, so the payout came back `None`, the
/// increment gated on a payout was skipped, and the stale number was what the
/// client's `completedQuests` mirror was told. Rewards stopped, the counter stopped,
/// and the tick never appeared — "event tiers are not progressing, they are not
/// getting checkmarked" (#166). A player can reach any of these paths with a stale
/// row whenever the window rolls between entering and completing.
pub(crate) async fn event_completion_in_window(
    conn: &mut AsyncPgConnection,
    sd: &blades_lib::static_data::StaticData,
    char_id: Uuid,
    template_id: Uuid,
    now: i64,
) -> Result<EventCompletion, BladeApiError> {
    let mut completion = EventCompletion::get_or_create(conn, char_id, template_id).await?;
    if let Some(window_start) = event_window_start_at(sd, template_id, now) {
        completion.reset_if_before(conn, window_start).await?;
    }
    Ok(completion)
}

/// Put an event instance back to how its next run starts.
///
/// Retail's exit after a completed run hands the instance back with
/// `completed: false` and every objective `{"status":"Active","progress":0.0,
/// "completed":false}` — 155/155 completed event exits. `completed` is also what
/// `/complete` uses as its once-per-run guard, so an instance left `true` would
/// make every later run's `/complete` a replay that pays nothing.
fn reset_event_instance_for_next_run(info: &mut Quest) {
    info.completed = false;
    for status in info.objective_statuses.values_mut() {
        *status = ObjectiveStatus {
            status: QuestStatus::Active,
            progress: 0.0,
            completed: false,
        };
    }
}

/// The event half of [`exit_quest_dungeon`].
///
/// `template_id` is the event TEMPLATE (`gldQuestId`); `instance_id` is the player's
/// own row id from the URL. They are never equal on an event quest — 1271 of 1271
/// captured event rows have `questId != gldQuestId` — and they key different
/// things, which is the whole of report #166: the template keys `event_dungeons`
/// and `event_completions`, the instance keys the quest row.
///
/// This used to clear the attempt and PAY the next tier on every exit. Dying paid,
/// and entering and walking out five times paid all five tiers without playing.
/// The milestone is now paid by `/complete`, exactly as retail pays it; see
/// `quest::complete_quest_in_tx`.
pub(crate) async fn event_dungeon_exit(
    conn: &mut AsyncPgConnection,
    sd: &blades_lib::static_data::StaticData,
    char_id: Uuid,
    template_id: Uuid,
    instance_id: Uuid,
    restart: bool,
    now: i64,
) -> Result<ExitDungeonResponse, BladeApiError> {
    let owner = char_id;
    let template = template_id;

    let attempt = {
        use crate::schema::event_dungeons::dsl::*;
        event_dungeons
            .filter(character_id.eq(owner))
            .filter(dungeon_id.eq(template))
            .select(EventDungeonEntryInfo::as_select())
            .for_update()
            .load(conn)
            .await?
            .into_iter()
            .next()
    };

    let quest_row = {
        use crate::schema::quests::dsl as q;
        q::quests
            .filter(q::id.eq(instance_id).and(q::character_id.eq(owner)))
            .select(QuestDbEntry::as_select())
            .for_update()
            .load(conn)
            .await?
            .into_iter()
            .next()
    };
    let completed = quest_row.as_ref().is_some_and(|r| r.info.0.completed);

    // The attempt lives on: a restart, or walking out of a run that was never
    // completed. Nothing is paid either way.
    if restart || !completed {
        let kept = match attempt.and_then(|a| a.dungeon_state.map(|state| (a.id, state))) {
            Some((row_id, value)) => {
                let mut state: DungeonState = serde_json::from_value(value)
                    .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;
                if restart {
                    state.dungeon_status = restarted_status(state.dungeon_status);
                    let state_json = serde_json::to_value(&state).map_err(|_| {
                        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3)
                    })?;
                    use crate::schema::event_dungeons::dsl::*;
                    diesel::update(event_dungeons.filter(id.eq(row_id)))
                        .set(dungeon_state.eq(Some(state_json)))
                        .execute(conn)
                        .await?;
                }
                Some(state.dungeon_status)
            }
            None => None,
        };
        log::info!(
            "event exit: character {} {} event quest {} (restart={}) — attempt {}, nothing paid",
            char_id,
            if completed {
                "restarted a completed run of"
            } else {
                "left an unfinished run of"
            },
            template_id,
            restart,
            if kept.is_some() {
                "kept"
            } else {
                "already gone"
            },
        );
        let character = release_current_dungeon(conn, char_id).await?;
        return Ok(ExitDungeonResponse {
            dungeon_status: kept,
            ..ExitDungeonResponse::character_only(character)
        });
    }

    // A completed run: end the attempt and set the instance up for its next run.
    {
        use crate::schema::event_dungeons::dsl::*;
        diesel::update(event_dungeons)
            .filter(character_id.eq(owner))
            .filter(dungeon_id.eq(template))
            .set(dungeon_state.eq(None::<serde_json::Value>))
            .execute(conn)
            .await?;
    }
    let mut row = quest_row.expect("`completed` is only true for a loaded row");
    reset_event_instance_for_next_run(&mut row.info.0);
    {
        use crate::schema::quests::dsl as q;
        diesel::update(q::quests.filter(q::id.eq(instance_id).and(q::character_id.eq(owner))))
            .set(QuestDbEntryInfo {
                info: JsonDbWrapper(row.info.0.clone()),
            })
            .execute(conn)
            .await?;
    }

    // Finished or not is read off the counter `/complete` just advanced: retail's
    // `gameEventQuestFinished` carries exactly 5 (56/56), a live `gameEventQuest` 1-4.
    let completion = event_completion_in_window(conn, sd, char_id, template_id, now).await?;
    let milestones = sd
        .event_quests
        .templates
        .get(&template_id)
        .map_or(0, |t| t.milestone_count());
    let exhausted = completion.completion_count as usize >= milestones;
    log::info!(
        "event exit: character {} finished a run of event quest {} ({}/{} tiers) — attempt ended",
        char_id,
        template_id,
        completion.completion_count,
        milestones,
    );

    let character = release_current_dungeon(conn, char_id).await?;
    let quest = QuestWithId {
        quest_id: instance_id,
        quest: row.info.0,
    };
    Ok(if exhausted {
        ExitDungeonResponse {
            game_event_quest_finished: Some(quest),
            ..ExitDungeonResponse::character_only(character)
        }
    } else {
        ExitDungeonResponse {
            dungeon_generated_data_list: row.generated_data.0.map(|inner| {
                vec![DungeonGeneratedDataWithId {
                    quest_id: instance_id,
                    inner,
                }]
            }),
            game_event_quest: Some(quest),
            ..ExitDungeonResponse::character_only(character)
        }
    })
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

    let _ = check_permission_for_character_and_get_it(
        &mut conn,
        validated_session,
        character_id_normal,
    )
    .await?;

    if app_state.event_quests.templates.contains_key(&gld_quest_id) {
        return handle_event_dungeon_entry(
            &mut conn,
            &app_state.game_data,
            &app_state.event_quests,
            &app_state.static_data,
            character_id_normal,
            gld_quest_id,
            quest_id,
            body,
            chrono::Utc::now().timestamp(),
        )
        .await;
    }

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

            let fresh_instance = match body.dungeon_instance {
                // first time entering
                Some(instance) => {
                    if quest.dungeon_state.is_some() {
                        return Err(BladeApiError::new(StatusCode::CONFLICT, 20003, 1));
                    }
                    Some(instance)
                }
                None if quest.dungeon_state.is_some() => None,
                // A resume with nothing to resume. Retail never answered a resume
                // with an error (7/7 captured resumes 200, each after an exit that
                // kept its attempt), and this server stranded players here by
                // clearing attempts on every exit: the client still holds its save
                // and asks to resume, and a 400 spins it for good. Start the run
                // again from the dungeon the row was last entered with. A quest
                // already completed stays refused — there is no run left to play.
                None => {
                    use crate::schema::quests::dsl::*;
                    let stored: Option<JsonDbWrapper<B64EncodedData>> = if quest.info.0.completed {
                        None
                    } else {
                        quests
                            .filter(id.eq(quest_id).and(character_id.eq(character_id_normal)))
                            .select(initial_state)
                            .first(&mut conn)
                            .await?
                    };
                    let Some(stored) = stored else {
                        return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20004, 2));
                    };
                    log::info!(
                        "enter: character {character_id_normal} resumed quest {quest_id} with no \
                         live attempt — starting a fresh one from its stored dungeonInstance"
                    );
                    Some(stored.0)
                }
            };

            if let Some(dungeon_instance) = fresh_instance {

                // `level` is the dungeon's own power and `seed` its generation
                // seed. Both were the constants `1` and `54321` — so every dungeon
                // any player has ever entered on the ordinary quest path reported
                // itself as a level-1 dungeon with the same seed, whatever the
                // quest was. Retail's captures carry the real pair (a level-40
                // dungeon with seed 578299371), and the event-dungeon path beside
                // this one already derives both; this makes the two agree.
                //
                // The level comes from the quest's own `difficultyLevel`, which is
                // what the enemy generation was already scaled to, so the number the
                // client is shown matches the fight it gets. The seed is generated
                // once and persisted with the dungeon state, so re-entering restores
                // it rather than rolling a new one.
                let status = DungeonStatus {
                    dungeon_settings_ids: vec![dungeon_settings_id],
                    revive_count: 0,
                    algorithm_version: 1,
                    current_state: body.current_state,
                    enemy_status: HashMap::default(),
                    seed: rand::random::<u32>() as i64,
                    level: quest.info.0.difficulty_level.max(1) as u64,
                    version: 1, //TODO: figure out where this version come from.
                    collected_chests: HashSet::default(),
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
mod event_dungeon_shadowing_guard {
    /// A filter that compares a column with itself matches every row. This shape hid
    /// behind `use …::dsl::*` in the event-dungeon entry and let one player's enter
    /// resume another player's attempt.
    #[test]
    fn no_filter_compares_a_column_with_itself() {
        let src = include_str!("dungeon.rs");
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for col in ["character_id", "event_id", "dungeon_id", "user_id", "quest_id"] {
            let needle = format!("{col}.eq({col})");
            assert!(!code.contains(&needle), "self-comparing filter `{needle}` matches every row");
        }
    }
}

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

    /// A themed re-opening is a NEW window for the completion ladder. The reset
    /// reads the window start from the themed calendar; against the untouched one
    /// it would find nothing (or last cycle's window) and the second run of a
    /// Halloween event would continue the first run's count instead of paying
    /// its five milestones again.
    #[test]
    fn a_themed_reopening_is_a_new_completion_window() {
        use blades_lib::features::game_events::{EventTheme, HALLOWEEN_THEME_QUESTS};

        let mut sd = static_data();
        let theme = EventTheme {
            start_secs: 1_791_590_400, // 2026-10-10 00:00 UTC
            end_secs: 1_793_404_800,   // 2026-10-31 00:00 UTC
            quest_ids: HALLOWEEN_THEME_QUESTS.to_vec(),
        };
        let openings: Vec<(i64, Uuid, bool)> =
            theme.plan(&sd.game_events).expect("plans").openings().collect();
        sd.game_event_theme = Some(theme);

        let undying = HALLOWEEN_THEME_QUESTS[2];
        let runs: Vec<i64> = openings
            .iter()
            .filter(|(_, q, _)| *q == undying)
            .map(|(s, _, _)| *s)
            .collect();
        assert!(runs.len() >= 2, "Wrath of the Undying opens {} time(s)", runs.len());
        let second = runs[1];
        let at = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().naive_utc();

        assert_eq!(
            event_window_start_at(&sd, undying, second + 3_600),
            Some(at(second)),
            "the second run is its own window"
        );
        // CONTROL: the untouched calendar does not know that window exists.
        let plain = StaticData {
            game_event_theme: None,
            ..sd.clone()
        };
        assert_ne!(event_window_start_at(&plain, undying, second + 3_600), Some(at(second)));
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

    /// Report #135 supplied the real group ids from EQ15. They all belong to the
    /// dungeon referenced by the event template, not to the event quest UUID.
    /// Pin both the indirection and the exact groups that production rejected.
    #[test]
    fn event_quest_generated_data_uses_its_referenced_dungeon() {
        let gd = game_data();
        let quest_id = Uuid::parse_str("e8f3614c-8672-4f77-9dad-4b400676f4b6").unwrap();
        let expected_dungeon =
            Uuid::parse_str("924f1147-fd7f-4736-9e2d-f33fa942dbdd").unwrap();
        let (dungeon_id, generated) = event_dungeon_data(&gd, quest_id)
            .expect("the EQ15 event quest must generate a real dungeon payload");

        assert_eq!(dungeon_id, expected_dungeon);
        for id in [
            "5dd87222-11c4-4152-991b-92703baff18a",
            "7461fd2c-c417-4b80-b185-6d491982787e",
            "a91ebfe6-0167-4643-bd6f-ed27d8dfad41",
            "9365e335-6682-4267-a9fb-cf1ac79c3b1f",
        ] {
            let id = Uuid::parse_str(id).unwrap();
            assert!(
                generated.enemy_generated_data.contains_key(&id),
                "reported enemy group {id} must be generated"
            );
        }
        let chest = Uuid::parse_str("d12be687-9c9f-4096-afd5-9854073d9870").unwrap();
        assert!(
            generated.chest_generated_data.contains_key(&chest),
            "reported chest group must be generated"
        );
    }

    /// Keep every shipped event template honest: its generated payload must name
    /// exactly the spawn groups in the dungeon the template references.
    #[test]
    fn every_event_quest_generates_the_referenced_dungeons_groups() {
        let (sd, gd) = (static_data(), game_data());
        assert!(!sd.event_quests.templates.is_empty());

        for quest_id in sd.event_quests.templates.keys() {
            let (dungeon_id, generated) = event_dungeon_data(&gd, *quest_id)
                .unwrap_or_else(|e| panic!("event quest {quest_id} cannot generate: {e}"));
            let dungeon = gd.dungeons.get(&dungeon_id).expect("resolved dungeon exists");

            let expected_enemies: HashSet<_> =
                dungeon.spawn_info.enemy_spawn_groups.keys().copied().collect();
            let actual_enemies: HashSet<_> =
                generated.enemy_generated_data.keys().copied().collect();
            assert_eq!(actual_enemies, expected_enemies, "event quest {quest_id}");

            let expected_chests: HashSet<_> =
                dungeon.spawn_info.chest.keys().copied().collect();
            let actual_chests: HashSet<_> =
                generated.chest_generated_data.keys().copied().collect();
            assert_eq!(actual_chests, expected_chests, "event quest {quest_id}");

            let expected_items: HashSet<_> =
                dungeon.spawn_info.item.keys().copied().collect();
            let actual_items: HashSet<_> =
                generated.item_generated_data.keys().copied().collect();
            assert_eq!(actual_items, expected_items, "event quest {quest_id}");
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
        let generated = jobs_gen::generated_data_for_job(&gd, &job, &crate::quest::shipped_scaling())
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
        let served: std::collections::HashSet<Uuid> = dungeon
            .spawn_info
            .enemy_spawn_groups
            .keys()
            .copied()
            .collect();
        let carried: std::collections::HashSet<Uuid> =
            generated.enemy_generated_data.keys().copied().collect();
        assert!(!carried.is_empty(), "a job must have enemies to kill");

        // SUBSET, not equality.
        //
        // The danger is generated data naming a spawn group the dungeon does not
        // have — nothing spawns, the Abyss failure. A dungeon carrying groups the
        // data does not mention is the opposite, and it is what retail does: a job
        // with no secret room simply has no entry for the secret-room boss, and a
        // Duel carries three of the reference's six.
        //
        // This asserted equality, and only passed because the non-duel path used to
        // emit all six groups unconditionally — the very defect of report #168. The
        // Duel path has contradicted it since PR #203 and duels work; the fixture
        // here just never exercised a Duel.
        let missing: Vec<&Uuid> = carried.difference(&served).collect();
        assert!(
            missing.is_empty(),
            "generated data names spawn groups the served dungeon does not have: {missing:?}"
        );
    }
}

/// The event half of [`enter_quest_dungeon`]. `quest_id` is the TEMPLATE,
/// `instance_quest_id` the player's own quest row; the caller has already checked
/// that the session owns `character_id`.
#[allow(clippy::too_many_arguments)]
async fn handle_event_dungeon_entry(
    conn: &mut AsyncPgConnection,
    game_data: &GameData,
    event_quests: &EventQuestData,
    sd: &blades_lib::static_data::StaticData,
    character_id: Uuid,
    quest_id: Uuid,
    instance_quest_id: Uuid,
    body: EnterDungeonRequest,
    now: i64,
) -> Result<Json<EnterDungeonResponse>, BladeApiError> {
    // Get the event quest template
    let event_template = event_quests.templates.get(&quest_id)
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

    // The event quest template points to the dungeon the client loads. The quest UUID
    // itself is only the key used by the quest and event-dungeon endpoints.
    let (dungeon_uuid, dungeon_data) = event_dungeon_data(game_data, quest_id)?;
    let enemy_level = 1;
    let max_entries = 1;

    log::info!("[event_dungeon] Processing event quest {} with event_id {}", quest_id, actual_event_id);

    // The completion record, with any count left over from a PREVIOUS window of this
    // event already dropped. Without that the counter is a lifetime total against
    // per-window tiers, so finishing an event locks the player out of it for ever.
    let completion = event_completion_in_window(conn, sd, character_id, quest_id, now).await?;
    let completion_count = completion.completion_count as usize;

    // Has the character already completed all tiers? Only a NEW attempt is refused
    // on this: a live one (say, restarted after the last tier's `/complete`) must
    // still be resumable, or the player is stuck inside a run they cannot re-enter.
    let max_completions = event_template.rewards.len();
    let event_finished = completion_count >= max_completions;

    let dungeon_id_clone = quest_id;
    // Same reason as `dungeon_id_clone`: inside `use event_dungeons::dsl::*` the bare
    // name `character_id` is the COLUMN, so the filter written against it compared
    // the column with itself and matched EVERY player's row. Entering a
    // Sigil event then "resumed" whichever player's live attempt came first, answered
    // 200, and never created this player's row — so their first update 404'd
    // (400-20000-2 in game, 2026-09-24, on the first spell cast).
    let owner_id = character_id;

    conn.transaction(|mut conn| {
        async move {
            let existing_entry = {
                use crate::schema::event_dungeons::dsl::*;

                event_dungeons::table()
                    .filter(character_id.eq(owner_id))
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
            let stored_instance = existing_entry.as_ref().and_then(|e| e.initial_state.clone());

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
                    // Heal attempts created before report #135's fix. Their stored
                    // status named the event quest UUID instead of the dungeon UUID,
                    // and their generated data was therefore empty.
                    dungeon_state_actual.dungeon_status.dungeon_settings_ids = vec![dungeon_uuid];

                    {
                        use crate::schema::event_dungeons::dsl::*;

                        let dungeon_state_json = serde_json::to_value(&dungeon_state_actual)
                            .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;
                        let generated_data_json = serde_json::to_value(&dungeon_data)
                            .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

                        diesel::update(event_dungeons)
                            .filter(id.eq(existing.id))
                            .set((
                                dungeon_state.eq(Some(dungeon_state_json)),
                                generated_data.eq(generated_data_json),
                                // entry_count intentionally left unchanged — this is a resume.
                            ))
                            .execute(&mut conn)
                            .await?;
                    };

                    return Ok(Json(EnterDungeonResponse {
                        dungeon_status: dungeon_state_actual.dungeon_status,
                    }));
                }

                // Existing row but dungeon_state is NULL — the player finished that
                // attempt and is starting the next run. Runs are unlimited by design:
                // only `completion_count >= max_completions` is allowed to block a new
                // attempt. Fall through, reusing this row's id.
            }

            if event_finished {
                return Err(BladeApiError::new(
                    StatusCode::FORBIDDEN,
                    20001,
                    1, // Already completed all event tiers
                ));
            }

            // Starting a new attempt — either no prior row at all, or the prior one
            // was finished and the player is on the next tier.
            let dungeon_instance = match body.dungeon_instance {
                Some(instance) => instance,
                // A resume with nothing to resume. Retail never answered a resume
                // with an error (7/7 captured resumes 200, each after an exit that
                // kept its attempt). This server did, by clearing the attempt on every
                // exit: the owner died in "Golden Madness", restarted, and his resume
                // came back 400 with the quest spinning for good (2026-09-24). Rows
                // stranded that way still hold the dungeon they were entered with, so
                // start the run again from it.
                None => {
                    let stored = stored_instance
                        .and_then(|v| serde_json::from_value::<B64EncodedData>(v).ok())
                        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, 20002, 2))?;
                    log::info!(
                        "[event_dungeon] character {owner_id} resumed event quest {dungeon_id_clone} \
                         with no live attempt — starting a fresh one from its stored dungeonInstance"
                    );
                    stored
                }
            };

            // A new run of the instance starts from scratch. Retail's exit resets the
            // instance after every completed run, and so does ours now; this catches
            // rows left `completed` by the old exit, which never reset them, so that
            // the run's `/complete` is not mistaken for a replay and left unpaid.
            {
                use crate::schema::quests::dsl as q;
                let row = q::quests
                    .filter(q::id.eq(instance_quest_id).and(q::character_id.eq(owner_id)))
                    .select(QuestDbEntry::as_select())
                    .for_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next();
                if let Some(mut row) = row.filter(|r| r.info.0.completed) {
                    reset_event_instance_for_next_run(&mut row.info.0);
                    diesel::update(
                        q::quests.filter(q::id.eq(instance_quest_id).and(q::character_id.eq(owner_id))),
                    )
                    .set(QuestDbEntryInfo { info: row.info })
                    .execute(&mut conn)
                    .await?;
                }
            }

            let enemy_level_i64 = enemy_level as i64;

            let status = DungeonStatus {
                dungeon_settings_ids: vec![dungeon_uuid],
                revive_count: 0,
                algorithm_version: 1,
                current_state: body.current_state,
                collected_chests: HashSet::default(),
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

#[cfg(test)]
mod event_window_reset_tests {
    /// Every path that reads an event completion count must drop a previous
    /// window's count first — so there is exactly ONE way to get the counter, and it
    /// resets.
    ///
    /// A source assertion rather than a handler test, for the same reason
    /// `every_anon_login_exit_provisions_a_character` is one: the bug it guards is
    /// precisely that ONE path forgot the call. Counting them is what the compiler
    /// cannot do.
    ///
    /// `/complete` is the path that decides which tier pays and what the client's
    /// checkmarks are told, so a stale count there stops rewards, stops the counter
    /// and never ticks the box (#166). Adding a path that loads the counter directly
    /// is exactly how this regresses.
    #[test]
    fn every_completion_counter_goes_through_the_window_reset() {
        let strip = |src: &str| -> String {
            src.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let dungeon = strip(include_str!("dungeon.rs"));
        let quest = strip(include_str!("quest.rs"));
        let dungeon_update = strip(include_str!("dungeon_update.rs"));

        // Spelled out at runtime so this test's own text does not match.
        let direct = format!("EventCompletion::{}(", "get_or_create");
        let direct_uses: usize = [&dungeon, &quest, &dungeon_update]
            .iter()
            .map(|s| s.matches(direct.as_str()).count())
            .sum();
        assert_eq!(
            direct_uses, 1,
            "only `event_completion_in_window` may load the counter"
        );

        let helper_at = dungeon
            .find("async fn event_completion_in_window(")
            .expect("the helper exists");
        let helper = &dungeon[helper_at..];
        let helper = &helper[..helper.find("\n}\n").expect("helper body ends")];
        assert!(
            helper.contains(direct.as_str()),
            "the one direct load is the helper's"
        );
        assert!(
            helper.contains("completion.reset_if_before(conn, window_start).await?"),
            "the helper must reset a previous window's count before handing it out"
        );

        // The readers: the enter gate, the exit, and `/complete`.
        let helper_calls = format!("{}(", "event_completion_in_window");
        let definition = format!("fn {helper_calls}");
        let calls = |src: &str| {
            src.matches(helper_calls.as_str()).count() - src.matches(definition.as_str()).count()
        };
        assert!(calls(&dungeon) >= 2, "enter and exit");
        assert!(calls(&quest) >= 1, "/complete");
    }
}

/// No dungeon may report itself as a level-1 dungeon with a fixed seed.
///
/// `enter_quest_dungeon`'s ordinary-quest branch answered `level: 1, seed: 54321`
/// for every quest any player ever entered, while the event-dungeon branch right
/// beside it derived both. Retail's captures carry the real pair — a level-40
/// dungeon with seed 578299371 — and `level` is what the client shows and scales
/// its presentation to.
///
/// Nothing caught it: the constants are on a handler path that needs a database,
/// and no test reached them. It took driving a character through a running server
/// to see a level-1 dungeon come back for a level-20 job.
#[cfg(test)]
mod dungeon_status_is_derived {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).expect("read src").flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// Every `DungeonStatus` literal must take its `seed` and `level` from
    /// something, not from a constant.
    #[test]
    fn no_dungeon_status_carries_a_literal_seed_or_level() {
        let mut files = Vec::new();
        rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);

        // The one place a literal is honest: `dungeon_update` answers an update
        // that arrives after the event dungeon's state was already cleared. There
        // is no seed or level left to report, the response exists only so a late
        // client retry is not stranded, and the client discards it. Anything else
        // that wants an exemption needs its own line here and a reason.
        const HONEST_PLACEHOLDERS: &[&str] = &["dungeon_update.rs"];

        let mut offenders = Vec::new();
        let mut found = 0;
        for f in files {
            let src = fs::read_to_string(&f).unwrap_or_default();
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let mut rest = src.as_str();
            while let Some(i) = rest.find("DungeonStatus {") {
                rest = &rest[i + "DungeonStatus {".len()..];
                let end = rest.find("};").unwrap_or(rest.len().min(800));
                let body = &rest[..end];
                found += 1;
                if HONEST_PLACEHOLDERS.contains(&name.as_str()) {
                    continue;
                }
                for field in ["seed", "level"] {
                    let Some(j) = body.find(&format!("{field}: ")) else {
                        continue;
                    };
                    let value = body[j + field.len() + 2..]
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .trim();
                    // A bare integer literal is the bug; anything derived is fine.
                    if value.parse::<i64>().is_ok() {
                        offenders.push(format!("{name}: {field}: {value}"));
                    }
                }
            }
        }
        assert!(
            found >= 2,
            "only {found} DungeonStatus literals found — the scan is broken"
        );
        assert!(
            offenders.is_empty(),
            "a dungeon's seed and level must be derived from the quest, not \
             hardcoded — the client shows `level` and scales to it:\n  {}",
            offenders.join("\n  ")
        );
    }
}

/// The exit body is read, and read leniently.
#[cfg(test)]
mod exit_request_parsing {
    use super::ExitDungeonRequest;

    #[test]
    fn the_retail_body_is_read() {
        assert!(ExitDungeonRequest::parse(br#"{"restart":true}"#).restart);
        assert!(!ExitDungeonRequest::parse(br#"{"restart":false}"#).restart);
    }

    /// A body we cannot read is an ordinary exit, never a rejection.
    #[test]
    fn a_missing_or_unreadable_body_is_an_ordinary_exit() {
        for body in [&b""[..], b"{}", b"not json", br#"{"restart":"yes"}"#] {
            assert_eq!(
                ExitDungeonRequest::parse(body),
                ExitDungeonRequest::default()
            );
        }
    }
}

/// A Sigil event run end to end against a real Postgres: enter, restart, die, leave,
/// `/complete`, replay, exit, and the next run — through the handlers' own bodies.
///
/// They SKIP without TEST_DATABASE_URL (CI provides one) rather than failing. The
/// event tables come from their real migration, not a copy.
#[cfg(test)]
mod event_run_lifecycle_db {
    use super::*;
    use actix_web::ResponseError;
    use blades_lib::static_data::StaticData;
    use blades_lib::user_data::{Backpack, CompleteInventory, CompleteWallet, Loadout, Treasury};
    use diesel_async::{AsyncConnection, AsyncPgConnection, SimpleAsyncConnection};
    use serde_json::{Value, json};
    use std::sync::OnceLock;

    /// 2026-05-03 00:00 UTC — inside the corpus's own event calendar.
    const NOW: i64 = 1_777_852_800;

    const TABLES: &str = "\
        CREATE TABLE characters ( \
            id UUID PRIMARY KEY, user_id UUID NOT NULL, character JSONB NOT NULL, \
            data JSONB NOT NULL, inventory JSONB NOT NULL, wallet JSONB NOT NULL, \
            town JSONB, server_state JSONB NOT NULL, source_alt_uuid UUID); \
        CREATE TABLE quests ( \
            id UUID NOT NULL, character_id UUID NOT NULL REFERENCES characters(id), \
            info JSONB NOT NULL, generated_data JSONB NOT NULL, dungeon_state JSONB, \
            initial_state JSONB, PRIMARY KEY (id, character_id));";
    const EVENT_TABLES: &str =
        include_str!("../../migrations/2026-09-04-000000-0000_add_event_quest_tables/up.sql");

    struct World {
        sd: StaticData,
        gd: GameData,
        events: EventQuestData,
    }

    fn world() -> &'static World {
        static WORLD: OnceLock<World> = OnceLock::new();
        WORLD.get_or_init(|| {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
            let read = |f: &str| std::fs::read_to_string(dir.join(f)).expect(f);
            World {
                sd: crate::static_loader::load(&dir),
                gd: serde_json::from_str(&read("parsed.json")).expect("parsed.json"),
                events: EventQuestData::from_json(
                    &serde_json::from_str(&read("event_quests.json")).expect("event_quests.json"),
                ),
            }
        })
    }

    async fn fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction()
            .await
            .expect("test transaction");
        let schema = format!("t{}", Uuid::new_v4().simple());
        conn.batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};"
        ))
        .await
        .unwrap();
        conn.batch_execute(TABLES).await.unwrap();
        conn.batch_execute(EVENT_TABLES).await.unwrap();
        Some(conn)
    }

    macro_rules! db {
        () => {
            match fixture().await {
                Some(c) => c,
                None => {
                    eprintln!("SKIP: TEST_DATABASE_URL unset — event run lifecycle NOT verified");
                    return;
                }
            }
        };
    }

    struct Player {
        user: Uuid,
        character: Uuid,
        instance: Uuid,
        template: Uuid,
    }

    /// A character holding one real minted event instance, stored the way `/quests`
    /// stores it.
    async fn seed(conn: &mut AsyncPgConnection) -> Player {
        let w = world();
        let (user, character) = (Uuid::new_v4(), Uuid::new_v4());
        let minted = crate::quest::event_quests::mint(&w.sd, &w.gd, character, 40, NOW)
            .into_iter()
            .next()
            .expect("the committed calendar opens an event at NOW");
        assert!(
            !minted.quest.objective_statuses.is_empty(),
            "an event instance has objectives to reset"
        );
        let inventory = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        diesel::sql_query(
            "INSERT INTO characters (id, user_id, character, data, inventory, wallet, town, server_state) \
             VALUES ($1, $2, $3::jsonb, '{}'::jsonb, $4::jsonb, $5::jsonb, NULL, '{}'::jsonb)",
        )
        .bind::<diesel::sql_types::Uuid, _>(character)
        .bind::<diesel::sql_types::Uuid, _>(user)
        .bind::<diesel::sql_types::Text, _>(
            serde_json::to_string(&blades_lib::user_data::CompleteCharacter::default()).unwrap(),
        )
        .bind::<diesel::sql_types::Text, _>(serde_json::to_string(&inventory).unwrap())
        .bind::<diesel::sql_types::Text, _>(
            serde_json::to_string(&CompleteWallet::default()).unwrap(),
        )
        .execute(conn)
        .await
        .unwrap();
        diesel::sql_query(
            "INSERT INTO quests (id, character_id, info, generated_data) \
             VALUES ($1, $2, $3::jsonb, $4::jsonb)",
        )
        .bind::<diesel::sql_types::Uuid, _>(minted.quest_id)
        .bind::<diesel::sql_types::Uuid, _>(character)
        .bind::<diesel::sql_types::Text, _>(serde_json::to_string(&minted.quest).unwrap())
        .bind::<diesel::sql_types::Text, _>(serde_json::to_string(&minted.dungeon).unwrap())
        .execute(conn)
        .await
        .unwrap();
        Player {
            user,
            character,
            instance: minted.quest_id,
            template: minted.quest.gld_quest_id,
        }
    }

    fn b64(s: &str) -> B64EncodedData {
        B64EncodedData { b64: s.to_string() }
    }

    async fn enter(
        conn: &mut AsyncPgConnection,
        p: &Player,
        dungeon_instance: Option<&str>,
    ) -> Result<DungeonStatus, BladeApiError> {
        let w = world();
        handle_event_dungeon_entry(
            conn,
            &w.gd,
            &w.events,
            &w.sd,
            p.character,
            p.template,
            p.instance,
            EnterDungeonRequest {
                dungeon_instance: dungeon_instance.map(b64),
                current_state: b64("SAVE-AT-ENTRY"),
            },
            NOW,
        )
        .await
        .map(|r| r.0.dungeon_status)
    }

    async fn exit(conn: &mut AsyncPgConnection, p: &Player, restart: bool) -> Value {
        let r = event_dungeon_exit(
            conn,
            &world().sd,
            p.character,
            p.template,
            p.instance,
            restart,
            NOW,
        )
        .await
        .expect("exit is never refused");
        serde_json::to_value(r).unwrap()
    }

    async fn complete(conn: &mut AsyncPgConnection, p: &Player) -> Value {
        let r = crate::quest::complete_quest_in_tx(
            conn,
            &world().sd,
            p.user,
            p.character,
            p.instance,
            NOW,
        )
        .await
        .expect("/complete answers");
        serde_json::to_value(r).unwrap()
    }

    #[derive(diesel::QueryableByName)]
    struct JsonCol {
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Jsonb>)]
        v: Option<Value>,
    }

    async fn json_of(conn: &mut AsyncPgConnection, sql: &str, a: Uuid, b: Uuid) -> Option<Value> {
        diesel::sql_query(sql)
            .bind::<diesel::sql_types::Uuid, _>(a)
            .bind::<diesel::sql_types::Uuid, _>(b)
            .get_results::<JsonCol>(conn)
            .await
            .unwrap()
            .into_iter()
            .next()
            .and_then(|r| r.v)
    }

    /// The live attempt, if any.
    async fn attempt(conn: &mut AsyncPgConnection, p: &Player) -> Option<Value> {
        json_of(
            conn,
            "SELECT dungeon_state AS v FROM event_dungeons WHERE character_id = $1 AND dungeon_id = $2",
            p.character,
            p.template,
        )
        .await
    }

    async fn quest_info(conn: &mut AsyncPgConnection, p: &Player) -> Value {
        json_of(
            conn,
            "SELECT info AS v FROM quests WHERE id = $1 AND character_id = $2",
            p.instance,
            p.character,
        )
        .await
        .expect("the instance row")
    }

    /// `(experience, wallet)` — everything a payout could touch that matters here.
    async fn purse(conn: &mut AsyncPgConnection, p: &Player) -> (u64, Value) {
        let ch = json_of(
            conn,
            "SELECT character AS v FROM characters WHERE id = $1 AND id = $2",
            p.character,
            p.character,
        )
        .await
        .unwrap();
        let wallet = json_of(
            conn,
            "SELECT wallet AS v FROM characters WHERE id = $1 AND id = $2",
            p.character,
            p.character,
        )
        .await
        .unwrap();
        (ch["experience"].as_u64().unwrap_or(0), wallet)
    }

    async fn completions(conn: &mut AsyncPgConnection, p: &Player) -> i32 {
        #[derive(diesel::QueryableByName)]
        struct N {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            n: i32,
        }
        diesel::sql_query(
            "SELECT completion_count AS n FROM event_completions WHERE character_id = $1 AND event_id = $2",
        )
        .bind::<diesel::sql_types::Uuid, _>(p.character)
        .bind::<diesel::sql_types::Uuid, _>(p.template)
        .get_results::<N>(conn)
        .await
        .unwrap()
        .into_iter().next()
        .map_or(0, |r| r.n)
    }

    /// Overwrite the live attempt's status, as a run in progress would have it.
    async fn mid_run(conn: &mut AsyncPgConnection, p: &Player, revives: u64, killed_enemy: &str) {
        let mut state = attempt(conn, p).await.expect("a live attempt");
        state["dungeonStatus"]["reviveCount"] = json!(revives);
        state["dungeonStatus"]["currentState"] = json!({"b64": "SAVE-MID-RUN"});
        let mut enemies = serde_json::Map::new();
        enemies.insert(
            format!("{killed_enemy}-2-0"),
            json!({
                "spawnGroupId": killed_enemy, "xpReward": 257, "killed": true,
                "time": 1_778_314_742_807u64,
                "loot": {"currencies": {"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2": 304}}
            }),
        );
        state["dungeonStatus"]["enemyStatus"] = Value::Object(enemies);
        diesel::sql_query(
            "UPDATE event_dungeons SET dungeon_state = $3 WHERE character_id = $1 AND dungeon_id = $2",
        )
        .bind::<diesel::sql_types::Uuid, _>(p.character)
        .bind::<diesel::sql_types::Uuid, _>(p.template)
        .bind::<diesel::sql_types::Jsonb, _>(state)
        .execute(conn)
        .await
        .unwrap();
    }

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    const ENEMY: &str = "045ad56d-b171-4ae5-a661-27e4b409faeb";

    /// Retail's restart: the attempt lives on, from the top, and nothing is paid.
    #[tokio::test]
    async fn a_restart_keeps_the_attempt_resets_it_and_pays_nothing() {
        let mut conn = db!();
        let p = seed(&mut conn).await;
        let entered = enter(&mut conn, &p, Some("INSTANCE"))
            .await
            .expect("fresh enter");
        mid_run(&mut conn, &p, 2, ENEMY).await;
        // CONTROL: the run really is mid-way, so a reset below is observable.
        let before = attempt(&mut conn, &p).await.unwrap();
        assert_eq!(before["dungeonStatus"]["reviveCount"], json!(2));
        assert_eq!(
            before["dungeonStatus"]["enemyStatus"][format!("{ENEMY}-2-0")]["killed"],
            json!(true)
        );
        let paid_before = purse(&mut conn, &p).await;

        let out = exit(&mut conn, &p, true).await;

        assert_eq!(
            keys(&out),
            ["character", "dungeonStatus"],
            "retail's restart shape"
        );
        let status = &out["dungeonStatus"];
        assert_eq!(
            status["seed"],
            json!(entered.seed),
            "same run: seed unchanged"
        );
        assert_eq!(status["reviveCount"], json!(0));
        assert_eq!(
            status["enemyStatus"][format!("{ENEMY}-2-0")]["killed"],
            json!(false)
        );
        assert_eq!(status["currentState"]["b64"], json!("SAVE-MID-RUN"));
        assert!(out["character"]["currentQuestDungeon"].is_null());

        let kept = attempt(&mut conn, &p).await.expect("the attempt is KEPT");
        assert_eq!(
            kept["dungeonStatus"]["reviveCount"],
            json!(0),
            "and the reset is stored"
        );
        assert_eq!(completions(&mut conn, &p).await, 0, "no tier consumed");
        assert_eq!(purse(&mut conn, &p).await, paid_before, "nothing paid");
    }

    /// Dying and walking out — any exit with no `/complete` behind it — pays nothing,
    /// however many times it is repeated. The old exit paid a tier every time, so
    /// five walk-outs emptied the event.
    #[tokio::test]
    async fn an_exit_without_complete_pays_nothing_however_often() {
        let mut conn = db!();
        let p = seed(&mut conn).await;
        let paid_before = purse(&mut conn, &p).await;
        enter(&mut conn, &p, Some("INSTANCE"))
            .await
            .expect("fresh enter");

        for (i, restart) in [false, true, false, false, true, false]
            .into_iter()
            .enumerate()
        {
            let out = exit(&mut conn, &p, restart).await;
            assert!(
                out.get("dungeonStatus").is_some(),
                "exit {i}: the attempt is kept"
            );
            assert!(
                out.get("gameEventQuest").is_none(),
                "exit {i}: the run did not end"
            );
            enter(&mut conn, &p, None).await.expect("and resumes");
        }
        assert_eq!(completions(&mut conn, &p).await, 0);
        assert_eq!(
            purse(&mut conn, &p).await,
            paid_before,
            "six exits, nothing paid"
        );
        assert_eq!(quest_info(&mut conn, &p).await["completed"], json!(false));
    }

    /// The owner's sequence from 2026-09-24: enter, die, restart, resume. The resume
    /// arrives without `dungeonInstance` and must find the attempt.
    #[tokio::test]
    async fn enter_restart_resume_succeeds() {
        let mut conn = db!();
        let p = seed(&mut conn).await;
        let entered = enter(&mut conn, &p, Some("INSTANCE"))
            .await
            .expect("fresh enter");
        mid_run(&mut conn, &p, 1, ENEMY).await;
        exit(&mut conn, &p, true).await;

        let resumed = enter(&mut conn, &p, None)
            .await
            .expect("the resume after a restart");
        assert_eq!(resumed.seed, entered.seed, "it is the same run");
        assert_eq!(resumed.revive_count, 0);
    }

    /// A resume that finds NO live attempt — rows the old exit stranded — starts the
    /// run again from the stored dungeon instead of 400ing. The control: with no row
    /// at all there is nothing to start from, and it is still refused.
    #[tokio::test]
    async fn a_resume_with_no_live_attempt_starts_a_fresh_one_from_the_stored_dungeon() {
        let mut conn = db!();
        let p = seed(&mut conn).await;

        // CONTROL first: never entered, so no stored dungeon.
        let err = enter(&mut conn, &p, None)
            .await
            .expect_err("nothing to resume or start");
        assert_eq!(err.status_code().as_u16(), 400);

        let entered = enter(&mut conn, &p, Some("INSTANCE"))
            .await
            .expect("fresh enter");
        // What the old exit left behind: dungeon_state NULL, entry_count 1.
        diesel::sql_query("UPDATE event_dungeons SET dungeon_state = NULL WHERE character_id = $1")
            .bind::<diesel::sql_types::Uuid, _>(p.character)
            .execute(&mut conn)
            .await
            .unwrap();

        let healed = enter(&mut conn, &p, None).await.expect("no longer a 400");
        assert_eq!(healed.revive_count, 0);
        assert!(
            attempt(&mut conn, &p).await.is_some(),
            "a live attempt again"
        );
        let _ = entered;
    }

    /// The whole ladder. Each real completion pays exactly its own tier, once, on
    /// `/complete`; a replayed `/complete` pays nothing; the exit that follows pays
    /// nothing and sets the instance up for its next run; the last tier ends the event.
    #[tokio::test]
    async fn complete_pays_each_tier_exactly_once_and_exit_pays_nothing() {
        let mut conn = db!();
        let p = seed(&mut conn).await;
        let tmpl = world()
            .sd
            .event_quests
            .templates
            .get(&p.template)
            .expect("template");
        let tiers = tmpl.milestone_count();
        assert!(tiers >= 2, "a ladder, or this proves nothing");
        let quest: Quest = serde_json::from_value(quest_info(&mut conn, &p).await).unwrap();

        let mut paid = Vec::new();
        for n in 0..tiers {
            enter(&mut conn, &p, Some("INSTANCE"))
                .await
                .unwrap_or_else(|e| panic!("run {n}: {e}"));

            let (xp0, _) = purse(&mut conn, &p).await;
            let first = complete(&mut conn, &p).await;
            let want = crate::quest::event_milestone_reward(&world().sd, p.instance, &quest, n)
                .expect("a tier left");
            assert_eq!(
                first["reward"],
                serde_json::to_value(&want).unwrap(),
                "run {n} pays tier {n}"
            );
            assert!(
                want.character_xp > 0,
                "tier {n} carries xp, so the xp check below bites"
            );
            let (xp1, wallet1) = purse(&mut conn, &p).await;
            assert_eq!(
                xp1,
                xp0 + want.character_xp,
                "run {n}: paid into the character"
            );
            assert_eq!(completions(&mut conn, &p).await as usize, n + 1);
            assert_eq!(
                first["character"]["completedQuests"][p.instance.to_string()],
                json!(n + 1),
                "the checkmark mirror is the tier counter"
            );

            let replay = complete(&mut conn, &p).await;
            assert_eq!(
                serde_json::from_value::<blades_lib::economy::RewardGrant>(
                    replay["reward"].clone()
                )
                .unwrap()
                .is_empty(),
                true,
                "run {n}: a replayed /complete pays nothing"
            );
            assert_eq!(completions(&mut conn, &p).await as usize, n + 1);

            let out = exit(&mut conn, &p, false).await;
            assert_eq!(
                purse(&mut conn, &p).await,
                (xp1, wallet1),
                "run {n}: exit pays nothing"
            );
            assert!(
                attempt(&mut conn, &p).await.is_none(),
                "run {n}: a completed run ends"
            );
            assert!(out.get("dungeonStatus").is_none());
            let info = quest_info(&mut conn, &p).await;
            assert_eq!(
                info["completed"],
                json!(false),
                "run {n}: reset for the next run"
            );
            assert!(
                info["objectiveStatuses"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|s| s == &json!({"status": "Active", "progress": 0.0, "completed": false})),
                "run {n}: objectives reset"
            );
            if n + 1 < tiers {
                assert_eq!(
                    keys(&out),
                    ["character", "dungeonGeneratedDataList", "gameEventQuest"]
                );
                assert_eq!(out["gameEventQuest"]["questId"], json!(p.instance));
            } else {
                assert_eq!(
                    keys(&out),
                    ["character", "gameEventQuestFinished"],
                    "the last tier"
                );
            }
            paid.push(first["reward"].clone());
        }
        assert_ne!(paid[0], paid[1], "successive runs pay successive tiers");

        let err = enter(&mut conn, &p, Some("INSTANCE"))
            .await
            .expect_err("event finished");
        assert_eq!(err.status_code().as_u16(), 403);
    }

    /// The old exit never reset an instance, so rows are sitting on production with
    /// `completed: true`. Left alone, every later run's `/complete` would read as a
    /// replay and pay nothing; the next fresh run resets it instead.
    #[tokio::test]
    async fn a_row_left_completed_by_the_old_exit_pays_on_its_next_run() {
        let mut conn = db!();
        let p = seed(&mut conn).await;
        diesel::sql_query(
            "UPDATE quests SET info = jsonb_set(info, '{completed}', 'true') WHERE id = $1",
        )
        .bind::<diesel::sql_types::Uuid, _>(p.instance)
        .execute(&mut conn)
        .await
        .unwrap();

        enter(&mut conn, &p, Some("INSTANCE"))
            .await
            .expect("fresh enter");
        let out = complete(&mut conn, &p).await;
        let reward: blades_lib::economy::RewardGrant =
            serde_json::from_value(out["reward"].clone()).unwrap();
        assert!(!reward.is_empty(), "the run pays its tier");
        assert_eq!(completions(&mut conn, &p).await, 1);
    }

    // ------------------------------------------------------------ ordinary quests

    /// An ordinary quest row with a live attempt, `completed` as given.
    async fn ordinary(conn: &mut AsyncPgConnection, completed: bool) -> (Uuid, Uuid) {
        let p = seed(conn).await;
        let quest_id = Uuid::new_v4();
        let mut info = quest_info(conn, &p).await;
        info["type"] = json!("NORMAL");
        info["completed"] = json!(completed);
        let mut enemies = serde_json::Map::new();
        enemies.insert(
            format!("{ENEMY}-0-0"),
            json!({"spawnGroupId": ENEMY, "xpReward": 1, "killed": true, "time": 1, "loot": {}}),
        );
        let state = json!({"dungeonStatus": {
            "dungeonSettingsIds": [Uuid::new_v4()], "reviveCount": 2, "level": 20,
            "seed": 578299371, "currentState": {"b64": "SAVE"}, "algorithmVersion": 1,
            "version": 1, "enemyStatus": Value::Object(enemies)
        }});
        diesel::sql_query(
            "INSERT INTO quests (id, character_id, info, generated_data, dungeon_state, initial_state) \
             VALUES ($1, $2, $3, 'null'::jsonb, $4, '{\"b64\":\"INSTANCE\"}'::jsonb)",
        )
        .bind::<diesel::sql_types::Uuid, _>(quest_id)
        .bind::<diesel::sql_types::Uuid, _>(p.character)
        .bind::<diesel::sql_types::Jsonb, _>(info)
        .bind::<diesel::sql_types::Jsonb, _>(state)
        .execute(conn)
        .await
        .unwrap();
        (p.character, quest_id)
    }

    async fn ordinary_state(conn: &mut AsyncPgConnection, (c, q): (Uuid, Uuid)) -> Option<Value> {
        json_of(
            conn,
            "SELECT dungeon_state AS v FROM quests WHERE id = $2 AND character_id = $1",
            c,
            q,
        )
        .await
    }

    /// The same restart gap existed on story quests and jobs.
    #[tokio::test]
    async fn an_ordinary_restart_keeps_and_resets_the_attempt() {
        let mut conn = db!();
        let row = ordinary(&mut conn, false).await;
        let out = serde_json::to_value(
            quest_dungeon_exit(&mut conn, row.0, row.1, true)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(keys(&out), ["character", "dungeonStatus"]);
        assert_eq!(out["dungeonStatus"]["seed"], json!(578299371));
        assert_eq!(out["dungeonStatus"]["reviveCount"], json!(0));
        let kept = ordinary_state(&mut conn, row).await.expect("kept");
        assert_eq!(kept["dungeonStatus"]["reviveCount"], json!(0));
        assert_eq!(
            kept["dungeonStatus"]["enemyStatus"][format!("{ENEMY}-0-0")]["killed"],
            json!(false)
        );
    }

    /// Walking out of an unfinished story dungeon keeps it (retail: 3/3), and a
    /// completed one ends — the control that the kept case is not "never clear".
    #[tokio::test]
    async fn an_ordinary_exit_ends_the_attempt_only_when_completed() {
        let mut conn = db!();
        let unfinished = ordinary(&mut conn, false).await;
        let out = serde_json::to_value(
            quest_dungeon_exit(&mut conn, unfinished.0, unfinished.1, false)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(keys(&out), ["character", "dungeonStatus"]);
        assert_eq!(
            out["dungeonStatus"]["reviveCount"],
            json!(2),
            "left, not restarted"
        );
        assert!(ordinary_state(&mut conn, unfinished).await.is_some());

        let finished = ordinary(&mut conn, true).await;
        let out = serde_json::to_value(
            quest_dungeon_exit(&mut conn, finished.0, finished.1, false)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(keys(&out), ["character"], "retail's completed-exit shape");
        assert!(ordinary_state(&mut conn, finished).await.is_none());
        assert!(out["character"]["currentQuestDungeon"].is_null());
    }
}
