//! Guild support console — the operator side of the guild subsystem.
//!
//! ## Why this exists
//!
//! Retail shipped **no** in-game way to change a guild's Grand Master. The
//! il2cpp dump has `CanPromote`/`CanDemote` as 8-byte stubs against 0x128-byte
//! `CanKick`/`CanBan`, and across 1,422 captured member records only
//! GRANDMASTER and MEMBER ever appear. When a retail guild lost its Grand
//! Master, a player contacted Bethesda support and a human fixed it out of
//! band.
//!
//! This module is that human. It is deliberately **not** a client-protocol
//! feature: no route here is reachable by the game, every one is dev-token
//! gated, and the web console at `/admin/guilds` (community_manager and above)
//! is its only intended caller.
//!
//! ## Relationship to automatic succession
//!
//! `guild_policy::successor` already hands the guild to the most senior
//! surviving member when a Grand Master *leaves*, so a guild can never freeze
//! on its own. That stays. This module covers the cases succession cannot:
//! a Grand Master who has stopped playing without leaving, one who should never
//! have had it, or a guild the community wants re-pointed.
//!
//! ## Convention
//!
//! Writes default to a **dry run**, same as the season rollover: a caller must
//! say `"apply": true` to change anything. The console shows the operator what
//! would happen before it happens, and a mistyped field reports instead of
//! writing.

use std::sync::Arc;

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use diesel::sql_types::{BigInt, Text, Uuid as SqlUuid};
use diesel::{QueryableByName, sql_query};
use diesel_async::RunQueryDsl;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    admin::check_import_token,
    guild_policy::GuildRank,
};

/// Same out-of-band envelope id the rest of the dev surface uses.
const GUILD_ADMIN_SERVICE_ID: u64 = 9002;

// ---------------------------------------------------------------------------
// Pure policy: what a Grand Master change would do
// ---------------------------------------------------------------------------

/// One guild member, reduced to what a succession decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberSlot {
    pub character_id: Uuid,
    pub rank: GuildRank,
}

/// Why a Grand Master change cannot go ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GmRefusal {
    /// The named character is not in this guild. Refusing rather than silently
    /// adding them: an operator who typed the wrong id wants to know.
    NotAMember,
    /// The guild has no members at all — there is nobody to promote.
    EmptyGuild,
}

/// The change a Grand Master handover would make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GmChange {
    /// Who becomes Grand Master.
    pub promote: Uuid,
    /// The outgoing Grand Master, demoted to MASTER. `None` when the guild had
    /// no Grand Master at all (the case this console mainly exists to repair).
    pub demote: Option<Uuid>,
    /// True when the target already held it, so applying is a no-op. Reported
    /// rather than refused: re-running a handover must be safe.
    pub already_grandmaster: bool,
}

/// Work out what promoting `target` to Grand Master would change.
///
/// Pure so the authorization decision can be tested without a database — the
/// guild subsystem's kick path shipped two auth holes (a member could kick the
/// owner, and could kick members of *other* guilds) precisely because that
/// logic only existed inline in a handler.
///
/// Demotes the outgoing Grand Master to MASTER rather than MEMBER. MASTER is
/// the rank immediately below, it is a real retail rank, and it is cosmetic —
/// only GRANDMASTER carries power — so the demotion removes authority without
/// also erasing the person's standing in the guild.
pub fn plan_grandmaster_change(
    members: &[MemberSlot],
    target: Uuid,
) -> Result<GmChange, GmRefusal> {
    if members.is_empty() {
        return Err(GmRefusal::EmptyGuild);
    }
    let target_slot = members
        .iter()
        .find(|m| m.character_id == target)
        .ok_or(GmRefusal::NotAMember)?;

    let current = members
        .iter()
        .find(|m| m.rank == GuildRank::Grandmaster)
        .map(|m| m.character_id);

    Ok(GmChange {
        promote: target,
        // Never "demote" the target to make room for themselves.
        demote: current.filter(|c| *c != target),
        already_grandmaster: target_slot.rank == GuildRank::Grandmaster,
    })
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(QueryableByName)]
struct GuildRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    tag_id: String,
    #[diesel(sql_type = Text)]
    guild_type: String,
    #[diesel(sql_type = BigInt)]
    trophies: i64,
    #[diesel(sql_type = BigInt)]
    created_at: i64,
    #[diesel(sql_type = BigInt)]
    member_count: i64,
    #[diesel(sql_type = BigInt)]
    grandmaster_count: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildSummary {
    pub id: String,
    pub name: String,
    pub tag_id: String,
    pub guild_type: String,
    pub trophies: i64,
    pub created_at: i64,
    pub member_count: i64,
    /// True when the guild has nobody holding GRANDMASTER. These are the guilds
    /// the console exists for, so the list surfaces them without a second call.
    pub leaderless: bool,
}

