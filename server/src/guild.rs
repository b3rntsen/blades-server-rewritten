//! Guilds — create / view / search / leaderboard / join / leave / kick / chat / exchange.
//!
//! `GET  /guilds/current`                     — the requester's guild (or null)
//! `GET  /guilds/{id}`                         — a specific guild
//! `GET  /guilds/search`                       — discover guilds
//! `GET  /guilds/leaderboard`                  — guilds by trophies
//! `POST /guilds`                              — create a guild (creator = LEADER)
//! `POST /guilds/{id}/join`                    — join a guild
//! `POST /guilds/current/leave`               — leave the current guild
//! `POST /guilds/current/kick/{memberId}`     — kick a member (LEADER/OFFICER)
//! `GET  /guilds/current/messages`            — the guild message board
//! `POST /guilds/current/messages`            — post a CLIENT chat message
//! `GET  /guilds/current/exchanges`           — list guild exchanges
//! `POST /guilds/current/exchanges`           — create an exchange request
//! `POST /guilds/current/exchanges/donate`    — donate to an exchange (debits donor)
//! `POST /guilds/current/exchanges/redeem`    — redeem donated items (credits requester)
//!
//! Guild ids are 24-hex Mongo ObjectId strings (retail). Membership lives in
//! `guild_members` (a user has one character, so one membership); the board is a
//! typed message log (JOIN/KICK/CLIENT).

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{apply_reward, consume_stackable, RewardGrant};
use blades_lib::user_data::{
    CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
    InventoryChangeTracker,
};
use diesel::prelude::*;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, json_db::JsonDbWrapper, models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe, util::check_permission_for_character_and_get_it,
};

const GUILD_SERVICE_ID: u64 = 9008;
const SEARCH_LIMIT: i64 = 50;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Derive a 24-hex (Mongo ObjectId-style) guild id from a uuid.
fn guild_id_from_uuid(u: Uuid) -> String {
    u.simple().to_string()[..24].to_string()
}

// ---- Diesel rows ---------------------------------------------------------------

#[derive(Queryable, Selectable, Insertable, AsChangeset, Clone)]
#[diesel(table_name = crate::schema::guilds)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct GuildRow {
    id: String,
    name: String,
    tag_id: String,
    guild_type: String,
    short_description: String,
    long_description: String,
    badge_icon_index: i32,
    region_index: i32,
    trophies: i64,
    created_at: i64,
}

#[derive(Queryable, Selectable, Insertable, Clone)]
#[diesel(table_name = crate::schema::guild_members)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct GuildMemberRow {
    guild_id: String,
    user_id: Uuid,
    character_id: Uuid,
    rank: String,
    join_date: i64,
}

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = crate::schema::guild_messages)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct GuildMessageRow {
    message_id: String,
    guild_id: String,
    user_id: Uuid,
    character_id: Uuid,
    message_type: String,
    type_specific_data: JsonDbWrapper<Value>,
    creation_time: i64,
}

// ---- Wire shapes ---------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildWire {
    id: String,
    name: String,
    tag_id: String,
    #[serde(rename = "type")]
    guild_type: String,
    short_description: String,
    long_description: String,
    badge_icon_index: i32,
    region_index: i32,
    trophies: i64,
    member_count: i64,
}

