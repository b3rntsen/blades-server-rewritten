//! Guild management for the website — a Grand Master's roster and the kick.
//!
//! Owner request (2026-11-15): "let a guild master manage a guild on their
//! profile … see guild member activity current season and before … kick people
//! from the admin page."
//!
//! Three dev-token routes, each with ONE intended web caller:
//!
//! ```text
//! GET  /api/dev/v1/guild-seats?userId=…        which guild seats a user's alts hold
//! GET  /api/dev/v1/guilds/{guild_id}/roster    members + activity + season history
//! POST /api/dev/v1/guilds/{guild_id}/kick      remove one seat (dry run by default)
//! ```
//!
//! # Seats are per ALT, and an alt may be parked
//!
//! A user's alts take turns on one `characters` row. Switching parks the
//! outgoing alt's guild seat in its newest `character_versions.server_state`
//! (`parkedGuildMembers`, admin.rs `ALT_ROW_TABLES`) and deletes the live
//! `guild_members` row. The guild score still counts that alt
//! (`season_store::FREEZE_STANDINGS_SQL` reads the newest version's parked
//! guild), so the roster lists parked seats too, flagged, and the kick can
//! remove one — from every version carrying it, so it cannot come back on the
//! next switch.
//!
//! # Who may kick
//!
//! Retail lets a Grand Master kick from the guild screen (`KickUserFromGuild
//! Request`; 9 captured `{"type":"KICK","kickedUserId":…}` board entries), so a
//! Grand Master kicking from their profile is the game's own power, with the
//! game's own rule: [`guild_policy::can_kick`] — Grand Master only, strictly
//! outranking the target, so never themselves and never another Grand Master.
//! A site admin (the web gates that) kicks as an operator, like the
//! Grand Master handover in `guild_admin`. [`plan_kick`] is the whole decision,
//! pure, so it is tested against the negatives.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use diesel::sql_types::{Array, BigInt, Bool, Int4, Nullable, Text, Uuid as SqlUuid};
use diesel::{OptionalExtension, QueryableByName, sql_query};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    admin::check_import_token,
    arena::season_store,
    guild::{SeatRemover, remove_seat_for_website},
    guild_policy::{GuildRank, can_kick},
};

/// The guild console's envelope id (`guild_admin`), codes 40+.
const SERVICE_ID: u64 = 9002;

/// Previous seasons kept per member. The page shows a summary, not an archive.
const HISTORY_SEASONS: usize = 8;

fn err(status: StatusCode, code: u64) -> BladeApiError {
    BladeApiError::new(status, SERVICE_ID, code)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Pure policy
// ---------------------------------------------------------------------------

/// The member being removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KickTarget {
    pub user_id: Uuid,
    /// The EFFECTIVE rank: a parked Grand Master seat in a guild that has a
    /// live Grand Master reads as MEMBER, because that is what it comes back as
    /// (`ALT_ROW_TABLES` restore).
    pub rank: GuildRank,
    pub parked: bool,
}

/// Who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickActor {
    /// A site admin. The web route gates the role; the arena sees only the token.
    Operator,
    /// A player, from their profile, holding this LIVE seat in the guild.
    Member { user_id: Uuid, rank: GuildRank },
    /// A player with no live seat in this guild.
    Outsider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KickRefusal {
    /// The actor is not (live) in this guild.
    ActorNotInGuild,
    /// Retail has no self-kick; leaving is its own action.
    Yourself,
    /// `can_kick` says no: not a Grand Master, or the target is one.
    NotPermitted,
    /// An operator removing the guild's only live member would delete the
    /// guild (a leave's teardown). Refused: that is not what "kick" means.
    LastMember,
}

/// May `actor` remove `target`? `live_members` counts the guild's live seats.
pub fn plan_kick(
    actor: KickActor,
    target: KickTarget,
    live_members: usize,
) -> Result<(), KickRefusal> {
    match actor {
        KickActor::Outsider => Err(KickRefusal::ActorNotInGuild),
        KickActor::Member { user_id, rank } => {
            if user_id == target.user_id {
                return Err(KickRefusal::Yourself);
            }
            if !can_kick(rank, target.rank) {
                return Err(KickRefusal::NotPermitted);
            }
            Ok(())
        }
        KickActor::Operator => {
            if !target.parked && live_members <= 1 {
                return Err(KickRefusal::LastMember);
            }
            Ok(())
        }
    }
}

/// Which seat of a user may manage its guild on the website: a Grand Master's.
/// A parked Grand Master seat counts only while the guild has no live one — a
/// returning alt never makes two (`ALT_ROW_TABLES` restore).
pub fn seat_can_manage(rank: &str, parked: bool, guild_has_live_grandmaster: bool) -> bool {
    GuildRank::from_wire(rank) == Some(GuildRank::Grandmaster)
        && (!parked || !guild_has_live_grandmaster)
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Newest version per parked alt of the characters in `$1`, one row per parked
/// guild seat. The same "newest version" rule the standings use.
const PARKED_SEATS_SQL: &str = r#"
SELECT e ->> 'guild_id'                    AS guild_id,
       v.character_id,
       v.source_alt_uuid                   AS alt_key,
       COALESCE(c.user_id, v.user_id)      AS user_id,
       COALESCE(e ->> 'rank', 'MEMBER')    AS rank,
       COALESCE((e ->> 'join_date')::bigint, 0) AS join_date,
       COALESCE(NULLIF(v.name, ''), v."character" ->> 'name', '') AS name,
       v.level,
       v.saved_at
  FROM characters c
  JOIN LATERAL (
        SELECT DISTINCT ON (x.source_alt_uuid) x.*
          FROM character_versions x
         WHERE x.character_id = c.id AND x.source_alt_uuid IS NOT NULL
         ORDER BY x.source_alt_uuid, x.saved_at DESC
       ) v ON true
  CROSS JOIN LATERAL jsonb_array_elements(
        CASE WHEN jsonb_typeof(v.server_state -> 'parkedGuildMembers') = 'array'
             THEN v.server_state -> 'parkedGuildMembers' ELSE '[]'::jsonb END) e
 WHERE c.id = ANY($1)
   AND v.source_alt_uuid IS DISTINCT FROM c.source_alt_uuid
   AND e ->> 'guild_id' IS NOT NULL
"#;

#[derive(QueryableByName, Debug, Clone)]
struct ParkedSeatRow {
    #[diesel(sql_type = Text)]
    guild_id: String,
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    alt_key: Option<Uuid>,
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    rank: String,
    #[diesel(sql_type = BigInt)]
    join_date: i64,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Int4)]
    level: i32,
    #[diesel(sql_type = BigInt)]
    saved_at: i64,
}