#[derive(QueryableByName)]
struct MemberRow {
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    rank: String,
    #[diesel(sql_type = BigInt)]
    join_date: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberView {
    pub character_id: Uuid,
    pub user_id: Uuid,
    pub rank: String,
    pub join_date: i64,
    /// Best-effort display name from the `characters` row; `None` when the
    /// character has no name or no longer exists. Never fabricated.
    pub name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildDetail {
    #[serde(flatten)]
    pub guild: GuildSummary,
    pub members: Vec<MemberView>,
}

/// `POST /…/api/dev/v1/guilds/{guild_id}/grandmaster`.
///
/// **Defaults to a dry run.** Absent or mistyped `apply` reports the change and
/// writes nothing.
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetGrandmasterRequest {
    pub character_id: Uuid,
    /// Write the change. Absent or false = report only.
    #[serde(default)]
    pub apply: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetGrandmasterResponse {
    pub applied: bool,
    pub guild_id: String,
    #[serde(flatten)]
    pub change: GmChange,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

const GUILD_LIST_SQL: &str = "
    SELECT g.id, g.name, g.tag_id, g.guild_type, g.trophies, g.created_at,
           COALESCE(m.cnt, 0)  AS member_count,
           COALESCE(m.gms, 0)  AS grandmaster_count
      FROM guilds g
      LEFT JOIN (
          SELECT guild_id,
                 COUNT(*)                                            AS cnt,
                 COUNT(*) FILTER (WHERE rank = 'GRANDMASTER')        AS gms
            FROM guild_members
           GROUP BY guild_id
      ) m ON m.guild_id = g.id
     ORDER BY g.trophies DESC, g.name ASC
";

fn to_summary(r: GuildRow) -> GuildSummary {
    GuildSummary {
        id: r.id,
        name: r.name,
        tag_id: r.tag_id,
        guild_type: r.guild_type,
        trophies: r.trophies,
        created_at: r.created_at,
        member_count: r.member_count,
        leaderless: r.grandmaster_count == 0,
    }
}

/// `GET /…/api/dev/v1/guilds` — every guild, leaderless ones flagged.
///
/// One aggregate query, not a per-guild count: the guild leaderboard shipped an
/// N+1 and this is the same shape.
#[get("/api/dev/v1/guilds")]
pub async fn list_guilds(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<Vec<GuildSummary>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = db(&app_state).await?;
    let rows: Vec<GuildRow> = sql_query(GUILD_LIST_SQL).get_results(&mut conn).await.map_err(|e| {
        warn!("guild console: list failed: {e}");
        err(StatusCode::INTERNAL_SERVER_ERROR, 10)
    })?;
    Ok(Json(rows.into_iter().map(to_summary).collect()))
}

/// `GET /…/api/dev/v1/guilds/{guild_id}` — one guild and its roster.
#[get("/api/dev/v1/guilds/{guild_id}")]
pub async fn get_guild_detail(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<String>,
) -> Result<Json<GuildDetail>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let guild_id = path.into_inner();
    let mut conn = db(&app_state).await?;

    let mut guilds: Vec<GuildRow> =
        sql_query(format!("{GUILD_LIST_SQL} ").replace("ORDER BY", "WHERE g.id = $1 ORDER BY"))
            .bind::<Text, _>(&guild_id)
            .get_results(&mut conn)
            .await
            .map_err(|e| {
                warn!("guild console: detail failed for {guild_id}: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, 11)
            })?;
    if guilds.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, 12));
    }
    let guild = to_summary(guilds.remove(0));

    let members: Vec<MemberRow> = sql_query(
        "SELECT character_id, user_id, rank, join_date
           FROM guild_members WHERE guild_id = $1
          ORDER BY CASE rank WHEN 'GRANDMASTER' THEN 0 WHEN 'MASTER' THEN 1
                             WHEN 'ELDER' THEN 2 ELSE 3 END, join_date ASC",
    )
    .bind::<Text, _>(&guild_id)
    .get_results(&mut conn)
    .await
    .map_err(|e| {
        warn!("guild console: roster failed for {guild_id}: {e}");
        err(StatusCode::INTERNAL_SERVER_ERROR, 13)
    })?;

    let names = character_names(&mut conn, &members).await;
    let members = members
        .into_iter()
        .map(|m| MemberView {
            name: names
                .iter()
                .find(|(id, _)| *id == m.character_id)
                .and_then(|(_, n)| n.clone()),
            character_id: m.character_id,
            user_id: m.user_id,
            rank: m.rank,
            join_date: m.join_date,
        })
        .collect();

    Ok(Json(GuildDetail { guild, members }))
}

/// `POST /…/api/dev/v1/guilds/{guild_id}/grandmaster` — hand a guild over.
#[post("/api/dev/v1/guilds/{guild_id}/grandmaster")]
pub async fn set_grandmaster(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<String>,
    body: web::Json<SetGrandmasterRequest>,
) -> Result<Json<SetGrandmasterResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let guild_id = path.into_inner();
    let body = body.into_inner();
    let mut conn = db(&app_state).await?;

