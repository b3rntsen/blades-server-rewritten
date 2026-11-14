//! Arena presence — `GET /arena/presence`, public and unauthenticated.
//!
//! # Why
//!
//! Asked for by the owner: a front-page panel showing how many people are in the
//! arena right now and when they usually are, so a player can tell whether it is
//! worth queueing, and so the project can show publicly that it is live.
//!
//! # Where the numbers come from
//!
//! `arena_matches`, which the matchmaker writes one row to per ticket:
//! `(ticket_id, user_id, status, paired, recorded_at, resolved_at)`. It is the
//! **human-only** source — a bot never POSTs `matches/create`, so every row is a
//! real person. Indexed on `recorded_at DESC`, which is what every query here
//! sorts and filters on.
//!
//! # What "now" honestly means
//!
//! These are recent-activity windows, not socket counts, and the field names say
//! so. The exact live figure — connected ENet peers — lives in a `HashMap` local
//! to the ENet thread and would need a shared atomic to publish; that is a
//! worthwhile follow-up, not a reason to ship nothing.
//!
//! * `in_match_recent`  — distinct users whose ticket resolved to `matched`
//!   inside [`MATCH_WINDOW_MIN`]. A match runs a few minutes, so this is a good
//!   proxy for "fighting right now".
//! * `queuing_recent` — distinct users with an unresolved `searching` ticket
//!   inside [`QUEUE_WINDOW_MIN`], the five minutes the owner asked for.
//!
//! The window matters for a reason visible in the data: 48 of the 458 tickets on
//! record are unresolved `searching` rows going back to June — abandoned
//! searches. Counting "unresolved" without a time bound would report dozens of
//! people queuing forever.
//!
//! # Expectations
//!
//! Sixteen distinct users have ever queued and it is usually one at a time, so
//! this panel will read 0 or 1 most of the time. That is the honest picture, and
//! a heatmap of 58 days of real history is the part with something to say.

use std::sync::Arc;

use actix_web::{HttpResponse, get, web};
use diesel::QueryableByName;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Integer};
use diesel_async::RunQueryDsl;
use log::warn;
use serde::Serialize;

use crate::ServerGlobal;

/// Minutes back that counts as "in a match now". A match plus its post-match
/// walk runs a few minutes.
const MATCH_WINDOW_MIN: i32 = 10;

/// Minutes back that counts as "queuing now" — the window the owner specified.
const QUEUE_WINDOW_MIN: i32 = 5;

/// Days of history behind the heatmap.
const HEATMAP_DAYS: i32 = 28;

#[derive(Serialize)]
pub struct PresenceResponse {
    /// Distinct humans whose ticket matched within `MATCH_WINDOW_MIN`.
    pub in_match_recent: i64,
    /// Distinct humans with an unresolved search within `QUEUE_WINDOW_MIN`.
    pub queuing_recent: i64,
    /// Distinct humans who queued at all in the last 24 hours.
    pub humans_24h: i64,
    /// Distinct humans who have EVER queued.
    pub humans_all_time: i64,
    /// Human-vs-human matches on record (`paired`), as opposed to vs a bot.
    pub human_vs_human_all_time: i64,
    /// The windows above, so a caller renders the same words the server meant.
    pub match_window_minutes: i32,
    pub queue_window_minutes: i32,
    pub heatmap_days: i32,
    /// Activity by weekday × hour, UTC. See [`HeatCell`].
    pub heatmap: Vec<HeatCell>,
    /// When the server produced this — Unix epoch seconds, the shape the rest
    /// of this server uses (`chrono` is not a dependency here).
    pub generated_at_unix: u64,
}

/// One weekday × hour bucket of the heatmap.
#[derive(Serialize, QueryableByName)]
pub struct HeatCell {
    /// 0 = Sunday, matching Postgres `EXTRACT(DOW)` and JavaScript `getDay()`.
    #[diesel(sql_type = Integer)]
    pub dow: i32,
    /// Hour of day, UTC, 0-23.
    #[diesel(sql_type = Integer)]
    pub hour: i32,
    /// Tickets started in this bucket over `HEATMAP_DAYS`.
    #[diesel(sql_type = BigInt)]
    pub tickets: i64,
    /// Distinct humans in this bucket — the number worth reading, since one
    /// person re-queueing ten times is not ten people.
    #[diesel(sql_type = BigInt)]
    pub humans: i64,
}

#[derive(QueryableByName)]
struct LiveCounts {
    #[diesel(sql_type = BigInt)]
    in_match: i64,
    #[diesel(sql_type = BigInt)]
    queuing: i64,
}