#[derive(QueryableByName, Debug, Clone)]
struct LiveSeatRow {
    #[diesel(sql_type = Text)]
    guild_id: String,
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    alt_key: Option<Uuid>,
    #[diesel(sql_type = SqlUuid)]
    stored_user_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    rank: String,
    #[diesel(sql_type = BigInt)]
    join_date: i64,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Int4)]
    level: i32,
}

const LIVE_SEAT_COLUMNS: &str = r#"
SELECT gm.guild_id, gm.character_id, c.source_alt_uuid AS alt_key,
       gm.user_id AS stored_user_id,
       COALESCE(c.user_id, gm.user_id) AS user_id,
       gm.rank, gm.join_date,
       COALESCE(c."character" ->> 'name', '') AS name,
       COALESCE((c."character" ->> 'level')::int, 0) AS level
  FROM guild_members gm
  LEFT JOIN characters c ON c.id = gm.character_id
"#;

async fn live_seats_of_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<LiveSeatRow>, diesel::result::Error> {
    sql_query(format!("{LIVE_SEAT_COLUMNS} WHERE gm.guild_id = $1"))
        .bind::<Text, _>(gid)
        .get_results(conn)
        .await
}

/// Parked seats in `gid`: candidates found by containment, then kept only when
/// the alt's NEWEST version still carries the seat.
async fn parked_seats_of_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<ParkedSeatRow>, diesel::result::Error> {
    #[derive(QueryableByName)]
    struct Cand {
        #[diesel(sql_type = SqlUuid)]
        character_id: Uuid,
    }
    let cands: Vec<Cand> = sql_query(
        "SELECT DISTINCT character_id FROM character_versions
          WHERE server_state -> 'parkedGuildMembers'
                @> jsonb_build_array(jsonb_build_object('guild_id', $1::text))",
    )
    .bind::<Text, _>(gid)
    .get_results(conn)
    .await?;
    if cands.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = cands.into_iter().map(|c| c.character_id).collect();
    let rows: Vec<ParkedSeatRow> = sql_query(PARKED_SEATS_SQL)
        .bind::<Array<SqlUuid>, _>(ids)
        .get_results(conn)
        .await?;
    Ok(rows.into_iter().filter(|r| r.guild_id == gid).collect())
}

fn effective_rank(stored: &str, parked: bool, guild_has_live_gm: bool) -> String {
    if parked && stored == GuildRank::Grandmaster.as_wire() && guild_has_live_gm {
        GuildRank::Member.as_wire().to_string()
    } else {
        stored.to_string()
    }
}

// ---------------------------------------------------------------------------
// GET /guild-seats?userId=
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatsQuery {
    pub user_id: Uuid,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SeatView {
    pub guild_id: String,
    pub guild_name: String,
    pub character_id: Uuid,
    pub alt_key: Option<Uuid>,
    pub name: String,
    pub rank: String,
    pub parked: bool,
    /// May this seat manage the guild on the website ([`seat_can_manage`]).
    pub can_manage: bool,
}

/// `GET /…/api/dev/v1/guild-seats?userId=<public arena user id>` — the seats
/// held by that account's live alt and its parked alts. The profile reads it
/// to decide whether to show "Manage guild" at all.
#[get("/blades.bgs.services/api/dev/v1/guild-seats")]
pub async fn guild_seats(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<SeatsQuery>,
) -> Result<Json<Vec<SeatView>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, 40))?;
    Ok(Json(seats_of_user(&mut conn, query.user_id).await?))
}

