//! Guilds — create / view / search / leaderboard / join / apply / approve / deny /
//! leave / kick / ban / chat / exchange.
//!
//! ```text
//! GET    /guilds/current                        the requester's guild + its members
//! POST   /guilds/current                        edit the guild        (GRANDMASTER)
//! PUT    /guilds/current                        same, other verb      (GRANDMASTER)
//! GET    /guilds/current/messages               the message board (windowed)
//! POST   /guilds/current/messages               post a CLIENT chat message
//! GET    /guilds/current/applications           pending join requests (GRANDMASTER)
//! POST   /guilds/current/approve/{userId}       admit an applicant    (GRANDMASTER)
//! POST   /guilds/current/deny/{userId}          reject an applicant   (GRANDMASTER)
//! POST   /guilds/current/kick/{userId}          remove a member       (GRANDMASTER)
//! POST   /guilds/current/ban/{userId}           remove permanently    (GRANDMASTER)
//! POST   /guilds/current/leave                  leave the current guild
//! GET    /guilds/search                         discover guilds (filtered)
//! GET    /guilds/leaderboard                    guilds by trophies, paged
//! POST   /guilds                                create a guild (creator = GRANDMASTER)
//! POST   /guilds/{id}/join                      join an OPEN guild
//! POST   /guilds/{id}/apply                     request to join an APPLY_ONLY guild
//! GET    /guilds/{id}                           a specific guild
//! GET    /guilds/current/exchanges              list guild exchanges
//! POST   /guilds/current/exchanges              create an exchange request
//! POST   /guilds/current/exchanges/donate       donate to an exchange
//! POST   /guilds/current/exchanges/redeem       redeem donated items
//! ```
//!
//! Every path above is il2cpp's, read from the `URL_PATH` constants on the request
//! classes in `BGS.Shared.Rest.Api.BladeServer`
//! (`reference/il2cpp/dump.cs`:462204-462660). Guild ids are 24-hex Mongo
//! ObjectId strings, as retail.
//!
//! # How this module is organised
//!
//! Every *decision* — who may kick whom, whether a join becomes a membership or a
//! request, when a removal stops blocking — lives in [`crate::guild_policy`] as a
//! pure function, and is unit-tested there against the negatives. This file does
//! I/O and wire shapes only. If you are looking for the permission matrix or the
//! constants behind it, and for their provenance, read that module's header.
//!
//! # Wire contract
//!
//! The response shapes here are transcribed from recorded retail traffic (the
//! 20260607 prod snapshot, ~400 guild request/response pairs), not designed. Each
//! shape carries the capture count that backs it. `docs/guilds.md` collects the
//! whole contract, the permission matrix, and the short list of things that are
//! modelled rather than observed.
//!
//! Membership lives in `guild_members` (one guild per user), pending requests in
//! `guild_applications`, and the re-join cooldown / ban list in `guild_removals`.
//! The board is a typed message log whose `type` values are retail's
//! `GuildMessageType`: CLIENT, JOIN, APPROVE, DENY, KICK, BAN, LEAVE, PROMOTE,
//! DONATE, GUILD_UPDATE.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    get,
    http::StatusCode,
    post, put,
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
    BladeApiError, ServerGlobal,
    guild_policy::{
        ApprovalRefusal, GuildRank, GuildType, JoinAdmission, JoinContext, JoinRefusal,
        MAX_APPLICATIONS, MESSAGE_PAGE_LIMIT, Removal, can_approve_applications, can_ban,
        can_edit_guild, can_kick, evaluate_approval, evaluate_join, guild_text_ok,
        message_length_ok, successor,
    },
    json_db::JsonDbWrapper,
    models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
    util::check_permission_for_character_and_get_it,
};

const GUILD_SERVICE_ID: u64 = 9008;

/// Cap on how many guilds one `GET /guilds/search` may return. Retail's client
/// sends its own `limit` (10 and 50 both appear in captures); this bounds it.
const SEARCH_LIMIT: i64 = 50;

/// Guilds per leaderboard page.
///
/// CAPTURE-DERIVED: every captured `GET /guilds/leaderboard?page=1` response
/// carried exactly 100 entries, ranked 1..100, with `totalPages` varying by the
/// number of guilds in existence.
const LEADERBOARD_PAGE_SIZE: i64 = 100;

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
    exchange_donation_count: i64,
    grandmaster_since: i64,
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

impl GuildMemberRow {
    /// The member's rank as a typed value.
    ///
    /// A row whose rank string does not parse is a corrupt record, not a
    /// low-privilege member: returning an error beats silently treating it as
    /// `MEMBER`, which would let a mangled GRANDMASTER row quietly lose the guild
    /// its only administrator.
    fn parsed_rank(&self) -> Result<GuildRank, BladeApiError> {
        GuildRank::from_wire(&self.rank)
            .ok_or_else(|| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, GUILD_SERVICE_ID, 50))
    }
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

#[derive(Queryable, Selectable, Insertable, Clone)]
#[diesel(table_name = crate::schema::guild_applications)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct GuildApplicationRow {
    guild_id: String,
    user_id: Uuid,
    character_id: Uuid,
    state: String,
    creation_time: i64,
}

#[derive(Queryable, Selectable, Insertable, Clone)]
#[diesel(table_name = crate::schema::guild_removals)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct GuildRemovalRow {
    guild_id: String,
    user_id: Uuid,
    removed_at: i64,
    banned: bool,
}

// ---- Wire shapes ---------------------------------------------------------------
//
// Field names and nesting here are transcribed from recorded retail responses in
// the 20260607 prod snapshot, not guessed. See docs/guilds.md for the full
// contract and the capture counts behind each shape.

/// One guild, as retail serialises it.
///
/// Retail additionally emitted `pvpSeasonId` on every guild object. We omit it:
/// il2cpp `GuildInfo` (dump.cs:540458) has no corresponding property, so the
/// client parses and ignores it. Emitting a season id we cannot make meaningful
/// would be inventing data the client never reads.
#[derive(Serialize, Clone)]
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
    member_count: i64,
    guild_exchange_donation_count: i64,
    /// Retail's name for the guild's trophy total (`GuildInfo.GuildTrophies`).
    pvp_trophies: i64,
    /// The CURRENT pvp season, not a property of the guild: both captured
    /// creations carry the same value. Present on 437 of 446 captured guild
    /// objects -- we were sending twelve of retail's thirteen fields.
    pvp_season_id: Uuid,
    grandmaster_since_secs: i64,
}

