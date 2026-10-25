//! Operator TEST characters (owner decision 2026-10-16).
//!
//! A trusted tester gets an alt whose level an operator sets at will, to tune
//! level-based loot. Its numbers are not earned, so it must never rank.
//!
//! **The marker** is a row in `test_characters`, keyed by the ALT id
//! (`characters.source_alt_uuid` / `character_versions.source_alt_uuid`), because
//! every ranking is per alt and a user's alts share one `characters` row. Every
//! ranking query excludes it with one `NOT EXISTS` on that primary key:
//!
//! | ranking | where |
//! | --- | --- |
//! | in-game arena leaderboard + website "all matches" players | `leaderboards::RANKED_CTE` (live and saved alts) |
//! | season standings, season awards, season guild ladder, `guilds.trophies` (in-game guild board + website "all matches" guilds) | `season_store::FREEZE_STANDINGS_SQL` |
//! | h2h ratings (all-time + per season), so the website's Top 100 per alt and its h2h guild board | `ranking::H2H_REPLAY_ROWS_CTE` (not rated), `ranking::H2H_BOARD_ENTRIES_SQL` (not listed), `arena_economy` live increment (not rated) |
//! | `guilds.trophies` between recomputes | `arena_economy` per-match guild increment (adds nothing) |
//!
//! The name also carries " (Test)", so the character is visibly one in game and on
//! the website. That is the label; the table is the rule.
//!
//! **The routes** are dev-token gated (`X-Import-Token`, like every operator route)
//! and every write is recorded in `test_character_audit` and the log:
//!
//! - `POST /…/dev/v1/test-characters/mark` — mark the user's LIVE alt as a test
//!   character and suffix its name. Refuses an alt with any arena history and the
//!   account's original character, so a real character cannot be marked by mistake.
//! - `POST /…/dev/v1/test-characters/{altUuid}/level` — set a marked alt's level
//!   ([`blades_lib::features::level_up::set_level`]). Refuses an alt that is not
//!   marked, and one that is not live (switch to it on the website first), so it
//!   can only ever write the test character's own row.
//! - `GET /…/dev/v1/test-characters` — the marked alts and the audit trail.

use std::sync::Arc;

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::{features::level_up, user_data::CompleteCharacter};
use diesel::{
    ExpressionMethods, OptionalExtension, QueryDsl, QueryableByName, SelectableHelper,
    sql_types::{BigInt, Jsonb, Nullable, Text, Uuid as SqlUuid},
};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, admin::check_import_token, json_db::JsonDbWrapper,
    models::CharacterDbEntryCharacterAlone, schema::characters,
};

pub(crate) const TEST_CHARACTER_SERVICE_ID: u64 = 9009;

const NO_CONNECTION: u64 = 1;
const ACTOR_REQUIRED: u64 = 2;
const NOT_A_TEST_CHARACTER: u64 = 3;
const NOT_LIVE: u64 = 4;
const LEVEL_OUT_OF_RANGE: u64 = 5;
const HAS_ARENA_HISTORY: u64 = 6;
const ORIGINAL_CHARACTER: u64 = 7;
const INTERNAL: u64 = 8;
const NO_CHARACTER: u64 = 9;
const NAME_MISMATCH: u64 = 10;
const UNCLAIMED_LEGACY_AWARD: u64 = 11;
const IMPORT_INTO_TEST_ALT: u64 = 12;

/// The label every test character's name ends with.
pub(crate) const TEST_NAME_SUFFIX: &str = "(Test)";

fn refuse(status: StatusCode, code: u64) -> BladeApiError {
    BladeApiError::new(status, TEST_CHARACTER_SERVICE_ID, code)
}

/// True when any of `alts` is a marked test alt.
pub(crate) async fn any_test_alt(
    conn: &mut AsyncPgConnection,
    alts: &[Uuid],
) -> Result<bool, diesel::result::Error> {
    #[derive(QueryableByName)]
    struct Hit {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    let hit: Hit =
        diesel::sql_query("SELECT COUNT(*) AS n FROM test_characters WHERE alt_uuid = ANY($1)")
            .bind::<diesel::sql_types::Array<SqlUuid>, _>(alts.to_vec())
            .get_result(conn)
            .await?;
    Ok(hit.n > 0)
}

/// `import-character` refuses an import that names no alt while a test alt is
/// live (see the call site in `admin::import_character`).
pub(crate) fn import_into_test_alt_refusal() -> BladeApiError {
    log::warn!("[test-character] refused an import with no sourceAltUuid onto a live test alt");
    refuse(StatusCode::CONFLICT, IMPORT_INTO_TEST_ALT)
}

/// True when the alt occupying `character_id`'s row right now is a test alt.
pub(crate) async fn live_alt_is_test(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
) -> Result<bool, diesel::result::Error> {
    #[derive(QueryableByName)]
    struct Hit {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        hit: bool,
    }
    let row: Hit = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM characters c \
                          JOIN test_characters t ON t.alt_uuid = c.source_alt_uuid \
                         WHERE c.id = $1) AS hit",
    )
    .bind::<SqlUuid, _>(character_id)
    .get_result(conn)
    .await?;
    Ok(row.hit)
}

