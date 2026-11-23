//! Guild management for the website — the roster, the kick, join requests and
//! co-guild masters.
//!
//! Owner requests: (2026-11-15) "let a guild master manage a guild on their
//! profile … see guild member activity current season and before … kick people
//! from the admin page"; (2026-11-22) "assign a co-guild master who on the web
//! has rights to kick and approve new members".
//!
//! Dev-token routes, each with ONE intended web caller:
//!
//! ```text
//! GET  /api/dev/v1/guild-seats?userId=…               which guild seats a user's alts hold
//! GET  /api/dev/v1/guilds/{guild_id}/roster           members + activity + requests + log
//! POST /api/dev/v1/guilds/{guild_id}/kick             remove one seat (dry run by default)
//! POST /api/dev/v1/guilds/{guild_id}/applications/decide   approve or reject a join request
//! POST /api/dev/v1/guilds/{guild_id}/co-masters       appoint or remove a co-guild master
//! ```
//!
//! # Seats are per ALT, played or not
//!
//! A user's alts take turns on one `characters` row. The played alt's seat is
//! in `guild_members`; every other alt's stays in its guild as a row of
//! `guild_alt_seats` (owner decision 2026-11-23: "If I play another alt,
//! everything in guilds stay the same. I can manage things on web for all alts
//! that are guild masters"). The roster lists both, the "parked" flag meaning
//! "not the alt being played"; the score counts both; the game lists both (an
//! alt not played under its alt key). Any seat of an account that is the Grand
//! Master's — or any seat of a co-guild master's account — manages the guild
//! here, played or not.
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
//!
//! # Co-guild masters are a WEBSITE role, not a rank
//!
//! Retail has no rank that could carry it. `GuildData._guildRanksData`
//! (`data/guild_data.json`, see `guild_policy`) grants MASTER, ELDER and MEMBER
//! nothing, the client draws its kick/approve buttons from that shipped asset,
//! and no capture holds a member of either rank. So a co-guild master keeps the
//! in-game rank they had; the role is a row in `guild_co_masters`, read only
//! here, and it works only on the website:
//!
//! * approve or reject join requests, and kick — anyone but the Grand Master,
//!   another co-guild master, or themselves;
//! * nothing else: no appointing, no editing, no handover. Those stay with the
//!   Grand Master (and, for appointing, a site operator).
//!
//! The role has power only while its account holds a LIVE seat in the guild —
//! the same rule as the Grand Master's, and it keeps every board entry authored
//! by a current member (#439: an entry by somebody the board does not know
//! stalled the guild screen). What a co-guild master writes is exactly what the
//! in-game action writes (`guild::remove_seat_for_website`,
//! `guild::decide_application_for_website`): same rows, same board entries with
//! the same captured keys, authored by the co-guild master.
//!
//! Every write lands in `guild_manage_log` in the transaction that performs it.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use diesel::sql_types::{Array, BigInt, Int4, Nullable, Text, Uuid as SqlUuid};
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
    guild::{
        SeatRemover, decide_application_for_website, remove_alt_seat_for_website,
        remove_seat_for_website,
    },
    guild_policy::{GuildRank, MAX_MEMBERS, can_approve_applications, can_kick},
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
    /// The target's account is a co-guild master of this guild.
    pub co_master: bool,
}

/// Who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickActor {
    /// A site admin. The web route gates the role; the arena sees only the token.
    Operator,
    /// A player, from the website, holding this LIVE seat in the guild.
    Member {
        user_id: Uuid,
        rank: GuildRank,
        /// Their account is a co-guild master of this guild.
        co_master: bool,
    },
    /// A player with no live seat in this guild.
    Outsider,
}

/// In what capacity a player acted — what the log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorRole {
    Grandmaster,
    CoMaster,
    Operator,
}

impl ActorRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorRole::Grandmaster => "GRANDMASTER",
            ActorRole::CoMaster => "CO_MASTER",
            ActorRole::Operator => "OPERATOR",
        }
    }
}