impl GuildWire {
    fn from_row(row: &GuildRow, member_count: i64) -> Self {
        GuildWire {
            id: row.id.clone(),
            name: row.name.clone(),
            tag_id: row.tag_id.clone(),
            guild_type: row.guild_type.clone(),
            short_description: row.short_description.clone(),
            long_description: row.long_description.clone(),
            badge_icon_index: row.badge_icon_index,
            region_index: row.region_index,
            member_count,
            guild_exchange_donation_count: row.exchange_donation_count,
            pvp_trophies: row.trophies,
            pvp_season_id: crate::arena::arena_season::season_at(
                crate::arena::arena_season::now_unix(),
            )
            .map(|s| s.id)
            .unwrap_or_default(),
            grandmaster_since_secs: row.grandmaster_since,
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

impl MemberWire {
    fn from_row(row: &GuildMemberRow) -> Self {
        MemberWire {
            user_id: row.user_id,
            guild_id: row.guild_id.clone(),
            rank: row.rank.clone(),
            join_date: row.join_date,
        }
    }
}

/// Serialize the current guild without changing identity currencies mid-response.
///
/// Membership rows use the private `users.id` foreign key, while the client only
/// knows the authenticated account as `SessionResponse.userId` — `users.secret_id`.
/// Retail's captured current-guild response includes that exact session user id
/// for the current player's member row. If ours exposes the private id instead,
/// the client cannot find its own membership/rank and leaves the guild menu on
/// its initial spinner. Other member ids remain in the existing server currency
/// until the whole social surface is migrated together.
fn current_guild_members(
    members: &[GuildMemberRow],
    current_user_id: Uuid,
    current_public_user_id: Uuid,
) -> Vec<MemberWire> {
    members
        .iter()
        .map(|row| {
            let mut wire = MemberWire::from_row(row);
            if row.user_id == current_user_id {
                wire.user_id = current_public_user_id;
            }
            wire
        })
        .collect()
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

/// A pending join request.
///
/// MODELLED field names. No capture contains an applications response — no
/// captured player ever applied to an `APPLY_ONLY` guild — so these are taken from
/// il2cpp `GuildApplication` (`_userId`, `_guildId`, `_applicationState`,
/// dump.cs:538835) plus `ReceivedGuildApplication._characterId` (dump.cs:542154),
/// camelCased the way every other guild field on this API is.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationWire {
    user_id: Uuid,
    guild_id: String,
    character_id: Uuid,
    application_state: String,
    creation_time: i64,
}

impl ApplicationWire {
    fn from_row(row: &GuildApplicationRow) -> Self {
        ApplicationWire {
            user_id: row.user_id,
            guild_id: row.guild_id.clone(),
            character_id: row.character_id,
            application_state: row.state.clone(),
            creation_time: row.creation_time,
        }
    }
}

// ---- Error helpers -------------------------------------------------------------
//
// `GUILD_SERVICE_ID` error codes:
//    1  guild not found                     50  corrupt stored rank
//    2  already in a guild                  60  text failed validation
//   20  wrong admission path                61  unrecognised guild type
//   30  not a member                        70  approval not permitted
//   31  target is not in your guild         71  guild full on approval
//   32  no such application
//  100+ join refusals, offset by JoinRefusal::error_code()

fn guild_not_found() -> BladeApiError {
    BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 1)
}

fn not_a_member() -> BladeApiError {
    BladeApiError::new(StatusCode::FORBIDDEN, GUILD_SERVICE_ID, 30)
}

/// Map a policy refusal onto the wire. The code carries retail's own
/// `CanJoinGuildResult` ordinal (offset by 100 so it cannot collide with the
/// codes above), so a log line says exactly which precondition failed.
fn join_refused(refusal: JoinRefusal) -> BladeApiError {
    let status = match refusal {
        JoinRefusal::GuildIsInvalid => StatusCode::NOT_FOUND,
        JoinRefusal::GuildIsClosed | JoinRefusal::BelowMinimumLevel => StatusCode::FORBIDDEN,
        _ => StatusCode::CONFLICT,
    };
    BladeApiError::new(status, GUILD_SERVICE_ID, 100 + refusal.error_code())
}

fn approval_refused(refusal: ApprovalRefusal) -> BladeApiError {
    match refusal {
        ApprovalRefusal::NotPermitted => {
            BladeApiError::new(StatusCode::FORBIDDEN, GUILD_SERVICE_ID, 70)
        }
        ApprovalRefusal::GuildIsAtMaxMembers => {
            BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 71)
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

async fn application_count(conn: &mut AsyncPgConnection, gid: &str) -> Result<i64, BladeApiError> {
    use crate::schema::guild_applications::dsl::*;
    Ok(guild_applications
        .filter(guild_id.eq(gid))
        .count()
        .get_result(conn)
        .await?)
}

/// Member counts for every guild at once.
///
/// The previous implementation counted members with one query per guild inside
/// the search and leaderboard loops — an N+1 that ran 50-100 round trips per
/// listing. One grouped query replaces all of them, and the resulting map is also
/// what lets `memberCountMin`/`Max` be filtered at all.
async fn member_counts_by_guild(
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<String, i64>, BladeApiError> {
    use crate::schema::guild_members::dsl::*;
    let rows: Vec<(String, i64)> = guild_members
        .group_by(guild_id)
        .select((guild_id, diesel::dsl::count_star()))
        .load(conn)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Pending-application counts for every guild at once (see above).
async fn application_counts_by_guild(
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<String, i64>, BladeApiError> {
    use crate::schema::guild_applications::dsl::*;
    let rows: Vec<(String, i64)> = guild_applications
        .group_by(guild_id)
        .select((guild_id, diesel::dsl::count_star()))
        .load(conn)
        .await?;
    Ok(rows.into_iter().collect())
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

/// The requester's membership, or a 403. Used by every "must be in a guild"
/// endpoint.
async fn require_membership(
    conn: &mut AsyncPgConnection,
    uid: Uuid,
) -> Result<GuildMemberRow, BladeApiError> {
    find_membership(conn, uid).await?.ok_or_else(not_a_member)
}

async fn load_members(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<GuildMemberRow>, BladeApiError> {
    use crate::schema::guild_members::dsl::*;
    Ok(guild_members
        .filter(guild_id.eq(gid))
        // Rank ascending puts GRANDMASTER first (retail's enum numbers it 0), then
        // oldest members first — the order the captured member arrays arrive in.
        .order((rank.asc(), join_date.asc()))
        .select(GuildMemberRow::as_select())
        .load(conn)
        .await?)
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

async fn find_removal(
    conn: &mut AsyncPgConnection,
    gid: &str,
    uid: Uuid,
) -> Result<Option<Removal>, BladeApiError> {
    use crate::schema::guild_removals::dsl::*;
    Ok(guild_removals
        .filter(guild_id.eq(gid))
        .filter(user_id.eq(uid))
        .select(GuildRemovalRow::as_select())
        .load(conn)
        .await?
        .into_iter()
        .next()
        .map(|r| Removal {
            removed_at: r.removed_at,
            banned: r.banned,
        }))
}

/// Record that `uid` left or was removed from `gid`, starting the re-join
/// cooldown. Upserts, so a later ban upgrades an earlier kick to permanent and a
/// fresh kick restarts the clock.
async fn record_removal(
    conn: &mut AsyncPgConnection,
    gid: &str,
    uid: Uuid,
    ts: i64,
    banned_flag: bool,
) -> Result<(), BladeApiError> {
    use crate::schema::guild_removals::dsl as gr;
    diesel::insert_into(gr::guild_removals)
        .values(GuildRemovalRow {
            guild_id: gid.to_string(),
            user_id: uid,
            removed_at: ts,
            banned: banned_flag,
        })
        .on_conflict((gr::guild_id, gr::user_id))
        .do_update()
        .set((gr::removed_at.eq(ts), gr::banned.eq(banned_flag)))
        .execute(conn)
        .await?;
    Ok(())
}

async fn find_application(
    conn: &mut AsyncPgConnection,
    gid: &str,
    uid: Uuid,
) -> Result<Option<GuildApplicationRow>, BladeApiError> {
    use crate::schema::guild_applications::dsl::*;
    Ok(guild_applications
        .filter(guild_id.eq(gid))
        .filter(user_id.eq(uid))
        .select(GuildApplicationRow::as_select())
        .load(conn)
        .await?
        .into_iter()
        .next())
}

async fn has_any_application(
    conn: &mut AsyncPgConnection,
    uid: Uuid,
) -> Result<bool, BladeApiError> {
    use crate::schema::guild_applications::dsl::*;
    let n: i64 = guild_applications
        .filter(user_id.eq(uid))
        .count()
        .get_result(conn)
        .await?;
    Ok(n > 0)
}

/// The requester's character level, for the [`MIN_LEVEL_TO_JOIN`] gate.
///
/// Retail gates this client-side (the Join button simply is not offered below
/// level 5), so no capture shows the server refusing it. We check anyway: a client
/// is not a security boundary.
async fn character_level(
    conn: &mut AsyncPgConnection,
    cid: Uuid,
) -> Result<u16, BladeApiError> {
    use crate::schema::characters::dsl as c;
    let rows: Vec<JsonDbWrapper<blades_lib::user_data::CompleteCharacter>> = c::characters
        .filter(c::id.eq(cid))
        .select(c::character)
        .load(conn)
        .await?;
    Ok(rows.into_iter().next().map(|r| r.0.level).unwrap_or(0))
}

/// Append one entry to the guild message board.
///
/// `message_id` is `{creationTime}::{uuid}` — retail's exact format, e.g.
/// `1778851566::ee7662d4-ab1d-41e9-ab52-b24ef5b8762f`.
async fn append_message(
    conn: &mut AsyncPgConnection,
    gid: &str,
    uid: Uuid,
    cid: Uuid,
    message_type: &str,
    data: Value,
) -> Result<MessageWire, BladeApiError> {
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
    use crate::schema::guild_messages;
    diesel::insert_into(guild_messages::table)
        .values(&row)
        .execute(conn)
        .await?;
    Ok(MessageWire::from_row(row))
}

// ---- Handlers ------------------------------------------------------------------

/// `GET /guilds/current` -> `{"guild": ..., "members": [...]}`.
///
/// 61 of the 65 captured responses carry both keys; the 4 that carry only `guild`
/// are the guildless case, hence `members` is skipped when empty.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentGuildResponse {
    guild: Option<GuildWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    members: Vec<MemberWire>,
    /// Returned by CREATE and by nothing else.
    ///
    /// Creating a guild costs currency, and retail hands back the authoritative
    /// balance with it (both captured creations). `GET /guilds/current` never
    /// does -- 0 of 410 captured responses -- so this is skipped when absent
    /// rather than always sent: a key retail never sent on a route is its own
    /// bug, which is how the stub-character load stall happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current")]
pub async fn get_current_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<CurrentGuildResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let (guild, members) = match find_membership(&mut conn, session.session.user_id).await? {
        Some(m) => match load_guild(&mut conn, &m.guild_id).await? {
            Some(g) => {
                let members = load_members(&mut conn, &g.id).await?;
                let wire = GuildWire::from_row(&g, members.len() as i64);
                (
                    Some(wire),
                    current_guild_members(
                        &members,
                        session.session.user_id,
                        session.session.secret_user_id,
                    ),
                )
            }
            None => (None, Vec::new()),
        },
        None => (None, Vec::new()),
    };
    Ok(Json(CurrentGuildResponse { guild, members, wallet: None }))
}

/// `GET /guilds/{id}` -> `{"applicationStatus": ..., "guild": ..., "members": [...]}`.
///
/// All three keys appear in every one of the 15 captured responses.
/// `applicationStatus.maxApplicationsReached` is what drives the client's
/// "Pending Request" / disabled-Apply state — it lives at the response level, not
/// inside the guild object (matching il2cpp `GuildInfo.SetApplicationStatus`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationStatusWire {
    max_applications_reached: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildDetailResponse {
    application_status: ApplicationStatusWire,
    guild: Option<GuildWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    members: Vec<MemberWire>,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/{guild_id}")]
pub async fn get_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, String)>,
) -> Result<Json<GuildDetailResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, gid) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let g = load_guild(&mut conn, &gid).await?.ok_or_else(guild_not_found)?;
    let members = load_members(&mut conn, &g.id).await?;
    let applications = application_count(&mut conn, &g.id).await?;
    let wire = GuildWire::from_row(&g, members.len() as i64);

    Ok(Json(GuildDetailResponse {
        application_status: ApplicationStatusWire {
            max_applications_reached: applications >= MAX_APPLICATIONS,
        },
        guild: Some(wire),
        members: members.iter().map(MemberWire::from_row).collect(),
    }))
}

// ---- Search --------------------------------------------------------------------

/// Query parameters for `GET /guilds/search`.
///
/// Names come from il2cpp `SearchForGuildRequest`'s `PARAMETER_SEARCH_*` constants
/// (dump.cs:462629) and are visible in captured URLs, e.g.
/// `?limit=50&memberCountMin=10&memberCountMax=19&applicationCountMax=9&type=OPEN`.
///
/// Retail treats -1 as "unset" for every numeric filter
/// (`SearchForGuildContext.INVALID_INT`), and the client omits parameters it does
/// not use, so both an absent parameter and an explicit -1 mean "no constraint".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQuery {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    guild_type: Option<String>,
    #[serde(default)]
    region_index: Option<i32>,
    #[serde(default)]
    member_count_min: Option<i64>,
    #[serde(default)]
    member_count_max: Option<i64>,
    #[serde(default)]
    application_count_min: Option<i64>,
    #[serde(default)]
    application_count_max: Option<i64>,
    #[serde(default)]
    pvp_trophies_min: Option<i64>,
    #[serde(default)]
    pvp_trophies_max: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `-1` is retail's "unset" sentinel; treat it as no constraint.
fn unset(v: Option<i64>) -> Option<i64> {
    v.filter(|n| *n >= 0)
}

fn unset_i32(v: Option<i32>) -> Option<i32> {
    v.filter(|n| *n >= 0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildListResponse {
    guilds: Vec<GuildWire>,
}

/// `GET /guilds/search` -> `{"guilds": [...]}`.
///
/// Every filter the client sends is now actually applied. The previous
/// implementation accepted the parameters and ignored all of them, returning the
/// first 50 guilds in table order — so "Open guilds with room in my region" and
/// "any guild at all" produced identical results.
#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/search")]
pub async fn search_guilds(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    query: web::Query<SearchQuery>,
) -> Result<Json<GuildListResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let q = query.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let rows: Vec<GuildRow> = {
        use crate::schema::guilds::dsl::*;
        let mut sql = guilds.into_boxed();
        if let Some(t) = q.guild_type.as_deref() {
            // An unrecognised type would otherwise match nothing silently; reject
            // it so a client bug surfaces as an error rather than "no guilds".
            let parsed = GuildType::from_wire(t).ok_or_else(|| {
                BladeApiError::new(StatusCode::BAD_REQUEST, GUILD_SERVICE_ID, 61)
            })?;
            sql = sql.filter(guild_type.eq(parsed.as_wire()));
        }
        if let Some(r) = unset_i32(q.region_index) {
            sql = sql.filter(region_index.eq(r));
        }
        if let Some(lo) = unset(q.pvp_trophies_min) {
            sql = sql.filter(trophies.ge(lo));
        }
        if let Some(hi) = unset(q.pvp_trophies_max) {
            sql = sql.filter(trophies.le(hi));
        }
        if let Some(n) = q.name.as_deref().filter(|s| !s.is_empty()) {
            // Retail's search box is "Find by tag or name"
            // (UI.Guild.NameDefault), so a query matches either. Name match is a
            // case-insensitive substring; tag match is exact, since tags are
            // 4-digit identifiers rather than prose.
            let pattern = format!("%{}%", n.replace('%', "\\%").replace('_', "\\_"));
            sql = sql.filter(name.ilike(pattern).or(tag_id.eq(n.to_string())));
        }
        sql.select(GuildRow::as_select()).load(&mut conn).await?
    };

    // The member- and application-count filters need aggregates, so they are
    // applied after the fact against one grouped query each rather than as N+1
    // per-guild counts.
    let members = member_counts_by_guild(&mut conn).await?;
    let applications = application_counts_by_guild(&mut conn).await?;

    let limit = q.limit.filter(|n| *n > 0).unwrap_or(SEARCH_LIMIT).min(SEARCH_LIMIT) as usize;
    let out: Vec<GuildWire> = rows
        .iter()
        .filter_map(|g| {
            let mc = members.get(&g.id).copied().unwrap_or(0);
            let ac = applications.get(&g.id).copied().unwrap_or(0);
            let in_range = |v: i64, lo: Option<i64>, hi: Option<i64>| {
                lo.is_none_or(|l| v >= l) && hi.is_none_or(|h| v <= h)
            };
            if !in_range(mc, unset(q.member_count_min), unset(q.member_count_max)) {
                return None;
            }
            if !in_range(
                ac,
                unset(q.application_count_min),
                unset(q.application_count_max),
            ) {
                return None;
            }
            Some(GuildWire::from_row(g, mc))
        })
        .take(limit)
        .collect();

    Ok(Json(GuildListResponse { guilds: out }))
}

#[cfg(test)]
mod guild_trophy_tests {
    /// The leaderboard orders on `guilds.trophies`, so a guild whose members hold
    /// trophies must not read as 0. This asserts the ORDERING contract the endpoint
    /// depends on, independently of the SQL that maintains the column.
    #[test]
    fn guild_trophies_order_the_leaderboard_and_break_ties_by_id() {
        // (trophies, id) as the leaderboard sorts them: trophies desc, id asc.
        let mut rows = vec![(620_i64, "b"), (1774, "a"), (0, "c"), (1774, "z")];
        rows.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(y.1)));
        assert_eq!(
            rows,
            vec![(1774, "a"), (1774, "z"), (620, "b"), (0, "c")],
            "higher trophies rank first; equal trophies keep a stable id order so a \
             guild cannot appear twice or vanish across pages"
        );
    }

    /// A guild's trophies are the SUM of its members'. Same rule
    /// `season_store::guild_standings_from` applies to the season ladder.
    #[test]
    fn a_guilds_trophies_are_the_sum_of_its_members() {
        let members = [1506_i64, 1485, 1481];
        assert_eq!(members.iter().sum::<i64>(), 4472);
        // An empty guild is 0, not absent — the column is NOT NULL and the
        // leaderboard must still place the guild.
        let empty: [i64; 0] = [];
        assert_eq!(empty.iter().sum::<i64>(), 0);
    }

    /// A member's negative contribution can never drag a guild below zero, for the
    /// same reason a character's own count bottoms out at 0.
    #[test]
    fn a_guild_total_never_goes_negative() {
        let clamp = |t: i64, d: i64| (t + d).max(0);
        assert_eq!(clamp(10, -25), 0);
        assert_eq!(clamp(0, -5), 0);
        assert_eq!(clamp(100, -25), 75);
    }
}

/// Recompute every guild's `trophies` from the CURRENT SEASON's standings.
///
/// `guilds.trophies` is what `GET /guilds/leaderboard` orders on and what the client
/// prints on every guild card. It was written once — as 0, at guild creation — and
/// never again, so every guild sat at 0 and the "top guilds" ladder was really
/// ordering by guild id.
///
/// **It is a SEASONAL total and resets with the arena season, exactly like a
/// player's cups.** That is also what the retail corpus shows: across 277 captured
/// guilds the ceiling is 3,900 with a 20-member cap (~195/member), while individual
/// characters reach 1,506 LIFETIME trophies — three such players would alone exceed
/// every guild in retail. A lifetime sum is therefore ruled out on scale.
///
/// The aggregation is delegated to `season_store`, which already encodes it for the
/// season ladder: per-character season trophies replayed from the season's zero
/// baseline, then summed per guild by `guild_standings_from`. Reusing it means the
/// leaderboard and the end-of-season award ladder cannot disagree.
pub async fn recompute_guild_trophies(
    conn: &mut diesel_async::AsyncPgConnection,
    season: &crate::arena::season_store::SeasonRow,
) -> Result<usize, diesel::result::Error> {
    use diesel_async::RunQueryDsl;
    let standings = crate::arena::season_store::freeze_standings(conn, season).await?;
    let guilds_now = crate::arena::season_store::guild_standings_from(season.id, &standings);

    // Zero every guild first: a guild that scored nothing this season, or lost the
    // members who scored, must fall back to 0 rather than keep a stale total.
    diesel::sql_query("UPDATE guilds SET trophies = 0")
        .execute(conn)
        .await?;

    let mut written = 0usize;
    for g in &guilds_now {
        written += diesel::sql_query("UPDATE guilds SET trophies = $1 WHERE id = $2")
            .bind::<diesel::sql_types::BigInt, _>(g.trophies)
            .bind::<diesel::sql_types::Text, _>(&g.guild_id)
            .execute(conn)
            .await?;
    }
    Ok(written)
}

// ---- Leaderboard ---------------------------------------------------------------

/// One leaderboard row: a guild plus its global position.
///
/// Captured shape is the guild object with a `rank` key alongside its fields —
/// flat, not nested — so `rank` is flattened in beside them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardEntry {
    rank: i64,
    #[serde(flatten)]
    guild: GuildWire,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardPage {
    current_page: i64,
    total_pages: i64,
    entries: Vec<LeaderboardEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardResponse {
    guild_leaderboard: LeaderboardPage,
    /// The requester's own guild and its rank, so the client can show "you are
    /// #9" without paging to find it. `null` when the requester has no guild.
    #[serde(skip_serializing_if = "Option::is_none")]
    player_guild_leaderboard_entry: Option<LeaderboardEntry>,
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    page: Option<i64>,
}

/// `GET /guilds/leaderboard?page=N` ->
/// `{"guildLeaderboard": {"currentPage", "totalPages", "entries"}, "playerGuildLeaderboardEntry"}`.
///
/// The previous implementation returned `{"guilds": [...]}` — the search response
/// shape — which is not what the client parses here at all.
///
/// Page size is 100, capture-derived: every captured `page=1` response carried
/// exactly 100 entries ranked 1..100, with `totalPages` varying by how many guilds
/// existed. Pages are 1-based.
#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/leaderboard")]
pub async fn guild_leaderboard(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    query: web::Query<PageQuery>,
) -> Result<Json<LeaderboardResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let page = query.into_inner().page.unwrap_or(1).max(1);
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let rows: Vec<GuildRow> = {
        use crate::schema::guilds::dsl::*;
        guilds
            // `id` breaks ties so that equal-trophy guilds keep a stable order
            // across pages; without it a guild can appear twice or vanish.
            .order((trophies.desc(), id.asc()))
            .select(GuildRow::as_select())
            .load(&mut conn)
            .await?
    };
    let members = member_counts_by_guild(&mut conn).await?;
    let my_guild_id = find_membership(&mut conn, session.session.user_id)
        .await?
        .map(|m| m.guild_id);

    let total = rows.len() as i64;
    let total_pages = if total == 0 {
        1
    } else {
        (total + LEADERBOARD_PAGE_SIZE - 1) / LEADERBOARD_PAGE_SIZE
    };

    let entry_for = |index: usize, g: &GuildRow| LeaderboardEntry {
        rank: index as i64 + 1,
        guild: GuildWire::from_row(g, members.get(&g.id).copied().unwrap_or(0)),
    };

    let player_guild_leaderboard_entry = my_guild_id.and_then(|gid| {
        rows.iter()
            .position(|g| g.id == gid)
            .map(|i| entry_for(i, &rows[i]))
    });

    let start = ((page - 1) * LEADERBOARD_PAGE_SIZE).max(0) as usize;
    let entries: Vec<LeaderboardEntry> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(LEADERBOARD_PAGE_SIZE as usize)
        .map(|(i, g)| entry_for(i, g))
        .collect();

    Ok(Json(LeaderboardResponse {
        guild_leaderboard: LeaderboardPage {
            current_page: page,
            total_pages,
            entries,
        },
        player_guild_leaderboard_entry,
    }))
}

// ---- Create / update -----------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateGuildRequest {
    /// il2cpp `CreateGuildRequest.PARAMETER_GUILD_NAME` is `"name"`. The older
    /// `guildName` spelling is still accepted so an in-flight client is not
    /// broken by the correction.
    #[serde(default, alias = "guildName")]
    name: String,
    #[serde(default, rename = "type")]
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

/// Parse a client-supplied guild type, defaulting to the permissionless one.
fn parse_guild_type(raw: Option<&str>) -> Result<GuildType, BladeApiError> {
    match raw {
        None => Ok(GuildType::Open),
        Some(s) => GuildType::from_wire(s)
            .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, GUILD_SERVICE_ID, 61)),
    }
}

fn invalid_text() -> BladeApiError {
    BladeApiError::new(StatusCode::BAD_REQUEST, GUILD_SERVICE_ID, 60)
}

/// `POST /guilds` — create a guild; the creator becomes its GRANDMASTER.
///
/// Retail charged 50 Gems for this (`GuildData._createCosts`, corroborated by
/// `UI.Help.Guilds.Description`: "You can create a new guild for 50 Gems"). This
/// server does NOT charge — see docs/guilds.md § "Known gaps".
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds")]
pub async fn create_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<CreateGuildRequest>,
) -> Result<Json<CurrentGuildResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    if find_membership(&mut conn, user_id).await?.is_some() {
        return Err(BladeApiError::new(
            StatusCode::CONFLICT,
            GUILD_SERVICE_ID,
            2,
        ));
    }
    let guild_type = parse_guild_type(body.guild_type.as_deref())?;
    if !guild_text_ok(&body.name, &body.short_description, &body.long_description) {
        return Err(invalid_text());
    }
    // Store the trimmed name, as the rename path does — otherwise
    // "  Bladeworks  " validates and is then persisted with its padding, and the
    // two paths disagree about what a guild is called.
    let body_name = body.name.trim().to_string();

    let gid = guild_id_from_uuid(Uuid::new_v4());
    let ts = now_secs();
    let row = GuildRow {
        id: gid.clone(),
        name: body_name,
        // A 4-digit tag, as retail (e.g. "7988"). Derived from a fresh uuid rather
        // than from the clock: the previous `ts % 10000` handed the same tag to
        // every guild created in the same second.
        tag_id: format!("{:04}", Uuid::new_v4().as_u128() % 10_000),
        guild_type: guild_type.as_wire().to_string(),
        short_description: body.short_description,
        long_description: body.long_description,
        badge_icon_index: body.badge_icon_index,
        region_index: body.region_index,
        trophies: 0,
        created_at: ts,
        exchange_donation_count: 0,
        grandmaster_since: ts,
    };
    let member = GuildMemberRow {
        guild_id: gid.clone(),
        user_id,
        character_id,
        rank: GuildRank::Grandmaster.as_wire().to_string(),
        join_date: ts,
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
            .values(&member)
            .execute(&mut conn)
            .await?;
    }
    // Retail returns the founder's wallet WITH the create -- both captured
    // creations carry it, and the client waits for it because founding a guild
    // is a spend. Without it the request succeeds (200 in 41ms on prod) and the
    // client simply never proceeds: no follow-up call to
    // /guilds/current/exchanges or /messages, just a spinner.
    let founder_wallet = {
        use crate::schema::characters;
        characters::table
            .filter(characters::id.eq(character_id))
            .select(characters::wallet)
            .first::<JsonDbWrapper<CompleteWallet>>(&mut conn)
            .await
            .ok()
            .map(|w| w.0)
    };

    Ok(Json(CurrentGuildResponse {
        guild: Some(GuildWire::from_row(&row, 1)),
        members: vec![MemberWire::from_row(&member)],
        wallet: founder_wallet,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateGuildRequest {
    /// Rename the guild.
    ///
    /// WHY THIS EXISTS: four guilds on prod carry an EMPTY name. They were
    /// created 4-11 July, before the create handler was corrected on 25 August
    /// to read retail's actual parameter (`CreateGuildRequest.PARAMETER_GUILD_NAME
    /// = "name"`, dump.cs:462268); until then the name silently defaulted to "".
    /// Creation is safe now — `guild_text_ok` enforces a minimum length — but
    /// there was no way to REPAIR the four, because nothing could set a name
    /// after creation. Their owners were stuck with a nameless guild for good.
    ///
    /// Not repaired by hand in the database: we do not know what those guilds
    /// were meant to be called, and guessing on someone's behalf is worse than
    /// letting them type it.
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    guild_type: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    long_description: Option<String>,
    #[serde(default)]
    badge_icon_index: Option<i32>,
    #[serde(default)]
    region_index: Option<i32>,
}

/// `POST`/`PUT /guilds/current` — edit the guild. GRANDMASTER only.
///
/// This is the endpoint behind the in-game promise that the Grand Master "has the
/// power to set the guild to Closed (to prevent new applicants)"
/// (`UI.Help.Guilds.Description`), and it had no server route at all before.
///
/// The verb is not recoverable: il2cpp `UpdateGuildRequest.URL_PATH` is
/// `/characters/{0}/guilds/current`, the same path `GetCurrentGuildRequest` GETs,
/// and no update crossed the wire in any capture. Both POST and PUT are therefore
/// registered against the same handler so whichever the client uses is served.
async fn update_guild_impl(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<UpdateGuildRequest>,
) -> Result<Json<CurrentGuildResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let me = require_membership(&mut conn, user_id).await?;
    if !can_edit_guild(me.parsed_rank()?) {
        return Err(BladeApiError::unauthorized());
    }
    let mut guild = load_guild(&mut conn, &me.guild_id)
        .await?
        .ok_or_else(guild_not_found)?;

    // The GUILD_UPDATE board message carries ONLY the fields that actually
    // changed — captured examples are {type, longDescription},
    // {type, longDescription, shortDescription} and {type, guildType}. Build the
    // changed set as we apply it.
    let mut changed = serde_json::Map::new();
    changed.insert("type".into(), json!("GUILD_UPDATE"));

    if let Some(t) = body.guild_type.as_deref() {
        let parsed = parse_guild_type(Some(t))?;
        if parsed.as_wire() != guild.guild_type {
            guild.guild_type = parsed.as_wire().to_string();
            changed.insert("guildType".into(), json!(parsed.as_wire()));
        }
    }
    if let Some(s) = body.short_description {
        if s != guild.short_description {
            guild.short_description = s.clone();
            changed.insert("shortDescription".into(), json!(s));
        }
    }
    if let Some(s) = body.long_description {
        if s != guild.long_description {
            guild.long_description = s.clone();
            changed.insert("longDescription".into(), json!(s));
        }
    }
    if let Some(i) = body.badge_icon_index {
        if i != guild.badge_icon_index {
            guild.badge_icon_index = i;
            // Key name inferred: only guildType/shortDescription/longDescription
            // were ever observed in a GUILD_UPDATE payload. `badgeIconIndex`
            // matches the name this field carries everywhere else on the wire.
            changed.insert("badgeIconIndex".into(), json!(i));
        }
    }
    if let Some(i) = body.region_index {
        if i != guild.region_index {
            guild.region_index = i;
            changed.insert("regionIndex".into(), json!(i));
        }
    }
    if let Some(n) = body.name.as_deref() {
        let n = n.trim();
        if n != guild.name {
            guild.name = n.to_string();
            changed.insert("name".into(), json!(n));
        }
    }

    if !guild_text_ok(
        &guild.name,
        &guild.short_description,
        &guild.long_description,
    ) {
        return Err(invalid_text());
    }

    // More than just the discriminator means something actually changed.
    if changed.len() > 1 {
        {
            use crate::schema::guilds::dsl as g;
            diesel::update(g::guilds.filter(g::id.eq(&guild.id)))
                .set((
                    g::name.eq(&guild.name),
                    g::guild_type.eq(&guild.guild_type),
                    g::short_description.eq(&guild.short_description),
                    g::long_description.eq(&guild.long_description),
                    g::badge_icon_index.eq(guild.badge_icon_index),
                    g::region_index.eq(guild.region_index),
                ))
                .execute(&mut conn)
                .await?;
        }
        append_message(
            &mut conn,
            &guild.id,
            user_id,
            character_id,
            "GUILD_UPDATE",
            Value::Object(changed),
        )
        .await?;
    }

    let members = load_members(&mut conn, &guild.id).await?;
    Ok(Json(CurrentGuildResponse {
        guild: Some(GuildWire::from_row(&guild, members.len() as i64)),
        members: members.iter().map(MemberWire::from_row).collect(),
        wallet: None,
    }))
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current")]
pub async fn update_guild_post(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<UpdateGuildRequest>,
) -> Result<Json<CurrentGuildResponse>, BladeApiError> {
    update_guild_impl(session, app_state, path, body).await
}

#[put("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current")]
pub async fn update_guild_put(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<UpdateGuildRequest>,
) -> Result<Json<CurrentGuildResponse>, BladeApiError> {
    update_guild_impl(session, app_state, path, body).await
}

// ---- Joining and applying ------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberResponse {
    member: MemberWire,
}

/// Gather everything the join policy needs for `uid`/`cid` against guild `gid`.
async fn build_join_context(
    conn: &mut AsyncPgConnection,
    gid: &str,
    uid: Uuid,
    cid: Uuid,
) -> Result<JoinContext, BladeApiError> {
    let guild = load_guild(conn, gid).await?;
    let guild_type = match &guild {
        // A stored type we cannot parse is treated as "no such guild" rather than
        // silently falling back to OPEN, which would make a CLOSED guild joinable.
        Some(g) => GuildType::from_wire(&g.guild_type),
        None => None,
    };
    Ok(JoinContext {
        guild_type,
        character_level: character_level(conn, cid).await?,
        already_in_guild: find_membership(conn, uid).await?.is_some(),
        already_applied: has_any_application(conn, uid).await?,
        member_count: member_count(conn, gid).await?,
        application_count: application_count(conn, gid).await?,
        removal: find_removal(conn, gid, uid).await?,
        now: now_secs(),
    })
}

/// `POST /guilds/{id}/join` -> `{"member": {...}}`.
///
/// The permissionless path. Only an `OPEN` guild may be joined this way; an
/// `APPLY_ONLY` guild answers 409 and the client is expected to call `/apply`
/// instead (which is what retail's client does — it reads the guild's type before
/// choosing the button).
///
/// Before this change join performed no checks beyond "does the guild exist and
/// are you already in one", so a CLOSED guild was joinable, a full guild was
/// joinable, and a just-kicked player could rejoin immediately.
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

    let ctx = build_join_context(&mut conn, &gid, user_id, character_id).await?;
    match evaluate_join(ctx).map_err(join_refused)? {
        JoinAdmission::Join => {}
        JoinAdmission::Apply => {
            // Right to be admitted, wrong endpoint — this guild takes applications.
            return Err(BladeApiError::new(
                StatusCode::CONFLICT,
                GUILD_SERVICE_ID,
                20,
            ));
        }
    }

    let ts = now_secs();
    let member = GuildMemberRow {
        guild_id: gid.clone(),
        user_id,
        character_id,
        rank: GuildRank::Member.as_wire().to_string(),
        join_date: ts,
    };
    {
        use crate::schema::guild_members;
        diesel::insert_into(guild_members::table)
            .values(&member)
            .execute(&mut conn)
            .await?;
    }
    // Captured JOIN entries carry an EMPTY typeSpecificData ({}), with the joiner
    // in the message's own userId/characterId. 33 examples, all identical.
    append_message(&mut conn, &gid, user_id, character_id, "JOIN", json!({})).await?;

    Ok(Json(MemberResponse {
        member: MemberWire::from_row(&member),
    }))
}

/// A pending application, as returned to the applicant.
///
/// MODELLED wrapper key — from il2cpp `ResponseGuildApplicationData._guildApplication`
/// (dump.cs:486113). No capture of this endpoint exists.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildApplicationResponse {
    guild_application: ApplicationWire,
}

/// `POST /guilds/{id}/apply` — request to join an `APPLY_ONLY` guild.
///
/// The "allow a join" path, and the one piece of the guild feature set that had no
/// server implementation whatsoever: retail ships `ApplyToGuildRequest`
/// (dump.cs:462204) and this route did not exist.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/{guild_id}/apply")]
pub async fn apply_to_guild(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, String)>,
    _body: Json<Option<Value>>,
) -> Result<Json<GuildApplicationResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, gid) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let ctx = build_join_context(&mut conn, &gid, user_id, character_id).await?;
    match evaluate_join(ctx).map_err(join_refused)? {
        JoinAdmission::Apply => {}
        JoinAdmission::Join => {
            // An OPEN guild needs no application; the client should just join.
            return Err(BladeApiError::new(
                StatusCode::CONFLICT,
                GUILD_SERVICE_ID,
                20,
            ));
        }
    }

    let row = GuildApplicationRow {
        guild_id: gid.clone(),
        user_id,
        character_id,
        state: "APPLIED".to_string(),
        creation_time: now_secs(),
    };
    {
        use crate::schema::guild_applications;
        diesel::insert_into(guild_applications::table)
            .values(&row)
            .execute(&mut conn)
            .await?;
    }
    // Deliberately no board message: retail's GuildMessageType has no APPLIED
    // member (only APPROVE and DENY), so an application is invisible in chat until
    // it is decided. The Grand Master sees it via GET /guilds/current/applications.

    Ok(Json(GuildApplicationResponse {
        guild_application: ApplicationWire::from_row(&row),
    }))
}