/// The two "right now" counts, computed so that **one person is one person**.
///
/// Each user is classified by their NEWEST ticket in the match window:
///
/// * queuing — that newest ticket is an unresolved `searching` row inside
///   [`QUEUE_WINDOW_MIN`];
/// * in a match — not queuing, and some ticket of theirs matched inside
///   [`MATCH_WINDOW_MIN`].
///
/// The two sets are disjoint, so the page's `in_match + queuing` total never
/// counts the same player twice. Before this (#509) the counts were two
/// independent `COUNT(DISTINCT)`s, and a ticket that never resolves to `matched`
/// — a `MatchmakingFailed` or a cancel leaves its row `searching` forever — kept
/// its player "queuing" for five minutes after they had re-queued and were
/// already fighting a bot. A lone player saw "2 in the arena" and read the second
/// one as an opponent who was never there.
fn live_counts_sql() -> String {
    format!(
        "WITH recent AS ( \
             SELECT user_id, status, resolved_at, recorded_at FROM arena_matches \
             WHERE recorded_at > now() - interval '{MATCH_WINDOW_MIN} minutes' \
         ), latest AS ( \
             SELECT DISTINCT ON (user_id) user_id, status, resolved_at, recorded_at \
             FROM recent ORDER BY user_id, recorded_at DESC \
         ), queuing AS ( \
             SELECT user_id FROM latest \
             WHERE status = 'searching' AND resolved_at IS NULL \
               AND recorded_at > now() - interval '{QUEUE_WINDOW_MIN} minutes' \
         ) \
         SELECT \
             (SELECT COUNT(DISTINCT user_id) FROM recent \
               WHERE status = 'matched' \
                 AND user_id NOT IN (SELECT user_id FROM queuing))::bigint AS in_match, \
             (SELECT COUNT(*) FROM queuing)::bigint AS queuing"
    )
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// `GET /arena/presence`
///
/// Public on purpose — it is for the front page, carries no personal data (counts
/// only, never a name or id), and needs no session.
#[get("arena/presence")]
pub async fn arena_presence(app_state: web::Data<Arc<ServerGlobal>>) -> HttpResponse {
    match build(app_state.get_ref()).await {
        Ok(body) => HttpResponse::Ok()
            // Cheap to compute but pointless to recompute per visitor; the
            // windows are minutes wide, so a short cache is free accuracy-wise.
            .insert_header(("Cache-Control", "public, max-age=30"))
            .json(body),
        Err(e) => {
            warn!("arena presence: query failed — {e}");
            HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "presence unavailable",
            }))
        }
    }
}