impl GuildWire {
    fn from_row(row: GuildRow, member_count: i64) -> Self {
        GuildWire {
            id: row.id,
            name: row.name,
            tag_id: row.tag_id,
            guild_type: row.guild_type,
            short_description: row.short_description,
            long_description: row.long_description,
            badge_icon_index: row.badge_icon_index,
            region_index: row.region_index,
            trophies: row.trophies,
            member_count,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberWire {
    user_id: Uuid,
    guild_id: String,
    rank: String,
    join_date: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageWire {
    message_id: String,
    guild_id: String,
    user_id: Uuid,
    character_id: Uuid,
    type_specific_data: Value,
    creation_time: i64,
    #[serde(rename = "type")]
    message_type: String,
}

impl MessageWire {
    fn from_row(row: GuildMessageRow) -> Self {
        MessageWire {
            message_id: row.message_id,
            guild_id: row.guild_id,
            user_id: row.user_id,
            character_id: row.character_id,
            type_specific_data: row.type_specific_data.0,
            creation_time: row.creation_time,
            message_type: row.message_type,
        }
    }
}

// ---- DB helpers ----------------------------------------------------------------

async fn member_count(conn: &mut AsyncPgConnection, gid: &str) -> Result<i64, BladeApiError> {
    use crate::schema::guild_members::dsl::*;
    Ok(guild_members
        .filter(guild_id.eq(gid))
        .count()
        .get_result(conn)
        .await?)
}

async fn find_membership(
    conn: &mut AsyncPgConnection,
    uid: Uuid,
) -> Result<Option<GuildMemberRow>, BladeApiError> {
    use crate::schema::guild_members::dsl::*;
    Ok(guild_members
        .filter(user_id.eq(uid))
        .select(GuildMemberRow::as_select())
        .load(conn)
        .await?
        .into_iter()
        .next())
}

async fn load_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Option<GuildRow>, BladeApiError> {
    use crate::schema::guilds::dsl::*;
    Ok(guilds
        .filter(id.eq(gid))
        .select(GuildRow::as_select())
        .load(conn)
        .await?
        .into_iter()
        .next())
}

async fn append_message(
    conn: &mut AsyncPgConnection,
    gid: &str,
    uid: Uuid,
    cid: Uuid,
    message_type: &str,
    data: Value,
) -> Result<(), BladeApiError> {
    use crate::schema::guild_messages;
    let ts = now_secs();
    let row = GuildMessageRow {
        message_id: format!("{}::{}", ts, Uuid::new_v4()),
        guild_id: gid.to_string(),
        user_id: uid,
        character_id: cid,
        message_type: message_type.to_string(),
        type_specific_data: JsonDbWrapper(data),
        creation_time: ts,
    };
    diesel::insert_into(guild_messages::table)
        .values(row)
        .execute(conn)
        .await?;
    Ok(())
}

// ---- Handlers ------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildResponse {
    guild: Option<GuildWire>,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current")]
pub async fn get_current_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GuildResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let guild = match find_membership(&mut conn, session.session.user_id).await? {
        Some(m) => match load_guild(&mut conn, &m.guild_id).await? {
            Some(g) => {
                let count = member_count(&mut conn, &g.id).await?;
                Some(GuildWire::from_row(g, count))
            }
            None => None,
        },
        None => None,
    };
    Ok(Json(GuildResponse { guild }))
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/{guild_id}")]
pub async fn get_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, String)>,
) -> Result<Json<GuildResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, gid) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let guild = match load_guild(&mut conn, &gid).await? {
        Some(g) => {
            let count = member_count(&mut conn, &g.id).await?;
            Some(GuildWire::from_row(g, count))
        }
        None => return Err(BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1)),
    };
    Ok(Json(GuildResponse { guild }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildListResponse {
    guilds: Vec<GuildWire>,
}

async fn list_guilds(
    conn: &mut AsyncPgConnection,
    by_trophies: bool,
) -> Result<Vec<GuildWire>, BladeApiError> {
    use crate::schema::guilds::dsl::*;
    let rows: Vec<GuildRow> = if by_trophies {
        guilds
            .order(trophies.desc())
            .limit(SEARCH_LIMIT)
            .select(GuildRow::as_select())
            .load(conn)
            .await?
    } else {
        guilds
            .limit(SEARCH_LIMIT)
            .select(GuildRow::as_select())
            .load(conn)
            .await?
    };
    let mut out = Vec::with_capacity(rows.len());
    for g in rows {
        let count = member_count(conn, &g.id).await?;
        out.push(GuildWire::from_row(g, count));
    }
    Ok(out)
}

/// `GET /guilds/search` — discover guilds (filters accepted but not applied; the
/// client filters the returned set).
#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/search")]
pub async fn search_guilds(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GuildListResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;
    Ok(Json(GuildListResponse {
        guilds: list_guilds(&mut conn, false).await?,
    }))
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/leaderboard")]
pub async fn guild_leaderboard(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GuildListResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;
    Ok(Json(GuildListResponse {
        guilds: list_guilds(&mut conn, true).await?,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateGuildRequest {
    #[serde(default)]
    guild_name: String,
    #[serde(default)]
    guild_type: Option<String>,
    #[serde(default)]
    short_description: String,
    #[serde(default)]
    long_description: String,
    #[serde(default)]
    badge_icon_index: i32,
    #[serde(default)]
    region_index: i32,
}

/// `POST /guilds` — create a guild; the creator joins as LEADER.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds")]
pub async fn create_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<CreateGuildRequest>,
) -> Result<Json<GuildResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    if find_membership(&mut conn, user_id).await?.is_some() {
        // Already in a guild — must leave first.
        return Err(BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 2));
    }

    let gid = guild_id_from_uuid(Uuid::new_v4());
    let ts = now_secs();
    let row = GuildRow {
        id: gid.clone(),
        name: body.guild_name,
        tag_id: format!("{:04}", (ts % 10000)),
        guild_type: body.guild_type.unwrap_or_else(|| "OPEN".to_string()),
        short_description: body.short_description,
        long_description: body.long_description,
        badge_icon_index: body.badge_icon_index,
        region_index: body.region_index,
        trophies: 0,
        created_at: ts,
    };
    {
        use crate::schema::guilds;
        diesel::insert_into(guilds::table)
            .values(&row)
            .execute(&mut conn)
            .await?;
    }
    {
        use crate::schema::guild_members;
        diesel::insert_into(guild_members::table)
            .values(GuildMemberRow {
                guild_id: gid.clone(),
                user_id,
                character_id,
                rank: "LEADER".to_string(),
                join_date: ts,
            })
            .execute(&mut conn)
            .await?;
    }
    Ok(Json(GuildResponse {
        guild: Some(GuildWire::from_row(row, 1)),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberResponse {
    member: MemberWire,
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/{guild_id}/join")]
pub async fn join_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, String)>,
    _body: Json<Option<Value>>,
) -> Result<Json<MemberResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, gid) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    if find_membership(&mut conn, user_id).await?.is_some() {
        return Err(BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 2));
    }
    if load_guild(&mut conn, &gid).await?.is_none() {
        return Err(BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1));
    }

    let ts = now_secs();
    let member = GuildMemberRow {
        guild_id: gid.clone(),
        user_id,
        character_id,
        rank: "MEMBER".to_string(),
        join_date: ts,
    };
    {
        use crate::schema::guild_members;
        diesel::insert_into(guild_members::table)
            .values(&member)
            .execute(&mut conn)
            .await?;
    }
    append_message(&mut conn, &gid, user_id, character_id, "JOIN", json!({})).await?;

    Ok(Json(MemberResponse {
        member: MemberWire {
            user_id,
            guild_id: gid,
            rank: member.rank,
            join_date: ts,
        },
    }))
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/leave")]
pub async fn leave_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    _body: Json<Option<Value>>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    if let Some(m) = find_membership(&mut conn, user_id).await? {
        {
            use crate::schema::guild_members::dsl as gm;
            diesel::delete(gm::guild_members.filter(gm::user_id.eq(user_id)))
                .execute(&mut conn)
                .await?;
        }
        append_message(&mut conn, &m.guild_id, user_id, character_id, "LEAVE", json!({})).await?;
    }
    Ok(Json(json!({})))
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/kick/{member_id}"
)]
pub async fn kick_member(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, member_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let me = find_membership(&mut conn, user_id)
        .await?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1))?;
    if me.rank != "LEADER" && me.rank != "OFFICER" {
        return Err(BladeApiError::unauthorized());
    }
    {
        use crate::schema::guild_members::dsl::*;
        diesel::delete(
            guild_members
                .filter(guild_id.eq(&me.guild_id))
                .filter(user_id.eq(member_id)),
        )
        .execute(&mut conn)
        .await?;
    }
    append_message(
        &mut conn,
        &me.guild_id,
        user_id,
        character_id,
        "KICK",
        json!({ "type": "KICK", "kickedUserId": member_id }),
    )
    .await?;
    Ok(Json(json!({})))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageBoardResponse {
    guild_message_board: Vec<MessageWire>,
}

async fn message_board(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<MessageWire>, BladeApiError> {
    use crate::schema::guild_messages::dsl::*;
    let rows: Vec<GuildMessageRow> = guild_messages
        .filter(guild_id.eq(gid))
        .order(creation_time.desc())
        .limit(100)
        .select(GuildMessageRow::as_select())
        .load(conn)
        .await?;
    Ok(rows.into_iter().map(MessageWire::from_row).collect())
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/messages")]
pub async fn get_messages(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<MessageBoardResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let board = match find_membership(&mut conn, session.session.user_id).await? {
        Some(m) => message_board(&mut conn, &m.guild_id).await?,
        None => Vec::new(),
    };
    Ok(Json(MessageBoardResponse {
        guild_message_board: board,
    }))
}

#[derive(Deserialize)]
struct PostMessageRequest {
    #[serde(default)]
    text: String,
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/messages")]
pub async fn post_message(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<PostMessageRequest>,
) -> Result<Json<MessageBoardResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let text = body.into_inner().text;
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let m = find_membership(&mut conn, user_id)
        .await?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1))?;
    append_message(
        &mut conn,
        &m.guild_id,
        user_id,
        character_id,
        "CLIENT",
        json!({ "type": "CLIENT", "text": text }),
    )
    .await?;
    Ok(Json(MessageBoardResponse {
        guild_message_board: message_board(&mut conn, &m.guild_id).await?,
    }))
}

// ---- Guild Exchange (gift) -------------------------------------------------------

/// A single donation entry stored inside the `donations` JSONB array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Donation {
    donator_user_id: Uuid,
    donator_character_id: Uuid,
    donated_amount: i64,
}