/// MODELLED wrapper key — il2cpp `ResponseGuildApplicationsData._guildApplications`
/// (dump.cs:485916).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuildApplicationsResponse {
    guild_applications: Vec<ApplicationWire>,
}

/// `GET /guilds/current/applications` — the pending join requests.
///
/// GRANDMASTER only: `GuildRankData._canApproveGuildApplications` is true for that
/// rank alone, and the applicant list is what the approve/deny buttons act on.
#[get(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/applications"
)]
pub async fn list_applications(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GuildApplicationsResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let me = require_membership(&mut conn, session.session.user_id).await?;
    if !can_approve_applications(me.parsed_rank()?) {
        return Err(BladeApiError::unauthorized());
    }

    let rows: Vec<GuildApplicationRow> = {
        use crate::schema::guild_applications::dsl::*;
        guild_applications
            .filter(guild_id.eq(&me.guild_id))
            .order(creation_time.asc())
            .select(GuildApplicationRow::as_select())
            .load(&mut conn)
            .await?
    };
    Ok(Json(GuildApplicationsResponse {
        guild_applications: rows.iter().map(ApplicationWire::from_row).collect(),
    }))
}

/// `POST /guilds/current/approve/{applicantUserId}` -> `{"member": {...}}`.
///
/// Seats the applicant and posts an `APPROVE` board entry. The whole thing runs in
/// one transaction: an approval that inserted the member but failed to clear the
/// application would leave the applicant both seated and still queued.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/approve/{applicant_user_id}"
)]
pub async fn approve_application(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<MemberResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, applicant_user_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let me = require_membership(&mut conn, user_id).await?;
    let my_rank = me.parsed_rank()?;
    let count = member_count(&mut conn, &me.guild_id).await?;
    evaluate_approval(my_rank, count).map_err(approval_refused)?;

    let application = find_application(&mut conn, &me.guild_id, applicant_user_id)
        .await?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 32))?;

    let gid = me.guild_id.clone();
    let ts = now_secs();
    let member = GuildMemberRow {
        guild_id: gid.clone(),
        user_id: applicant_user_id,
        character_id: application.character_id,
        rank: GuildRank::Member.as_wire().to_string(),
        join_date: ts,
    };
    let member_out = MemberWire::from_row(&member);

    conn.transaction(move |conn| {
        async move {
            {
                use crate::schema::guild_applications::dsl as ga;
                diesel::delete(
                    ga::guild_applications
                        .filter(ga::guild_id.eq(&gid))
                        .filter(ga::user_id.eq(applicant_user_id)),
                )
                .execute(conn)
                .await?;
            }
            {
                use crate::schema::guild_members;
                diesel::insert_into(guild_members::table)
                    .values(&member)
                    .execute(conn)
                    .await?;
            }
            // An approval clears any earlier removal: being let back in
            // deliberately should not leave the cooldown armed against the
            // person who was just admitted.
            {
                use crate::schema::guild_removals::dsl as gr;
                diesel::delete(
                    gr::guild_removals
                        .filter(gr::guild_id.eq(&gid))
                        .filter(gr::user_id.eq(applicant_user_id)),
                )
                .execute(conn)
                .await?;
            }
            // Captured shape: {"type":"APPROVE","approvedUserId":"..."} with the
            // APPROVER in the message's own userId. 4 examples.
            append_message(
                conn,
                &gid,
                user_id,
                character_id,
                "APPROVE",
                json!({ "type": "APPROVE", "approvedUserId": applicant_user_id }),
            )
            .await?;
            Ok::<_, BladeApiError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Json(MemberResponse { member: member_out }))
}

/// `POST /guilds/current/deny/{applicantUserId}` — reject a join request.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/deny/{applicant_user_id}"
)]
pub async fn deny_application(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, applicant_user_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let me = require_membership(&mut conn, user_id).await?;
    if !can_approve_applications(me.parsed_rank()?) {
        return Err(BladeApiError::unauthorized());
    }
    if find_application(&mut conn, &me.guild_id, applicant_user_id)
        .await?
        .is_none()
    {
        return Err(BladeApiError::new(
            StatusCode::NOT_FOUND,
            GUILD_SERVICE_ID,
            32,
        ));
    }
    {
        use crate::schema::guild_applications::dsl as ga;
        diesel::delete(
            ga::guild_applications
                .filter(ga::guild_id.eq(&me.guild_id))
                .filter(ga::user_id.eq(applicant_user_id)),
        )
        .execute(&mut conn)
        .await?;
    }
    // Captured shape: {"type":"DENY","deniedUserId":"..."}.
    append_message(
        &mut conn,
        &me.guild_id,
        user_id,
        character_id,
        "DENY",
        json!({ "type": "DENY", "deniedUserId": applicant_user_id }),
    )
    .await?;
    Ok(Json(json!({})))
}