/// The name a test character carries: its own, suffixed once.
pub(crate) fn test_name(name: &str) -> String {
    let base = name.trim();
    if base.ends_with(TEST_NAME_SUFFIX) {
        base.to_string()
    } else {
        format!("{base} {TEST_NAME_SUFFIX}")
    }
}

/// True for a name carrying the test label. Such a name cannot be chosen by a
/// player (parentheses are outside the game's name characters) and a character
/// carrying it cannot be renamed away from it (`admin::rename_current_character`).
pub(crate) fn is_test_name(name: &str) -> bool {
    name.trim().ends_with(TEST_NAME_SUFFIX)
}

fn actor_of(raw: &str) -> Result<String, BladeApiError> {
    let actor = raw.trim();
    if actor.is_empty() || actor.chars().count() > 200 {
        return Err(refuse(StatusCode::BAD_REQUEST, ACTOR_REQUIRED));
    }
    Ok(actor.to_string())
}

async fn audit(
    conn: &mut AsyncPgConnection,
    alt_uuid: Uuid,
    user_id: Uuid,
    character_id: Option<Uuid>,
    action: &str,
    actor: &str,
    detail: Value,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO test_character_audit \
             (alt_uuid, user_id, character_id, action, actor, detail) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind::<SqlUuid, _>(alt_uuid)
    .bind::<SqlUuid, _>(user_id)
    .bind::<Nullable<SqlUuid>, _>(character_id)
    .bind::<Text, _>(action)
    .bind::<Text, _>(actor)
    .bind::<Jsonb, _>(detail)
    .execute(conn)
    .await?;
    Ok(())
}

// ---- mark -----------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkTestCharacterRequest {
    pub user_id: Uuid,
    /// The alt to mark. Must be the user's LIVE alt.
    pub alt_uuid: Uuid,
    /// The live character's current name, exactly. A second key the operator
    /// reads off the website, so a mistyped or stale alt id cannot mark the
    /// wrong character.
    pub expected_name: String,
    /// Who is doing this, for the audit trail.
    pub actor: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkTestCharacterResponse {
    pub alt_uuid: Uuid,
    pub character_id: Uuid,
    pub name: String,
    pub level: u16,
    /// False when the alt was already marked; nothing was written.
    pub newly_marked: bool,
}

#[post("/blades.bgs.services/api/dev/v1/test-characters/mark")]
pub async fn mark_test_character(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<MarkTestCharacterRequest>,
) -> Result<Json<MarkTestCharacterResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let actor = actor_of(&body.actor)?;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, NO_CONNECTION))?;
    conn.transaction(move |conn| async move { apply_mark(conn, &body, &actor).await }.scope_boxed())
        .await
        .map(Json)
}