pub(crate) async fn seats_of_user(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> Result<Vec<SeatView>, diesel::result::Error> {
    let live: Vec<LiveSeatRow> = sql_query(format!(
        "{LIVE_SEAT_COLUMNS} WHERE c.user_id = $1 OR gm.user_id = $1"
    ))
    .bind::<SqlUuid, _>(user_id)
    .get_results(conn)
    .await?;
    #[derive(QueryableByName)]
    struct Id {
        #[diesel(sql_type = SqlUuid)]
        id: Uuid,
    }
    let chars: Vec<Uuid> = sql_query("SELECT id FROM characters WHERE user_id = $1")
        .bind::<SqlUuid, _>(user_id)
        .get_results::<Id>(conn)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let parked: Vec<ParkedSeatRow> = if chars.is_empty() {
        Vec::new()
    } else {
        sql_query(PARKED_SEATS_SQL)
            .bind::<Array<SqlUuid>, _>(chars)
            .get_results(conn)
            .await?
    };

    let mut guild_ids: Vec<String> = live.iter().map(|s| s.guild_id.clone()).collect();
    guild_ids.extend(parked.iter().map(|s| s.guild_id.clone()));
    guild_ids.sort();
    guild_ids.dedup();
    #[derive(QueryableByName)]
    struct GuildHead {
        #[diesel(sql_type = Text)]
        id: String,
        #[diesel(sql_type = Text)]
        name: String,
        #[diesel(sql_type = Bool)]
        has_gm: bool,
    }
    let heads: Vec<GuildHead> = sql_query(
        "SELECT g.id, g.name,
                EXISTS (SELECT 1 FROM guild_members m
                         WHERE m.guild_id = g.id AND m.rank = 'GRANDMASTER') AS has_gm
           FROM guilds g WHERE g.id = ANY($1)",
    )
    .bind::<Array<Text>, _>(guild_ids)
    .get_results(conn)
    .await?;
    let head = |gid: &str| heads.iter().find(|h| h.id == gid);

    let mut out = Vec::new();
    for s in live {
        let Some(h) = head(&s.guild_id) else { continue };
        out.push(SeatView {
            can_manage: seat_can_manage(&s.rank, false, h.has_gm),
            guild_id: s.guild_id,
            guild_name: h.name.clone(),
            character_id: s.character_id,
            alt_key: s.alt_key,
            name: s.name,
            rank: s.rank,
            parked: false,
        });
    }
    for s in parked {
        // A guild deleted while the alt was away: the restore skips it, so do we.
        let Some(h) = head(&s.guild_id) else { continue };
        out.push(SeatView {
            can_manage: seat_can_manage(&s.rank, true, h.has_gm),
            rank: effective_rank(&s.rank, true, h.has_gm),
            guild_id: s.guild_id,
            guild_name: h.name.clone(),
            character_id: s.character_id,
            alt_key: s.alt_key,
            name: s.name,
            parked: true,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// GET /guilds/{guild_id}/roster
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug, Default, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SeasonStats {
    pub matches: i32,
    pub wins: i32,
    pub trophies: i64,
    /// Standing inside the guild on this season's score; `None` = unranked.
    pub rank_in_guild: Option<usize>,
    pub weight_pct: i64,
    /// Hundredths of a point (`season_store::guild_weighted_shares`).
    pub contribution_centi: i64,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemberSeason {
    pub season_number: i32,
    pub season_name: String,
    pub rank: i32,
    pub trophies: i64,
    pub matches: i32,
    pub wins: i32,
    /// Was the alt in THIS guild when that season closed?
    pub in_this_guild: bool,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RosterMember {
    pub character_id: Uuid,
    pub alt_key: Option<Uuid>,
    /// PUBLIC arena user id (never `secret_user_id`). The website maps it to
    /// the owning Discord account for admins only.
    pub user_id: Uuid,
    pub name: String,
    pub level: i32,
    pub rank: String,
    pub join_date: i64,
    pub parked: bool,
    /// Newest of: last arena match by this alt, last login of the account
    /// (live alt only), the moment a parked alt was switched out.
    pub last_active: Option<i64>,
    pub last_match: Option<i64>,
    pub season: SeasonStats,
    pub history: Vec<MemberSeason>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GuildSeason {
    pub season_number: i32,
    pub season_name: String,
    pub rank: i32,
    pub trophies: i64,
    pub members: i32,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SeasonHead {
    pub number: i32,
    pub name: String,
    pub starts_at: i64,
    pub ends_at: i64,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RosterResponse {
    pub guild_id: String,
    pub name: String,
    pub tag_id: String,
    pub trophies: i64,
    pub season: Option<SeasonHead>,
    pub members: Vec<RosterMember>,
    pub history: Vec<GuildSeason>,
}

/// One member, before the activity is joined on.
#[derive(Debug, Clone)]
struct Seat {
    character_id: Uuid,
    alt_key: Option<Uuid>,
    user_id: Uuid,
    name: String,
    level: i32,
    rank: String,
    join_date: i64,
    parked: bool,
    parked_at: Option<i64>,
}

/// Season contribution per member: the guild's standings rows, weighted exactly
/// as `recompute_guild_trophies` weights them. Keyed by (character, alt).
fn season_stats_for(
    gid: &str,
    standings: &[season_store::StandingRow],
) -> HashMap<(Uuid, Option<Uuid>), SeasonStats> {
    let rows: Vec<&season_store::StandingRow> = standings
        .iter()
        .filter(|s| s.guild_id.as_deref() == Some(gid))
        .collect();
    let scores: Vec<i64> = rows.iter().map(|r| r.trophies).collect();
    season_store::guild_weighted_shares(&scores)
        .into_iter()
        .map(|share| {
            let r = rows[share.index];
            (
                (r.character_id, r.source_alt_uuid),
                SeasonStats {
                    matches: r.matches,
                    wins: r.wins,
                    trophies: r.trophies,
                    rank_in_guild: Some(share.rank_in_guild),
                    weight_pct: share.weight_pct,
                    contribution_centi: share.contribution_centi,
                },
            )
        })
        .collect()
}

/// `GET /…/api/dev/v1/guilds/{guild_id}/roster`.
#[get("/blades.bgs.services/api/dev/v1/guilds/{guild_id}/roster")]
pub async fn guild_roster(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<String>,
) -> Result<Json<RosterResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let gid = path.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, 40))?;
    match build_roster(&mut conn, &gid).await? {
        Some(r) => Ok(Json(r)),
        None => Err(err(StatusCode::NOT_FOUND, 41)),
    }
}

pub(crate) async fn build_roster(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Option<RosterResponse>, diesel::result::Error> {
    #[derive(QueryableByName)]
    struct Head {
        #[diesel(sql_type = Text)]
        name: String,
        #[diesel(sql_type = Text)]
        tag_id: String,
        #[diesel(sql_type = BigInt)]
        trophies: i64,
    }
    let Some(head) = sql_query("SELECT name, tag_id, trophies FROM guilds WHERE id = $1")
        .bind::<Text, _>(gid)
        .get_result::<Head>(conn)
        .await
        .optional()?
    else {
        return Ok(None);
    };

    let live = live_seats_of_guild(conn, gid).await?;
    let has_gm = live
        .iter()
        .any(|s| s.rank == GuildRank::Grandmaster.as_wire());
    let parked = parked_seats_of_guild(conn, gid).await?;
    let mut seats: Vec<Seat> = live
        .into_iter()
        .map(|s| Seat {
            character_id: s.character_id,
            alt_key: s.alt_key,
            user_id: s.user_id,
            name: s.name,
            level: s.level,
            rank: s.rank,
            join_date: s.join_date,
            parked: false,
            parked_at: None,
        })
        .collect();
    seats.extend(parked.into_iter().map(|s| Seat {
        rank: effective_rank(&s.rank, true, has_gm),
        character_id: s.character_id,
        alt_key: s.alt_key,
        user_id: s.user_id,
        name: s.name,
        level: s.level,
        join_date: s.join_date,
        parked: true,
        parked_at: Some(s.saved_at),
    }));

    // Current season: the exact standings the guild score is built from.
    let season: Option<season_store::SeasonRow> = {
        use crate::schema::arena_seasons;
        use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
        arena_seasons::table
            .filter(arena_seasons::status.eq("active"))
            .select(season_store::SeasonRow::as_select())
            .order(arena_seasons::starts_at.desc())
            .first(conn)
            .await
            .optional()?
    };
    let stats = match &season {
        Some(s) => season_stats_for(gid, &season_store::freeze_standings(conn, s).await?),
        None => HashMap::new(),
    };

    let char_ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = seats.iter().map(|s| s.character_id).collect();
        v.sort();
        v.dedup();
        v
    };
    let user_ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = seats
            .iter()
            .filter(|s| !s.parked)
            .map(|s| s.user_id)
            .collect();
        v.sort();
        v.dedup();
        v
    };

    #[derive(QueryableByName)]
    struct LastMatch {
        #[diesel(sql_type = SqlUuid)]
        character_id: Uuid,
        #[diesel(sql_type = Nullable<SqlUuid>)]
        alt: Option<Uuid>,
        #[diesel(sql_type = BigInt)]
        at: i64,
    }
    let last_matches: Vec<LastMatch> = sql_query(
        "SELECT character_id, source_alt_uuid AS alt,
                EXTRACT(EPOCH FROM MAX(recorded_at))::bigint AS at
           FROM arena_match_results
          WHERE character_id = ANY($1)
          GROUP BY character_id, source_alt_uuid",
    )
    .bind::<Array<SqlUuid>, _>(char_ids.clone())
    .get_results(conn)
    .await?;

    #[derive(QueryableByName)]
    struct LastLogin {
        #[diesel(sql_type = SqlUuid)]
        user_id: Uuid,
        #[diesel(sql_type = BigInt)]
        at: i64,
    }
    let last_logins: Vec<LastLogin> = sql_query(
        "SELECT user_id, EXTRACT(EPOCH FROM MAX(last_seen))::bigint AS at
           FROM device_bindings WHERE user_id = ANY($1) GROUP BY user_id",
    )
    .bind::<Array<SqlUuid>, _>(user_ids)
    .get_results(conn)
    .await?;

    #[derive(QueryableByName)]
    struct Hist {
        #[diesel(sql_type = SqlUuid)]
        character_id: Uuid,
        #[diesel(sql_type = Nullable<SqlUuid>)]
        alt: Option<Uuid>,
        #[diesel(sql_type = Int4)]
        rank: i32,
        #[diesel(sql_type = BigInt)]
        trophies: i64,
        #[diesel(sql_type = Int4)]
        matches: i32,
        #[diesel(sql_type = Int4)]
        wins: i32,
        #[diesel(sql_type = Nullable<Text>)]
        guild_id: Option<String>,
        #[diesel(sql_type = Int4)]
        number: i32,
        #[diesel(sql_type = Text)]
        name: String,
    }
    let history: Vec<Hist> = sql_query(
        "SELECT s.character_id, s.source_alt_uuid AS alt, s.rank, s.trophies, s.matches,
                s.wins, s.guild_id, a.number, a.name
           FROM arena_season_standings s
           JOIN arena_seasons a ON a.id = s.season_id
          WHERE s.character_id = ANY($1) AND a.status = 'ended'
          ORDER BY a.number DESC",
    )
    .bind::<Array<SqlUuid>, _>(char_ids)
    .get_results(conn)
    .await?;

    let guild_history: Vec<GuildSeasonRow> = sql_query(
        "SELECT a.number, a.name, gs.rank, gs.trophies, gs.members
           FROM arena_season_guild_standings gs
           JOIN arena_seasons a ON a.id = gs.season_id
          WHERE gs.guild_id = $1 AND a.status = 'ended'
          ORDER BY a.number DESC LIMIT 12",
    )
    .bind::<Text, _>(gid)
    .get_results(conn)
    .await?;

    // A row with no alt was recorded before alts were tracked: read as the
    // alt that is live now (schema comment on `arena_match_results`).
    let belongs = |seat: &Seat, character_id: Uuid, alt: Option<Uuid>| {
        seat.character_id == character_id
            && (alt == seat.alt_key || (alt.is_none() && !seat.parked))
    };

    let members = seats
        .into_iter()
        .map(|seat| {
            let last_match = last_matches
                .iter()
                .filter(|m| belongs(&seat, m.character_id, m.alt))
                .map(|m| m.at)
                .max();
            let last_login = if seat.parked {
                None
            } else {
                last_logins
                    .iter()
                    .find(|l| l.user_id == seat.user_id)
                    .map(|l| l.at)
            };
            let last_active = [last_match, last_login, seat.parked_at]
                .into_iter()
                .flatten()
                .max();
            let season = stats
                .iter()
                .find(|((c, a), _)| *c == seat.character_id && *a == seat.alt_key)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            let hist = history
                .iter()
                .filter(|h| belongs(&seat, h.character_id, h.alt))
                .take(HISTORY_SEASONS)
                .map(|h| MemberSeason {
                    season_number: h.number,
                    season_name: h.name.clone(),
                    rank: h.rank,
                    trophies: h.trophies,
                    matches: h.matches,
                    wins: h.wins,
                    in_this_guild: h.guild_id.as_deref() == Some(gid),
                })
                .collect();
            RosterMember {
                character_id: seat.character_id,
                alt_key: seat.alt_key,
                user_id: seat.user_id,
                name: seat.name,
                level: seat.level,
                rank: seat.rank,
                join_date: seat.join_date,
                parked: seat.parked,
                last_active,
                last_match,
                season,
                history: hist,
            }
        })
        .collect::<Vec<_>>();
    let mut members = members;
    sort_roster(&mut members);

    Ok(Some(RosterResponse {
        guild_id: gid.to_string(),
        name: head.name,
        tag_id: head.tag_id,
        trophies: head.trophies,
        season: season.map(|s| SeasonHead {
            number: s.number,
            name: s.name,
            starts_at: s.starts_at,
            ends_at: s.ends_at,
        }),
        members,
        history: guild_history
            .into_iter()
            .map(|g| GuildSeason {
                season_number: g.number,
                season_name: g.name,
                rank: g.rank,
                trophies: g.trophies,
                members: g.members,
            })
            .collect(),
    }))
}

#[derive(QueryableByName)]
struct GuildSeasonRow {
    #[diesel(sql_type = Int4)]
    number: i32,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Int4)]
    rank: i32,
    #[diesel(sql_type = BigInt)]
    trophies: i64,
    #[diesel(sql_type = Int4)]
    members: i32,
}

/// Best contributors first; then the most recently active; parked seats last.
fn sort_roster(members: &mut [RosterMember]) {
    members.sort_by(|a, b| {
        a.parked
            .cmp(&b.parked)
            .then(
                b.season
                    .contribution_centi
                    .cmp(&a.season.contribution_centi),
            )
            .then(b.last_active.cmp(&a.last_active))
            .then(a.join_date.cmp(&b.join_date))
    });
}

// ---------------------------------------------------------------------------
// POST /guilds/{guild_id}/kick
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct KickRequest {
    pub character_id: Uuid,
    /// Which alt's seat — required for a parked seat, ignored for a live one
    /// (the live row has exactly one).
    #[serde(default)]
    pub alt_key: Option<Uuid>,
    #[serde(default)]
    pub parked: bool,
    /// The PUBLIC arena user id of a Grand Master kicking from their profile.
    /// Absent = an operator (the web gates that to admins).
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    /// Write. Absent or false = report only, like the rest of the console.
    #[serde(default)]
    pub apply: bool,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct KickResponse {
    pub applied: bool,
    pub guild_id: String,
    pub character_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub rank: String,
    pub parked: bool,
    /// Who inherited the guild, when the operator removed its Grand Master.
    pub successor: Option<Uuid>,
    /// `guilds.trophies` after the recompute (applied only).
    pub guild_trophies: Option<i64>,
}

fn refusal_error(r: KickRefusal) -> BladeApiError {
    match r {
        KickRefusal::ActorNotInGuild => err(StatusCode::FORBIDDEN, 44),
        KickRefusal::Yourself => err(StatusCode::BAD_REQUEST, 45),
        KickRefusal::NotPermitted => err(StatusCode::FORBIDDEN, 46),
        KickRefusal::LastMember => err(StatusCode::CONFLICT, 47),
    }
}

/// `POST /…/api/dev/v1/guilds/{guild_id}/kick`.
#[post("/blades.bgs.services/api/dev/v1/guilds/{guild_id}/kick")]
pub async fn kick_guild_member(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<String>,
    body: web::Json<KickRequest>,
) -> Result<Json<KickResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let gid = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, 40))?;
    let resp = kick(&mut conn, &gid, &body).await?;
    info!(
        "guild manage: kick {} ({}, {}) from {} by {} — {}",
        resp.character_id,
        resp.name,
        if resp.parked {
            "parked seat"
        } else {
            "live seat"
        },
        gid,
        body.actor_user_id
            .map_or("operator".to_string(), |a| format!("grandmaster {a}")),
        if resp.applied { "APPLIED" } else { "dry run" },
    );
    Ok(Json(resp))
}

pub(crate) async fn kick(
    conn: &mut AsyncPgConnection,
    gid: &str,
    body: &KickRequest,
) -> Result<KickResponse, BladeApiError> {
    let live = live_seats_of_guild(conn, gid).await?;
    #[derive(QueryableByName)]
    struct Found {
        #[diesel(sql_type = Text)]
        id: String,
    }
    let found: Option<Found> = sql_query("SELECT id FROM guilds WHERE id = $1")
        .bind::<Text, _>(gid)
        .get_result(conn)
        .await
        .optional()?;
    if found.is_none_or(|f| f.id != gid) {
        return Err(err(StatusCode::NOT_FOUND, 41));
    }
    let has_gm = live
        .iter()
        .any(|s| s.rank == GuildRank::Grandmaster.as_wire());

    // The target seat.
    let (target, live_target, parked_alt) = if body.parked {
        let alt = body
            .alt_key
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, 42))?;
        let parked = parked_seats_of_guild(conn, gid).await?;
        let seat = parked
            .into_iter()
            .find(|s| s.character_id == body.character_id && s.alt_key == Some(alt))
            .ok_or_else(|| err(StatusCode::NOT_FOUND, 43))?;
        let rank = effective_rank(&seat.rank, true, has_gm);
        ((seat.user_id, rank, seat.name), None, Some(alt))
    } else {
        let seat = live
            .iter()
            .find(|s| s.character_id == body.character_id)
            .cloned()
            .ok_or_else(|| err(StatusCode::NOT_FOUND, 43))?;
        (
            (seat.user_id, seat.rank.clone(), seat.name.clone()),
            Some(seat),
            None,
        )
    };
    let (target_user, target_rank_wire, target_name) = target;
    let target_rank =
        GuildRank::from_wire(&target_rank_wire).ok_or_else(|| err(StatusCode::CONFLICT, 48))?;

    // The actor.
    let actor = match body.actor_user_id {
        None => KickActor::Operator,
        Some(actor_user) => match live.iter().find(|s| s.user_id == actor_user) {
            Some(s) => match GuildRank::from_wire(&s.rank) {
                Some(rank) => KickActor::Member {
                    user_id: actor_user,
                    rank,
                },
                None => return Err(err(StatusCode::CONFLICT, 48)),
            },
            None => KickActor::Outsider,
        },
    };
    plan_kick(
        actor,
        KickTarget {
            user_id: target_user,
            rank: target_rank,
            parked: body.parked,
        },
        live.len(),
    )
    .map_err(refusal_error)?;

    let mut resp = KickResponse {
        applied: false,
        guild_id: gid.to_string(),
        character_id: body.character_id,
        user_id: target_user,
        name: target_name,
        rank: target_rank_wire,
        parked: body.parked,
        successor: None,
        guild_trophies: None,
    };
    if !body.apply {
        return Ok(resp);
    }

    let remover = match (actor, body.actor_user_id) {
        (KickActor::Member { user_id, .. }, Some(_)) => {
            let character_id = live
                .iter()
                .find(|s| s.user_id == user_id)
                .map(|s| s.character_id)
                .unwrap_or(user_id);
            SeatRemover::GrandMaster {
                user_id,
                character_id,
            }
        }
        _ => SeatRemover::Operator,
    };
    let ts = now_unix();
    let gid_owned = gid.to_string();
    let character_id = body.character_id;
    let successor = conn
        .transaction::<_, BladeApiError, _>(move |conn| {
            async move {
                match (live_target, parked_alt) {
                    (Some(seat), _) => {
                        remove_seat_for_website(
                            conn,
                            &gid_owned,
                            seat.stored_user_id,
                            seat.user_id,
                            seat.character_id,
                            target_rank,
                            remover,
                            ts,
                        )
                        .await
                    }
                    (None, Some(alt)) => {
                        remove_parked_seat(conn, &gid_owned, character_id, alt, ts).await?;
                        Ok(None)
                    }
                    (None, None) => Err(err(StatusCode::NOT_FOUND, 43)),
                }
            }
            .scope_boxed()
        })
        .await?;
    resp.applied = true;
    resp.successor = successor;

    // The guild score loses this member now, not after the next match.
    resp.guild_trophies = recompute_and_read(conn, gid).await;
    Ok(resp)
}