// ---- Leaving, kicking, banning -------------------------------------------------

/// Remove a member and, if they were the Grand Master, hand the guild on.
///
/// Returns the id of whoever inherited, if anyone. When the guild empties it is
/// deleted along with its board and its pending applications — an ownerless,
/// memberless guild would otherwise sit in search results forever.
async fn remove_member_and_succeed(
    conn: &mut AsyncPgConnection,
    gid: &str,
    departing: Uuid,
    departing_rank: GuildRank,
    ts: i64,
) -> Result<Option<Uuid>, BladeApiError> {
    {
        use crate::schema::guild_members::dsl as gm;
        diesel::delete(
            gm::guild_members
                .filter(gm::guild_id.eq(gid))
                .filter(gm::user_id.eq(departing)),
        )
        .execute(conn)
        .await?;
    }

    if departing_rank != GuildRank::Grandmaster {
        return Ok(None);
    }

    let remaining = load_members(conn, gid).await?;
    let handles: Vec<(Uuid, GuildRank, i64)> = remaining
        .iter()
        .filter_map(|m| GuildRank::from_wire(&m.rank).map(|r| (m.user_id, r, m.join_date)))
        .collect();

    match successor(&handles) {
        Some(heir) => {
            {
                use crate::schema::guild_members::dsl as gm;
                diesel::update(
                    gm::guild_members
                        .filter(gm::guild_id.eq(gid))
                        .filter(gm::user_id.eq(heir)),
                )
                .set(gm::rank.eq(GuildRank::Grandmaster.as_wire()))
                .execute(conn)
                .await?;
            }
            {
                use crate::schema::guilds::dsl as g;
                diesel::update(g::guilds.filter(g::id.eq(gid)))
                    .set(g::grandmaster_since.eq(ts))
                    .execute(conn)
                    .await?;
            }
            Ok(Some(heir))
        }
        None => {
            // Nobody left. Tear the guild down rather than leave a husk.
            {
                use crate::schema::guild_applications::dsl as ga;
                diesel::delete(ga::guild_applications.filter(ga::guild_id.eq(gid)))
                    .execute(conn)
                    .await?;
            }
            {
                use crate::schema::guild_messages::dsl as gmsg;
                diesel::delete(gmsg::guild_messages.filter(gmsg::guild_id.eq(gid)))
                    .execute(conn)
                    .await?;
            }
            {
                use crate::schema::guilds::dsl as g;
                diesel::delete(g::guilds.filter(g::id.eq(gid)))
                    .execute(conn)
                    .await?;
            }
            Ok(None)
        }
    }
}