impl KickActor {
    /// The managing capacity of this actor, if it has one.
    pub fn role(self) -> Option<ActorRole> {
        match self {
            KickActor::Operator => Some(ActorRole::Operator),
            KickActor::Member {
                rank, co_master, ..
            } => {
                if rank == GuildRank::Grandmaster {
                    Some(ActorRole::Grandmaster)
                } else if co_master {
                    Some(ActorRole::CoMaster)
                } else {
                    None
                }
            }
            KickActor::Outsider => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KickRefusal {
    /// The actor is not (live) in this guild.
    ActorNotInGuild,
    /// Retail has no self-kick; leaving is its own action.
    Yourself,
    /// Neither the Grand Master's rule nor the co-guild master's allows it.
    NotPermitted,
    /// An operator removing the guild's only live member would delete the
    /// guild (a leave's teardown). Refused: that is not what "kick" means.
    LastMember,
}

/// May `actor` remove `target`? `live_members` counts the guild's seats — the
/// played alts' and the others' (the name predates alts staying in guilds).
pub fn plan_kick(
    actor: KickActor,
    target: KickTarget,
    live_members: usize,
) -> Result<(), KickRefusal> {
    match actor {
        KickActor::Outsider => Err(KickRefusal::ActorNotInGuild),
        KickActor::Member {
            user_id,
            rank,
            co_master,
        } => {
            if user_id == target.user_id {
                return Err(KickRefusal::Yourself);
            }
            // The game's own rule: a Grand Master removes anyone below them,
            // co-guild masters included.
            if can_kick(rank, target.rank) {
                return Ok(());
            }
            // The website's: a co-guild master removes anyone except the Grand
            // Master and the other co-guild masters.
            if co_master && target.rank != GuildRank::Grandmaster && !target.co_master {
                return Ok(());
            }
            Err(KickRefusal::NotPermitted)
        }
        KickActor::Operator => {
            // Every seat counts, played alt or not: removing the last one would
            // delete the guild (a leave's teardown).
            let _ = target.parked;
            if live_members <= 1 {
                return Err(KickRefusal::LastMember);
            }
            Ok(())
        }
    }
}

/// May `actor` approve or reject a join request? The Grand Master (the game's
/// rule) or a co-guild master (the website's). Not an operator: the board
/// entry needs an author who is a member, and nobody asked for it.
pub fn plan_moderation(actor: KickActor) -> Result<ActorRole, KickRefusal> {
    match actor {
        KickActor::Outsider => Err(KickRefusal::ActorNotInGuild),
        KickActor::Operator => Err(KickRefusal::NotPermitted),
        KickActor::Member {
            rank, co_master, ..
        } => {
            if can_approve_applications(rank) {
                Ok(ActorRole::Grandmaster)
            } else if co_master {
                Ok(ActorRole::CoMaster)
            } else {
                Err(KickRefusal::NotPermitted)
            }
        }
    }
}

/// May `actor` appoint or remove a co-guild master? The Grand Master and a
/// site operator — never a co-guild master.
pub fn plan_appointment(actor: KickActor) -> Result<ActorRole, KickRefusal> {
    match actor {
        KickActor::Outsider => Err(KickRefusal::ActorNotInGuild),
        KickActor::Operator => Ok(ActorRole::Operator),
        KickActor::Member { rank, .. } => {
            if rank == GuildRank::Grandmaster {
                Ok(ActorRole::Grandmaster)
            } else {
                Err(KickRefusal::NotPermitted)
            }
        }
    }
}

/// Which seat of a user may manage its guild on the website: a Grand Master's,
/// whether or not that alt is the one being played. A guild has exactly one
/// Grand Master seat now that seats never leave on a switch.
pub fn seat_can_manage(rank: &str) -> bool {
    GuildRank::from_wire(rank) == Some(GuildRank::Grandmaster)
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Seats of alts that are not being played (`guild_alt_seats`), named from the
/// alt's newest version. Filtered by the caller's `WHERE`.
const PARKED_SEATS_SQL: &str = r#"
SELECT s.guild_id,
       s.character_id,
       s.alt_key                           AS alt_key,
       s.member_id,
       s.user_id,
       s.rank,
       s.join_date,
       COALESCE(NULLIF(v.name, ''), v."character" ->> 'name', '') AS name,
       COALESCE(v.level, 0)                AS level,
       s.parked_at                         AS saved_at
  FROM guild_alt_seats s
  LEFT JOIN LATERAL (
        SELECT x.name, x.level, x."character"
          FROM character_versions x
         WHERE x.user_id = s.user_id
           AND (x.source_alt_uuid = s.alt_key OR x.id = s.alt_key)
         ORDER BY x.saved_at DESC, x.id DESC
         LIMIT 1
       ) v ON true
"#;

#[derive(QueryableByName, Debug, Clone)]
pub(crate) struct ParkedSeatRow {
    #[diesel(sql_type = Text)]
    guild_id: String,
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    alt_key: Option<Uuid>,
    /// The id the game lists this seat under (`alt_member_ids`).
    #[diesel(sql_type = SqlUuid)]
    member_id: Uuid,
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
pub(crate) struct LiveSeatRow {
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

pub(crate) async fn live_seats_of_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<LiveSeatRow>, diesel::result::Error> {
    // A developer lock child's live seat is its parent alt's seat, which the
    // parent's version still lists as parked (`crate::dev_alt_lock`): list it once.
    sql_query(format!(
        "{LIVE_SEAT_COLUMNS} WHERE gm.guild_id = $1 \
           AND NOT EXISTS (SELECT 1 FROM dev_alt_lock_accounts dl \
                            WHERE dl.child_character_id = gm.character_id)"
    ))
    .bind::<Text, _>(gid)
    .get_results(conn)
    .await
}

/// Parked seats in `gid`: candidates found by containment, then kept only when
/// the alt's NEWEST version still carries the seat.
pub(crate) async fn parked_seats_of_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<ParkedSeatRow>, diesel::result::Error> {
    sql_query(format!("{PARKED_SEATS_SQL} WHERE s.guild_id = $1"))
        .bind::<Text, _>(gid)
        .get_results(conn)
        .await
}

/// The accounts appointed co-guild master of `gid`. A row is a ROLE, not a
/// power: the callers require a live seat before it counts.
async fn co_masters_of_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<Uuid>, diesel::result::Error> {
    #[derive(QueryableByName)]
    struct U {
        #[diesel(sql_type = SqlUuid)]
        user_id: Uuid,
    }
    Ok(
        sql_query("SELECT user_id FROM guild_co_masters WHERE guild_id = $1")
            .bind::<Text, _>(gid)
            .get_results::<U>(conn)
            .await?
            .into_iter()
            .map(|u| u.user_id)
            .collect(),
    )
}

/// Is this seat a co-guild master's? Never the Grand Master's: that rank
/// already holds everything the role grants.
fn is_co_master(co_masters: &[Uuid], user_id: Uuid, rank: &str) -> bool {
    rank != GuildRank::Grandmaster.as_wire() && co_masters.contains(&user_id)
}

/// The seat a player acts from, and how the board names them: the account for
/// the alt being played, the alt key for any other alt (the id the game lists
/// that member under).
#[derive(Debug, Clone)]
struct ActorSeat {
    name: String,
    author_user: Uuid,
    author_character: Uuid,
}

/// Who `actor_user_id` is to this guild. Absent = an operator. An account acts
/// from its STRONGEST seat here — played alt or not, a Grand Master's before
/// anything else, the played alt's on a tie (owner decision 2026-11-23: "I can
/// manage things on web for all alts that are guild masters"). An account may
/// hold several seats in one guild (one per alt).
fn resolve_actor(
    live: &[LiveSeatRow],
    parked: &[ParkedSeatRow],
    co_masters: &[Uuid],
    actor_user_id: Option<Uuid>,
) -> Result<(KickActor, Option<ActorSeat>), BladeApiError> {
    let Some(actor_user) = actor_user_id else {
        return Ok((KickActor::Operator, None));
    };
    // (rank, played-first, join date) — smallest wins.
    let mut candidates: Vec<((u8, u8, i64), String, ActorSeat)> = Vec::new();
    for s in live.iter().filter(|s| s.user_id == actor_user) {
        let Some(rank) = GuildRank::from_wire(&s.rank) else {
            return Err(err(StatusCode::CONFLICT, 48));
        };
        candidates.push((
            (rank as u8, 0, s.join_date),
            s.rank.clone(),
            ActorSeat {
                name: s.name.clone(),
                author_user: actor_user,
                author_character: s.character_id,
            },
        ));
    }
    for s in parked.iter().filter(|s| s.user_id == actor_user) {
        let Some(rank) = GuildRank::from_wire(&s.rank) else {
            return Err(err(StatusCode::CONFLICT, 48));
        };
        candidates.push((
            (rank as u8, 1, s.join_date),
            s.rank.clone(),
            ActorSeat {
                name: s.name.clone(),
                author_user: s.member_id,
                author_character: s.member_id,
            },
        ));
    }
    candidates.sort_by_key(|c| c.0);
    match candidates.into_iter().next() {
        Some((_, rank_wire, seat)) => {
            let rank =
                GuildRank::from_wire(&rank_wire).ok_or_else(|| err(StatusCode::CONFLICT, 48))?;
            Ok((
                KickActor::Member {
                    user_id: actor_user,
                    rank,
                    co_master: is_co_master(co_masters, actor_user, &rank_wire),
                },
                Some(seat),
            ))
        }
        None => Ok((KickActor::Outsider, None)),
    }
}

/// One line of `guild_manage_log`.
struct LogLine<'a> {
    gid: &'a str,
    action: &'a str,
    role: ActorRole,
    actor_user_id: Option<Uuid>,
    actor_name: &'a str,
    target_user_id: Uuid,
    target_character_id: Option<Uuid>,
    target_name: &'a str,
    ts: i64,
}

/// Record a management write. Called INSIDE the transaction that performs it:
/// an action with no record, or a record of an action that rolled back, is
/// exactly what a log must not hold.
async fn write_log(
    conn: &mut AsyncPgConnection,
    l: LogLine<'_>,
) -> Result<(), diesel::result::Error> {
    sql_query(
        "INSERT INTO guild_manage_log (guild_id, at, action, actor_role, actor_user_id,
             actor_name, target_user_id, target_character_id, target_name)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind::<Text, _>(l.gid)
    .bind::<BigInt, _>(l.ts)
    .bind::<Text, _>(l.action)
    .bind::<Text, _>(l.role.as_str())
    .bind::<Nullable<SqlUuid>, _>(l.actor_user_id)
    .bind::<Text, _>(l.actor_name)
    .bind::<SqlUuid, _>(l.target_user_id)
    .bind::<Nullable<SqlUuid>, _>(l.target_character_id)
    .bind::<Text, _>(l.target_name)
    .execute(conn)
    .await?;
    Ok(())
}

/// An operator's label for the log (the web sends the admin's Discord name).
/// Bounded and stripped of control characters: it is shown back in a page.
fn operator_label(raw: Option<&str>) -> String {
    let cleaned: String = raw
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "site operator".to_string()
    } else {
        cleaned.to_string()
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
    /// This account is a co-guild master of the guild (and not its Grand Master).
    pub co_master: bool,
    /// May this seat kick and decide join requests on the website right now: a
    /// LIVE Grand Master or co-guild master seat.
    pub can_moderate: bool,
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
    let parked: Vec<ParkedSeatRow> = sql_query(format!("{PARKED_SEATS_SQL} WHERE s.user_id = $1"))
        .bind::<SqlUuid, _>(user_id)
        .get_results(conn)
        .await?;

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
    }
    let heads: Vec<GuildHead> = sql_query("SELECT g.id, g.name FROM guilds g WHERE g.id = ANY($1)")
        .bind::<Array<Text>, _>(guild_ids)
        .get_results(conn)
        .await?;
    let head = |gid: &str| heads.iter().find(|h| h.id == gid);
    #[derive(QueryableByName)]
    struct Co {
        #[diesel(sql_type = Text)]
        guild_id: String,
    }
    let co_guilds: Vec<String> =
        sql_query("SELECT guild_id FROM guild_co_masters WHERE user_id = $1")
            .bind::<SqlUuid, _>(user_id)
            .get_results::<Co>(conn)
            .await?
            .into_iter()
            .map(|c| c.guild_id)
            .collect();
    let co = |gid: &str, rank: &str| {
        rank != GuildRank::Grandmaster.as_wire() && co_guilds.iter().any(|g| g == gid)
    };

    let mut out = Vec::new();
    for s in live {
        let Some(h) = head(&s.guild_id) else { continue };
        let co_master = co(&s.guild_id, &s.rank);
        out.push(SeatView {
            co_master,
            can_moderate: co_master || s.rank == GuildRank::Grandmaster.as_wire(),
            can_manage: seat_can_manage(&s.rank),
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
        let Some(h) = head(&s.guild_id) else { continue };
        let co_master = co(&s.guild_id, &s.rank);
        out.push(SeatView {
            // A seat whose alt is not played holds its full rank and role.
            co_master,
            can_moderate: co_master || s.rank == GuildRank::Grandmaster.as_wire(),
            can_manage: seat_can_manage(&s.rank),
            rank: s.rank,
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
    /// A co-guild master (website role; the in-game rank is unchanged).
    pub co_master: bool,
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
    /// `OPEN` / `APPLY_ONLY` / `CLOSED` — only an apply-only guild gets requests.
    pub guild_type: String,
    pub max_members: i64,
    /// Pending join requests, oldest first.
    pub applications: Vec<ApplicationView>,
    /// The newest website management actions (`guild_manage_log`). The web
    /// decides who is shown them.
    pub log: Vec<LogEntry>,
}

/// A pending join request, with what a manager needs to decide it.
#[derive(Serialize, Debug, QueryableByName, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationView {
    #[diesel(sql_type = SqlUuid)]
    pub character_id: Uuid,
    /// PUBLIC arena user id; the website keeps it server-side.
    #[diesel(sql_type = SqlUuid)]
    pub user_id: Uuid,
    #[diesel(sql_type = Text)]
    pub name: String,
    #[diesel(sql_type = Int4)]
    pub level: i32,
    #[diesel(sql_type = BigInt)]
    pub creation_time: i64,
}

#[derive(Serialize, Debug, QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    #[diesel(sql_type = BigInt)]
    pub at: i64,
    #[diesel(sql_type = Text)]
    pub action: String,
    #[diesel(sql_type = Text)]
    pub actor_role: String,
    #[diesel(sql_type = Text)]
    pub actor_name: String,
    #[diesel(sql_type = Text)]
    pub target_name: String,
}

/// How many log lines the roster carries.
const LOG_LINES: i64 = 50;

async fn applications_of_guild(
    conn: &mut AsyncPgConnection,
    gid: &str,
) -> Result<Vec<ApplicationView>, diesel::result::Error> {
    sql_query(
        r#"SELECT a.character_id, a.user_id,
                  COALESCE(c."character" ->> 'name', '') AS name,
                  COALESCE((c."character" ->> 'level')::int, 0) AS level,
                  a.creation_time
             FROM guild_applications a
             LEFT JOIN characters c ON c.id = a.character_id
            WHERE a.guild_id = $1
            ORDER BY a.creation_time ASC, a.user_id ASC"#,
    )
    .bind::<Text, _>(gid)
    .get_results(conn)
    .await
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
        #[diesel(sql_type = Text)]
        guild_type: String,
    }
    let Some(head) =
        sql_query("SELECT name, tag_id, trophies, guild_type FROM guilds WHERE id = $1")
            .bind::<Text, _>(gid)
            .get_result::<Head>(conn)
            .await
            .optional()?
    else {
        return Ok(None);
    };

    let live = live_seats_of_guild(conn, gid).await?;
    let parked = parked_seats_of_guild(conn, gid).await?;
    let co_masters = co_masters_of_guild(conn, gid).await?;
    let applications = applications_of_guild(conn, gid).await?;
    let log: Vec<LogEntry> = sql_query(
        "SELECT at, action, actor_role, actor_name, target_name
           FROM guild_manage_log WHERE guild_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind::<Text, _>(gid)
    .bind::<BigInt, _>(LOG_LINES)
    .get_results(conn)
    .await?;
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
        rank: s.rank,
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
                co_master: is_co_master(&co_masters, seat.user_id, &seat.rank),
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
        guild_type: head.guild_type,
        max_members: MAX_MEMBERS,
        applications,
        log,
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
    /// The PUBLIC arena user id of a Grand Master or co-guild master kicking
    /// from the website. Absent = an operator (the web gates that to admins).
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    /// Who the operator is, for the log (the web sends the admin's name).
    #[serde(default)]
    pub operator: Option<String>,
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
            .map_or("operator".to_string(), |a| format!("member {a}")),
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
    let parked = parked_seats_of_guild(conn, gid).await?;

    // The target seat.
    let (target, live_target, parked_alt) = if body.parked {
        let alt = body
            .alt_key
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, 42))?;
        // Alt key AND character: an alt label is shared by an original and its
        // numbered copies, so the label alone may match two accounts' seats.
        let seat = parked
            .iter()
            .find(|s| s.alt_key == Some(alt) && s.character_id == body.character_id)
            .cloned()
            .ok_or_else(|| err(StatusCode::NOT_FOUND, 43))?;
        ((seat.user_id, seat.rank, seat.name), None, Some(alt))
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

    let co_masters = co_masters_of_guild(conn, gid).await?;
    let (actor, actor_seat) = resolve_actor(&live, &parked, &co_masters, body.actor_user_id)?;
    plan_kick(
        actor,
        KickTarget {
            user_id: target_user,
            rank: target_rank,
            parked: body.parked,
            co_master: is_co_master(&co_masters, target_user, &target_rank_wire),
        },
        live.len() + parked.len(),
    )
    .map_err(refusal_error)?;
    // `plan_kick` passed, so the actor has a managing capacity.
    let role = actor.role().ok_or_else(|| err(StatusCode::FORBIDDEN, 46))?;

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

    let (remover, actor_name) = match (actor, actor_seat) {
        (KickActor::Member { .. }, Some(seat)) => (
            SeatRemover::Manager {
                user_id: seat.author_user,
                character_id: seat.author_character,
            },
            seat.name.clone(),
        ),
        _ => (
            SeatRemover::Operator,
            operator_label(body.operator.as_deref()),
        ),
    };
    let ts = now_unix();
    let gid_owned = gid.to_string();
    let character_id = body.character_id;
    let actor_user_id = body.actor_user_id;
    let log_target_name = resp.name.clone();
    let successor = conn
        .transaction::<_, BladeApiError, _>(move |conn| {
            async move {
                let heir = match (live_target, parked_alt) {
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
                        .await?
                    }
                    (None, Some(alt)) => {
                        // The game lists this member, so the board says it
                        // went, exactly as for a played alt.
                        remove_alt_seat_for_website(
                            conn,
                            &gid_owned,
                            target_user,
                            alt,
                            target_rank,
                            remover,
                            ts,
                        )
                        .await?
                    }
                    (None, None) => return Err(err(StatusCode::NOT_FOUND, 43)),
                };
                write_log(
                    conn,
                    LogLine {
                        gid: &gid_owned,
                        action: "KICK",
                        role,
                        actor_user_id,
                        actor_name: &actor_name,
                        target_user_id: target_user,
                        target_character_id: Some(character_id),
                        target_name: &log_target_name,
                        ts,
                    },
                )
                .await?;
                Ok(heir)
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

// ---------------------------------------------------------------------------
// POST /guilds/{guild_id}/applications/decide
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DecideRequest {
    /// The applicant's character (what the roster's `applications` lists).
    pub character_id: Uuid,
    /// The PUBLIC arena user id of the Grand Master or co-guild master deciding.
    /// Required: a join request is decided by a member, never by an operator.
    pub actor_user_id: Uuid,
    pub approve: bool,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DecideResponse {
    pub guild_id: String,
    pub character_id: Uuid,
    pub name: String,
    pub approved: bool,
    /// Live members after the decision.
    pub member_count: i64,
    pub guild_trophies: Option<i64>,
}

/// `POST /…/api/dev/v1/guilds/{guild_id}/applications/decide`.
#[post("/blades.bgs.services/api/dev/v1/guilds/{guild_id}/applications/decide")]
pub async fn decide_guild_application(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<String>,
    body: web::Json<DecideRequest>,
) -> Result<Json<DecideResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let gid = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, 40))?;
    let resp = decide_application(&mut conn, &gid, &body).await?;
    info!(
        "guild manage: {} of {} ({}) to {} by member {}",
        if resp.approved { "APPROVE" } else { "DENY" },
        resp.character_id,
        resp.name,
        gid,
        body.actor_user_id,
    );
    Ok(Json(resp))
}

pub(crate) async fn decide_application(
    conn: &mut AsyncPgConnection,
    gid: &str,
    body: &DecideRequest,
) -> Result<DecideResponse, BladeApiError> {
    let live = live_seats_of_guild(conn, gid).await?;
    let parked = parked_seats_of_guild(conn, gid).await?;
    if live.is_empty() && parked.is_empty() {
        // No guild, or one with nobody in it: nobody can decide for it.
        return Err(err(StatusCode::NOT_FOUND, 41));
    }
    let co_masters = co_masters_of_guild(conn, gid).await?;
    let (actor, actor_seat) = resolve_actor(&live, &parked, &co_masters, Some(body.actor_user_id))?;
    let role = plan_moderation(actor).map_err(refusal_error)?;
    let actor_seat = actor_seat.ok_or_else(|| err(StatusCode::FORBIDDEN, 44))?;

    let application = applications_of_guild(conn, gid)
        .await?
        .into_iter()
        .find(|a| a.character_id == body.character_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, 49))?;

    let ts = now_unix();
    let gid_owned = gid.to_string();
    // The board entry is authored the way the game names the actor's seat.
    let (actor_user, actor_character, actor_name) = (
        actor_seat.author_user,
        actor_seat.author_character,
        actor_seat.name.clone(),
    );
    let log_actor = body.actor_user_id;
    let approve = body.approve;
    let (applicant_user, applicant_character, applicant_name) = (
        application.user_id,
        application.character_id,
        application.name.clone(),
    );
    conn.transaction::<_, BladeApiError, _>(move |conn| {
        async move {
            decide_application_for_website(
                conn,
                &gid_owned,
                actor_user,
                actor_character,
                applicant_user,
                approve,
            )
            .await?;
            write_log(
                conn,
                LogLine {
                    gid: &gid_owned,
                    action: if approve { "APPROVE" } else { "DENY" },
                    role,
                    actor_user_id: Some(log_actor),
                    actor_name: &actor_name,
                    target_user_id: applicant_user,
                    target_character_id: Some(applicant_character),
                    target_name: &applicant_name,
                    ts,
                },
            )
            .await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    // A new member's cups count towards the guild now, not after the next match.
    let guild_trophies = if approve {
        recompute_and_read(conn, gid).await
    } else {
        None
    };
    Ok(DecideResponse {
        guild_id: gid.to_string(),
        character_id: application.character_id,
        name: application.name,
        approved: approve,
        member_count: (live.len() + parked.len()) as i64 + i64::from(approve),
        guild_trophies,
    })
}

// ---------------------------------------------------------------------------
// POST /guilds/{guild_id}/co-masters
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CoMasterRequest {
    /// The member's character, as the roster lists it.
    pub character_id: Uuid,
    /// `true` appoints, `false` removes the role.
    pub appoint: bool,
    /// The PUBLIC arena user id of the Grand Master. Absent = an operator (the
    /// web gates that to admins).
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    /// Who the operator is, for the log.
    #[serde(default)]
    pub operator: Option<String>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CoMasterResponse {
    pub guild_id: String,
    pub character_id: Uuid,
    pub name: String,
    pub co_master: bool,
    /// `false` when the member already was (or already was not) one.
    pub changed: bool,
}

/// `POST /…/api/dev/v1/guilds/{guild_id}/co-masters`.
#[post("/blades.bgs.services/api/dev/v1/guilds/{guild_id}/co-masters")]
pub async fn set_guild_co_master(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<String>,
    body: web::Json<CoMasterRequest>,
) -> Result<Json<CoMasterResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let gid = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, 40))?;
    let resp = set_co_master(&mut conn, &gid, &body).await?;
    info!(
        "guild manage: co-master {} for {} ({}) in {} by {} — {}",
        if body.appoint { "APPOINT" } else { "REMOVE" },
        resp.character_id,
        resp.name,
        gid,
        body.actor_user_id
            .map_or("operator".to_string(), |a| format!("grandmaster {a}")),
        if resp.changed { "changed" } else { "no change" },
    );
    Ok(Json(resp))
}

pub(crate) async fn set_co_master(
    conn: &mut AsyncPgConnection,
    gid: &str,
    body: &CoMasterRequest,
) -> Result<CoMasterResponse, BladeApiError> {
    let live = live_seats_of_guild(conn, gid).await?;
    let parked = parked_seats_of_guild(conn, gid).await?;
    if live.is_empty() && parked.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, 41));
    }
    let co_masters = co_masters_of_guild(conn, gid).await?;
    let (actor, actor_seat) = resolve_actor(&live, &parked, &co_masters, body.actor_user_id)?;
    let role = plan_appointment(actor).map_err(refusal_error)?;

    // The role is given to a member who is here: a LIVE seat. (Removing it
    // works on the same seat; a co-guild master who left has no row to remove —
    // the departure deleted it.)
    let target = live
        .iter()
        .find(|s| s.character_id == body.character_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, 43))?;
    if target.rank == GuildRank::Grandmaster.as_wire() {
        // The Grand Master already holds everything the role grants.
        return Err(err(StatusCode::CONFLICT, 50));
    }
    let was = co_masters.contains(&target.user_id);
    let mut resp = CoMasterResponse {
        guild_id: gid.to_string(),
        character_id: target.character_id,
        name: target.name.clone(),
        co_master: body.appoint,
        changed: was != body.appoint,
    };
    if !resp.changed {
        return Ok(resp);
    }

    let ts = now_unix();
    let gid_owned = gid.to_string();
    let appoint = body.appoint;
    let actor_user_id = body.actor_user_id;
    let actor_name = match actor_seat {
        Some(seat) => seat.name.clone(),
        None => operator_label(body.operator.as_deref()),
    };
    let (target_user, target_character, target_name) =
        (target.user_id, target.character_id, target.name.clone());
    let changed = conn
        .transaction::<_, BladeApiError, _>(move |conn| {
            async move {
                let n = if appoint {
                    sql_query(
                        "INSERT INTO guild_co_masters (guild_id, user_id, appointed_by, appointed_at)
                         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                    )
                    .bind::<Text, _>(&gid_owned)
                    .bind::<SqlUuid, _>(target_user)
                    .bind::<Nullable<SqlUuid>, _>(actor_user_id)
                    .bind::<BigInt, _>(ts)
                    .execute(conn)
                    .await?
                } else {
                    sql_query("DELETE FROM guild_co_masters WHERE guild_id = $1 AND user_id = $2")
                        .bind::<Text, _>(&gid_owned)
                        .bind::<SqlUuid, _>(target_user)
                        .execute(conn)
                        .await?
                };
                if n > 0 {
                    write_log(
                        conn,
                        LogLine {
                            gid: &gid_owned,
                            action: if appoint {
                                "APPOINT_CO_MASTER"
                            } else {
                                "REMOVE_CO_MASTER"
                            },
                            role,
                            actor_user_id,
                            actor_name: &actor_name,
                            target_user_id: target_user,
                            target_character_id: Some(target_character),
                            target_name: &target_name,
                            ts,
                        },
                    )
                    .await?;
                }
                Ok(n > 0)
            }
            .scope_boxed()
        })
        .await?;
    resp.changed = changed;
    Ok(resp)
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
            co_master: false,
        }
    }

    #[test]
    fn a_grandmaster_may_kick_anyone_below() {
        for rank in [GuildRank::Master, GuildRank::Elder, GuildRank::Member] {
            let actor = KickActor::Member {
                user_id: GM,
                rank: GuildRank::Grandmaster,
                co_master: false,
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
            co_master: false,
        };
        let me = KickTarget {
            user_id: GM,
            rank: GuildRank::Member,
            parked: true,
            co_master: false,
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
            let actor = KickActor::Member {
                user_id: GM,
                rank,
                co_master: false,
            };
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
        // A seat whose alt is not played is a member too: the last seat is the
        // last seat.
        assert_eq!(
            plan_kick(KickActor::Operator, target(GuildRank::Member, true), 1),
            Err(KickRefusal::LastMember)
        );
        assert_eq!(
            plan_kick(KickActor::Operator, target(GuildRank::Member, true), 2),
            Ok(())
        );
    }

    #[test]
    fn only_a_grandmaster_seat_manages_played_or_not() {
        assert!(seat_can_manage("GRANDMASTER"));
        assert!(!seat_can_manage("MASTER"));
        assert!(!seat_can_manage("ELDER"));
        assert!(!seat_can_manage("MEMBER"));
        assert!(!seat_can_manage("LEADER"), "unknown ranks never manage");
    }

    // ---- co-guild masters: the pure rules ---------------------------------

    const CO: Uuid = Uuid::from_u128(3);

    fn co_actor() -> KickActor {
        KickActor::Member {
            user_id: CO,
            rank: GuildRank::Member,
            co_master: true,
        }
    }

    #[test]
    fn a_co_master_kicks_members_of_every_rank_below_grandmaster() {
        for rank in [GuildRank::Master, GuildRank::Elder, GuildRank::Member] {
            assert_eq!(plan_kick(co_actor(), target(rank, false), 5), Ok(()));
            assert_eq!(plan_kick(co_actor(), target(rank, true), 5), Ok(()));
        }
    }

    #[test]
    fn a_co_master_cannot_kick_the_grandmaster_another_co_master_or_themselves() {
        assert_eq!(
            plan_kick(co_actor(), target(GuildRank::Grandmaster, false), 5),
            Err(KickRefusal::NotPermitted)
        );
        let other_co = KickTarget {
            co_master: true,
            ..target(GuildRank::Member, false)
        };
        assert_eq!(
            plan_kick(co_actor(), other_co, 5),
            Err(KickRefusal::NotPermitted)
        );
        let me = KickTarget {
            user_id: CO,
            rank: GuildRank::Member,
            parked: false,
            co_master: true,
        };
        assert_eq!(plan_kick(co_actor(), me, 5), Err(KickRefusal::Yourself));
    }

    #[test]
    fn a_grandmaster_may_kick_a_co_master() {
        let actor = KickActor::Member {
            user_id: GM,
            rank: GuildRank::Grandmaster,
            co_master: false,
        };
        let co = KickTarget {
            co_master: true,
            ..target(GuildRank::Member, false)
        };
        assert_eq!(plan_kick(actor, co, 5), Ok(()));
    }

    #[test]
    fn join_requests_are_decided_by_the_grandmaster_and_co_masters_only() {
        let gm = KickActor::Member {
            user_id: GM,
            rank: GuildRank::Grandmaster,
            co_master: false,
        };
        assert_eq!(plan_moderation(gm), Ok(ActorRole::Grandmaster));
        assert_eq!(plan_moderation(co_actor()), Ok(ActorRole::CoMaster));
        for rank in [GuildRank::Master, GuildRank::Elder, GuildRank::Member] {
            let plain = KickActor::Member {
                user_id: OTHER,
                rank,
                co_master: false,
            };
            assert_eq!(
                plan_moderation(plain),
                Err(KickRefusal::NotPermitted),
                "{rank:?}"
            );
        }
        assert_eq!(
            plan_moderation(KickActor::Outsider),
            Err(KickRefusal::ActorNotInGuild)
        );
        assert_eq!(
            plan_moderation(KickActor::Operator),
            Err(KickRefusal::NotPermitted)
        );
    }

    #[test]
    fn only_the_grandmaster_and_an_operator_appoint() {
        let gm = KickActor::Member {
            user_id: GM,
            rank: GuildRank::Grandmaster,
            co_master: false,
        };
        assert_eq!(plan_appointment(gm), Ok(ActorRole::Grandmaster));
        assert_eq!(
            plan_appointment(KickActor::Operator),
            Ok(ActorRole::Operator)
        );
        assert_eq!(
            plan_appointment(co_actor()),
            Err(KickRefusal::NotPermitted),
            "a co-guild master never appoints"
        );
        assert_eq!(
            plan_appointment(KickActor::Outsider),
            Err(KickRefusal::ActorNotInGuild)
        );
    }

    #[test]
    fn the_role_of_an_actor_is_what_the_log_records() {
        assert_eq!(co_actor().role(), Some(ActorRole::CoMaster));
        assert_eq!(KickActor::Operator.role(), Some(ActorRole::Operator));
        assert_eq!(KickActor::Outsider.role(), None);
        let plain = KickActor::Member {
            user_id: OTHER,
            rank: GuildRank::Member,
            co_master: false,
        };
        assert_eq!(plain.role(), None);
        // A Grand Master with a stale co-master row is still the Grand Master.
        let both = KickActor::Member {
            user_id: GM,
            rank: GuildRank::Grandmaster,
            co_master: true,
        };
        assert_eq!(both.role(), Some(ActorRole::Grandmaster));
    }

    #[test]
    fn an_operator_label_is_bounded_and_printable() {
        assert_eq!(operator_label(None), "site operator");
        assert_eq!(operator_label(Some("  \n ")), "site operator");
        assert_eq!(operator_label(Some("Flappety\u{7}Flap")), "FlappetyFlap");
        assert_eq!(operator_label(Some(&"x".repeat(200))).len(), 64);
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
            b_user: Uuid,
            b_char: Uuid,
            p_user: Uuid,
            p_char: Uuid,
            pa_alt: Uuid,
            pa_member: Uuid,
        }

        async fn seed(c: &mut AsyncPgConnection) -> World {
            let w = World {
                gm_user: Uuid::new_v4(),
                gm_char: Uuid::new_v4(),
                a_user: Uuid::new_v4(),
                a_char: Uuid::new_v4(),
                b_user: Uuid::new_v4(),
                b_char: Uuid::new_v4(),
                p_user: Uuid::new_v4(),
                p_char: Uuid::new_v4(),
                pa_alt: Uuid::new_v4(),
                pa_member: Uuid::new_v4(),
            };
            let (b_user, b_char) = (w.b_user, w.b_char);
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
                 -- PA's seat as the 2026-11-23 migration leaves it: still in the guild.
                 INSERT INTO alt_member_ids (member_id, user_id, alt_key, character_id)
                     VALUES ('{pm}', '{pu}', '{pa}', '{pc}');
                 INSERT INTO guild_alt_seats (user_id, alt_key, member_id, guild_id, character_id,
                     rank, join_date, parked_at)
                     VALUES ('{pu}', '{pa}', '{pm}', '{G}', '{pc}', 'MEMBER', 15, 200);
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
                pm = w.pa_member,
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
                operator: None,
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
                "a member's seat holds no power, played or not"
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
                // The alt brings its re-join cooldown back when it is played.
                assert_eq!(row.s["parkedGuildRemovals"][0]["guild_id"], G);
            }
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_alt_seats").await,
                0
            );
            // The game listed PA under its alt key, so the board says it went —
            // an operator's removal is the member's own LEAVE.
            assert_eq!(
                count(
                    &mut c,
                    &format!(
                        "SELECT COUNT(*) AS n FROM guild_messages WHERE guild_id = '{G}' \
                          AND message_type = 'LEAVE' AND user_id = '{}' AND alt_key = '{}'",
                        w.pa_member, w.pa_alt
                    )
                )
                .await,
                1
            );
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

        /// Owner decision 2026-11-23: "I can manage things on web for all alts
        /// that are guild masters". P plays PB while its alt PA holds the guild:
        /// P manages from the website, and the board names PA, the member the
        /// game lists.
        #[tokio::test]
        async fn a_grand_master_alt_that_is_not_played_manages_on_the_web() {
            let mut c = db!();
            let w = seed(&mut c).await;
            sql_query(format!(
                "UPDATE guild_members SET rank = 'MEMBER' WHERE user_id = '{}'",
                w.gm_user
            ))
            .execute(&mut c)
            .await
            .unwrap();
            sql_query(format!(
                "UPDATE guild_alt_seats SET rank = 'GRANDMASTER' WHERE alt_key = '{}'",
                w.pa_alt
            ))
            .execute(&mut c)
            .await
            .unwrap();

            // Review F2: P's PLAYED alt is a plain member of the same guild; P
            // still acts from its strongest seat there, PA's Grand Master seat.
            sql_query(format!(
                "INSERT INTO guild_members (guild_id, user_id, character_id, rank, join_date) \
                 VALUES ('{G}', '{}', '{}', 'MEMBER', 40)",
                w.p_user, w.p_char
            ))
            .execute(&mut c)
            .await
            .unwrap();

            let seats = seats_of_user(&mut c, w.p_user).await.unwrap();
            assert_eq!(seats.len(), 2);
            let pa = seats.iter().find(|s| s.parked).unwrap();
            assert!(pa.can_manage && pa.can_moderate);
            assert_eq!(pa.alt_key, Some(w.pa_alt));

            let mut r = req(w.a_char);
            r.actor_user_id = Some(w.p_user);
            let resp = kick(&mut c, G, &r).await.unwrap();
            assert!(resp.applied);
            assert_eq!(
                count(
                    &mut c,
                    &format!(
                        "SELECT COUNT(*) AS n FROM guild_messages WHERE message_type = 'KICK' \
                          AND user_id = '{}' AND alt_key = '{}' \
                          AND type_specific_data->>'kickedUserId' = '{}'",
                        w.pa_member, w.pa_alt, w.a_user
                    )
                )
                .await,
                1,
                "authored by PA, under the id the game lists PA by"
            );
        }

        /// Review F5: the binary refuses to start without the 2026-11-23 schema.
        #[tokio::test]
        async fn the_startup_check_names_a_missing_table_or_column() {
            let mut c = db!();
            assert_eq!(crate::guild::required_schema_present(&mut c).await, Ok(()));
            sql_query("ALTER TABLE guild_messages DROP COLUMN target_account")
                .execute(&mut c)
                .await
                .unwrap();
            assert_eq!(
                crate::guild::required_schema_present(&mut c).await,
                Err("guild_messages.target_account".to_string())
            );
            sql_query("DROP TABLE guild_alt_seats")
                .execute(&mut c)
                .await
                .unwrap();
            assert_eq!(
                crate::guild::required_schema_present(&mut c).await,
                Err("guild_alt_seats".to_string())
            );
        }

        // ---- co-guild masters ------------------------------------------------

        #[derive(QueryableByName)]
        struct Count {
            #[diesel(sql_type = BigInt)]
            n: i64,
        }

        async fn count(c: &mut AsyncPgConnection, sql: &str) -> i64 {
            sql_query(sql).get_result::<Count>(c).await.unwrap().n
        }

        fn co_req(character_id: Uuid, appoint: bool, actor: Option<Uuid>) -> CoMasterRequest {
            CoMasterRequest {
                character_id,
                appoint,
                actor_user_id: actor,
                operator: None,
            }
        }

        /// Appoint A co-guild master, as the Grand Master.
        async fn appoint_a(c: &mut AsyncPgConnection, w: &World) {
            let r = set_co_master(c, G, &co_req(w.a_char, true, Some(w.gm_user)))
                .await
                .unwrap();
            assert!(r.changed && r.co_master);
        }

        /// A pending join request by a fresh level-12 character "Quill", who was
        /// kicked from this guild before (the approval must clear that).
        async fn applicant(c: &mut AsyncPgConnection) -> (Uuid, Uuid) {
            let (user, character) = (Uuid::new_v4(), Uuid::new_v4());
            c.batch_execute(&format!(
                "INSERT INTO users (id, secret_id, data) VALUES ('{user}', gen_random_uuid(), '{{}}');
                 INSERT INTO characters (id, user_id, source_alt_uuid, character, data, inventory, wallet) VALUES
                   ('{character}', '{user}', gen_random_uuid(), '{{\"name\":\"Quill\",\"level\":12}}', '{{}}', '{{}}', '{{}}');
                 INSERT INTO guild_applications (guild_id, user_id, character_id, state, creation_time)
                   VALUES ('{G}', '{user}', '{character}', 'APPLIED', 77);
                 INSERT INTO guild_removals (guild_id, user_id, removed_at, banned)
                   VALUES ('{G}', '{user}', 5, false);"
            ))
            .await
            .unwrap();
            (user, character)
        }

        fn decide(character_id: Uuid, actor: Uuid, approve: bool) -> DecideRequest {
            DecideRequest {
                character_id,
                actor_user_id: actor,
                approve,
            }
        }

        #[tokio::test]
        async fn the_grandmaster_and_an_operator_appoint_and_remove_and_it_is_logged() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;

            // The seat lookup the website gates on, and the roster flag.
            let a = seats_of_user(&mut c, w.a_user).await.unwrap();
            assert!(a[0].co_master && a[0].can_moderate && !a[0].can_manage);
            assert_eq!(a[0].rank, "MEMBER", "the in-game rank is untouched");
            let roster = build_roster(&mut c, G).await.unwrap().unwrap();
            let flagged: Vec<Uuid> = roster
                .members
                .iter()
                .filter(|m| m.co_master)
                .map(|m| m.character_id)
                .collect();
            assert_eq!(flagged, vec![w.a_char]);
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_members WHERE rank <> 'MEMBER' AND rank <> 'GRANDMASTER'").await,
                0
            );
            // No board entry: the game has no such event to show.
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_messages").await,
                0
            );