/// Strip a parked seat from every version of that alt that carries it, and
/// arm the re-join cooldown the alt brings back with it (`parkedGuildRemovals`,
/// only on versions that already park removals — a key added to an older
/// version would change what its restore does). No board entry: the switch
/// already posted the alt's LEAVE (#439).
pub(crate) async fn remove_parked_seat(
    conn: &mut AsyncPgConnection,
    gid: &str,
    character_id: Uuid,
    alt: Uuid,
    ts: i64,
) -> Result<usize, diesel::result::Error> {
    sql_query(
        r#"
UPDATE character_versions v
   SET server_state = jsonb_set(v.server_state, '{parkedGuildMembers}',
         COALESCE((SELECT jsonb_agg(e) FROM jsonb_array_elements(v.server_state -> 'parkedGuildMembers') e
                    WHERE e ->> 'guild_id' IS DISTINCT FROM $1), '[]'::jsonb))
       || CASE WHEN jsonb_typeof(v.server_state -> 'parkedGuildRemovals') = 'array'
               THEN jsonb_build_object('parkedGuildRemovals',
                      COALESCE((SELECT jsonb_agg(r) FROM jsonb_array_elements(v.server_state -> 'parkedGuildRemovals') r
                                 WHERE r ->> 'guild_id' IS DISTINCT FROM $1), '[]'::jsonb)
                      || jsonb_build_array(jsonb_build_object('guild_id', $1::text, 'removed_at', $4::bigint, 'banned', false)))
               ELSE '{}'::jsonb END
 WHERE v.character_id = $2 AND v.source_alt_uuid = $3
   AND v.server_state -> 'parkedGuildMembers'
       @> jsonb_build_array(jsonb_build_object('guild_id', $1::text))"#,
    )
    .bind::<Text, _>(gid)
    .bind::<SqlUuid, _>(character_id)
    .bind::<SqlUuid, _>(alt)
    .bind::<BigInt, _>(ts)
    .execute(conn)
    .await
}