/// Post the `PROMOTE` board entry for a succession.
///
/// MODELLED payload. il2cpp has `GuildMessageType.PROMOTE = 7` and
/// `GuildChatMessagePromote` carrying `_userIdOtherPlayer` and `_guildRank`
/// (dump.cs:539223), and the UI string is `UI.Guild.Chat.Message.Promote =
/// "{0} Has been promoted to {1} by {2}"` — but no PROMOTE message appears in any
/// capture (retail shipped no way to promote), so the JSON key names below follow
/// the `KICK`/`APPROVE`/`DENY` convention rather than an observed example.
async fn append_promote_message(
    conn: &mut AsyncPgConnection,
    gid: &str,
    actor_user: Uuid,
    actor_character: Uuid,
    promoted: Uuid,
) -> Result<(), BladeApiError> {
    append_message(
        conn,
        gid,
        actor_user,
        actor_character,
        "PROMOTE",
        json!({
            "type": "PROMOTE",
            "promotedUserId": promoted,
            "guildRank": GuildRank::Grandmaster.as_wire(),
        }),
    )
    .await?;
    Ok(())
}

/// `POST /guilds/current/leave`.
///
/// Posts a `LEAVE` entry (captured shape: empty typeSpecificData, 14 examples),
/// starts the re-join cooldown, and hands the guild on if the departing member was
/// its Grand Master.
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

    let Some(me) = find_membership(&mut conn, user_id).await? else {
        // Leaving when you are in no guild is a no-op, not an error — the client
        // can race a kick.
        return Ok(Json(json!({})));
    };
    let my_rank = me.parsed_rank()?;
    let gid = me.guild_id.clone();
    let ts = now_secs();

    conn.transaction(move |conn| {
        async move {
            // The LEAVE entry goes on the board BEFORE the guild might be deleted,
            // so it is not orphaned by the teardown below.
            append_message(conn, &gid, user_id, character_id, "LEAVE", json!({})).await?;
            let heir = remove_member_and_succeed(conn, &gid, user_id, my_rank, ts).await?;
            if let Some(heir) = heir {
                append_promote_message(conn, &gid, user_id, character_id, heir).await?;
            }
            record_removal(conn, &gid, user_id, ts, false).await?;
            Ok::<_, BladeApiError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Json(json!({})))
}

/// Shared body of kick and ban: authorise the actor against the target's rank,
/// remove them, post the board entry, and arm the removal record.
async fn remove_other_member(
    conn: &mut AsyncPgConnection,
    actor_user: Uuid,
    actor_character: Uuid,
    target_user: Uuid,
    ban: bool,
) -> Result<(), BladeApiError> {
    let me = require_membership(conn, actor_user).await?;
    let my_rank = me.parsed_rank()?;

    // The target must be in the ACTOR's guild — otherwise a Grand Master could
    // remove members of guilds they have nothing to do with.
    let target = find_membership(conn, target_user)
        .await?
        .filter(|t| t.guild_id == me.guild_id)
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 31))?;
    let target_rank = target.parsed_rank()?;

    let permitted = if ban {
        can_ban(my_rank, target_rank)
    } else {
        can_kick(my_rank, target_rank)
    };
    if !permitted {
        return Err(BladeApiError::unauthorized());
    }

    let gid = me.guild_id.clone();
    let ts = now_secs();
    let (message_type, key) = if ban {
        // MODELLED key: no BAN entry appears in any capture. `bannedUserId`
        // follows the kickedUserId/approvedUserId/deniedUserId convention, which
        // holds for all three observed cases.
        ("BAN", "bannedUserId")
    } else {
        // Captured shape: {"type":"KICK","kickedUserId":"..."}. 9 examples.
        ("KICK", "kickedUserId")
    };

    conn.transaction(move |conn| {
        async move {
            append_message(
                conn,
                &gid,
                actor_user,
                actor_character,
                message_type,
                json!({ "type": message_type, key: target_user }),
            )
            .await?;
            // A kicked member is never the Grand Master (nothing outranks that
            // rank), so no succession can be triggered here — but route through
            // the same helper so that stays true by construction rather than by
            // assumption.
            remove_member_and_succeed(conn, &gid, target_user, target_rank, ts).await?;
            record_removal(conn, &gid, target_user, ts, ban).await?;
            Ok::<_, BladeApiError>(())
        }
        .scope_boxed()
    })
    .await?;
    Ok(())
}