#[derive(Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = crate::schema::guild_exchanges)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct GuildExchangeRow {
    id: String,
    guild_id: String,
    requester_user_id: Uuid,
    requester_character_id: Uuid,
    item_template_id: Uuid,
    requested_amount: i64,
    max_donation_amount: i64,
    donations: JsonDbWrapper<Vec<Donation>>,
    donation_sum: i64,
    creation_time: i64,
    redeemed: bool,
}

/// Wire shape for a single guild exchange (used in list + create responses).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildExchangeWire {
    guild_id: String,
    requester_user_id: Uuid,
    requester_character_id: Uuid,
    item_template_id: Uuid,
    requested_amount: i64,
    max_donation_amount: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    donations: Option<Vec<Donation>>,
    creation_time: i64,
    donation_sum: i64,
}

impl GuildExchangeWire {
    fn from_row(row: &GuildExchangeRow, include_donations: bool) -> Self {
        GuildExchangeWire {
            guild_id: row.guild_id.clone(),
            requester_user_id: row.requester_user_id,
            requester_character_id: row.requester_character_id,
            item_template_id: row.item_template_id,
            requested_amount: row.requested_amount,
            max_donation_amount: row.max_donation_amount,
            donations: if include_donations {
                Some(row.donations.0.clone())
            } else {
                None
            },
            creation_time: row.creation_time,
            donation_sum: row.donation_sum,
        }
    }
}