async fn apply_mark(
    conn: &mut AsyncPgConnection,
    body: &MarkTestCharacterRequest,
    actor: &str,
) -> Result<MarkTestCharacterResponse, BladeApiError> {
    let (row, live_alt) = lock_live(conn, body.user_id).await?;
    if live_alt != Some(body.alt_uuid) {
        return Err(refuse(StatusCode::CONFLICT, NOT_LIVE));
    }
    let mut character = row.character.0;
    if character.name.trim() != body.expected_name.trim()
        && character.name.trim() != test_name(&body.expected_name)
    {
        return Err(refuse(StatusCode::CONFLICT, NAME_MISMATCH));
    }

    if any_test_alt(conn, &[body.alt_uuid]).await? {
        return Ok(MarkTestCharacterResponse {
            alt_uuid: body.alt_uuid,
            character_id: row.id,
            name: character.name,
            level: character.level,
            newly_marked: false,
        });
    }

    // A real character must not be marked by mistake: marking hides it from every
    // board. The account's original character (its alt id is the row's own id, the
    // shape an import of a capture leaves) is never a test one…
    if body.alt_uuid == row.id {
        return Err(refuse(StatusCode::CONFLICT, ORIGINAL_CHARACTER));
    }
    // …and neither is an alt that has played the arena. A test alt is marked as
    // soon as it is made, so it has none. A match recorded before alts were
    // tracked (NULL) counts for the alt live now — this one — as every arena
    // reader counts it.
    #[derive(QueryableByName)]
    struct History {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    let history: History = diesel::sql_query(
        "SELECT (SELECT COUNT(*) FROM arena_match_results \
                  WHERE character_id = $1 AND COALESCE(source_alt_uuid, $2) = $2) \
              + (SELECT COUNT(*) FROM arena_season_standings \
                  WHERE character_id = $1 AND source_alt_uuid = $2) \
              + (SELECT COUNT(*) FROM arena_season_awards \
                  WHERE character_id = $1 AND source_alt_uuid = $2) AS n",
    )
    .bind::<SqlUuid, _>(row.id)
    .bind::<SqlUuid, _>(body.alt_uuid)
    .get_result(conn)
    .await?;
    if history.n > 0 {
        return Err(refuse(StatusCode::CONFLICT, HAS_ARENA_HISTORY));
    }
    // A season award recorded before alts were tracked (NULL alt) is claimable
    // by whichever alt is live (`season_store::award_belongs_to_alt`). While one
    // is unclaimed, the test alt could collect the real character's reward.
    let legacy: History = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM arena_season_awards \
          WHERE character_id = $1 AND source_alt_uuid IS NULL AND granted_at IS NULL",
    )
    .bind::<SqlUuid, _>(row.id)
    .get_result(conn)
    .await?;
    if legacy.n > 0 {
        return Err(refuse(StatusCode::CONFLICT, UNCLAIMED_LEGACY_AWARD));
    }

    diesel::sql_query(
        "INSERT INTO test_characters (alt_uuid, user_id, marked_by, note) VALUES ($1, $2, $3, $4)",
    )
    .bind::<SqlUuid, _>(body.alt_uuid)
    .bind::<SqlUuid, _>(body.user_id)
    .bind::<Text, _>(actor)
    .bind::<Nullable<Text>, _>(body.note.as_deref())
    .execute(conn)
    .await?;

    let previous_name = character.name.clone();
    character.name = test_name(&previous_name);
    character.version += 1;
    let (name, level) = (character.name.clone(), character.level);
    diesel::update(characters::table.filter(characters::id.eq(row.id)))
        .set(characters::character.eq(JsonDbWrapper(character)))
        .execute(conn)
        .await?;

    audit(
        conn,
        body.alt_uuid,
        body.user_id,
        Some(row.id),
        "mark",
        actor,
        json!({ "previousName": previous_name, "name": name, "level": level, "note": body.note }),
    )
    .await?;
    log::info!(
        "[test-character] {actor} marked alt {} of user {} (character {}) as a test character: \
         {previous_name:?} -> {name:?}, level {level}",
        body.alt_uuid,
        body.user_id,
        row.id,
    );

    Ok(MarkTestCharacterResponse {
        alt_uuid: body.alt_uuid,
        character_id: row.id,
        name,
        level,
        newly_marked: true,
    })
}

/// The user's live row, locked, with the alt it is.
async fn lock_live(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> Result<(CharacterDbEntryCharacterAlone, Option<Uuid>), BladeApiError> {
    let row: Option<CharacterDbEntryCharacterAlone> = characters::table
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryCharacterAlone::as_select())
        .for_update()
        .first(conn)
        .await
        .optional()?;
    let row = row.ok_or_else(|| refuse(StatusCode::NOT_FOUND, NO_CHARACTER))?;
    let alt: Option<Uuid> = characters::table
        .filter(characters::id.eq(row.id))
        .select(characters::source_alt_uuid)
        .first(conn)
        .await?;
    Ok((row, alt))
}

// ---- level ----------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetTestCharacterLevelRequest {
    pub level: u16,
    pub actor: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetTestCharacterLevelResponse {
    pub alt_uuid: Uuid,
    pub character_id: Uuid,
    pub name: String,
    pub from_level: u16,
    pub level: u16,
    pub experience: u64,
    pub stamina_attribute_points: u32,
    pub magicka_attribute_points: u32,
    pub abilities_reset: bool,
}

#[post("/blades.bgs.services/api/dev/v1/test-characters/{alt_uuid}/level")]
pub async fn set_test_character_level(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: web::Json<SetTestCharacterLevelRequest>,
) -> Result<Json<SetTestCharacterLevelResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let alt_uuid = path.into_inner();
    let body = body.into_inner();
    let actor = actor_of(&body.actor)?;
    let app_state = app_state.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, NO_CONNECTION))?;
    let data = app_state.level_up_data.clone();
    conn.transaction(move |conn| {
        async move { apply_set_level(conn, alt_uuid, body.level, &actor, &data).await }
            .scope_boxed()
    })
    .await
    .map(Json)
}