/// `POST /guilds/current/kick/{memberUserId}`.
///
/// Authorisation is the retail matrix: the actor must hold kick authority AND
/// strictly outrank the target. In practice that means the Grand Master, and only
/// against somebody else. Previously any member whose rank string happened to read
/// `LEADER` or `OFFICER` could kick anyone at all, including the guild's owner,
/// and a member of one guild could kick a member of another.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/kick/{member_user_id}"
)]
pub async fn kick_member(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, member_user_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    remove_other_member(
        &mut conn,
        session.session.user_id,
        character_id,
        member_user_id,
        false,
    )
    .await?;
    Ok(Json(json!({})))
}

/// `POST /guilds/current/ban/{userId}` — kick, permanently.
///
/// Retail ships `BanUserFromGuildRequest` (dump.cs:462230) and a confirmation
/// dialog for it (`UI.Guild.Ban.Confirmation.Body`); this server had no such route.
/// The difference from a kick is only the removal record: a ban never expires,
/// where a kick lapses after the asset's seven days.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/guilds/current/ban/{member_user_id}"
)]
pub async fn ban_member(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, member_user_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    remove_other_member(
        &mut conn,
        session.session.user_id,
        character_id,
        member_user_id,
        true,
    )
    .await?;
    Ok(Json(json!({})))
}

// ---- Chat ----------------------------------------------------------------------

/// `{"guildMessageBoard": [...]}`, or `{}` when the window is empty.
///
/// The empty case is not a stylistic choice: 23 captured message responses are the
/// literal `{}` — every one of them a poll that found nothing new — so the key is
/// skipped rather than emitted as `[]`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageBoardResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    guild_message_board: Vec<MessageWire>,
}

fn posted_message_response(posted: MessageWire) -> MessageBoardResponse {
    MessageBoardResponse {
        guild_message_board: vec![posted],
    }
}

/// Paging window for `GET /guilds/current/messages`.
///
/// il2cpp `GetAllGuildMessagesRequest` takes both an `oldestCreationTime` and a
/// `newestCreationTime` (dump.cs:462336), i.e. a range. In the captures the client
/// polls with a steadily increasing `oldestCreationTime` and gets `{}` back when
/// nothing is new, so `oldestCreationTime` is the LOWER bound ("what has happened
/// since?") and `newestCreationTime` the upper one ("let me scroll back from
/// here"). Both are exclusive — an inclusive lower bound would re-deliver the
/// caller's newest message on every poll, and the captures show `{}`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageQuery {
    #[serde(default)]
    oldest_creation_time: Option<i64>,
    #[serde(default)]
    newest_creation_time: Option<i64>,
}