/// Load all non-redeemed exchanges for a guild.
async fn load_exchanges(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<GuildExchangeRow>, BladeApiError> {
    use crate::schema::guild_exchanges::dsl::*;
    Ok(guild_exchanges
        .filter(guild_id.eq(gid))
        .filter(redeemed.eq(false))
        .select(GuildExchangeRow::as_select())
        .load(conn)
        .await?)
}

/// Load economy entry for the session character (must be owned by the session user).
async fn load_economy(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    user_id: Uuid,
) -> Result<CharacterDbEntryEconomy, BladeApiError> {
    use crate::schema::characters;
    characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryEconomy::as_select())
        .for_no_key_update()
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 10))
}

async fn write_economy(
    conn: &mut AsyncPgConnection,
    entry: CharacterDbEntryEconomy,
) -> Result<(), BladeApiError> {
    use crate::schema::characters;
    diesel::update(characters::table)
        .filter(characters::id.eq(entry.id))
        .set(entry)
        .execute(conn)
        .await?;
    Ok(())
}

// ---- Exchange handlers -------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeListResponse {
    guild_exchanges: Vec<GuildExchangeWire>,
}

/// `GET /guilds/current/exchanges` — list all active (non-redeemed) exchanges in
/// the caller's guild.
#[get(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/exchanges"
)]
pub async fn list_exchanges(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<ExchangeListResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let m = find_membership(&mut conn, session.session.user_id)
        .await?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1))?;

    let rows = load_exchanges(&mut conn, &m.guild_id).await?;
    let wires = rows
        .iter()
        .map(|r| GuildExchangeWire::from_row(r, true))
        .collect();
    Ok(Json(ExchangeListResponse {
        guild_exchanges: wires,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateExchangeRequest {
    item_template_id: Uuid,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateExchangeResponse {
    guild_exchange: GuildExchangeWire,
}

/// `POST /guilds/current/exchanges` — create an exchange request (requestedAmount=10,
/// maxDonationAmount=5, donationSum=0).
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/exchanges"
)]
pub async fn create_exchange(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<CreateExchangeRequest>,
) -> Result<Json<CreateExchangeResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let item_template_id = body.into_inner().item_template_id;
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let m = find_membership(&mut conn, user_id)
        .await?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1))?;

    let ts = now_secs();
    let row = GuildExchangeRow {
        id: Uuid::new_v4().to_string(),
        guild_id: m.guild_id,
        requester_user_id: user_id,
        requester_character_id: character_id,
        item_template_id,
        requested_amount: 10,
        max_donation_amount: 5,
        donations: JsonDbWrapper(vec![]),
        donation_sum: 0,
        creation_time: ts,
        redeemed: false,
    };
    {
        use crate::schema::guild_exchanges;
        diesel::insert_into(guild_exchanges::table)
            .values(&row)
            .execute(&mut conn)
            .await?;
    }
    Ok(Json(CreateExchangeResponse {
        guild_exchange: GuildExchangeWire::from_row(&row, false),
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DonateRequest {
    requester_user_id: Uuid,
    requester_character_id: Uuid,
    item_template_id: Uuid,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DonateResponse {
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
    character: CompleteCharacterWithIdWithoutData,
}

/// `POST /guilds/current/exchanges/donate` — donate `maxDonationAmount` of the
/// `itemTemplateId` stackable from the donor's backpack. The donor must be in the same
/// guild as the requester. The item is debited from the donor's inventory.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/exchanges/donate"
)]
pub async fn donate_exchange(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<DonateRequest>,
) -> Result<Json<DonateResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let donor_user_id = session.session.user_id;
    let donor_character_id = path.into_inner();
    let req = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, donor_character_id)
        .await?;

    // Donor must be a guild member.
    let m = find_membership(&mut conn, donor_user_id)
        .await?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1))?;

    conn.transaction(move |conn| {
        async move {
            // Find the exchange (must be in same guild, not redeemed).
            use crate::schema::guild_exchanges::dsl as ge;
            let exchange: GuildExchangeRow = ge::guild_exchanges
                .filter(ge::guild_id.eq(&m.guild_id))
                .filter(ge::requester_user_id.eq(req.requester_user_id))
                .filter(ge::requester_character_id.eq(req.requester_character_id))
                .filter(ge::item_template_id.eq(req.item_template_id))
                .filter(ge::redeemed.eq(false))
                .select(GuildExchangeRow::as_select())
                .for_no_key_update()
                .load(conn)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 11))?;

            let donate_amount = exchange.max_donation_amount as u64;

            // Debit the donor's stackable.
            let mut entry = load_economy(conn, donor_character_id, donor_user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            consume_stackable(
                &mut entry.inventory.0,
                exchange.item_template_id,
                donate_amount,
                &mut tracker,
            )
            .map_err(BladeApiError::from_economy)?;
            entry.inventory.0.backpack_version += 1;

            let inventory_update = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let character_out = CompleteCharacterWithIdWithoutData {
                id: entry.id,
                character: entry.character.0.clone(),
            };
            write_economy(conn, entry).await?;

            // Update the exchange row: append donation + update sum.
            let mut donations = exchange.donations.0.clone();
            donations.push(Donation {
                donator_user_id: donor_user_id,
                donator_character_id: donor_character_id,
                donated_amount: donate_amount as i64,
            });
            let new_sum = exchange.donation_sum + donate_amount as i64;
            diesel::update(ge::guild_exchanges.filter(ge::id.eq(&exchange.id)))
                .set((
                    ge::donations.eq(JsonDbWrapper(donations)),
                    ge::donation_sum.eq(new_sum),
                ))
                .execute(conn)
                .await?;

            Ok::<_, BladeApiError>(Json(DonateResponse {
                wallet,
                inventory: inventory_update,
                character: character_out,
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildExchangeRedeemReward {
    stackable_items: std::collections::HashMap<Uuid, i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildExchangeRedeemInfo {
    reward: GuildExchangeRedeemReward,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RedeemResponse {
    inventory: CompleteInventoryUpdate,
    guild_exchange_redeem: GuildExchangeRedeemInfo,
}

/// `POST /guilds/current/exchanges/redeem` — redeem all of the session user's
/// non-redeemed exchanges that have a donationSum > 0. Credits the requester the
/// donated stackables and marks each exchange as redeemed.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/exchanges/redeem"
)]
pub async fn redeem_exchange(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    _body: Json<Option<Value>>,
) -> Result<Json<RedeemResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    conn.transaction(move |conn| {
        async move {
            // Load all non-redeemed exchanges for this user with sum > 0.
            use crate::schema::guild_exchanges::dsl as ge;
            let exchanges: Vec<GuildExchangeRow> = ge::guild_exchanges
                .filter(ge::requester_user_id.eq(user_id))
                .filter(ge::requester_character_id.eq(character_id))
                .filter(ge::redeemed.eq(false))
                .filter(ge::donation_sum.gt(0))
                .select(GuildExchangeRow::as_select())
                .for_no_key_update()
                .load(conn)
                .await?;

            let mut entry = load_economy(conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            let mut reward_stackables: std::collections::HashMap<Uuid, i64> =
                std::collections::HashMap::new();

            for ex in &exchanges {
                let amount = ex.donation_sum as u64;
                let reward = RewardGrant {
                    stackable_items: std::collections::HashMap::from([(
                        ex.item_template_id,
                        amount,
                    )]),
                    ..Default::default()
                };
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                *reward_stackables
                    .entry(ex.item_template_id)
                    .or_insert(0) += ex.donation_sum;
            }
            entry.inventory.0.backpack_version += 1;

            let inventory_update = entry.inventory.0.generate_client_update(&tracker);
            write_economy(conn, entry).await?;

            // Mark all redeemed.
            for ex in &exchanges {
                diesel::update(ge::guild_exchanges.filter(ge::id.eq(&ex.id)))
                    .set(ge::redeemed.eq(true))
                    .execute(conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(RedeemResponse {
                inventory: inventory_update,
                guild_exchange_redeem: GuildExchangeRedeemInfo {
                    reward: GuildExchangeRedeemReward {
                        stackable_items: reward_stackables,
                    },
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_id_is_24_hex() {
        let id = guild_id_from_uuid(Uuid::from_u128(0x1234_5678_9abc_def0_1122_3344_5566_7788));
        assert_eq!(id.len(), 24);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