async fn apply_set_level(
    conn: &mut AsyncPgConnection,
    alt_uuid: Uuid,
    level: u16,
    actor: &str,
    data: &level_up::LevelUpData,
) -> Result<SetTestCharacterLevelResponse, BladeApiError> {
    // The marker is the ONLY way in: the row written below is found through it.
    let user_id: Option<Uuid> =
        diesel::sql_query("SELECT user_id FROM test_characters WHERE alt_uuid = $1")
            .bind::<SqlUuid, _>(alt_uuid)
            .get_result::<UserIdRow>(conn)
            .await
            .optional()?
            .map(|r| r.user_id);
    let Some(user_id) = user_id else {
        return Err(refuse(StatusCode::FORBIDDEN, NOT_A_TEST_CHARACTER));
    };

    // Live means the user's one character row currently IS this alt. Anything else
    // is another alt's row — the tester's real character, most likely — and is
    // never touched.
    let (row, live_alt) = lock_live(conn, user_id).await?;
    if live_alt != Some(alt_uuid) {
        return Err(refuse(StatusCode::CONFLICT, NOT_LIVE));
    }
    let mut character: CompleteCharacter = row.character.0;
    let done = level_up::set_level(&mut character, level, data).map_err(|e| {
        log::info!("[test-character] set level refused for alt {alt_uuid}: {e:?}");
        refuse(StatusCode::BAD_REQUEST, LEVEL_OUT_OF_RANGE)
    })?;
    let response = SetTestCharacterLevelResponse {
        alt_uuid,
        character_id: row.id,
        name: character.name.clone(),
        from_level: done.from_level,
        level: character.level,
        experience: character.experience,
        stamina_attribute_points: character.stamina_attribute_points,
        magicka_attribute_points: character.magicka_attribute_points,
        abilities_reset: done.abilities_reset,
    };
    let written = diesel::update(
        characters::table
            .filter(characters::id.eq(row.id))
            .filter(characters::source_alt_uuid.eq(alt_uuid)),
    )
    .set(characters::character.eq(JsonDbWrapper(character)))
    .execute(conn)
    .await?;
    if written != 1 {
        return Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL));
    }

    audit(
        conn,
        alt_uuid,
        user_id,
        Some(row.id),
        "set-level",
        actor,
        json!({
            "fromLevel": done.from_level,
            "level": done.to_level,
            "fromExperience": done.from_experience,
            "staminaAttributePoints": done.stamina_attribute_points,
            "magickaAttributePoints": done.magicka_attribute_points,
            "abilitiesReset": done.abilities_reset,
        }),
    )
    .await?;
    log::info!(
        "[test-character] {actor} set alt {alt_uuid} (character {}) level {} -> {} \
         (abilities reset: {})",
        row.id,
        done.from_level,
        done.to_level,
        done.abilities_reset,
    );
    Ok(response)
}

#[derive(QueryableByName)]
struct UserIdRow {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
}

// ---- list -----------------------------------------------------------------------

#[derive(Debug, Serialize, QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct TestCharacterRow {
    #[diesel(sql_type = SqlUuid)]
    pub alt_uuid: Uuid,
    #[diesel(sql_type = SqlUuid)]
    pub user_id: Uuid,
    #[diesel(sql_type = Text)]
    pub marked_by: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub note: Option<String>,
    #[diesel(sql_type = BigInt)]
    pub marked_at: i64,
    /// The alt occupies the user's character row right now.
    #[diesel(sql_type = diesel::sql_types::Bool)]
    pub live: bool,
    #[diesel(sql_type = Nullable<Text>)]
    pub name: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub level: Option<i64>,
}

#[derive(Debug, Serialize, QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct TestCharacterAuditRow {
    #[diesel(sql_type = SqlUuid)]
    pub alt_uuid: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub character_id: Option<Uuid>,
    #[diesel(sql_type = Text)]
    pub action: String,
    #[diesel(sql_type = Text)]
    pub actor: String,
    #[diesel(sql_type = Jsonb)]
    pub detail: Value,
    #[diesel(sql_type = BigInt)]
    pub at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCharactersResponse {
    pub test_characters: Vec<TestCharacterRow>,
    /// Newest first, at most 200.
    pub audit: Vec<TestCharacterAuditRow>,
}