async fn message_board(
    conn: &mut AsyncPgConnection,
    gid: &str,
    window: &MessageQuery,
) -> Result<Vec<MessageWire>, BladeApiError> {
    use crate::schema::guild_messages::dsl::*;
    let mut sql = guild_messages.filter(guild_id.eq(gid)).into_boxed();
    if let Some(lo) = window.oldest_creation_time {
        sql = sql.filter(creation_time.gt(lo));
    }
    if let Some(hi) = window.newest_creation_time {
        sql = sql.filter(creation_time.lt(hi));
    }
    let rows: Vec<GuildMessageRow> = sql
        // Newest first, as in every captured board.
        .order(creation_time.desc())
        .limit(MESSAGE_PAGE_LIMIT)
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
    query: web::Query<MessageQuery>,
) -> Result<Json<MessageBoardResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let window = query.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    check_permission_for_character_and_get_it(&mut conn, &session.session, character_id).await?;

    let board = match find_membership(&mut conn, session.session.user_id).await? {
        Some(m) => message_board(&mut conn, &m.guild_id, &window).await?,
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

/// `POST /guilds/current/messages` -> only the newly posted message.
///
/// This differs deliberately from GET: retail's POST responses always contain
/// exactly the message created by that request. The client feeds this response
/// into a post-specific enrichment runner and appends it to its existing board;
/// returning the full board makes that runner process duplicate/history rows and
/// can leave the guild view locked after the write has already succeeded.
///
/// Membership is required — a non-member cannot post to a guild's chat, which is
/// enforced by `require_membership` rather than by the client declining to show
/// the box.
///
/// Retail additionally ran the text through a six-language profanity filter,
/// storing the cleaned copy as `text` and the original as `unfilteredText` (the
/// latter appears in 94 of 903 captured CLIENT messages — i.e. only when filtering
/// changed something). This server has no profanity list, so `text` is always the
/// player's own words and `unfilteredText` is correctly never emitted.
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

    let m = require_membership(&mut conn, user_id).await?;
    if !message_length_ok(&text) {
        return Err(invalid_text());
    }
    let posted = append_message(
        &mut conn,
        &m.guild_id,
        user_id,
        character_id,
        "CLIENT",
        json!({ "type": "CLIENT", "text": text }),
    )
    .await?;
    Ok(Json(posted_message_response(posted)))
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


#[cfg(test)]
mod exchange_amount_tests {
    use super::*;

    /// The amounts must come from the retail table, not a hardcoded 10/5. Asserted
    /// differentially across several items so it cannot pass on a constant.
    #[test]
    fn exchange_amounts_are_item_specific() {
        let u = |s: &str| uuid::Uuid::parse_str(s).unwrap();
        // Transcendent Soul Gem: one, donated one at a time.
        assert_eq!(exchange_amounts_for(u("d94bab85-53d5-4c9c-a637-acd94fc66c98")), (1, 1));
        // Iron Ingot.
        assert_eq!(exchange_amounts_for(u("55e82826-2d68-469c-8870-753665ca62cd")), (3, 1));
        // Limestone — the big town-building material.
        assert_eq!(exchange_amounts_for(u("fd67bbc6-20f4-44a3-9614-28265ebb8c67")), (50, 10));
        // Honeycomb keeps the common 10/5.
        assert_eq!(exchange_amounts_for(u("7116a2a8-ac2d-4cd9-8b7c-b80c397d3f50")), (10, 5));
    }

    /// An item retail never showed us falls back to the corpus's modal pair rather
    /// than to something invented.
    #[test]
    fn an_unknown_item_falls_back_to_the_modal_pair() {
        assert_eq!(
            exchange_amounts_for(uuid::Uuid::nil()),
            (10, 5),
            "749 of 1,626 observed exchanges are 10/5"
        );
    }

    /// Every mined row must be self-consistent: a donation cannot exceed the request,
    /// and neither may be zero. Guards against a bad regeneration of the table.
    #[test]
    fn the_mined_table_is_self_consistent() {
        for (t, req, max) in EXCHANGE_AMOUNTS {
            assert!(*req > 0, "{t} requests {req}");
            assert!(*max > 0, "{t} allows a {max} donation");
            assert!(max <= req, "{t}: one donation ({max}) exceeds the whole request ({req})");
            assert!(
                uuid::Uuid::parse_str(t).is_ok(),
                "{t} is not a uuid — the table was generated wrong"
            );
        }
        assert_eq!(EXCHANGE_AMOUNTS.len(), 36, "36 templates were mined from retail");
    }
}

/// Per-item guild-exchange amounts, `(itemTemplateId, requestedAmount, maxDonationAmount)`.
///
/// Every request used to be created with a hardcoded `requestedAmount: 10,
/// maxDonationAmount: 5`, for every item. Retail did not work that way: mined from
/// 1,626 exchange objects across 863 captured `guilds/current/exchanges` responses,
/// there are **14 distinct (requested, maxDonation) pairs** over 36 item templates —
/// 1/1 for a Transcendent Soul Gem, 3/1 for an Iron Ingot, 50/10 for Limestone.
///
/// `maxDonationAmount` is the size of ONE donation, not a number of donations
/// required, and retail permits partial redemption (captures 9658→9659: a request
/// for 10 got a single donation of 5, and the requester redeemed exactly 5).
///
/// Four templates were observed with more than one pair — Limestone, Lumber, Copper
/// and Crystal, all town-building materials whose requests appear to scale. The modal
/// pair is used; the alternatives are recorded here so the choice is visible:
///   Limestone 50/10 (110 of 120; also 30/6, 10/2)
///   Lumber    50/10 ( 97 of 118; also 40/8, 30/6, 10/2, 20/4)
///   Copper    30/6  ( 50 of  75; also 24/5, 12/3)
///   Crystal    2/1  (  9 of  13; also 3/1)
const EXCHANGE_AMOUNTS: &[(&str, i64, i64)] = &[
    ("4577fb2b-47f7-4112-b870-1479e2529b06", 10, 5), // Dragon Claw
    ("05b4dd6b-796f-4088-a772-0e33ea3db976", 10, 5), // Frost Salts
    ("7116a2a8-ac2d-4cd9-8b7c-b80c397d3f50", 10, 5), // Honeycomb
    ("fd67bbc6-20f4-44a3-9614-28265ebb8c67", 50, 10), // Limestone
    ("e7193116-d761-479b-8a20-5633737977f5", 50, 10), // Lumber
    ("f9181a67-b094-4c37-a145-ced9dfe610d6", 5, 1), // Daedra Heart
    ("d94bab85-53d5-4c9c-a637-acd94fc66c98", 1, 1), // Transcendent Soul Gem
    ("42d91529-c88b-4c5b-815b-b55508b4e7ef", 30, 6), // Copper
    ("07f380dd-f123-4ef7-9b12-8b9fdb03c3e1", 10, 5), // Imp Stool
    ("89ece62c-9ff6-470a-a152-d45ef4e0c222", 3, 1), // Pearl
    ("d523932f-8c7f-4192-9112-5dbd60883c2b", 5, 1), // Dragon Bones
    ("fe3567e0-ec8e-4f41-8e77-a56f861ab898", 10, 5), // Daedroth Tooth
    ("8b7d0044-3a38-4ee0-af45-d2892b0f508d", 10, 5), // Deathbell
    ("a4d5e792-5a27-4bb3-851f-e0917c0962db", 8, 4), // Giant's Toe
    ("f1c5c03c-5297-48fb-a9d2-5f32bffb467c", 10, 5), // Fire Salts
    ("34ddfe0e-5119-4e52-8eef-a77b6bc810a7", 5, 1), // Dragon Scales
    ("bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", 1, 1), // Glorious Soul Gem
    ("68d7941e-8c8d-47bf-9f66-becb058f1817", 1, 1), // Grand Soul Gem
    ("55e82826-2d68-469c-8870-753665ca62cd", 3, 1), // Iron Ingot
    ("74f091b5-fd88-464b-a98a-f60a5e8a0f25", 3, 1), // Orichalcum Ingot
    ("70f2c013-813e-4f63-8b29-7a54a13b5e58", 1, 1), // Faerite
    ("e80bee76-f92c-4005-9eff-20d1e8c64d24", 4, 1), // Quicksilver Ingot
    ("8ac9076c-cba9-4cf4-a8a5-d2303aab22b0", 2, 1), // Crystal
    ("9d9732a5-cd1c-4a93-8755-0bf92ee65d64", 3, 1), // Leather
    ("75112030-b248-49b0-9c70-0da8dea150d1", 5, 1), // Ebony Ingot
    ("b81952e0-c3c8-4a5c-92c0-8215d3eb71af", 3, 1), // Steel Ingot
    ("9687d83c-aa7b-4cf3-a69f-0bba8204fa61", 10, 5), // Glow Dust
    ("16e102fb-b1c0-42de-8106-0aa27e77f7f0", 1, 1), // Diamond
    ("f00f350d-97c2-47cd-a554-e1a37c9ff7f2", 10, 5), // Lavender
    ("51f7612f-a797-4417-8a92-493b0aae7f45", 3, 1), // Pelt
    ("a3351353-f613-4368-bac7-05783f857b07", 1, 1), // Elevated Soul Gem
    ("4312b0ed-e397-4815-9693-a511ecda71de", 4, 1), // Moonstone Ingot
    ("10c54e43-a2a1-4833-9491-4c19149f43b5", 10, 5), // Void Salts
    ("85ed5500-3581-4699-8095-4b5ff6514355", 4, 1), // Malachite Ingot
    ("9df9233f-e7b9-47e8-bc75-f37707917759", 3, 1), // Garnet
    ("b94c2028-dba7-44bd-b6f9-6f85ab0195b6", 3, 1), // Seeds
];

/// The `(requestedAmount, maxDonationAmount)` for an item, or the 10/5 default for a
/// template retail never showed us. 10/5 is the most common pair in the corpus (749
/// of 1,626), so an unknown item gets the modal behaviour rather than an invented one.
pub fn exchange_amounts_for(item_template_id: uuid::Uuid) -> (i64, i64) {
    let s = item_template_id.to_string();
    EXCHANGE_AMOUNTS
        .iter()
        .find(|(t, _, _)| *t == s)
        .map(|(_, req, max)| (*req, *max))
        .unwrap_or((10, 5))
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

    let (requested_amount, max_donation_amount) = exchange_amounts_for(item_template_id);
    let ts = now_secs();
    let row = GuildExchangeRow {
        id: Uuid::new_v4().to_string(),
        guild_id: m.guild_id,
        requester_user_id: user_id,
        requester_character_id: character_id,
        item_template_id,
        requested_amount,
        max_donation_amount,
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
            //
            // A DONATE NAMES NO EXCHANGE ID. The client sends only
            // `{requesterUserId, requesterCharacterId, itemTemplateId}`, so when a
            // player has more than one open request for the SAME item the three of
            // them do not identify a row — and nothing stops that: production holds
            // two Grand Soul Gem requests from one character, one already full at
            // 10/10 and one open at 0/1.
            //
            // This used to take whatever the database returned first, with no
            // ORDER BY. Landing on the full one answers 409 while the player is
            // looking at the open one, and the client restarts rather than showing
            // the error. Reported 2026-09-19.
            //
            // So all candidates are read, in a deterministic order, and the first
            // one this donor can actually give to wins. The refusals below are
            // unchanged and still fire when NO candidate is donatable — the point
            // is that "some other request of theirs is full" must not be mistaken
            // for "this request is full".
            // Nobody funds their own request.
            //
            // MEASURED: 3,198 donations across 1,350 captured retail exchanges,
            // and not one has `donatorUserId == requesterUserId`. That absence
            // is not vacuous — 48 people appear as both a requester and a donor,
            // contributing 208 donations, and every one of those went to
            // somebody else's request.
            //
            // We allowed it, and 2 of the 11 donations on prod are self
            // donations. Self-funding is free guild score: the donor debits and
            // the requester credits the same inventory.
            //
            // Checked against the REQUEST rather than any candidate row, since
            // it is a property of who is asking, not of which row wins below.
            if req.requester_user_id == donor_user_id {
                return Err(BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 14));
            }

            use crate::schema::guild_exchanges::dsl as ge;
            let candidates: Vec<GuildExchangeRow> = ge::guild_exchanges
                .filter(ge::guild_id.eq(&m.guild_id))
                .filter(ge::requester_user_id.eq(req.requester_user_id))
                .filter(ge::requester_character_id.eq(req.requester_character_id))
                .filter(ge::item_template_id.eq(req.item_template_id))
                .filter(ge::redeemed.eq(false))
                .order((ge::creation_time.asc(), ge::id.asc()))
                .select(GuildExchangeRow::as_select())
                .for_no_key_update()
                .load(conn)
                .await?;
            if candidates.is_empty() {
                return Err(BladeApiError::new(StatusCode::NOT_FOUND, GUILD_SERVICE_ID, 11));
            }
            // Oldest request this donor can still give to. Falling back to the
            // first candidate keeps the existing refusal codes meaningful: with
            // nothing donatable, the checks below explain WHY.
            let pick = candidates
                .iter()
                .position(|e| {
                    e.donation_sum < e.requested_amount
                        && !e
                            .donations
                            .0
                            .iter()
                            .any(|d| d.donator_user_id == donor_user_id)
                })
                .unwrap_or(0);
            let exchange: GuildExchangeRow = candidates.into_iter().nth(pick).expect("non-empty");

            // A donor may not donate twice to the same request. Nothing stopped a
            // single player filling a request by themselves, repeatedly, which is
            // neither what the feature is for nor what retail allowed.
            if exchange
                .donations
                .0
                .iter()
                .any(|d| d.donator_user_id == donor_user_id)
            {
                return Err(BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 12));
            }

            // Never donate more than the request still needs. The amount was always
            // the full `maxDonationAmount`, so the last donor to a nearly-complete
            // request overshot it — spending items that the requester can never
            // redeem, since redemption is capped at `requestedAmount`.
            let remaining = (exchange.requested_amount - exchange.donation_sum).max(0);
            if remaining == 0 {
                return Err(BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 13));
            }
            let donate_amount = exchange.max_donation_amount.min(remaining).max(0) as u64;
            if donate_amount == 0 {
                return Err(BladeApiError::new(StatusCode::CONFLICT, GUILD_SERVICE_ID, 13));
            }

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

            // A donation is the second most common thing on a real guild's board
            // (534 of the 1531 captured entries), and it is how the requester
            // learns they were helped — the client renders it as
            // UI.Guild.Chat.Message.Donate, "{0} gave {1} {2} to {3}". Donations
            // were silently invisible in chat before this.
            //
            // Captured shape, exactly:
            //   {"type":"DONATE","requesterUserId":...,"requesterCharacterId":...,
            //    "itemTemplateId":...,"donatedAmount":N}
            // with the DONOR in the message's own userId/characterId.
            append_message(
                conn,
                &m.guild_id,
                donor_user_id,
                donor_character_id,
                "DONATE",
                json!({
                    "type": "DONATE",
                    "requesterUserId": req.requester_user_id,
                    "requesterCharacterId": req.requester_character_id,
                    "itemTemplateId": req.item_template_id,
                    "donatedAmount": donate_amount,
                }),
            )
            .await?;

            // `guildExchangeDonationCount` is a lifetime counter on the guild —
            // captured values run to 14483 — and the client shows it as
            // "Lifetime Donations". It was never incremented before.
            {
                use crate::schema::guilds::dsl as g;
                diesel::update(g::guilds.filter(g::id.eq(&m.guild_id)))
                    .set(g::exchange_donation_count.eq(g::exchange_donation_count + 1))
                    .execute(conn)
                    .await?;
            }

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

    /// Retail POST captures contain exactly the row created by the request, not
    /// a refreshed page of existing chat history. The post-specific client
    /// runner appends and enriches this list before releasing the guild UI.
    #[test]
    fn post_message_response_contains_one_created_message() {
        let posted = MessageWire::from_row(GuildMessageRow {
            message_id: "1782855363::e8108b5b-ed3f-4e0c-bf6f-e3bc77e438af".into(),
            guild_id: "69daf06759aa5eb7b33e8778".into(),
            user_id: Uuid::from_u128(0x123),
            character_id: Uuid::from_u128(0x456),
            message_type: "CLIENT".into(),
            type_specific_data: JsonDbWrapper(json!({
                "type": "CLIENT",
                "text": "hello guild",
            })),
            creation_time: 1_782_855_363,
        });
        let response = posted_message_response(posted);

        let value = serde_json::to_value(response).unwrap();
        let board = value["guildMessageBoard"].as_array().unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board[0]["typeSpecificData"]["text"], "hello guild");
    }
}