    let rows: Vec<MemberRow> =
        sql_query("SELECT character_id, user_id, rank, join_date FROM guild_members WHERE guild_id = $1")
            .bind::<Text, _>(&guild_id)
            .get_results(&mut conn)
            .await
            .map_err(|e| {
                warn!("guild console: roster read failed for {guild_id}: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, 14)
            })?;

    // An unrecognised rank string is REFUSED, not quietly read as MEMBER.
    //
    // Prod currently holds 4 rows at 'LEADER' — this codebase's pre-retail
    // vocabulary — because the migration that rewrites them to 'GRANDMASTER'
    // has not been applied there yet. Defaulting those to MEMBER would make
    // every one of those guilds look leaderless to the console, and appointing
    // a Grand Master would find no sitting holder to step down, leaving the
    // guild with two. Refusing is the safe direction: the operator gets a clear
    // failure and the database gets migrated, instead of a silent second
    // leader.
    let mut slots: Vec<MemberSlot> = Vec::with_capacity(rows.len());
    for r in &rows {
        match GuildRank::from_wire(&r.rank) {
            Some(rank) => slots.push(MemberSlot { character_id: r.character_id, rank }),
            None => {
                warn!(
                    "guild console: {} holds unknown rank {:?} in guild {} — refusing; \
                     the guild rank migration has probably not been applied",
                    r.character_id, r.rank, guild_id
                );
                return Err(err(StatusCode::CONFLICT, 21));
            }
        }
    }

    let change = plan_grandmaster_change(&slots, body.character_id).map_err(|refusal| {
        match refusal {
            GmRefusal::EmptyGuild => err(StatusCode::CONFLICT, 15),
            GmRefusal::NotAMember => err(StatusCode::BAD_REQUEST, 16),
        }
    })?;

    if body.apply && !change.already_grandmaster {
        let now = now_unix();
        if let Some(outgoing) = change.demote {
            sql_query(
                "UPDATE guild_members SET rank = 'MASTER' WHERE guild_id = $1 AND character_id = $2",
            )
            .bind::<Text, _>(&guild_id)
            .bind::<SqlUuid, _>(outgoing)
            .execute(&mut conn)
            .await
            .map_err(|e| {
                warn!("guild console: demote failed in {guild_id}: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, 17)
            })?;
        }
        sql_query(
            "UPDATE guild_members SET rank = 'GRANDMASTER' WHERE guild_id = $1 AND character_id = $2",
        )
        .bind::<Text, _>(&guild_id)
        .bind::<SqlUuid, _>(change.promote)
        .execute(&mut conn)
        .await
        .map_err(|e| {
            warn!("guild console: promote failed in {guild_id}: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, 18)
        })?;
        sql_query("UPDATE guilds SET grandmaster_since = $1 WHERE id = $2")
            .bind::<BigInt, _>(now)
            .bind::<Text, _>(&guild_id)
            .execute(&mut conn)
            .await
            .map_err(|e| {
                warn!("guild console: grandmaster_since failed for {guild_id}: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, 19)
            })?;
    }

    // Every admin action is logged — a support tool that can silently re-point
    // a community's guild must leave a trail.
    info!(
        "guild console: grandmaster of {} -> {} ({}), outgoing {:?}, already_gm {}",
        guild_id,
        change.promote,
        if body.apply { "APPLIED" } else { "dry run" },
        change.demote,
        change.already_grandmaster,
    );

    Ok(Json(SetGrandmasterResponse { applied: body.apply, guild_id, change }))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn err(status: StatusCode, code: u64) -> BladeApiError {
    BladeApiError::new(status, GUILD_ADMIN_SERVICE_ID, code)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn db(
    app_state: &ServerGlobal,
) -> Result<diesel_async::pooled_connection::bb8::PooledConnection<'_, diesel_async::AsyncPgConnection>, BladeApiError>
{
    app_state.db_pool.get().await.map_err(|e| {
        warn!("guild console: no db connection: {e}");
        err(StatusCode::SERVICE_UNAVAILABLE, 20)
    })
}

#[derive(QueryableByName)]
struct NameRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Nullable<Text>)]
    name: Option<String>,
}

/// Display names for a roster, in ONE query. Missing names stay `None`.
async fn character_names(
    conn: &mut diesel_async::AsyncPgConnection,
    members: &[MemberRow],
) -> Vec<(Uuid, Option<String>)> {
    if members.is_empty() {
        return Vec::new();
    }
    let ids: Vec<Uuid> = members.iter().map(|m| m.character_id).collect();
    let rows: Result<Vec<NameRow>, _> =
        sql_query("SELECT id, character->>'name' AS name FROM characters WHERE id = ANY($1)")
            .bind::<diesel::sql_types::Array<SqlUuid>, _>(ids)
            .get_results(conn)
            .await;
    match rows {
        Ok(rows) => rows.into_iter().map(|r| (r.id, r.name)).collect(),
        Err(e) => {
            // A missing name must never fail the console — the roster is the
            // point, the names are a convenience.
            warn!("guild console: name lookup failed: {e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn slot(n: u128, rank: GuildRank) -> MemberSlot {
        MemberSlot { character_id: id(n), rank }
    }

    /// The repair this console exists for: a guild with nobody holding
    /// GRANDMASTER. Nothing to demote, somebody to promote.
    #[test]
    fn a_leaderless_guild_gets_a_grandmaster_and_demotes_nobody() {
        let members = [slot(1, GuildRank::Master), slot(2, GuildRank::Member)];
        let change = plan_grandmaster_change(&members, id(2)).unwrap();
        assert_eq!(change.promote, id(2));
        assert_eq!(change.demote, None, "there was no outgoing grandmaster");
        assert!(!change.already_grandmaster);
    }

    /// The ordinary handover: the sitting Grand Master steps down to MASTER,
    /// not to MEMBER — the demotion removes authority, not standing.
    #[test]
    fn a_handover_demotes_the_sitting_grandmaster_to_master() {
        let members = [slot(1, GuildRank::Grandmaster), slot(2, GuildRank::Member)];
        let change = plan_grandmaster_change(&members, id(2)).unwrap();
        assert_eq!(change.promote, id(2));
        assert_eq!(change.demote, Some(id(1)));
    }

    /// Re-running a handover must be safe. An operator who does not see the
    /// first response, or a double-clicked button, must not corrupt the guild.
    #[test]
    fn promoting_the_sitting_grandmaster_is_a_reported_no_op() {
        let members = [slot(1, GuildRank::Grandmaster), slot(2, GuildRank::Member)];
        let change = plan_grandmaster_change(&members, id(1)).unwrap();
        assert!(change.already_grandmaster);
        assert_eq!(
            change.demote, None,
            "the target must never be demoted to make room for themselves"
        );
    }

    /// A typo in a character id must be refused, never silently absorbed by
    /// adding that character to the guild.
    #[test]
    fn a_non_member_is_refused() {
        let members = [slot(1, GuildRank::Grandmaster)];
        assert_eq!(plan_grandmaster_change(&members, id(99)), Err(GmRefusal::NotAMember));
    }

    /// An empty guild is refused distinctly from a bad id, so the console can
    /// tell the operator to delete it rather than hunt for the right character.
    #[test]
    fn an_empty_guild_is_refused_distinctly() {
        assert_eq!(plan_grandmaster_change(&[], id(1)), Err(GmRefusal::EmptyGuild));
    }

    /// Guards the handler's demote step. If a guild somehow holds two
    /// GRANDMASTERs (the schema does not forbid it), planning still yields ONE
    /// demotion — but the invariant that matters is that the plan never demotes
    /// the incoming holder, which the handler relies on for its UPDATE order.
    #[test]
    fn a_second_grandmaster_never_demotes_the_incoming_one() {
        let members = [
            slot(1, GuildRank::Grandmaster),
            slot(2, GuildRank::Grandmaster),
            slot(3, GuildRank::Member),
        ];
        let change = plan_grandmaster_change(&members, id(2)).unwrap();
        assert_ne!(change.demote, Some(id(2)), "must not demote the promotee");
        assert!(change.already_grandmaster);
    }

    /// The dry-run default, same property the season rollover needed a test
    /// for. This endpoint re-points a whole community's guild; a mistyped
    /// field must report, not write.
    #[test]
    fn the_request_defaults_to_a_dry_run() {
        let body = format!(r#"{{"characterId":"{}"}}"#, id(1));
        let r: SetGrandmasterRequest = serde_json::from_str(&body).unwrap();
        assert!(!r.apply, "an absent `apply` must not write");

        let typo = format!(r#"{{"characterId":"{}","aply":true}}"#, id(1));
        let r: SetGrandmasterRequest = serde_json::from_str(&typo).unwrap();
        assert!(!r.apply, "a mistyped `apply` must not write");

        // Positive control: without this the two above would pass even if
        // `apply` were hard-wired false and the endpoint silently inert.
        let armed = format!(r#"{{"characterId":"{}","apply":true}}"#, id(1));
        let r: SetGrandmasterRequest = serde_json::from_str(&armed).unwrap();
        assert!(r.apply, "an explicit `apply: true` must still work");
    }

    /// An un-migrated 'LEADER' must never read as MEMBER.
    ///
    /// This is the exact state prod is in: 4 rows still hold 'LEADER', the
    /// vocabulary this codebase used before the retail ranks landed, because
    /// the rewrite migration has not been applied there. If that parsed as
    /// MEMBER the guild would look leaderless, and appointing a Grand Master
    /// would find nobody to step down — leaving two.
    #[test]
    fn the_pre_retail_rank_is_not_silently_a_member() {
        assert_eq!(GuildRank::from_wire("LEADER"), None, "'LEADER' is not a retail rank");
        assert_eq!(GuildRank::from_wire("OFFICER"), None, "'OFFICER' is not a retail rank");
        // The four that ARE retail.
        for (wire, rank) in [
            ("GRANDMASTER", GuildRank::Grandmaster),
            ("MASTER", GuildRank::Master),
            ("ELDER", GuildRank::Elder),
            ("MEMBER", GuildRank::Member),
        ] {
            assert_eq!(GuildRank::from_wire(wire), Some(rank));
        }
    }

    /// With the sitting holder's rank unreadable, planning must NOT conclude
    /// the guild is leaderless. Guarded at the handler (which refuses), this
    /// pins why: the plan would otherwise demote nobody and create a second
    /// Grand Master.
    #[test]
    fn a_guild_read_as_leaderless_would_create_a_second_grandmaster() {
        // What the handler would have built if 'LEADER' fell back to MEMBER.
        let misread = [slot(1, GuildRank::Member), slot(2, GuildRank::Member)];
        let change = plan_grandmaster_change(&misread, id(2)).unwrap();
        assert_eq!(
            change.demote, None,
            "this is the hazard: nobody is demoted, so the real leader keeps the rank"
        );
        // Read correctly, the sitting holder is found and steps down.
        let correct = [slot(1, GuildRank::Grandmaster), slot(2, GuildRank::Member)];
        assert_eq!(plan_grandmaster_change(&correct, id(2)).unwrap().demote, Some(id(1)));
    }

    /// `characterId` has no default: omitting it must be a 400 from serde
    /// rather than a promotion of the nil uuid.
    #[test]
    fn a_missing_character_id_is_rejected_not_defaulted() {
        assert!(
            serde_json::from_str::<SetGrandmasterRequest>(r#"{"apply":true}"#).is_err(),
            "a body with no characterId must be rejected"
        );
    }
}
