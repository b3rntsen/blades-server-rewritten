//! Per-alt "Fight AI" preference (owner, 2026-11-15).
//!
//! > "make a flag on each alt's profile: match AI true, false. People can then
//! > choose to queue up, and if anyone pops in, they get human fights, no AI
//! > fights."
//!
//! With the flag off the matchmaker never hands that player an AI opponent: the
//! ticket waits — past every fallback tier and the humans-first safety valve —
//! until another human queues, and is then paired like any other ticket (FIFO).
//! The default (no row) is `true`, today's behaviour.
//!
//! STORAGE. `arena_alt_preferences (user_id, alt_key)`, where `alt_key` is the alt
//! as the h2h boards key it: `characters.source_alt_uuid`, else the character id.
//! Not a column on `characters` (nor in `server_state`): `switch-alt` swaps that one
//! live row between alts, so a flag stored on it would describe whichever alt is
//! live rather than the alt it was set on, and would rely on every version path
//! copying it. Keyed by alt, a parked alt keeps its setting with nothing to copy.
//!
//! The website writes it through the token-gated dev route below, always for the
//! signed-in player's own arena account, so `user_id` scopes every write.

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use diesel::{
    QueryableByName,
    sql_types::{Bool, Nullable, Uuid as SqlUuid},
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

use crate::{
    ServerGlobal,
    admin::{IMPORT_SERVICE_ID, check_import_token},
    error::BladeApiError,
};

/// The live alt's key and its preference (NULL = no row = fights AI).
const LIVE_SQL: &str = "\
    SELECT COALESCE(c.source_alt_uuid, c.id) AS alt_key, p.fight_ai \
      FROM characters c \
      LEFT JOIN arena_alt_preferences p \
             ON p.user_id = c.user_id AND p.alt_key = COALESCE(c.source_alt_uuid, c.id) \
     WHERE c.user_id = $1 \
     LIMIT 1";

const PREFS_SQL: &str = "SELECT alt_key, fight_ai FROM arena_alt_preferences WHERE user_id = $1";

const UPSERT_SQL: &str = "\
    INSERT INTO arena_alt_preferences (user_id, alt_key, fight_ai, updated_at) \
    VALUES ($1, $2, $3, now()) \
    ON CONFLICT (user_id, alt_key) \
    DO UPDATE SET fight_ai = EXCLUDED.fight_ai, updated_at = now()";

#[derive(QueryableByName)]
struct LiveRow {
    #[diesel(sql_type = SqlUuid)]
    alt_key: Uuid,
    #[diesel(sql_type = Nullable<Bool>)]
    fight_ai: Option<bool>,
}

#[derive(QueryableByName)]
struct PrefRow {
    #[diesel(sql_type = SqlUuid)]
    alt_key: Uuid,
    #[diesel(sql_type = Bool)]
    fight_ai: bool,
}

/// The live alt of `user_id` and whether it fights AI. `None` = no character.
pub async fn live_alt_fights_ai(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> Result<Option<(Uuid, bool)>, diesel::result::Error> {
    let rows: Vec<LiveRow> = diesel::sql_query(LIVE_SQL)
        .bind::<SqlUuid, _>(user_id)
        .load(conn)
        .await?;
    Ok(rows
        .into_iter()
        .next()
        .map(|r| (r.alt_key, r.fight_ai.unwrap_or(true))))
}

/// Does the player queueing now accept an AI opponent? Read once at enqueue.
///
/// `true` on ANY failure — no database, no character, a missing table on a box
/// whose migration has not run. Losing the opt-out degrades to today's behaviour;
/// failing closed would strand players in a queue that never ends.
pub async fn fights_ai(pool: &crate::DbPool, user_id: Uuid) -> bool {
    let Ok(mut conn) = pool.get().await else {
        return true;
    };
    match live_alt_fights_ai(&mut conn, user_id).await {
        Ok(found) => found.map(|(_, f)| f).unwrap_or(true),
        Err(e) => {
            log::warn!("fight-ai: lookup for user {user_id} failed ({e}) — assuming Fight AI on");
            true
        }
    }
}

/// Set one alt's preference.
pub async fn set_fight_ai(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
    fight_ai: bool,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(UPSERT_SQL)
        .bind::<SqlUuid, _>(user_id)
        .bind::<SqlUuid, _>(alt_key)
        .bind::<Bool, _>(fight_ai)
        .execute(conn)
        .await
        .map(|_| ())
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FightAiResponse {
    /// The alt live on the arena account now (`None` = no character yet).
    pub live_alt_key: Option<Uuid>,
    /// The live alt's setting (true when there is none).
    pub fight_ai: bool,
    /// Every alt with a stored setting; an alt not listed fights AI.
    pub alts: HashMap<Uuid, bool>,
}

async fn read_all(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> Result<FightAiResponse, diesel::result::Error> {
    let live = live_alt_fights_ai(conn, user_id).await?;
    let rows: Vec<PrefRow> = diesel::sql_query(PREFS_SQL)
        .bind::<SqlUuid, _>(user_id)
        .load(conn)
        .await?;
    Ok(FightAiResponse {
        live_alt_key: live.map(|(k, _)| k),
        fight_ai: live.map(|(_, f)| f).unwrap_or(true),
        alts: rows.into_iter().map(|r| (r.alt_key, r.fight_ai)).collect(),
    })
}

/// `GET /…/dev/v1/characters/fight-ai?userId=` — this account's per-alt settings.
#[get("/blades.bgs.services/api/dev/v1/characters/fight-ai")]
pub async fn get_fight_ai(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<HashMap<String, String>>,
) -> Result<Json<FightAiResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let user_id = query
        .get("userId")
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 91))?;
    let mut conn =
        app_state.db_pool.get().await.map_err(|_| {
            BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 5)
        })?;
    read_all(&mut conn, user_id).await.map(Json).map_err(|e| {
        log::warn!("fight-ai: read for user {user_id} failed: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 92)
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetFightAiRequest {
    pub user_id: Uuid,
    /// The alt to set. Absent = the alt live now (400 when there is none).
    #[serde(default)]
    pub alt_key: Option<Uuid>,
    pub fight_ai: bool,
}

/// `POST /…/dev/v1/characters/fight-ai {userId, altKey?, fightAi}`.
///
/// Any alt key of this user is accepted, including one the arena has not been
/// given yet: a captured alt can be set before its first transfer, and the row is
/// found when it arrives (`source_alt_uuid` = the captured uuid). The write is
/// scoped to `userId`, so it can never touch another account.
#[post("/blades.bgs.services/api/dev/v1/characters/fight-ai")]
pub async fn set_fight_ai_route(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<SetFightAiRequest>,
) -> Result<Json<FightAiResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let mut conn =
        app_state.db_pool.get().await.map_err(|_| {
            BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 5)
        })?;
    let fail = |e: diesel::result::Error| {
        log::warn!("fight-ai: write for user {} failed: {e}", body.user_id);
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 92)
    };
    let alt_key = match body.alt_key {
        Some(k) => k,
        None => live_alt_fights_ai(&mut conn, body.user_id)
            .await
            .map_err(fail)?
            .map(|(k, _)| k)
            .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 91))?,
    };
    set_fight_ai(&mut conn, body.user_id, alt_key, body.fight_ai)
        .await
        .map_err(fail)?;
    log::info!(
        "fight-ai: user {} alt {alt_key} — Fight AI {}",
        body.user_id,
        if body.fight_ai { "on" } else { "off" }
    );
    read_all(&mut conn, body.user_id)
        .await
        .map(Json)
        .map_err(fail)
}
