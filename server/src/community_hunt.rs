//! Community hunt counters — owner decision d48 (tracker #498), step 1 only.
//!
//! A "Spirit of the Hunt" style community event: while an event window is open,
//! every town job a player completes is counted, by where it was done (the job
//! board's kit: cave, forest, stone dungeon, Ayleid ruin, arena). The web shows
//! the community's totals, the top contributors and the player's own count.
//!
//! **Nothing here pays anything.** The reward table is a later decision; these
//! rows are counters only.
//!
//! **Off by default.** A completion is recorded only while a `community_events`
//! row has `starts_at <= now < ends_at`. The migration creates the table empty.
//!
//! **Never fails a completion.** The insert runs in its own savepoint and any
//! error is logged and dropped: a broken counter must not cost a player the job
//! reward that `/complete` is paying in the same transaction.

use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::{
    HttpRequest, get,
    web::{self, Json},
};
use diesel::sql_types::{BigInt, Nullable, Text, Uuid as SqlUuid};
use diesel::{QueryableByName, sql_query};
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, admin::check_import_token};

/// Record one completed job against the event open at `now`, if any.
///
/// Idempotent per (event, character, job): `/complete` already refuses to count a
/// replay, and the primary key makes a second insert a no-op besides.
pub(crate) async fn record_job_completion(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    character_id: Uuid,
    quest_id: Uuid,
    location_type: &str,
    now: i64,
) {
    let location_type = location_type.to_owned();
    let result = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            async move {
                sql_query(
                    "INSERT INTO community_hunt_completions \
                       (event_id, character_id, quest_id, user_id, source_alt_uuid, \
                        location_type, completed_at) \
                     SELECT e.id, $1, $2, $3, \
                            (SELECT c.source_alt_uuid FROM characters c WHERE c.id = $1), \
                            $4, to_timestamp($5) \
                       FROM community_events e \
                      WHERE e.starts_at <= to_timestamp($5) AND e.ends_at > to_timestamp($5) \
                      ORDER BY e.starts_at DESC \
                      LIMIT 1 \
                     ON CONFLICT DO NOTHING",
                )
                .bind::<SqlUuid, _>(character_id)
                .bind::<SqlUuid, _>(quest_id)
                .bind::<SqlUuid, _>(user_id)
                .bind::<Text, _>(location_type)
                .bind::<BigInt, _>(now)
                .execute(conn)
                .await
            }
            .scope_boxed()
        })
        .await;
    match result {
        Ok(n) if n > 0 => {
            log::info!("community hunt: character {character_id} completed job {quest_id}")
        }
        Ok(_) => {}
        Err(e) => log::warn!(
            "community hunt: could not record job {quest_id} for character {character_id}: {e}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Read side
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventInfo {
    pub id: String,
    pub name: String,
    /// Epoch seconds.
    pub starts_at: i64,
    pub ends_at: i64,
    /// Whether the window is open now. A closed event is still reported (the most
    /// recent one that has started) so its final standings stay visible.
    pub active: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Contributor {
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    pub total: i64,
    pub by_location: BTreeMap<String, i64>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlayerProgress {
    #[serde(flatten)]
    pub tally: Tally,
    /// One entry per alt that completed a job, most first.
    pub characters: Vec<Contributor>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HuntProgress {
    pub event: Option<EventInfo>,
    pub community: Tally,
    /// Distinct alts that completed at least one job.
    pub contributors: i64,
    pub top: Vec<Contributor>,
    /// Present only when the caller asked for a user.
    pub player: Option<PlayerProgress>,
}

#[derive(QueryableByName)]
struct EventRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = BigInt)]
    starts_at: i64,
    #[diesel(sql_type = BigInt)]
    ends_at: i64,
}

#[derive(QueryableByName)]
struct LocationRow {
    #[diesel(sql_type = Text)]
    location_type: String,
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct ContributorRow {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

const EVENT_COLUMNS: &str = "id, name, \
     extract(epoch FROM starts_at)::bigint AS starts_at, \
     extract(epoch FROM ends_at)::bigint AS ends_at";

/// The event to report: the named one, else the open one, else the most recent
/// that has started (so the last event's final standings remain visible).
async fn pick_event(
    conn: &mut AsyncPgConnection,
    event_id: Option<&str>,
    now: i64,
) -> diesel::QueryResult<Option<EventRow>> {
    let rows: Vec<EventRow> = match event_id {
        Some(id) => {
            sql_query(format!(
                "SELECT {EVENT_COLUMNS} FROM community_events WHERE id = $1"
            ))
            .bind::<Text, _>(id)
            .load(conn)
            .await?
        }
        None => {
            sql_query(format!(
                "SELECT {EVENT_COLUMNS} FROM community_events \
                  WHERE starts_at <= to_timestamp($1) \
                  ORDER BY (ends_at > to_timestamp($1)) DESC, starts_at DESC LIMIT 1"
            ))
            .bind::<BigInt, _>(now)
            .load(conn)
            .await?
        }
    };
    Ok(rows.into_iter().next())
}

async fn tally(
    conn: &mut AsyncPgConnection,
    event_id: &str,
    user_id: Option<Uuid>,
) -> diesel::QueryResult<Tally> {
    let rows: Vec<LocationRow> = sql_query(
        "SELECT location_type, count(*) AS n FROM community_hunt_completions \
          WHERE event_id = $1 AND ($2::uuid IS NULL OR user_id = $2) \
          GROUP BY location_type",
    )
    .bind::<Text, _>(event_id)
    .bind::<Nullable<SqlUuid>, _>(user_id)
    .load(conn)
    .await?;
    let mut out = Tally::default();
    for r in rows {
        out.total += r.n;
        out.by_location.insert(r.location_type, r.n);
    }
    Ok(out)
}

/// Per-alt counts, most first (ties: whoever got there first). An alt is its
/// `source_alt_uuid`, or the character row when that is unknown; its name is the
/// one its newest completing character row carries now.
async fn per_alt(
    conn: &mut AsyncPgConnection,
    event_id: &str,
    user_id: Option<Uuid>,
    limit: i64,
) -> diesel::QueryResult<Vec<Contributor>> {
    let rows: Vec<ContributorRow> = sql_query(
        "SELECT COALESCE( \
                  (SELECT c.character->>'name' FROM characters c WHERE c.id = t.last_char), \
                  (SELECT c.character->>'name' FROM characters c \
                    WHERE c.user_id = t.user_id AND c.source_alt_uuid = t.alt LIMIT 1), \
                  '') AS name, \
                t.n \
           FROM (SELECT user_id, COALESCE(source_alt_uuid, character_id) AS alt, \
                        count(*) AS n, min(completed_at) AS first_at, \
                        (array_agg(character_id ORDER BY completed_at DESC))[1] AS last_char \
                   FROM community_hunt_completions \
                  WHERE event_id = $1 AND ($2::uuid IS NULL OR user_id = $2) \
                  GROUP BY 1, 2) t \
          ORDER BY t.n DESC, t.first_at ASC \
          LIMIT $3",
    )
    .bind::<Text, _>(event_id)
    .bind::<Nullable<SqlUuid>, _>(user_id)
    .bind::<BigInt, _>(limit)
    .load(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Contributor {
            name: r.name,
            count: r.n,
        })
        .collect())
}

pub(crate) async fn progress(
    conn: &mut AsyncPgConnection,
    event_id: Option<&str>,
    user_id: Option<Uuid>,
    limit: i64,
    now: i64,
) -> diesel::QueryResult<HuntProgress> {
    let Some(event) = pick_event(conn, event_id, now).await? else {
        return Ok(HuntProgress {
            event: None,
            community: Tally::default(),
            contributors: 0,
            top: Vec::new(),
            player: user_id.map(|_| PlayerProgress {
                tally: Tally::default(),
                characters: Vec::new(),
            }),
        });
    };
    let community = tally(conn, &event.id, None).await?;
    let contributors = sql_query(
        "SELECT count(DISTINCT (user_id, COALESCE(source_alt_uuid, character_id)))::bigint AS n \
           FROM community_hunt_completions WHERE event_id = $1",
    )
    .bind::<Text, _>(&event.id)
    .get_result::<CountRow>(conn)
    .await?
    .n;
    let top = per_alt(conn, &event.id, None, limit).await?;
    let player = match user_id {
        Some(u) => Some(PlayerProgress {
            tally: tally(conn, &event.id, Some(u)).await?,
            characters: per_alt(conn, &event.id, Some(u), 100).await?,
        }),
        None => None,
    };
    Ok(HuntProgress {
        event: Some(EventInfo {
            active: event.starts_at <= now && now < event.ends_at,
            id: event.id,
            name: event.name,
            starts_at: event.starts_at,
            ends_at: event.ends_at,
        }),
        community,
        contributors,
        top,
        player,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressQuery {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /dev/v1/community-hunt/progress[?userId=&eventId=&limit=]` — read-only,
/// `X-Import-Token` gated like every dev route. Totals per location type, per
/// player (when `userId` is given) and community-wide, plus the top contributors
/// by character name. Counts only: no user ids leave in `top`.
#[get("/blades.bgs.services/api/dev/v1/community-hunt/progress")]
pub async fn get_community_hunt_progress(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<ProgressQuery>,
) -> Result<Json<HuntProgress>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let q = query.into_inner();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let mut conn = app_state.db_pool.get().await.unwrap();
    let now = chrono::Utc::now().timestamp();
    Ok(Json(
        progress(&mut conn, q.event_id.as_deref(), q.user_id, limit, now).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quest::jobs_gen;
    use crate::quest::report353_quest_town_xp_db as fx;
    use diesel_async::SimpleAsyncConnection;
    use serde_json::{Value, json};

    const CAVE: &str = "18e81559-3561-47ef-b73e-9f3bc34ba0b8";
    const FOREST: &str = "57d639c2-ec4c-4e6b-9995-ff6a7ef3e712";
    const STONE: &str = "4d3153a0-cfc5-405c-b065-92547ee9fbbc";
    const AYLEID: &str = "86e6a720-caa4-4b13-b8b8-73f3dcf049a2";
    const ARENA: &str = "e7418cc7-01de-4c84-ba00-e221f8783d51";

    fn uuid(s: &str) -> Option<Uuid> {
        Some(Uuid::parse_str(s).unwrap())
    }

    #[test]
    fn location_type_is_the_job_boards_kit() {
        assert_eq!(jobs_gen::location_type(uuid(CAVE)), "cave");
        assert_eq!(jobs_gen::location_type(uuid(FOREST)), "forest");
        assert_eq!(jobs_gen::location_type(uuid(STONE)), "dungeon");
        assert_eq!(jobs_gen::location_type(uuid(AYLEID)), "ayleid");
        assert_eq!(
            jobs_gen::location_type(uuid(ARENA)),
            "arena",
            "a Duel's arena"
        );
        // Control: what the board does not know is not guessed.
        assert_eq!(jobs_gen::location_type(None), "unknown");
        assert_eq!(jobs_gen::location_type(Some(Uuid::nil())), "unknown");
    }

    /// The stored job row must keep where it was rolled: `/complete` reads it back.
    #[test]
    fn a_stored_job_row_keeps_its_dungeon_template() {
        let job = json!({
            "questId": "11111111-2222-4333-8444-555555555555",
            "version": 1, "difficultyLevel": 10, "seed": 7,
            "jobSetup": {"jobType": 0, "dungeonTemplateId": FOREST},
        });
        let entry = jobs_gen::job_quest_db_entry(
            &job,
            Uuid::new_v4(),
            &fx::world().gd,
            &Default::default(),
            0,
        )
        .expect("a row");
        assert_eq!(entry.info.0.job_dungeon_template_id, uuid(FOREST));
        let wire = serde_json::to_value(&entry.info.0).unwrap();
        assert_eq!(
            wire["jobDungeonTemplateId"],
            json!(FOREST),
            "persisted in info"
        );
    }

    const MIGRATION: &str =
        include_str!("../../migrations/2026-11-13-000000-0000_community_hunt/up.sql");

    macro_rules! db {
        () => {
            match fx::fixture().await {
                Some(mut c) => {
                    c.batch_execute(MIGRATION).await.unwrap();
                    c
                }
                None => {
                    eprintln!("SKIP: TEST_DATABASE_URL unset — community hunt NOT verified");
                    return;
                }
            }
        };
    }

    fn job_row(template: &str) -> Value {
        let mut row = fx::quest_row("JOB", jobs_gen::JOB_SENTINEL_GLD);
        row["jobReward"] = json!({"characterXp": 10});
        row["jobDungeonTemplateId"] = json!(template);
        row
    }

    async fn open_event(conn: &mut AsyncPgConnection, id: &str, from: i64, to: i64) {
        sql_query(
            "INSERT INTO community_events (id, name, starts_at, ends_at) \
             VALUES ($1, 'Spirit of the Hunt', to_timestamp($2), to_timestamp($3))",
        )
        .bind::<Text, _>(id)
        .bind::<BigInt, _>(from)
        .bind::<BigInt, _>(to)
        .execute(conn)
        .await
        .unwrap();
    }

    async fn rename(conn: &mut AsyncPgConnection, character: Uuid, name: &str) {
        sql_query("UPDATE characters SET character = jsonb_set(character, '{name}', to_jsonb($2::text)) WHERE id = $1")
            .bind::<SqlUuid, _>(character)
            .bind::<Text, _>(name)
            .execute(conn)
            .await
            .unwrap();
    }

    async fn completions(conn: &mut AsyncPgConnection) -> i64 {
        sql_query("SELECT count(*) AS n FROM community_hunt_completions")
            .get_result::<CountRow>(conn)
            .await
            .unwrap()
            .n
    }

    /// Off by default: with no event configured a completed job counts nowhere,
    /// and the job is still paid.
    #[tokio::test]
    async fn no_event_window_records_nothing() {
        let mut conn = db!();
        let ids = fx::seed(&mut conn, job_row(CAVE)).await;
        let out = fx::complete(&mut conn, ids).await;
        assert_eq!(
            out["reward"]["characterXp"],
            json!(10),
            "the job still pays"
        );
        assert_eq!(completions(&mut conn).await, 0);
        let p = progress(&mut conn, None, Some(ids.0), 20, fx::NOW)
            .await
            .unwrap();
        assert_eq!(p.event, None);
    }

    /// A window that closed before, or opens after, `now` counts nothing either.
    #[tokio::test]
    async fn a_closed_window_records_nothing() {
        let mut conn = db!();
        open_event(&mut conn, "past", fx::NOW - 200, fx::NOW - 100).await;
        open_event(&mut conn, "future", fx::NOW + 100, fx::NOW + 200).await;
        let ids = fx::seed(&mut conn, job_row(CAVE)).await;
        fx::complete(&mut conn, ids).await;
        assert_eq!(completions(&mut conn).await, 0);
    }

    /// Inside the window: counted once (a replay is not a second job), by the
    /// board's location, and reported per player and community-wide.
    #[tokio::test]
    async fn a_job_in_the_window_is_counted_once_by_location() {
        let mut conn = db!();
        open_event(&mut conn, "hunt-1", fx::NOW - 100, fx::NOW + 100).await;

        let a1 = fx::seed(&mut conn, job_row(CAVE)).await;
        rename(&mut conn, a1.1, "Arwald").await;
        fx::complete(&mut conn, a1).await;
        fx::complete(&mut conn, a1).await; // replay
        // Same player, same character, a second job in a forest.
        let quest2 = Uuid::new_v4();
        sql_query(
            "INSERT INTO quests (id, character_id, info, generated_data) \
             VALUES ($1, $2, $3::jsonb, 'null'::jsonb)",
        )
        .bind::<SqlUuid, _>(quest2)
        .bind::<SqlUuid, _>(a1.1)
        .bind::<Text, _>(job_row(FOREST).to_string())
        .execute(&mut conn)
        .await
        .unwrap();
        fx::complete(&mut conn, (a1.0, a1.1, quest2)).await;

        let b = fx::seed(&mut conn, job_row(ARENA)).await;
        rename(&mut conn, b.1, "HauDrauf").await;
        fx::complete(&mut conn, b).await;

        // Control: a story quest is not a job and is not counted.
        let story = fx::seed(&mut conn, fx::quest_row("NORMAL", Uuid::new_v4())).await;
        fx::complete(&mut conn, story).await;

        assert_eq!(completions(&mut conn).await, 3);
        let p = progress(&mut conn, None, Some(a1.0), 20, fx::NOW)
            .await
            .unwrap();
        let event = p.event.expect("the open event");
        assert_eq!((event.id.as_str(), event.active), ("hunt-1", true));
        assert_eq!(p.community.total, 3);
        assert_eq!(
            p.community.by_location,
            BTreeMap::from([
                ("arena".into(), 1),
                ("cave".into(), 1),
                ("forest".into(), 1)
            ])
        );
        assert_eq!(p.contributors, 2);
        assert_eq!(
            p.top,
            vec![
                Contributor {
                    name: "Arwald".into(),
                    count: 2
                },
                Contributor {
                    name: "HauDrauf".into(),
                    count: 1
                },
            ]
        );
        let me = p.player.expect("asked for a user");
        assert_eq!(me.tally.total, 2);
        assert_eq!(
            me.tally.by_location,
            BTreeMap::from([("cave".into(), 1), ("forest".into(), 1)])
        );
        assert_eq!(
            me.characters,
            vec![Contributor {
                name: "Arwald".into(),
                count: 2
            }]
        );

        // After the window closes the final standings are still reported.
        let after = progress(&mut conn, None, None, 1, fx::NOW + 500)
            .await
            .unwrap();
        let ev = after.event.expect("last event");
        assert!(!ev.active);
        assert_eq!(after.community.total, 3);
        assert_eq!(after.top.len(), 1, "limit applies");
    }

    /// A job row stored before the template was kept still counts, as `unknown`.
    #[tokio::test]
    async fn a_job_row_without_a_template_counts_as_unknown() {
        let mut conn = db!();
        open_event(&mut conn, "hunt-1", fx::NOW - 100, fx::NOW + 100).await;
        let mut row = job_row(CAVE);
        row.as_object_mut().unwrap().remove("jobDungeonTemplateId");
        let ids = fx::seed(&mut conn, row).await;
        fx::complete(&mut conn, ids).await;
        let p = progress(&mut conn, None, None, 20, fx::NOW).await.unwrap();
        assert_eq!(
            p.community.by_location,
            BTreeMap::from([("unknown".into(), 1)])
        );
    }
}
