//! Every arena match of one calendar month (UTC), humans and AI, for the
//! website's "Monthly points" Top 100 (tracker #515, owner decision d53).
//!
//! HauDrauf's rule — human win +30 / loss +10, AI win +5 / loss +2, a daily
//! cap of 120 × day-of-month until 2500, reset each month — is scored on the
//! website (`web/lib/top100-monthly.ts`), so the owner can evaluate it beside
//! Elo without an arena change per tweak. The arena only hands over the facts:
//! one row per player per finished match from `arena_match_results` (the
//! durable audit the victory card writes), in `recorded_at, id` order, keyed on
//! the alt exactly as the h2h board keys it. Read-only; nothing in-game moves.

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    web::{self, Json},
};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

use crate::{
    ServerGlobal,
    admin::{IMPORT_SERVICE_ID, check_import_token},
    arena::top100_boards::{cached, store},
    error::BladeApiError,
};

#[derive(Deserialize)]
pub struct MonthQuery {
    #[serde(default)]
    pub month: Option<String>,
}

/// `[start, end)` of the month `value` names — `current` (or absent) for the
/// month containing `now`, else `YYYY-MM` — and its `YYYY-MM` label.
pub fn month_bounds(
    value: Option<&str>,
    now: DateTime<Utc>,
) -> Option<(String, DateTime<Utc>, DateTime<Utc>)> {
    let (year, month) = match value {
        None | Some("") | Some("current") => (now.year(), now.month()),
        Some(v) => {
            let (y, m) = v.split_once('-')?;
            if y.len() != 4 || m.len() != 2 {
                return None;
            }
            let (y, m) = (y.parse::<i32>().ok()?, m.parse::<u32>().ok()?);
            if !(2020..=2100).contains(&y) || !(1..=12).contains(&m) {
                return None;
            }
            (y, m)
        }
    };
    let start = NaiveDate::from_ymd_opt(year, month, 1)?;
    let end = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    Some((
        format!("{year:04}-{month:02}"),
        Utc.from_utc_datetime(&start.and_hms_opt(0, 0, 0)?),
        Utc.from_utc_datetime(&end.and_hms_opt(0, 0, 0)?),
    ))
}

/// One player-side of one finished match. The alt key and the name follow the
/// h2h board (`ranking::H2H_BOARD_ENTRIES_SQL`): the live character's name when
/// the row is its live alt, else the newest saved version of that alt.
pub(crate) const MONTHLY_MATCHES_SQL: &str = "\
    WITH rows AS ( \
         SELECT r.id, r.recorded_at, r.character_id, r.is_h2h, r.win, \
                COALESCE(r.source_alt_uuid, c.source_alt_uuid, r.character_id) AS alt_key, \
                COALESCE(c.source_alt_uuid, c.id) AS live_alt, \
                c.character ->> 'name' AS live_name \
         FROM arena_match_results r \
         INNER JOIN characters c ON c.id = r.character_id \
         WHERE r.recorded_at >= $1 AND r.recorded_at < $2 \
     ) \
     SELECT r.recorded_at, r.character_id, r.alt_key, r.is_h2h, r.win, \
            COALESCE(CASE WHEN r.alt_key = r.live_alt THEN r.live_name ELSE ( \
                SELECT v.character ->> 'name' FROM character_versions v \
                 WHERE v.character_id = r.character_id AND v.source_alt_uuid = r.alt_key \
                 ORDER BY v.saved_at DESC LIMIT 1) END, '') AS name \
     FROM rows r \
     WHERE NOT EXISTS (SELECT 1 FROM test_characters tc WHERE tc.alt_uuid = r.alt_key) \
     ORDER BY r.recorded_at, r.id";