/// Recompute the guild ladder for the active season and read this guild's
/// total. Logged and swallowed on failure: the kick itself is done.
async fn recompute_and_read(conn: &mut AsyncPgConnection, gid: &str) -> Option<i64> {
    let season: Option<season_store::SeasonRow> = {
        use crate::schema::arena_seasons;
        use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
        match arena_seasons::table
            .filter(arena_seasons::status.eq("active"))
            .select(season_store::SeasonRow::as_select())
            .order(arena_seasons::starts_at.desc())
            .first(conn)
            .await
            .optional()
        {
            Ok(s) => s,
            Err(e) => {
                warn!("guild manage: season read failed after kick: {e}");
                None
            }
        }
    };
    if let Some(season) = season
        && let Err(e) = crate::guild::recompute_guild_trophies(conn, &season).await
    {
        warn!("guild manage: trophy recompute failed after kick in {gid}: {e}");
    }
    #[derive(QueryableByName)]
    struct T {
        #[diesel(sql_type = BigInt)]
        trophies: i64,
    }
    sql_query("SELECT trophies FROM guilds WHERE id = $1")
        .bind::<Text, _>(gid)
        .get_result::<T>(conn)
        .await
        .optional()
        .ok()
        .flatten()
        .map(|t| t.trophies)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GM: Uuid = Uuid::from_u128(1);
    const OTHER: Uuid = Uuid::from_u128(2);

    fn target(rank: GuildRank, parked: bool) -> KickTarget {
        KickTarget {
            user_id: OTHER,
            rank,
            parked,
        }
    }

    #[test]
    fn a_grandmaster_may_kick_anyone_below() {
        for rank in [GuildRank::Master, GuildRank::Elder, GuildRank::Member] {
            let actor = KickActor::Member {
                user_id: GM,
                rank: GuildRank::Grandmaster,
            };
            assert_eq!(plan_kick(actor, target(rank, false), 5), Ok(()));
            assert_eq!(plan_kick(actor, target(rank, true), 5), Ok(()));
        }
    }

    #[test]
    fn a_grandmaster_cannot_kick_themselves_or_another_grandmaster() {
        let actor = KickActor::Member {
            user_id: GM,
            rank: GuildRank::Grandmaster,
        };
        let me = KickTarget {
            user_id: GM,
            rank: GuildRank::Member,
            parked: true,
        };
        assert_eq!(plan_kick(actor, me, 5), Err(KickRefusal::Yourself));
        assert_eq!(
            plan_kick(actor, target(GuildRank::Grandmaster, false), 5),
            Err(KickRefusal::NotPermitted)
        );
    }

    #[test]
    fn ranks_below_grandmaster_cannot_kick_at_all() {
        for rank in [GuildRank::Master, GuildRank::Elder, GuildRank::Member] {
            let actor = KickActor::Member { user_id: GM, rank };
            assert_eq!(
                plan_kick(actor, target(GuildRank::Member, false), 5),
                Err(KickRefusal::NotPermitted),
                "{rank:?}"
            );
        }
    }

    #[test]
    fn a_player_outside_the_guild_cannot_kick() {
        assert_eq!(
            plan_kick(KickActor::Outsider, target(GuildRank::Member, false), 5),
            Err(KickRefusal::ActorNotInGuild)
        );
    }

    #[test]
    fn an_operator_may_remove_even_the_grandmaster_but_not_the_last_member() {
        assert_eq!(
            plan_kick(
                KickActor::Operator,
                target(GuildRank::Grandmaster, false),
                3
            ),
            Ok(())
        );
        assert_eq!(
            plan_kick(KickActor::Operator, target(GuildRank::Member, false), 1),
            Err(KickRefusal::LastMember)
        );
        // A parked seat is not a live member: removing it never empties the guild.
        assert_eq!(
            plan_kick(KickActor::Operator, target(GuildRank::Member, true), 1),
            Ok(())
        );
    }

    #[test]
    fn only_a_grandmaster_seat_manages_and_a_parked_one_only_while_leaderless() {
        assert!(seat_can_manage("GRANDMASTER", false, true));
        assert!(!seat_can_manage("MASTER", false, true));
        assert!(!seat_can_manage("ELDER", false, false));
        assert!(
            !seat_can_manage("LEADER", false, false),
            "unknown ranks never manage"
        );
        assert!(seat_can_manage("GRANDMASTER", true, false));
        assert!(!seat_can_manage("GRANDMASTER", true, true));
    }

    #[test]
    fn season_stats_weight_only_this_guilds_rows() {
        let row = |c: u128, g: &str, t: i64| season_store::StandingRow {
            season_id: Uuid::nil(),
            character_id: Uuid::from_u128(c),
            source_alt_uuid: None,
            rank: 1,
            trophies: t,
            matches: 3,
            wins: 2,
            guild_id: Some(g.into()),
            high_water: t,
            arena: 0,
            arena_level: 0,
        };
        let standings = vec![row(1, "g", 100), row(2, "other", 900), row(3, "g", 300)];
        let s = season_stats_for("g", &standings);
        assert_eq!(s.len(), 2);
        let best = &s[&(Uuid::from_u128(3), None)];
        assert_eq!(best.rank_in_guild, Some(1));
        assert_eq!(best.contribution_centi, 300 * 30);
        assert!(!s.contains_key(&(Uuid::from_u128(2), None)));
    }

    /// The database half: kicking live and parked seats, the score recompute,
    /// and the seat lookups, against the shipped migrations.
    mod db {
        use super::*;
        use diesel_async::SimpleAsyncConnection;

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
            // Every migration, in order: the roster reads tables from most of them.
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../migrations");
            let mut dirs: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("up.sql").exists())
                .collect();
            dirs.sort();
            for d in dirs {
                let sql = std::fs::read_to_string(d.join("up.sql")).unwrap();
                conn.batch_execute(&sql)
                    .await
                    .unwrap_or_else(|e| panic!("{}: {e}", d.display()));
            }
            Some(conn)
        }

        macro_rules! db {
            () => {
                match fixture().await {
                    Some(c) => c,
                    None => {
                        eprintln!("SKIP: TEST_DATABASE_URL unset — guild manage SQL NOT verified");
                        return;
                    }
                }
            };
        }

        const G: &str = "aaaaaaaaaaaaaaaaaaaaaaaa";
        const SEASON: Uuid = Uuid::from_u128(0x5ea5);

        /// Three live members (GM, A, B), all scoring this season, and user P
        /// whose alt PA is parked in the guild (scoring too) while alt PB is live.
        struct World {
            gm_user: Uuid,
            gm_char: Uuid,
            a_user: Uuid,
            a_char: Uuid,
            p_user: Uuid,
            p_char: Uuid,
            pa_alt: Uuid,
        }

        async fn seed(c: &mut AsyncPgConnection) -> World {
            let w = World {
                gm_user: Uuid::new_v4(),
                gm_char: Uuid::new_v4(),
                a_user: Uuid::new_v4(),
                a_char: Uuid::new_v4(),
                p_user: Uuid::new_v4(),
                p_char: Uuid::new_v4(),
                pa_alt: Uuid::new_v4(),
            };
            let (b_user, b_char) = (Uuid::new_v4(), Uuid::new_v4());
            let (gm_alt, a_alt, b_alt, pb_alt) = (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            let mut sql = format!(
                "INSERT INTO users (id, secret_id, data) VALUES
                   ('{gu}', gen_random_uuid(), '{{}}'), ('{au}', gen_random_uuid(), '{{}}'),
                   ('{bu}', gen_random_uuid(), '{{}}'), ('{pu}', gen_random_uuid(), '{{}}');
                 INSERT INTO arena_seasons (id, number, name, starts_at, ends_at, status)
                   VALUES ('{SEASON}', 4, 'Now', 1000, 4000000000, 'active');
                 INSERT INTO guilds (id, name, tag_id, trophies) VALUES ('{G}', 'New Bladers', '0001', 0);
                 INSERT INTO characters (id, user_id, source_alt_uuid, character, data, inventory, wallet) VALUES
                   ('{gc}', '{gu}', '{gm_alt}', '{{\"name\":\"Master\",\"level\":60}}', '{{}}', '{{}}', '{{}}'),
                   ('{ac}', '{au}', '{a_alt}', '{{\"name\":\"Alpha\",\"level\":40}}', '{{}}', '{{}}', '{{}}'),
                   ('{bc}', '{bu}', '{b_alt}', '{{\"name\":\"Beta\",\"level\":30}}', '{{}}', '{{}}', '{{}}'),
                   ('{pc}', '{pu}', '{pb_alt}', '{{\"name\":\"PeeBee\",\"level\":1}}', '{{}}', '{{}}', '{{}}');
                 INSERT INTO guild_members (guild_id, user_id, character_id, rank, join_date) VALUES
                   ('{G}', '{gu}', '{gc}', 'GRANDMASTER', 10),
                   ('{G}', '{au}', '{ac}', 'MEMBER', 20),
                   ('{G}', '{bu}', '{bc}', 'MEMBER', 30);
                 INSERT INTO character_versions (id, character_id, user_id, source_alt_uuid, name, level,
                     character, data, inventory, wallet, server_state, reason, saved_at) VALUES
                   (gen_random_uuid(), '{pc}', '{pu}', '{pa}', 'PeeAy', 50, '{{}}', '{{}}', '{{}}', '{{}}',
                    '{{\"parkedGuildMembers\":[{{\"guild_id\":\"{G}\",\"rank\":\"MEMBER\",\"join_date\":15}}],\"parkedGuildRemovals\":[]}}',
                    'switch', 100),
                   (gen_random_uuid(), '{pc}', '{pu}', '{pa}', 'PeeAy', 50, '{{}}', '{{}}', '{{}}', '{{}}',
                    '{{\"parkedGuildMembers\":[{{\"guild_id\":\"{G}\",\"rank\":\"MEMBER\",\"join_date\":15}}],\"parkedGuildRemovals\":[]}}',
                    'switch', 200);
                 INSERT INTO device_bindings (device_id, user_id, last_seen) VALUES
                   ('dev-a', '{au}', to_timestamp(5000));",
                gu = w.gm_user,
                au = w.a_user,
                bu = b_user,
                pu = w.p_user,
                gc = w.gm_char,
                ac = w.a_char,
                bc = b_char,
                pc = w.p_char,
                pa = w.pa_alt,
            );
            // Wins worth 30/20/10/40 cups (GM, A, B, parked PA).
            for (cid, alt, d, t) in [
                (w.gm_char, gm_alt, 30, 2001),
                (w.a_char, a_alt, 20, 2002),
                (b_char, b_alt, 10, 2003),
                (w.p_char, w.pa_alt, 40, 2004),
            ] {
                sql.push_str(&format!(
                    "INSERT INTO arena_match_results (id, character_id, win, rounds_won, rounds_lost,
                       trophy_delta, recorded_at, is_h2h, source_alt_uuid)
                     VALUES (gen_random_uuid(), '{cid}', true, 2, 0, {d}, to_timestamp({t}), false, '{alt}');"
                ));
            }
            c.batch_execute(&sql).await.unwrap();
            w
        }

        async fn trophies(c: &mut AsyncPgConnection) -> i64 {
            build_roster(c, G).await.unwrap().unwrap().trophies
        }

        fn req(character_id: Uuid) -> KickRequest {
            KickRequest {
                character_id,
                alt_key: None,
                parked: false,
                actor_user_id: None,
                apply: true,
            }
        }

        #[tokio::test]
        async fn roster_lists_live_and_parked_seats_with_activity() {
            let mut c = db!();
            let w = seed(&mut c).await;
            let r = build_roster(&mut c, G).await.unwrap().unwrap();
            assert_eq!(r.members.len(), 4);
            let parked: Vec<_> = r.members.iter().filter(|m| m.parked).collect();
            assert_eq!(parked.len(), 1);
            assert_eq!(parked[0].alt_key, Some(w.pa_alt));
            assert_eq!(parked[0].name, "PeeAy");
            assert_eq!(parked[0].season.trophies, 40);
            assert_eq!(parked[0].last_active, Some(2004));
            let a = r
                .members
                .iter()
                .find(|m| m.character_id == w.a_char)
                .unwrap();
            assert_eq!(a.level, 40);
            assert_eq!(a.last_match, Some(2002));
            assert_eq!(
                a.last_active,
                Some(5000),
                "the login is newer than the match"
            );
            assert_eq!(a.season.matches, 1);
            // The live roster is sorted best contributor first.
            assert_eq!(r.members[0].character_id, w.gm_char);
            // Seat lookup: the GM manages, A does not, P's parked seat shows.
            let gm = seats_of_user(&mut c, w.gm_user).await.unwrap();
            assert!(gm.len() == 1 && gm[0].can_manage);
            let a = seats_of_user(&mut c, w.a_user).await.unwrap();
            assert!(a.len() == 1 && !a[0].can_manage);
            let p = seats_of_user(&mut c, w.p_user).await.unwrap();
            assert!(p.len() == 1 && p[0].parked && !p[0].can_manage);
        }

        #[tokio::test]
        async fn admin_kick_removes_the_live_seat_recomputes_and_arms_the_cooldown() {
            let mut c = db!();
            let w = seed(&mut c).await;
            crate::guild::recompute_guild_trophies_for_tests(&mut c).await;
            let before = trophies(&mut c).await;
            // 40/30/20/10 all in the top bracket (30%): 30.0 points.
            assert_eq!(before, 30);

            // Dry run writes nothing.
            let mut dry = req(w.a_char);
            dry.apply = false;
            assert!(!kick(&mut c, G, &dry).await.unwrap().applied);
            assert_eq!(
                build_roster(&mut c, G)
                    .await
                    .unwrap()
                    .unwrap()
                    .members
                    .len(),
                4
            );

            let r = kick(&mut c, G, &req(w.a_char)).await.unwrap();
            assert!(r.applied);
            assert_eq!(r.guild_trophies, Some(24), "40/30/10 at 30% = 24");
            let roster = build_roster(&mut c, G).await.unwrap().unwrap();
            assert!(roster.members.iter().all(|m| m.character_id != w.a_char));

            #[derive(QueryableByName)]
            struct N {
                #[diesel(sql_type = BigInt)]
                n: i64,
            }
            let removal: N = sql_query(
                "SELECT COUNT(*) AS n FROM guild_removals WHERE guild_id = $1 AND user_id = $2 AND NOT banned",
            )
            .bind::<Text, _>(G)
            .bind::<SqlUuid, _>(w.a_user)
            .get_result(&mut c)
            .await
            .unwrap();
            assert_eq!(removal.n, 1, "a kick arms the re-join cooldown");
            let board: N = sql_query(
                "SELECT COUNT(*) AS n FROM guild_messages WHERE guild_id = $1 AND message_type = 'LEAVE' AND user_id = $2",
            )
            .bind::<Text, _>(G)
            .bind::<SqlUuid, _>(w.a_user)
            .get_result(&mut c)
            .await
            .unwrap();
            assert_eq!(board.n, 1, "an operator kick posts the member's LEAVE");
        }

        #[tokio::test]
        async fn grandmaster_kick_posts_retails_kick_entry() {
            let mut c = db!();
            let w = seed(&mut c).await;
            let mut r = req(w.a_char);
            r.actor_user_id = Some(w.gm_user);
            assert!(kick(&mut c, G, &r).await.unwrap().applied);
            #[derive(QueryableByName)]
            struct M {
                #[diesel(sql_type = SqlUuid)]
                user_id: Uuid,
                #[diesel(sql_type = Text)]
                kicked: String,
            }
            let m: M = sql_query(
                "SELECT user_id, type_specific_data ->> 'kickedUserId' AS kicked
                   FROM guild_messages WHERE guild_id = $1 AND message_type = 'KICK'",
            )
            .bind::<Text, _>(G)
            .get_result(&mut c)
            .await
            .unwrap();
            assert_eq!(m.user_id, w.gm_user);
            assert_eq!(m.kicked, w.a_user.to_string());
        }

        #[tokio::test]
        async fn a_member_cannot_kick_and_nobody_kicks_the_grandmaster_from_a_profile() {
            let mut c = db!();
            let w = seed(&mut c).await;
            let mut r = req(w.gm_char);
            r.actor_user_id = Some(w.a_user);
            assert!(kick(&mut c, G, &r).await.is_err(), "member kicking the GM");
            let mut r = req(w.gm_char);
            r.actor_user_id = Some(w.gm_user);
            assert!(kick(&mut c, G, &r).await.is_err(), "GM kicking themselves");
            let mut r = req(w.a_char);
            r.actor_user_id = Some(w.p_user);
            assert!(
                kick(&mut c, G, &r).await.is_err(),
                "a parked seat holds no live power"
            );
            assert_eq!(
                build_roster(&mut c, G)
                    .await
                    .unwrap()
                    .unwrap()
                    .members
                    .len(),
                4
            );
        }

        #[tokio::test]
        async fn kicking_a_parked_seat_strips_every_version_and_lowers_the_score() {
            let mut c = db!();
            let w = seed(&mut c).await;
            crate::guild::recompute_guild_trophies_for_tests(&mut c).await;
            let mut r = req(w.p_char);
            r.parked = true;
            r.alt_key = Some(w.pa_alt);
            let resp = kick(&mut c, G, &r).await.unwrap();
            assert!(resp.applied && resp.parked);
            assert_eq!(resp.guild_trophies, Some(18), "30/20/10 at 30% = 18");
            #[derive(QueryableByName)]
            struct S {
                #[diesel(sql_type = diesel::sql_types::Jsonb)]
                s: serde_json::Value,
            }
            let rows: Vec<S> = sql_query(
                "SELECT server_state AS s FROM character_versions WHERE character_id = $1",
            )
            .bind::<SqlUuid, _>(w.p_char)
            .get_results(&mut c)
            .await
            .unwrap();
            assert_eq!(rows.len(), 2);
            for row in rows {
                assert_eq!(row.s["parkedGuildMembers"], serde_json::json!([]));
                assert_eq!(row.s["parkedGuildRemovals"][0]["guild_id"], G);
            }
            assert!(
                build_roster(&mut c, G)
                    .await
                    .unwrap()
                    .unwrap()
                    .members
                    .iter()
                    .all(|m| !m.parked)
            );
            assert!(seats_of_user(&mut c, w.p_user).await.unwrap().is_empty());
        }
    }
}