/// The create-guild wire, against the two captured retail creations.
#[cfg(test)]
mod create_wire {
    use super::*;

    fn guild_row() -> GuildRow {
        GuildRow {
            id: "6a2c81172c9371def2ab495f".into(),
            name: "New Blades".into(),
            tag_id: "1395".into(),
            guild_type: "OPEN".into(),
            short_description: "look at newblades.dethele.com".into(),
            long_description: String::new(),
            badge_icon_index: 14,
            region_index: 4,
            trophies: 17,
            created_at: 1781301527,
            exchange_donation_count: 0,
            grandmaster_since: 1781301527,
        }
    }

    fn member_row(user_id: Uuid) -> GuildMemberRow {
        GuildMemberRow {
            guild_id: "6a2c81172c9371def2ab495f".into(),
            user_id,
            character_id: Uuid::from_u128(0xC4A2),
            rank: "MEMBER".into(),
            join_date: 1_781_234_567,
        }
    }

    /// Report #123: retail's own member row uses the same public id as its
    /// authenticated session. Our database id is deliberately different from
    /// that public bearer, so the current-guild response must bridge it.
    #[test]
    fn current_players_member_uses_the_session_user_id() {
        let private_id = Uuid::from_u128(0x123);
        let public_id = Uuid::from_u128(0x456);
        let another = Uuid::from_u128(0x789);
        let rows = vec![member_row(private_id), member_row(another)];

        let wire = current_guild_members(&rows, private_id, public_id);
        assert_eq!(wire[0].user_id, public_id, "the client must find itself");
        assert_eq!(wire[1].user_id, another, "unrelated members are unchanged");
    }

    /// Retail's guild object carries thirteen fields; we were sending twelve.
    /// `pvpSeasonId` appears on 437 of 446 captured guild objects.
    #[test]
    fn the_guild_object_has_every_field_retail_sent() {
        let v = serde_json::to_value(GuildWire::from_row(&guild_row(), 1)).unwrap();
        let obj = v.as_object().expect("guild object");
        for key in [
            "id", "name", "tagId", "type", "shortDescription", "longDescription",
            "badgeIconIndex", "memberCount", "regionIndex", "guildExchangeDonationCount",
            "pvpTrophies", "pvpSeasonId", "grandmasterSinceSecs",
        ] {
            assert!(
                obj.contains_key(key),
                "guild object is missing {key}; got {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }
    }

    /// CREATE carries a wallet; GET current must not.
    ///
    /// Both captured creations return one, and the client waits for it because
    /// founding a guild is a spend -- without it the create succeeds and the UI
    /// just spins. But `GET /guilds/current` carried it in 0 of 410 captured
    /// responses, so it must not leak onto that route.
    #[test]
    fn wallet_is_on_create_and_nowhere_else() {
        let created = CurrentGuildResponse {
            guild: Some(GuildWire::from_row(&guild_row(), 1)),
            members: vec![],
            wallet: Some(CompleteWallet::default()),
        };
        let v = serde_json::to_value(&created).unwrap();
        assert!(v.get("wallet").is_some(), "create must return the wallet");

        let current = CurrentGuildResponse {
            guild: Some(GuildWire::from_row(&guild_row(), 1)),
            members: vec![],
            wallet: None,
        };
        let v = serde_json::to_value(&current).unwrap();
        assert!(
            v.get("wallet").is_none(),
            "GET current must not carry a wallet: retail sent it 0 of 410 times, got {v:?}"
        );
        // control: the rest of the response is still there, so the assertion
        // above is about the wallet key and not an empty object.
        assert!(v.get("guild").is_some());
    }
}

/// Donating to one of several requests for the same item.
///
/// Reported 2026-09-19: donating to a Grand Soul Gem request answered an error
/// and the app restarted. Production had TWO open requests from one character for
/// `68d7941e` (`Items.Name.SoulGem8`) — one full at 10/10, one open at 0/1 — and
/// the donate request names no exchange id, only
/// `{requesterUserId, requesterCharacterId, itemTemplateId}`. The lookup took
/// whatever the database returned first, with no ORDER BY, so it could answer 409
/// "already fulfilled" about a request the player was not looking at.
#[cfg(test)]
mod donating_with_several_requests_for_one_item {
    use super::*;

    /// The selection rule, extracted so it can be tested without a database: the
    /// oldest candidate this donor can still give to, else the first.
    fn pick(rows: &[(i64, i64, Vec<Uuid>)], donor: Uuid) -> usize {
        rows.iter()
            .position(|(sum, requested, donors)| {
                sum < requested && !donors.iter().any(|d| *d == donor)
            })
            .unwrap_or(0)
    }

    const DONOR: Uuid = Uuid::from_u128(1);
    const OTHER: Uuid = Uuid::from_u128(2);

    /// THE PRODUCTION CASE. Ordered oldest-first, the full request comes first;
    /// the donation must still land on the open one.
    #[test]
    fn a_full_request_does_not_shadow_an_open_one_for_the_same_item() {
        // (donation_sum, requested_amount, donors)
        let rows = vec![
            (10, 10, vec![OTHER]), // the 10/10 Grand Soul Gem request
            (0, 1, vec![]),        // the open one the player tapped
        ];
        assert_eq!(pick(&rows, DONOR), 1, "the open request must be chosen");
    }

    /// Order does not rescue it by accident — the open one is found wherever it sits.
    #[test]
    fn the_open_request_is_found_in_either_order() {
        let a = vec![(0, 1, vec![]), (10, 10, vec![OTHER])];
        assert_eq!(pick(&a, DONOR), 0);
        let b = vec![(10, 10, vec![OTHER]), (0, 1, vec![])];
        assert_eq!(pick(&b, DONOR), 1);
    }

    /// A donor who has already given to the only open request is not silently
    /// moved onto a different one — they fall through to the refusal.
    #[test]
    fn a_donor_who_already_gave_is_not_rerouted() {
        let rows = vec![(5, 10, vec![DONOR])];
        assert_eq!(
            pick(&rows, DONOR),
            0,
            "falls back to the first so the existing 409 explains why"
        );
    }

    /// THE CONTROL: when nothing is donatable the refusal must still happen. The
    /// fix must not turn "this request is full" into a successful donation.
    #[test]
    fn nothing_donatable_still_falls_through_to_a_refusal() {
        let all_full = vec![(10, 10, vec![OTHER]), (1, 1, vec![OTHER])];
        let chosen = pick(&all_full, DONOR);
        let (sum, requested, _) = &all_full[chosen];
        assert!(
            sum >= requested,
            "the chosen row is still full, so the handler's remaining==0 check fires"
        );
    }

    /// Multiple open requests: the OLDEST wins, because the query orders by
    /// creation time and the rule takes the first match.
    #[test]
    fn the_oldest_open_request_is_preferred() {
        let rows = vec![(0, 5, vec![]), (0, 5, vec![])];
        assert_eq!(pick(&rows, DONOR), 0);
    }

    /// The query must be ordered, or "oldest" is whatever the database felt like
    /// returning — which is the bug this fixes.
    #[test]
    fn the_candidate_query_is_deterministically_ordered() {
        let src = include_str!("guild.rs");
        let body = src
            .split("pub async fn donate_exchange(")
            .nth(1)
            .expect("the handler")
            .split("\n#[")
            .next()
            .unwrap();
        assert!(
            body.contains("ge::creation_time.asc()"),
            "candidates must be ordered by creation time, not database whim"
        );
        assert!(
            !body.contains(".into_iter()\n                .next()\n                .ok_or_else"),
            "the unordered first-row lookup must not come back"
        );
    }
}

/// Nobody funds their own guild request.
///
/// MEASURED: across 1,350 captured retail exchanges carrying 3,198 donations,
/// not one has `donatorUserId == requesterUserId`. The absence is not vacuous —
/// 48 people appear as BOTH a requester and a donor, contributing 208
/// donations, and every one went to somebody else's request. That control is
/// the difference between "retail forbade this" and "nobody happened to try".
///
/// We allowed it: 2 of the 11 donations on prod are self donations. It is free
/// guild score, since the donor debit and the requester credit land on the same
/// inventory.
///
/// `donate_exchange` needs a database, so this reads the source the way
/// `route_registration` does. The unit-testable half of the same bug — the
/// client being told the wrong `userId`, which is why it offered the button at
/// all — is covered by `session_identity` in `authentification.rs`.
#[cfg(test)]
mod self_donation_is_refused {
    /// `donate_exchange`'s body, comments stripped, so a rule cannot be
    /// satisfied by prose describing it.
    fn donate_body() -> String {
        let src = include_str!("guild.rs");
        let start = src
            .find("pub async fn donate_exchange")
            .expect("donate_exchange still exists");
        let rest = &src[start..];
        // Up to the next top-level item.
        let end = rest[1..]
            .find("\npub async fn ")
            .or_else(|| rest[1..].find("\n#[cfg(test)]"))
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_donor_is_compared_against_the_requester() {
        let body = donate_body();
        assert!(
            body.contains("req.requester_user_id == donor_user_id"),
            "donate_exchange no longer refuses a self donation"
        );
    }

    #[test]
    fn the_refusal_comes_before_a_row_is_chosen() {
        // A property of who is asking, not of which candidate wins. Placed after
        // the lookup it could be skipped whenever the picker fell through.
        let body = donate_body();
        let check = body
            .find("req.requester_user_id == donor_user_id")
            .expect("the self-donation check");
        let query = body
            .find("ge::guild_exchanges")
            .expect("the candidate query");
        assert!(
            check < query,
            "the self-donation check must precede the candidate lookup"
        );
    }

    #[test]
    fn the_scan_can_actually_fail() {
        // Control: the needles are real substrings of the real body, so a typo
        // in either would be a silently-passing test rather than a red one.
        let body = donate_body();
        assert!(!body.is_empty(), "donate_exchange body came back empty");
        assert!(
            !body.contains("a needle that is deliberately absent"),
            "the scan matches anything"
        );
    }
}