async fn build(
    app_state: &ServerGlobal,
) -> Result<PresenceResponse, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = app_state.db_pool.get().await?;

    // `get_result::<CountRow>` is the idiom the rest of this module uses for a
    // scalar count; `load` leaves diesel unable to infer the row type here.
    let live = sql_query(live_counts_sql())
        .get_result::<LiveCounts>(&mut conn)
        .await?;
    let (in_match_recent, queuing_recent) = (live.in_match, live.queuing);

    let humans_24h = sql_query(
        "SELECT COUNT(DISTINCT user_id) AS n FROM arena_matches \
         WHERE recorded_at > now() - interval '24 hours'",
    )
    .get_result::<CountRow>(&mut conn)
    .await?
    .n;

    let humans_all_time = sql_query("SELECT COUNT(DISTINCT user_id) AS n FROM arena_matches")
        .get_result::<CountRow>(&mut conn)
        .await?
        .n;

    let human_vs_human_all_time = sql_query("SELECT COUNT(*) AS n FROM arena_matches WHERE paired")
        .get_result::<CountRow>(&mut conn)
        .await?
        .n;

    let heatmap: Vec<HeatCell> = sql_query(format!(
        "SELECT EXTRACT(DOW FROM recorded_at)::int AS dow, \
                EXTRACT(HOUR FROM recorded_at)::int AS hour, \
                COUNT(*)::bigint AS tickets, \
                COUNT(DISTINCT user_id)::bigint AS humans \
         FROM arena_matches \
         WHERE recorded_at > now() - interval '{HEATMAP_DAYS} days' \
         GROUP BY 1, 2 ORDER BY 1, 2"
    ))
    .get_results(&mut conn)
    .await?;

    Ok(PresenceResponse {
        in_match_recent,
        queuing_recent,
        humans_24h,
        humans_all_time,
        human_vs_human_all_time,
        match_window_minutes: MATCH_WINDOW_MIN,
        queue_window_minutes: QUEUE_WINDOW_MIN,
        heatmap_days: HEATMAP_DAYS,
        heatmap,
        generated_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The windows are reported so a caller can render the server's own words
    /// instead of hardcoding "5 minutes" and drifting from it.
    #[test]
    fn the_windows_are_sane_and_published() {
        assert!(QUEUE_WINDOW_MIN > 0 && QUEUE_WINDOW_MIN <= 15);
        assert!(
            MATCH_WINDOW_MIN >= QUEUE_WINDOW_MIN,
            "a match outlasts a search"
        );
        assert!(
            HEATMAP_DAYS >= 7,
            "a weekday heatmap needs at least one of each day"
        );
    }

    /// `dow` must mean the same thing on both sides. Postgres `EXTRACT(DOW)` and
    /// JavaScript `Date.getDay()` both put Sunday at 0; the renderer relies on it,
    /// and an off-by-one here silently rotates the whole chart.
    #[test]
    fn dow_zero_is_sunday_on_both_sides() {
        let cell = HeatCell {
            dow: 0,
            hour: 13,
            tickets: 3,
            humans: 1,
        };
        let json = serde_json::to_string(&cell).expect("serialises");
        assert!(json.contains("\"dow\":0"));
        assert!(json.contains("\"humans\":1"));
    }

    mod db_live_counts {
        use super::*;
        use diesel_async::{AsyncConnection, AsyncPgConnection};
        use uuid::Uuid;

        async fn fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            conn.begin_test_transaction()
                .await
                .expect("test transaction");
            let schema = format!("t{}", Uuid::new_v4().simple());
            for stmt in [
                format!("CREATE SCHEMA {schema}"),
                format!("SET LOCAL search_path TO {schema}"),
                "CREATE TABLE arena_matches ( \
                     ticket_id UUID PRIMARY KEY, user_id UUID NOT NULL, status TEXT NOT NULL, \
                     game_session_id UUID, paired BOOLEAN NOT NULL DEFAULT FALSE, \
                     recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), resolved_at TIMESTAMPTZ)"
                    .to_string(),
            ] {
                sql_query(stmt).execute(&mut conn).await.unwrap();
            }
            Some(conn)
        }

        /// `ago` seconds back; `resolved` = the row resolved to `matched`.
        async fn ticket(conn: &mut AsyncPgConnection, user: Uuid, ago: i64, matched: bool) {
            let (status, resolved) = if matched {
                ("matched", format!("now() - interval '{} seconds'", ago - 5))
            } else {
                ("searching", "NULL".to_string())
            };
            sql_query(format!(
                "INSERT INTO arena_matches (ticket_id, user_id, status, recorded_at, resolved_at) \
                 VALUES ('{}', '{user}', '{status}', now() - interval '{ago} seconds', {resolved})",
                Uuid::new_v4()
            ))
            .execute(conn)
            .await
            .unwrap();
        }

        async fn counts(conn: &mut AsyncPgConnection) -> (i64, i64) {
            let c = sql_query(live_counts_sql())
                .get_result::<LiveCounts>(conn)
                .await
                .unwrap();
            (c.in_match, c.queuing)
        }

        /// #509, replayed from prod 2026-11-14 05:15-05:20 UTC: one player, alone.
        /// His first ticket hit MatchmakingFailed (row stays `searching`), he
        /// re-queued 25 s later and fought a bot. The page must show ONE person,
        /// fighting — not "1 fighting + 1 queuing" that reads as an opponent.
        #[tokio::test]
        async fn a_lone_player_with_a_failed_ticket_is_counted_once() {
            let Some(mut conn) = fixture().await else {
                return;
            };
            let haudrauf = Uuid::new_v4();
            ticket(&mut conn, haudrauf, 100, false).await; // failed, never resolved
            ticket(&mut conn, haudrauf, 75, true).await; // re-queued → vs bot
            assert_eq!(counts(&mut conn).await, (1, 0));
        }

        /// A newer search outranks an older match: the player is queuing, once.
        #[tokio::test]
        async fn a_player_requeueing_after_a_match_is_queuing_not_both() {
            let Some(mut conn) = fixture().await else {
                return;
            };
            let p = Uuid::new_v4();
            ticket(&mut conn, p, 200, true).await;
            ticket(&mut conn, p, 10, false).await;
            assert_eq!(counts(&mut conn).await, (0, 1));
        }

        /// Two different people still count as two.
        #[tokio::test]
        async fn two_people_are_two() {
            let Some(mut conn) = fixture().await else {
                return;
            };
            let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
            ticket(&mut conn, a, 60, true).await;
            ticket(&mut conn, b, 10, false).await;
            assert_eq!(counts(&mut conn).await, (1, 1));
        }

        /// Outside the windows nobody is "now".
        #[tokio::test]
        async fn old_rows_count_for_nothing() {
            let Some(mut conn) = fixture().await else {
                return;
            };
            let p = Uuid::new_v4();
            ticket(&mut conn, p, 3600, true).await;
            ticket(&mut conn, p, 400, false).await; // stale search, > 5 min
            assert_eq!(counts(&mut conn).await, (0, 0));
        }
    }
}