#[get("/blades.bgs.services/api/dev/v1/test-characters")]
pub async fn list_test_characters(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<TestCharactersResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, NO_CONNECTION))?;
    Ok(Json(load_listing(&mut conn).await?))
}

async fn load_listing(
    conn: &mut AsyncPgConnection,
) -> Result<TestCharactersResponse, diesel::result::Error> {
    let test_characters = diesel::sql_query(
        "SELECT t.alt_uuid, t.user_id, t.marked_by, t.note, t.marked_at, \
                (c.id IS NOT NULL) AS live, \
                COALESCE(c.character ->> 'name', saved.name) AS name, \
                COALESCE((c.character ->> 'level')::bigint, saved.level::bigint) AS level \
         FROM test_characters t \
         LEFT JOIN characters c ON c.user_id = t.user_id AND c.source_alt_uuid = t.alt_uuid \
         LEFT JOIN LATERAL ( \
             SELECT v.name, v.level FROM character_versions v \
             WHERE v.user_id = t.user_id AND v.source_alt_uuid = t.alt_uuid \
             ORDER BY v.saved_at DESC LIMIT 1 \
         ) saved ON true \
         ORDER BY t.marked_at DESC, t.alt_uuid",
    )
    .get_results(conn)
    .await?;
    let audit = diesel::sql_query(
        "SELECT alt_uuid, character_id, action, actor, detail, at \
         FROM test_character_audit ORDER BY id DESC LIMIT 200",
    )
    .get_results(conn)
    .await?;
    Ok(TestCharactersResponse {
        test_characters,
        audit,
    })
}