#[derive(Debug, Clone, PartialEq, diesel::QueryableByName)]
pub struct MonthlyMatchRow {
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    pub recorded_at: DateTime<Utc>,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub character_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub alt_key: Uuid,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    pub is_h2h: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    pub win: bool,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub name: String,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct MonthlyMatchesResponse {
    pub month: String,
    pub starts_at: i64,
    pub ends_at: i64,
    /// `[character_id, alt_key, name]`, each alt once.
    pub players: Vec<(Uuid, Uuid, String)>,
    /// `[at (unix s), player index, human opponent (0/1), win (0/1)]`, in match order.
    pub matches: Vec<[i64; 4]>,
}

/// Compact `rows` (already in match order): players once, matches by index.
pub fn monthly_payload(
    month: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    rows: Vec<MonthlyMatchRow>,
) -> MonthlyMatchesResponse {
    let mut index: HashMap<(Uuid, Uuid), usize> = HashMap::new();
    let mut players: Vec<(Uuid, Uuid, String)> = Vec::new();
    let mut matches = Vec::with_capacity(rows.len());
    for r in rows {
        let i = *index.entry((r.character_id, r.alt_key)).or_insert_with(|| {
            players.push((r.character_id, r.alt_key, String::new()));
            players.len() - 1
        });
        // The newest non-empty name wins (a rename mid-month shows the new one).
        if !r.name.is_empty() {
            players[i].2 = r.name;
        }
        matches.push([
            r.recorded_at.timestamp(),
            i as i64,
            i64::from(r.is_h2h),
            i64::from(r.win),
        ]);
    }
    MonthlyMatchesResponse {
        month,
        starts_at: start.timestamp(),
        ends_at: end.timestamp(),
        players,
        matches,
    }
}

pub async fn load_monthly_rows(
    conn: &mut diesel_async::AsyncPgConnection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<MonthlyMatchRow>, diesel::result::Error> {
    diesel::sql_query(MONTHLY_MATCHES_SQL)
        .bind::<diesel::sql_types::Timestamptz, _>(start)
        .bind::<diesel::sql_types::Timestamptz, _>(end)
        .get_results(conn)
        .await
}

/// `GET /api/dev/v1/arena-ranking/monthly-matches?month=current|YYYY-MM`.
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/monthly-matches")]
pub async fn get_arena_monthly_matches(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<MonthQuery>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let (month, start, end) = month_bounds(query.month.as_deref(), Utc::now())
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 89))?;
    let key = format!("monthly-matches:{month}");
    if let Some(v) = cached(&key) {
        return Ok(Json(v));
    }
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let rows = load_monthly_rows(&mut conn, start, end)
        .await
        .map_err(|e| {
            log::warn!("arena monthly matches failed: {e}");
            BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 90)
        })?;
    let value = serde_json::to_value(monthly_payload(month, start, end, rows))
        .map_err(BladeApiError::generic_internal_error)?;
    store(key, &value);
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn month_bounds_cover_the_utc_calendar_month() {
        let now = at("2026-11-15T12:00:00Z");
        let (m, s, e) = month_bounds(None, now).unwrap();
        assert_eq!(m, "2026-11");
        assert_eq!(s, at("2026-11-01T00:00:00Z"));
        assert_eq!(e, at("2026-12-01T00:00:00Z"));
        assert_eq!(month_bounds(Some("current"), now).unwrap().0, "2026-11");
        let (m, s, e) = month_bounds(Some("2026-12"), now).unwrap();
        assert_eq!(
            (m.as_str(), s, e),
            (
                "2026-12",
                at("2026-12-01T00:00:00Z"),
                at("2027-01-01T00:00:00Z")
            )
        );
    }

    #[test]
    fn month_bounds_reject_anything_else() {
        let now = at("2026-11-15T12:00:00Z");
        for bad in [
            "2026-13",
            "2026-00",
            "26-11",
            "2026-1",
            "2026/11",
            "x",
            "2026-11-01",
            "1999-01",
        ] {
            assert!(month_bounds(Some(bad), now).is_none(), "{bad}");
        }
    }

    #[test]
    fn payload_names_each_alt_once_and_keeps_match_order() {
        let (c, alt2) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let d = Uuid::from_u128(3);
        let row = |t: &str, ch, alt, h2h, win, name: &str| MonthlyMatchRow {
            recorded_at: at(t),
            character_id: ch,
            alt_key: alt,
            is_h2h: h2h,
            win,
            name: name.into(),
        };
        let p = monthly_payload(
            "2026-11".into(),
            at("2026-11-01T00:00:00Z"),
            at("2026-12-01T00:00:00Z"),
            vec![
                row("2026-11-02T10:00:00Z", c, c, false, true, "Old"),
                row("2026-11-02T11:00:00Z", d, d, true, false, "Dee"),
                row("2026-11-03T10:00:00Z", c, alt2, true, true, "Alt"),
                row("2026-11-04T10:00:00Z", c, c, true, false, "New"),
            ],
        );
        assert_eq!(
            p.players,
            vec![
                (c, c, "New".into()),
                (d, d, "Dee".into()),
                (c, alt2, "Alt".into())
            ]
        );
        assert_eq!(
            p.matches[0],
            [at("2026-11-02T10:00:00Z").timestamp(), 0, 0, 1]
        );
        assert_eq!(p.matches[1][1..], [1, 1, 0]);
        assert_eq!(p.matches[2][1..], [2, 1, 1]);
        assert_eq!(p.matches[3][1..], [0, 1, 0]);
        assert_eq!(p.starts_at, at("2026-11-01T00:00:00Z").timestamp());
    }

    mod db {
        use super::*;
        use diesel_async::{AsyncConnection, AsyncPgConnection};

        async fn fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL reachable");
            conn.begin_test_transaction().await.unwrap();
            let schema = format!("t{}", Uuid::new_v4().simple());
            for stmt in [
                format!("CREATE SCHEMA {schema}"),
                format!("SET LOCAL search_path TO {schema}"),
                "CREATE TABLE arena_match_results (id UUID PRIMARY KEY, character_id UUID NOT NULL, \
                 win BOOLEAN NOT NULL, recorded_at TIMESTAMPTZ NOT NULL, \
                 is_h2h BOOLEAN NOT NULL DEFAULT false, source_alt_uuid UUID)"
                    .into(),
                "CREATE TABLE characters (id UUID PRIMARY KEY, character JSONB NOT NULL, source_alt_uuid UUID)"
                    .into(),
                "CREATE TABLE character_versions (id UUID PRIMARY KEY, character_id UUID NOT NULL, \
                 source_alt_uuid UUID, character JSONB NOT NULL, saved_at BIGINT NOT NULL)"
                    .into(),
            ] {
                diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
            }
            crate::testchar::apply_migration_for_tests(&mut conn).await;
            Some(conn)
        }

        /// The month window, match order, alt keying and naming, and the
        /// test-character exclusion, against a real Postgres.
        #[tokio::test]
        async fn loads_one_month_in_order_keyed_on_the_alt() {
            let Some(mut conn) = fixture().await else {
                return;
            };
            for stmt in [
                // Live character 1 (live alt = itself), with a saved alt 2.
                "INSERT INTO characters VALUES ('00000000-0000-0000-0000-000000000001', '{\"name\":\"Live\"}', NULL)",
                "INSERT INTO character_versions VALUES (gen_random_uuid(), '00000000-0000-0000-0000-000000000001', \
                 '00000000-0000-0000-0000-000000000002', '{\"name\":\"Parked\"}', 1)",
                // Character 3 is a test character.
                "INSERT INTO characters VALUES ('00000000-0000-0000-0000-000000000003', '{\"name\":\"Tester\"}', NULL)",
                "INSERT INTO test_characters (alt_uuid, user_id, marked_by) VALUES \
                 ('00000000-0000-0000-0000-000000000003', gen_random_uuid(), 'test')",
                "INSERT INTO arena_match_results VALUES \
                 ('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-000000000001', true, '2026-10-31T23:59:59Z', false, NULL), \
                 ('00000000-0000-0000-0000-0000000000a2', '00000000-0000-0000-0000-000000000001', true, '2026-11-02T00:00:00Z', true, NULL), \
                 ('00000000-0000-0000-0000-0000000000a3', '00000000-0000-0000-0000-000000000001', false, '2026-11-01T00:00:00Z', false, '00000000-0000-0000-0000-000000000002'), \
                 ('00000000-0000-0000-0000-0000000000a4', '00000000-0000-0000-0000-000000000003', true, '2026-11-03T00:00:00Z', false, NULL), \
                 ('00000000-0000-0000-0000-0000000000a5', '00000000-0000-0000-0000-000000000001', true, '2026-12-01T00:00:00Z', false, NULL)",
            ] {
                diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
            }
            let (_, s, e) = month_bounds(Some("2026-11"), Utc::now()).unwrap();
            let rows = load_monthly_rows(&mut conn, s, e).await.unwrap();
            let got: Vec<_> = rows
                .iter()
                .map(|r| (r.alt_key, r.is_h2h, r.win, r.name.as_str()))
                .collect();
            assert_eq!(
                got,
                vec![
                    (Uuid::from_u128(2), false, false, "Parked"),
                    (Uuid::from_u128(1), true, true, "Live"),
                ]
            );
        }
    }
}