            // Appointing twice changes nothing and logs nothing.
            let again = set_co_master(&mut c, G, &co_req(w.a_char, true, Some(w.gm_user)))
                .await
                .unwrap();
            assert!(!again.changed);

            // An operator (site admin) appoints B and removes A.
            let mut by_op = co_req(w.b_char, true, None);
            by_op.operator = Some("FlappetyFlap".into());
            assert!(set_co_master(&mut c, G, &by_op).await.unwrap().changed);
            let removed = set_co_master(&mut c, G, &co_req(w.a_char, false, None))
                .await
                .unwrap();
            assert!(removed.changed && !removed.co_master);
            let a = seats_of_user(&mut c, w.a_user).await.unwrap();
            assert!(!a[0].co_master && !a[0].can_moderate);

            let log = build_roster(&mut c, G).await.unwrap().unwrap().log;
            let lines: Vec<String> = log
                .iter()
                .rev()
                .map(|l| {
                    format!(
                        "{} {} {} -> {}",
                        l.action, l.actor_role, l.actor_name, l.target_name
                    )
                })
                .collect();
            assert_eq!(
                lines,
                [
                    "APPOINT_CO_MASTER GRANDMASTER Master -> Alpha",
                    "APPOINT_CO_MASTER OPERATOR FlappetyFlap -> Beta",
                    "REMOVE_CO_MASTER OPERATOR site operator -> Alpha",
                ]
            );
        }

        #[tokio::test]
        async fn nobody_but_the_grandmaster_or_an_operator_appoints() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;
            // A co-guild master cannot appoint, nor remove a peer or themselves.
            assert!(
                set_co_master(&mut c, G, &co_req(w.b_char, true, Some(w.a_user)))
                    .await
                    .is_err()
            );
            assert!(
                set_co_master(&mut c, G, &co_req(w.a_char, false, Some(w.a_user)))
                    .await
                    .is_err()
            );
            // An ordinary member cannot; an outsider (a parked seat holds no live power) cannot.
            assert!(
                set_co_master(&mut c, G, &co_req(w.a_char, true, Some(w.b_user)))
                    .await
                    .is_err()
            );
            assert!(
                set_co_master(&mut c, G, &co_req(w.b_char, true, Some(w.p_user)))
                    .await
                    .is_err()
            );
            // The Grand Master is never a co-guild master; a non-member cannot be one.
            assert!(
                set_co_master(&mut c, G, &co_req(w.gm_char, true, None))
                    .await
                    .is_err()
            );
            assert!(
                set_co_master(&mut c, G, &co_req(w.p_char, true, None))
                    .await
                    .is_err()
            );
            assert!(
                set_co_master(
                    &mut c,
                    "bbbbbbbbbbbbbbbbbbbbbbbb",
                    &co_req(w.a_char, true, None)
                )
                .await
                .is_err()
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_co_masters").await,
                1
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_manage_log").await,
                1
            );
        }

        #[tokio::test]
        async fn a_co_master_kick_is_the_in_game_kick_authored_by_them() {
            let mut c = db!();
            let w = seed(&mut c).await;
            crate::guild::recompute_guild_trophies_for_tests(&mut c).await;
            appoint_a(&mut c, &w).await;
            let mut r = req(w.b_char);
            r.actor_user_id = Some(w.a_user);
            let resp = kick(&mut c, G, &r).await.unwrap();
            assert!(resp.applied);
            assert_eq!(resp.guild_trophies, Some(27), "40/30/20 at 30% = 27");

            #[derive(QueryableByName)]
            struct M {
                #[diesel(sql_type = SqlUuid)]
                user_id: Uuid,
                #[diesel(sql_type = SqlUuid)]
                character_id: Uuid,
                #[diesel(sql_type = diesel::sql_types::Jsonb)]
                data: serde_json::Value,
            }
            let m: Vec<M> = sql_query(
                "SELECT user_id, character_id, type_specific_data AS data
                   FROM guild_messages WHERE guild_id = $1",
            )
            .bind::<Text, _>(G)
            .get_results(&mut c)
            .await
            .unwrap();
            assert_eq!(m.len(), 1, "one board entry, the KICK");
            assert_eq!((m[0].user_id, m[0].character_id), (w.a_user, w.a_char));
            assert_eq!(
                m[0].data,
                serde_json::json!({ "type": "KICK", "kickedUserId": w.b_user }),
                "exactly the captured retail shape"
            );
            assert_eq!(
                count(
                    &mut c,
                    &format!(
                        "SELECT COUNT(*) AS n FROM guild_members WHERE user_id = '{}'",
                        w.b_user
                    )
                )
                .await,
                0
            );
            assert_eq!(
                count(&mut c, &format!("SELECT COUNT(*) AS n FROM guild_removals WHERE user_id = '{}' AND NOT banned", w.b_user)).await,
                1,
                "the re-join cooldown is armed"
            );
            let log = build_roster(&mut c, G).await.unwrap().unwrap().log;
            assert_eq!(
                (
                    log[0].action.as_str(),
                    log[0].actor_role.as_str(),
                    log[0].actor_name.as_str(),
                    log[0].target_name.as_str()
                ),
                ("KICK", "CO_MASTER", "Alpha", "Beta")
            );
        }

        #[tokio::test]
        async fn a_co_master_cannot_kick_the_grandmaster_a_peer_or_themselves() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;
            set_co_master(&mut c, G, &co_req(w.b_char, true, Some(w.gm_user)))
                .await
                .unwrap();
            for victim in [w.gm_char, w.b_char, w.a_char] {
                let mut r = req(victim);
                r.actor_user_id = Some(w.a_user);
                assert!(kick(&mut c, G, &r).await.is_err(), "{victim}");
            }
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_members").await,
                3
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_messages").await,
                0
            );
            assert_eq!(
                count(
                    &mut c,
                    "SELECT COUNT(*) AS n FROM guild_manage_log WHERE action = 'KICK'"
                )
                .await,
                0,
                "a refused kick is not logged as one"
            );
            // The Grand Master, though, may remove a co-guild master — and the
            // role goes with the seat.
            let mut r = req(w.b_char);
            r.actor_user_id = Some(w.gm_user);
            assert!(kick(&mut c, G, &r).await.unwrap().applied);
            assert_eq!(
                count(
                    &mut c,
                    &format!(
                        "SELECT COUNT(*) AS n FROM guild_co_masters WHERE user_id = '{}'",
                        w.b_user
                    )
                )
                .await,
                0
            );
        }

        #[tokio::test]
        async fn a_co_master_has_no_power_in_another_guild() {
            let mut c = db!();
            let w = seed(&mut c).await;
            // X is the co-guild master of a different guild.
            let (x_user, x_char, y_user, y_char) = (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            const G2: &str = "cccccccccccccccccccccccc";
            c.batch_execute(&format!(
                "INSERT INTO users (id, secret_id, data) VALUES
                   ('{x_user}', gen_random_uuid(), '{{}}'), ('{y_user}', gen_random_uuid(), '{{}}');
                 INSERT INTO guilds (id, name, tag_id, trophies) VALUES ('{G2}', 'Elsewhere', '0002', 0);
                 INSERT INTO characters (id, user_id, source_alt_uuid, character, data, inventory, wallet) VALUES
                   ('{x_char}', '{x_user}', gen_random_uuid(), '{{\"name\":\"Xan\",\"level\":9}}', '{{}}', '{{}}', '{{}}'),
                   ('{y_char}', '{y_user}', gen_random_uuid(), '{{\"name\":\"Yor\",\"level\":9}}', '{{}}', '{{}}', '{{}}');
                 INSERT INTO guild_members (guild_id, user_id, character_id, rank, join_date) VALUES
                   ('{G2}', '{y_user}', '{y_char}', 'GRANDMASTER', 1),
                   ('{G2}', '{x_user}', '{x_char}', 'MEMBER', 2);"
            ))
            .await
            .unwrap();
            assert!(
                set_co_master(&mut c, G2, &co_req(x_char, true, Some(y_user)))
                    .await
                    .unwrap()
                    .changed
            );
            let (_, q_char) = applicant(&mut c).await;

            let mut r = req(w.a_char);
            r.actor_user_id = Some(x_user);
            assert!(kick(&mut c, G, &r).await.is_err(), "kick in another guild");
            assert!(
                decide_application(&mut c, G, &decide(q_char, x_user, true))
                    .await
                    .is_err()
            );
            assert!(
                decide_application(&mut c, G, &decide(q_char, x_user, false))
                    .await
                    .is_err()
            );
            // Nor may they reach a member of the other guild through their own.
            let mut r = req(w.a_char);
            r.actor_user_id = Some(x_user);
            assert!(
                kick(&mut c, G2, &r).await.is_err(),
                "target is not in the actor's guild"
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_applications").await,
                1
            );
            assert_eq!(
                count(
                    &mut c,
                    &format!("SELECT COUNT(*) AS n FROM guild_members WHERE guild_id = '{G}'")
                )
                .await,
                3
            );
        }

        #[tokio::test]
        async fn a_removed_co_master_loses_the_rights_at_once() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;
            let (_, q_char) = applicant(&mut c).await;
            set_co_master(&mut c, G, &co_req(w.a_char, false, Some(w.gm_user)))
                .await
                .unwrap();
            let mut r = req(w.b_char);
            r.actor_user_id = Some(w.a_user);
            assert!(kick(&mut c, G, &r).await.is_err());
            assert!(
                decide_application(&mut c, G, &decide(q_char, w.a_user, true))
                    .await
                    .is_err()
            );
            assert!(
                decide_application(&mut c, G, &decide(q_char, w.a_user, false))
                    .await
                    .is_err()
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_applications").await,
                1
            );
        }

        #[tokio::test]
        async fn a_co_master_approval_seats_the_applicant_as_the_game_does() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;
            let (q_user, q_char) = applicant(&mut c).await;

            // The roster lists the request with what a manager needs.
            let roster = build_roster(&mut c, G).await.unwrap().unwrap();
            assert_eq!(roster.applications.len(), 1);
            assert_eq!(
                (
                    roster.applications[0].name.as_str(),
                    roster.applications[0].level,
                    roster.applications[0].character_id
                ),
                ("Quill", 12, q_char)
            );

            // An ordinary member is refused; so is an unknown request.
            assert!(
                decide_application(&mut c, G, &decide(q_char, w.b_user, true))
                    .await
                    .is_err()
            );
            assert!(
                decide_application(&mut c, G, &decide(Uuid::new_v4(), w.a_user, true))
                    .await
                    .is_err()
            );

            let r = decide_application(&mut c, G, &decide(q_char, w.a_user, true))
                .await
                .unwrap();
            assert!(r.approved);
            // Three played seats, P's other alt, and the newcomer.
            assert_eq!(r.member_count, 5);

            #[derive(QueryableByName)]
            struct Seat {
                #[diesel(sql_type = SqlUuid)]
                character_id: Uuid,
                #[diesel(sql_type = Text)]
                rank: String,
            }
            let seat: Seat = sql_query(
                "SELECT character_id, rank FROM guild_members WHERE guild_id = $1 AND user_id = $2",
            )
            .bind::<Text, _>(G)
            .bind::<SqlUuid, _>(q_user)
            .get_result(&mut c)
            .await
            .unwrap();
            assert_eq!((seat.character_id, seat.rank.as_str()), (q_char, "MEMBER"));
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_applications").await,
                0
            );
            assert_eq!(
                count(
                    &mut c,
                    &format!("SELECT COUNT(*) AS n FROM guild_removals WHERE user_id = '{q_user}'")
                )
                .await,
                0,
                "an approval clears the earlier removal, as in the game"
            );
            #[derive(QueryableByName)]
            struct M {
                #[diesel(sql_type = SqlUuid)]
                user_id: Uuid,
                #[diesel(sql_type = Text)]
                message_type: String,
                #[diesel(sql_type = diesel::sql_types::Jsonb)]
                data: serde_json::Value,
            }
            let m: Vec<M> = sql_query(
                "SELECT user_id, message_type, type_specific_data AS data FROM guild_messages WHERE guild_id = $1",
            )
            .bind::<Text, _>(G)
            .get_results(&mut c)
            .await
            .unwrap();
            assert_eq!(m.len(), 1);
            assert_eq!(
                (m[0].user_id, m[0].message_type.as_str()),
                (w.a_user, "APPROVE")
            );
            assert_eq!(
                m[0].data,
                serde_json::json!({ "type": "APPROVE", "approvedUserId": q_user })
            );
            let log = build_roster(&mut c, G).await.unwrap().unwrap().log;
            assert_eq!(
                (
                    log[0].action.as_str(),
                    log[0].actor_role.as_str(),
                    log[0].actor_name.as_str(),
                    log[0].target_name.as_str()
                ),
                ("APPROVE", "CO_MASTER", "Alpha", "Quill")
            );
        }

        #[tokio::test]
        async fn a_co_master_rejection_drops_the_request_and_posts_deny() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;
            let (q_user, q_char) = applicant(&mut c).await;
            let r = decide_application(&mut c, G, &decide(q_char, w.a_user, false))
                .await
                .unwrap();
            assert!(!r.approved);
            assert_eq!(r.member_count, 4);
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_applications").await,
                0
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_members").await,
                3
            );
            #[derive(QueryableByName)]
            struct M {
                #[diesel(sql_type = SqlUuid)]
                user_id: Uuid,
                #[diesel(sql_type = diesel::sql_types::Jsonb)]
                data: serde_json::Value,
            }
            let m: M = sql_query(
                "SELECT user_id, type_specific_data AS data FROM guild_messages WHERE message_type = 'DENY'",
            )
            .get_result(&mut c)
            .await
            .unwrap();
            assert_eq!(m.user_id, w.a_user);
            assert_eq!(
                m.data,
                serde_json::json!({ "type": "DENY", "deniedUserId": q_user })
            );
            // The Grand Master decides on the website too.
            let (_, q2) = applicant(&mut c).await;
            assert!(
                decide_application(&mut c, G, &decide(q2, w.gm_user, true))
                    .await
                    .unwrap()
                    .approved
            );
            assert_eq!(
                count(
                    &mut c,
                    "SELECT COUNT(*) AS n FROM guild_manage_log WHERE action IN ('DENY','APPROVE')"
                )
                .await,
                2
            );
        }

        #[tokio::test]
        async fn a_full_guild_refuses_an_approval_and_keeps_the_request() {
            let mut c = db!();
            let w = seed(&mut c).await;
            appoint_a(&mut c, &w).await;
            let (_, q_char) = applicant(&mut c).await;
            c.batch_execute(&format!(
                "INSERT INTO guild_members (guild_id, user_id, character_id, rank, join_date)
                 SELECT '{G}', gen_random_uuid(), gen_random_uuid(), 'MEMBER', 99 FROM generate_series(1, 17);"
            ))
            .await
            .unwrap();
            assert!(
                decide_application(&mut c, G, &decide(q_char, w.a_user, true))
                    .await
                    .is_err()
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_applications").await,
                1
            );
            assert_eq!(
                count(&mut c, "SELECT COUNT(*) AS n FROM guild_messages").await,
                0
            );
        }
    }
}