/// The marker tables, as the real migration creates them — for the test
/// fixtures of every module whose ranking queries read them.
#[cfg(test)]
pub(crate) async fn apply_migration_for_tests(conn: &mut AsyncPgConnection) {
    diesel_async::SimpleAsyncConnection::batch_execute(
        conn,
        include_str!("../../migrations/2026-10-16-000001-0000_add_test_characters/up.sql"),
    )
    .await
    .expect("test_characters migration applies");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_is_added_once() {
        assert_eq!(test_name("HauDrauf"), "HauDrauf (Test)");
        assert_eq!(test_name(" HauDrauf (Test) "), "HauDrauf (Test)");
        assert!(is_test_name("Anyone (Test)"));
        assert!(!is_test_name("Tester"));
    }

    #[test]
    fn an_actor_is_required() {
        assert!(actor_of("  ").is_err());
        assert!(actor_of(&"x".repeat(201)).is_err());
        assert_eq!(actor_of(" claude ").unwrap(), "claude");
    }

    /// Every ranking reads the marker. A ranking query that loses its exclusion
    /// puts the test character back on a board, so each is pinned here; the DB
    /// tests below prove the exclusions actually filter.
    #[test]
    fn every_ranking_query_excludes_test_alts() {
        let marker = "FROM test_characters tc WHERE tc.alt_uuid";
        let ranked = crate::arena::leaderboards::RANKED_CTE;
        assert_eq!(ranked.matches(marker).count(), 2, "live AND saved alts");
        assert!(crate::arena::ranking::H2H_REPLAY_ROWS_CTE.contains("FROM test_characters tc"));
        assert!(crate::arena::ranking::h2h_board_sql(true).contains(marker));
        assert!(crate::arena::ranking::h2h_board_sql(false).contains(marker));
        assert!(crate::arena::season_store::FREEZE_STANDINGS_SQL.contains(marker));
    }

    #[test]
    fn an_unlabelled_import_cannot_land_on_a_test_alt() {
        let src = include_str!("admin.rs");
        let body = src.split("pub async fn import_character(").nth(1).unwrap();
        let guard = body
            .find("import_into_test_alt_refusal")
            .expect("the import checks the marker");
        let write = body
            .find("characters::character.eq(entry.character)")
            .unwrap();
        assert!(guard < write, "before the live row is overwritten");
        assert!(body[..guard].contains("incoming_alt.is_none()"));
    }

    #[test]
    fn the_live_guild_trophy_increment_checks_the_marker() {
        let src = include_str!("arena/arena_economy.rs");
        let update = src
            .find("UPDATE guilds SET trophies = GREATEST")
            .expect("the per-match guild increment exists");
        let check = src[..update]
            .rfind("testchar::live_alt_is_test")
            .expect("it checks the marker");
        assert!(update - check < 1200, "the check guards this very update");
    }

    #[test]
    fn the_live_h2h_increment_checks_the_marker() {
        let src = include_str!("arena/arena_economy.rs");
        let body = src
            .split("async fn persist_h2h_rating_pair_locked(")
            .nth(1)
            .expect("the live increment exists");
        let body = &body[..body.find("\n}\n").unwrap()];
        let check = body
            .find("testchar::any_test_alt")
            .expect("it checks the marker");
        let first_write = body.find("upsert_h2h_state").expect("it writes ratings");
        assert!(
            check < first_write,
            "the check comes before any rating is written"
        );
    }

    mod db {
        use super::super::*;
        use diesel_async::SimpleAsyncConnection;

        const SCHEMA: &str = "
            CREATE TABLE characters (
                id UUID PRIMARY KEY,
                user_id UUID NOT NULL UNIQUE,
                character JSONB NOT NULL,
                data JSONB NOT NULL DEFAULT '{}'::jsonb,
                inventory JSONB NOT NULL DEFAULT '{}'::jsonb,
                wallet JSONB NOT NULL DEFAULT '{}'::jsonb,
                town JSONB,
                server_state JSONB NOT NULL DEFAULT '{}'::jsonb,
                source_alt_uuid UUID);
            CREATE TABLE character_versions (
                id UUID PRIMARY KEY, character_id UUID NOT NULL, user_id UUID NOT NULL,
                source_alt_uuid UUID, name TEXT NOT NULL DEFAULT '', level INTEGER NOT NULL DEFAULT 0,
                saved_at BIGINT NOT NULL DEFAULT 0);
            CREATE TABLE arena_match_results (
                id UUID PRIMARY KEY, character_id UUID NOT NULL, source_alt_uuid UUID);
            CREATE TABLE arena_season_standings (
                season_id UUID NOT NULL, character_id UUID NOT NULL, source_alt_uuid UUID);
            CREATE TABLE arena_season_awards (
                season_id UUID NOT NULL, character_id UUID NOT NULL, source_alt_uuid UUID,
                granted_at BIGINT);
        ";

        async fn fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            conn.begin_test_transaction().await.unwrap();
            let schema = format!("t{}", Uuid::new_v4().simple());
            conn.batch_execute(&format!(
                "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};"
            ))
            .await
            .unwrap();
            conn.batch_execute(SCHEMA).await.unwrap();
            conn.batch_execute(include_str!(
                "../../migrations/2026-10-16-000001-0000_add_test_characters/up.sql"
            ))
            .await
            .unwrap();
            Some(conn)
        }

        macro_rules! db {
            () => {
                match fixture().await {
                    Some(c) => c,
                    None => {
                        eprintln!("SKIP: TEST_DATABASE_URL unset — test characters NOT verified");
                        return;
                    }
                }
            };
        }

        fn level_table() -> level_up::LevelUpData {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../deploy/static/level_rewards.json");
            let raw = std::fs::read_to_string(&p).unwrap();
            level_up::LevelUpData::from_json(&serde_json::from_str(&raw).unwrap())
        }

        /// A live character row on `alt` (None = an unstamped row).
        async fn seed(
            conn: &mut AsyncPgConnection,
            id: Uuid,
            user: Uuid,
            alt: Option<Uuid>,
            name: &str,
            level: u16,
        ) {
            let mut ch = CompleteCharacter::default();
            ch.name = name.into();
            ch.level = level;
            ch.experience = 777;
            ch.stamina_attribute_points = u32::from(level.min(50)) - 1;
            diesel::sql_query(
                "INSERT INTO characters (id, user_id, character, source_alt_uuid) VALUES ($1,$2,$3,$4)",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<SqlUuid, _>(user)
            .bind::<Jsonb, _>(serde_json::to_value(ch).unwrap())
            .bind::<Nullable<SqlUuid>, _>(alt)
            .execute(conn)
            .await
            .unwrap();
        }

        async fn live(conn: &mut AsyncPgConnection, id: Uuid) -> CompleteCharacter {
            #[derive(QueryableByName)]
            struct R {
                #[diesel(sql_type = Jsonb)]
                character: Value,
            }
            let r: R = diesel::sql_query("SELECT character FROM characters WHERE id = $1")
                .bind::<SqlUuid, _>(id)
                .get_result(conn)
                .await
                .unwrap();
            serde_json::from_value(r.character).unwrap()
        }

        async fn audit_actions(conn: &mut AsyncPgConnection) -> Vec<String> {
            load_listing(conn)
                .await
                .unwrap()
                .audit
                .into_iter()
                .map(|a| format!("{}:{}", a.action, a.actor))
                .collect()
        }

        fn mark(user: Uuid, alt: Uuid) -> MarkTestCharacterRequest {
            mark_named(user, alt, "Tester")
        }

        fn mark_named(user: Uuid, alt: Uuid, name: &str) -> MarkTestCharacterRequest {
            MarkTestCharacterRequest {
                user_id: user,
                alt_uuid: alt,
                expected_name: name.into(),
                actor: "ops".into(),
                note: Some("loot tuning".into()),
            }
        }

        fn code(e: &BladeApiError) -> String {
            let body = actix_web::ResponseError::error_response(e);
            format!("{}", body.status())
        }

        #[actix_web::test]
        async fn marking_labels_the_live_alt_and_is_audited() {
            let mut c = db!();
            let (id, user, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            seed(&mut c, id, user, Some(alt), "Tester", 48).await;

            let done = apply_mark(&mut c, &mark(user, alt), "ops").await.unwrap();
            assert!(done.newly_marked);
            assert_eq!(done.name, "Tester (Test)");
            assert_eq!(live(&mut c, id).await.name, "Tester (Test)");
            assert!(any_test_alt(&mut c, &[alt]).await.unwrap());
            assert_eq!(audit_actions(&mut c).await, ["mark:ops"]);

            // Idempotent: a second mark writes nothing.
            let again = apply_mark(&mut c, &mark(user, alt), "ops").await.unwrap();
            assert!(!again.newly_marked);
            assert_eq!(live(&mut c, id).await.name, "Tester (Test)");
            assert_eq!(audit_actions(&mut c).await.len(), 1);
        }

        /// The safety rails on marking. Each refusal names its own reason and
        /// leaves the character and the marker table exactly as they were.
        #[actix_web::test]
        async fn a_real_character_cannot_be_marked() {
            let mut c = db!();
            let refused = |e: BladeApiError| e.error_code();

            // The control: a fresh, correctly named alt CAN be marked; the
            // refusals below are therefore each down to their one difference.
            let (id, user, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            seed(&mut c, id, user, Some(alt), "Fresh", 48).await;
            assert_eq!(
                refused(
                    apply_mark(&mut c, &mark_named(user, alt, "Wrong"), "ops")
                        .await
                        .unwrap_err()
                ),
                NAME_MISMATCH,
                "a name that is not the live character's"
            );
            assert!(
                apply_mark(&mut c, &mark_named(user, alt, "Fresh"), "ops")
                    .await
                    .is_ok()
            );

            // The original character: alt id == row id (an imported capture).
            let (id, user) = (Uuid::new_v4(), Uuid::new_v4());
            seed(&mut c, id, user, Some(id), "HauDrauf", 100).await;
            let e = apply_mark(&mut c, &mark_named(user, id, "HauDrauf"), "ops")
                .await
                .unwrap_err();
            assert_eq!(refused(e), ORIGINAL_CHARACTER);

            // An alt with arena history, stamped or legacy (NULL = the live alt).
            for stamped in [true, false] {
                let (id, user, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
                seed(&mut c, id, user, Some(alt), "Veteran", 60).await;
                diesel::sql_query(
                    "INSERT INTO arena_match_results (id, character_id, source_alt_uuid) VALUES ($1,$2,$3)",
                )
                .bind::<SqlUuid, _>(Uuid::new_v4())
                .bind::<SqlUuid, _>(id)
                .bind::<Nullable<SqlUuid>, _>(stamped.then_some(alt))
                .execute(&mut c)
                .await
                .unwrap();
                let e = apply_mark(&mut c, &mark_named(user, alt, "Veteran"), "ops")
                    .await
                    .unwrap_err();
                assert_eq!(refused(e), HAS_ARENA_HISTORY, "stamped={stamped}");
                assert_eq!(live(&mut c, id).await.name, "Veteran");
            }

            // An unclaimed legacy (NULL-alt) award the test alt could collect.
            let (id, user, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            seed(&mut c, id, user, Some(alt), "Laureate", 48).await;
            diesel::sql_query(
                "INSERT INTO arena_season_awards (season_id, character_id) VALUES ($1, $2)",
            )
            .bind::<SqlUuid, _>(Uuid::new_v4())
            .bind::<SqlUuid, _>(id)
            .execute(&mut c)
            .await
            .unwrap();
            let e = apply_mark(&mut c, &mark_named(user, alt, "Laureate"), "ops")
                .await
                .unwrap_err();
            assert_eq!(refused(e), UNCLAIMED_LEGACY_AWARD);
            // …a claimed one is harmless.
            diesel::sql_query(
                "UPDATE arena_season_awards SET granted_at = 1 WHERE character_id = $1",
            )
            .bind::<SqlUuid, _>(id)
            .execute(&mut c)
            .await
            .unwrap();
            assert!(
                apply_mark(&mut c, &mark_named(user, alt, "Laureate"), "ops")
                    .await
                    .is_ok()
            );

            // An alt that is not live (parked in a version).
            let (id, user, live_alt, parked) = (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            seed(&mut c, id, user, Some(live_alt), "Main", 90).await;
            let e = apply_mark(&mut c, &mark_named(user, parked, "Main"), "ops")
                .await
                .unwrap_err();
            assert_eq!(refused(e), NOT_LIVE);

            let marked: Vec<_> = load_listing(&mut c)
                .await
                .unwrap()
                .test_characters
                .into_iter()
                .map(|t| t.name.unwrap_or_default())
                .collect();
            assert_eq!(marked.len(), 2, "only the two controls: {marked:?}");
            assert!(marked.iter().all(|n| n.ends_with("(Test)")));
        }

        #[actix_web::test]
        async fn live_alt_is_test_follows_the_alt_not_the_row() {
            let mut c = db!();
            let (id, user, test_alt, real_alt) = (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            seed(&mut c, id, user, Some(test_alt), "Tester", 48).await;
            assert!(!live_alt_is_test(&mut c, id).await.unwrap(), "unmarked");
            apply_mark(&mut c, &mark(user, test_alt), "ops")
                .await
                .unwrap();
            assert!(live_alt_is_test(&mut c, id).await.unwrap());
            diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
                .bind::<SqlUuid, _>(real_alt)
                .bind::<SqlUuid, _>(id)
                .execute(&mut c)
                .await
                .unwrap();
            assert!(
                !live_alt_is_test(&mut c, id).await.unwrap(),
                "the real alt is live again"
            );
        }

        #[actix_web::test]
        async fn setting_the_level_writes_only_the_marked_live_alt() {
            let mut c = db!();
            let data = level_table();
            let (id, user, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            seed(&mut c, id, user, Some(alt), "Tester", 48).await;
            apply_mark(&mut c, &mark(user, alt), "ops").await.unwrap();

            let done = apply_set_level(&mut c, alt, 90, "ops", &data)
                .await
                .unwrap();
            assert_eq!((done.from_level, done.level, done.experience), (48, 90, 0));
            let ch = live(&mut c, id).await;
            assert_eq!((ch.level, ch.experience), (90, 0));
            assert_eq!(
                ch.stamina_attribute_points + ch.magicka_attribute_points,
                49
            );
            assert_eq!(ch.name, "Tester (Test)");

            let e = apply_set_level(&mut c, alt, 0, "ops", &data)
                .await
                .unwrap_err();
            assert_eq!(code(&e), "400 Bad Request");
            let e = apply_set_level(&mut c, alt, 101, "ops", &data)
                .await
                .unwrap_err();
            assert_eq!(code(&e), "400 Bad Request");
            assert_eq!(
                live(&mut c, id).await.level,
                90,
                "a refusal changes nothing"
            );

            assert_eq!(audit_actions(&mut c).await, ["set-level:ops", "mark:ops"]);
        }

        #[actix_web::test]
        async fn an_unmarked_or_switched_out_alt_is_refused() {
            let mut c = db!();
            let data = level_table();

            // Not marked at all — the tester's real character.
            let (id, user) = (Uuid::new_v4(), Uuid::new_v4());
            seed(&mut c, id, user, Some(id), "HauDrauf", 100).await;
            let e = apply_set_level(&mut c, id, 10, "ops", &data)
                .await
                .unwrap_err();
            assert_eq!(code(&e), "403 Forbidden");
            assert_eq!(live(&mut c, id).await.level, 100);

            // Marked, but the user has switched back to his real character: the live
            // row is NOT the test alt and must not be written.
            let (id, user, test_alt, real_alt) = (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            seed(&mut c, id, user, Some(test_alt), "Tester", 48).await;
            apply_mark(&mut c, &mark(user, test_alt), "ops")
                .await
                .unwrap();
            diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
                .bind::<SqlUuid, _>(real_alt)
                .bind::<SqlUuid, _>(id)
                .execute(&mut c)
                .await
                .unwrap();
            let e = apply_set_level(&mut c, test_alt, 10, "ops", &data)
                .await
                .unwrap_err();
            assert_eq!(code(&e), "409 Conflict");
            assert_eq!(
                live(&mut c, id).await.level,
                48,
                "the real alt's row is untouched"
            );
            assert_eq!(audit_actions(&mut c).await, ["mark:ops"]);

            let listed = load_listing(&mut c).await.unwrap().test_characters;
            assert_eq!(listed.len(), 1);
            assert!(!listed[0].live, "listed as switched out");
        }
    }
}
