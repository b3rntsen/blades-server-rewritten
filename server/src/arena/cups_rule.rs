//! d54: switch the season trophy rule and recalculate the season under it.
//!
//! The owner wants HauDrauf's cups rule (tracker #515, [`CupsRule::Haudrauf2500`])
//! in game THIS season, flippable back. The rule is selected in the ranking
//! config row (`arena_ranking_config.config.cups_rule`, default `classic`), and
//! the live match path ([`ranking::trophy_delta`]) pays what it selects from the
//! moment the config is saved. Matches already played need this recalculation.
//!
//! # Where season trophies live, and what the recalculation does to each
//!
//! | place | read by | recalculation |
//! | --- | --- | --- |
//! | `arena_match_results.trophy_delta` | season close (`season_store::FREEZE_STANDINGS_SQL` → standings, awards, guild ladder), `guilds.trophies` recompute, website monthly feed | rewritten to the selected rule's value |
//! | `arena_match_results.classic_trophy_delta` | this module, the website's season feed (#515) | set once to the original value, never changed again |
//! | `characters.character.pvpTrophies` (live alt) | in-game season board (`leaderboards::RANKED_CTE`), the next live match's "before" total | set to the replayed total |
//! | `character_versions.character.pvpTrophies` (newest version of a parked alt) | in-game season board, restored on alt switch | set to the replayed total |
//! | `guilds.trophies` | in-game guild board | `guild::recompute_guild_trophies` |
//!
//! Left alone on purpose:
//! * `trophies_after` — the counter as it stood when the match was played;
//!   nothing reads it back (the season close replays deltas from zero).
//! * `matchmakingPvpTrophies` / `highestArena*` — the season high-water mark the
//!   ladder promotes on and the MATCHMAKER pairs on. Promotion chests were paid
//!   off it; lowering it would invite a second payout, raising it would skip
//!   one. Under the new rule the live path still raises it as usual.
//! * `arena_season_standings` / awards — written only when a season closes, so
//!   they read the rewritten deltas automatically. A closed season is never
//!   recalculated (the action runs on the ACTIVE season only).
//!
//! # Lossless in both directions
//!
//! A replay derives every match from its classic value (`classic_trophy_delta`,
//! or `trophy_delta` on a row never rewritten), so `classic` → `haudrauf_2500` →
//! `classic` restores the stored numbers exactly. Classic replay from zero equals
//! the live counters (proven on prod data: all 39 alts of the 2026-11 season,
//! `testdata/d54_season_*.csv`); the dry run reports any alt where it would not,
//! before anything is written.
//!
//! The replay is the season close's own: per `(character, alt)`, chronological
//! by `(recorded_at, id)`, clamped at zero after every match.

use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::{HttpRequest, post, web};
use diesel::QueryableByName;
use diesel::sql_types::{Array, BigInt, Bool, Integer, Nullable, Uuid as SqlUuid};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ServerGlobal;
use crate::arena::arena_ladder::MatchOutcome;
use crate::arena::ranking::{self, CupsRule, RankingConfig};
use crate::arena::season_store::SeasonRow;
use crate::error::BladeApiError;

/// Serialises recalculations against each other (one at a time is the only
/// sane number for a rewrite of the season).
const RECALC_ADVISORY_LOCK_ID: i64 = 7_202_411_150_054;

/// What one match pays under `rule`, from its classic value.
///
/// A voided AI match was stored as 0 and stays 0 under every rule.
pub fn replay_delta(
    config: &RankingConfig,
    rule: CupsRule,
    classic: i64,
    h2h: bool,
    outcome: MatchOutcome,
    before: i64,
) -> i64 {
    match rule {
        CupsRule::Classic => classic,
        CupsRule::Haudrauf2500 => {
            if !h2h && classic == 0 {
                return 0;
            }
            let (base, award) = ranking::split_stored_classic(config, classic, h2h, outcome);
            ranking::haudrauf_delta(base, award, h2h, outcome.win, before)
        }
    }
}

/// One stored match, as the replay needs it.
#[derive(Debug, Clone, Copy)]
pub struct ReplayInput<K> {
    pub key: K,
    pub h2h: bool,
    pub outcome: MatchOutcome,
    /// `COALESCE(classic_trophy_delta, trophy_delta)`.
    pub classic: i64,
}

/// Replay `rows` (already in match order) under `rule`: the per-row delta in
/// input order, and each key's final total (clamped at zero after every match).
pub fn replay<K: Ord + Copy>(
    config: &RankingConfig,
    rule: CupsRule,
    rows: &[ReplayInput<K>],
) -> (Vec<i64>, BTreeMap<K, i64>) {
    let mut totals: BTreeMap<K, i64> = BTreeMap::new();
    let mut deltas = Vec::with_capacity(rows.len());
    for r in rows {
        let total = totals.entry(r.key).or_insert(0);
        let d = replay_delta(config, rule, r.classic, r.h2h, r.outcome, *total);
        *total = (*total + d).max(0);
        deltas.push(d);
    }
    (deltas, totals)
}

/// Whose ladder a match counts on: the character row plus the alt that played
/// (NULL alt = the alt live now), exactly as `FREEZE_STANDINGS_SQL` keys it.
pub type AltKey = (Uuid, Option<Uuid>);

#[derive(QueryableByName)]
struct MatchRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    alt: Option<Uuid>,
    #[diesel(sql_type = Bool)]
    is_h2h: bool,
    #[diesel(sql_type = Bool)]
    win: bool,
    #[diesel(sql_type = Integer)]
    rounds_won: i32,
    #[diesel(sql_type = Integer)]
    rounds_lost: i32,
    #[diesel(sql_type = BigInt)]
    trophy_delta: i64,
    #[diesel(sql_type = BigInt)]
    classic: i64,
}

/// The season's matches, keyed and ordered as the season close replays them.
const SEASON_ROWS_SQL: &str = "\
    SELECT r.id, r.character_id, COALESCE(r.source_alt_uuid, c.source_alt_uuid) AS alt, \
           r.is_h2h, r.win, r.rounds_won, r.rounds_lost, r.trophy_delta, \
           COALESCE(r.classic_trophy_delta, r.trophy_delta) AS classic \
    FROM arena_match_results r \
    JOIN characters c ON c.id = r.character_id \
    WHERE r.recorded_at >= to_timestamp($1) AND r.recorded_at < to_timestamp($2) \
    ORDER BY r.recorded_at, r.id";

/// A season counter that currently belongs to this season: the in-game board's
/// own test (`leaderboards::RANKED_CTE`).
const IN_SEASON: &str = "(x.character->>'pvpSeasonId' IS NULL \
     OR x.character->>'pvpSeasonId' = '00000000-0000-0000-0000-000000000000' \
     OR x.character->>'pvpSeasonId' = $3::text)";

#[derive(QueryableByName)]
struct CounterRow {
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    alt: Option<Uuid>,
    /// `characters.id` for the live row, `character_versions.id` for a parked alt.
    #[diesel(sql_type = SqlUuid)]
    row_id: Uuid,
    #[diesel(sql_type = Bool)]
    parked: bool,
    #[diesel(sql_type = BigInt)]
    trophies: i64,
}

/// Every in-season counter of the alts in `$1/$2` (character ids / alts, NULL
/// alt as the nil uuid): the live row when that alt is live, else the newest
/// version of the alt — the same row the in-game board reads.
fn counters_sql() -> String {
    format!(
        "WITH k AS ( \
             SELECT * FROM UNNEST($1::uuid[], $2::uuid[]) AS k(character_id, alt) \
         ) \
         SELECT x.id AS character_id, x.source_alt_uuid AS alt, x.id AS row_id, false AS parked, \
                COALESCE((x.character->>'pvpTrophies')::bigint, 0) AS trophies \
         FROM characters x JOIN k ON k.character_id = x.id \
              AND COALESCE(x.source_alt_uuid, '00000000-0000-0000-0000-000000000000'::uuid) = k.alt \
         WHERE {IN_SEASON} \
         UNION ALL \
         SELECT k.character_id, v.source_alt_uuid AS alt, v.id AS row_id, true AS parked, \
                COALESCE((v.character->>'pvpTrophies')::bigint, 0) AS trophies \
         FROM k \
         JOIN characters live ON live.id = k.character_id \
         JOIN LATERAL ( \
             SELECT cv.id, cv.source_alt_uuid, cv.character FROM character_versions cv \
             WHERE cv.user_id = live.user_id AND cv.source_alt_uuid = k.alt \
             ORDER BY cv.saved_at DESC LIMIT 1 \
         ) v ON true \
         CROSS JOIN LATERAL (SELECT v.character) AS x(character) \
         WHERE live.source_alt_uuid IS DISTINCT FROM k.alt AND {IN_SEASON}"
    )
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CounterChange {
    pub character_id: Uuid,
    pub alt: Option<Uuid>,
    /// `true` = a parked alt's newest version, `false` = the live row.
    pub parked: bool,
    pub before: i64,
    pub after: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecalcReport {
    pub rule: CupsRule,
    pub dry_run: bool,
    pub season_id: Uuid,
    pub season_name: String,
    pub matches: usize,
    /// Match rows whose `trophy_delta` changes (or would).
    pub rows_rewritten: usize,
    pub alts: usize,
    /// Counters that change (or would), best first by new total.
    pub counters_changed: Vec<CounterChange>,
    /// Alts whose counter disagrees with the CLASSIC replay of their matches —
    /// a balance the season did not earn (an import carrying cups). Expected
    /// empty; under `classic` these are exactly the counters that would move.
    pub classic_mismatches: usize,
    pub guilds_recomputed: usize,
}

#[derive(Debug)]
pub enum RecalcError {
    NoActiveSeason,
    Db(diesel::result::Error),
}

impl From<diesel::result::Error> for RecalcError {
    fn from(e: diesel::result::Error) -> Self {
        Self::Db(e)
    }
}

/// Recalculate the ACTIVE season under `config.cups_rule`. Idempotent: a second
/// run with the same rule changes nothing. `dry_run` computes the same report
/// and writes nothing.
pub async fn recalculate(
    conn: &mut AsyncPgConnection,
    config: &RankingConfig,
    dry_run: bool,
) -> Result<RecalcReport, RecalcError> {
    let config = config.clone();
    conn.transaction::<_, RecalcError, _>(|conn| {
        async move {
            diesel::sql_query("SELECT pg_advisory_xact_lock($1)")
                .bind::<BigInt, _>(RECALC_ADVISORY_LOCK_ID)
                .execute(conn)
                .await?;
            // Hold off new match results until the rewrite commits, so no result
            // lands between reading the season and writing its counters. The
            // persistence queue waits; nothing is lost.
            if !dry_run {
                diesel::sql_query("LOCK TABLE arena_match_results IN SHARE ROW EXCLUSIVE MODE")
                    .execute(conn)
                    .await?;
            }
            let season = active_season(conn)
                .await?
                .ok_or(RecalcError::NoActiveSeason)?;
            recalculate_season(conn, &config, &season, dry_run).await
        }
        .scope_boxed()
    })
    .await
}

async fn active_season(conn: &mut AsyncPgConnection) -> Result<Option<SeasonRow>, RecalcError> {
    use crate::schema::arena_seasons;
    use diesel::{ExpressionMethods, OptionalExtension, QueryDsl, SelectableHelper};
    Ok(arena_seasons::table
        .filter(arena_seasons::status.eq("active"))
        .select(SeasonRow::as_select())
        .order(arena_seasons::starts_at.desc())
        .first(conn)
        .await
        .optional()?)
}

pub async fn recalculate_season(
    conn: &mut AsyncPgConnection,
    config: &RankingConfig,
    season: &SeasonRow,
    dry_run: bool,
) -> Result<RecalcReport, RecalcError> {
    let rule = config.cups_rule;
    let cutoff = season.ends_at.min(super::arena_season::now_unix());
    let rows: Vec<MatchRow> = diesel::sql_query(SEASON_ROWS_SQL)
        .bind::<BigInt, _>(season.starts_at)
        .bind::<BigInt, _>(cutoff)
        .get_results(conn)
        .await?;
    let inputs: Vec<ReplayInput<AltKey>> = rows
        .iter()
        .map(|r| ReplayInput {
            key: (r.character_id, r.alt),
            h2h: r.is_h2h,
            outcome: MatchOutcome {
                rounds_won: r.rounds_won.clamp(0, 255) as u8,
                rounds_lost: r.rounds_lost.clamp(0, 255) as u8,
                win: r.win,
            },
            classic: r.classic,
        })
        .collect();
    let (deltas, totals) = replay(config, rule, &inputs);
    let (_, classic_totals) = replay(config, CupsRule::Classic, &inputs);

    // Match rows: rewrite only what changes; the classic value is kept the
    // first time a row is touched and never again.
    let (ids, new): (Vec<Uuid>, Vec<i64>) = rows
        .iter()
        .zip(&deltas)
        .filter(|(r, d)| r.trophy_delta != **d)
        .map(|(r, d)| (r.id, *d))
        .unzip();
    let rows_rewritten = ids.len();

    // Counters.
    let nil = Uuid::nil();
    let (kc, ka): (Vec<Uuid>, Vec<Uuid>) =
        totals.keys().map(|(c, a)| (*c, a.unwrap_or(nil))).unzip();
    let counters: Vec<CounterRow> = diesel::sql_query(counters_sql())
        .bind::<Array<SqlUuid>, _>(&kc)
        .bind::<Array<SqlUuid>, _>(&ka)
        .bind::<diesel::sql_types::Text, _>(season.id.to_string())
        .get_results(conn)
        .await?;
    let mut classic_mismatches = 0;
    let mut changes: Vec<(CounterChange, Uuid)> = Vec::new();
    for c in &counters {
        let key = (c.character_id, c.alt);
        let Some(&after) = totals.get(&key) else {
            continue;
        };
        if classic_totals.get(&key).copied() != Some(c.trophies) {
            classic_mismatches += 1;
        }
        if after != c.trophies {
            changes.push((
                CounterChange {
                    character_id: c.character_id,
                    alt: c.alt,
                    parked: c.parked,
                    before: c.trophies,
                    after,
                },
                c.row_id,
            ));
        }
    }
    changes.sort_by(|a, b| {
        b.0.after
            .cmp(&a.0.after)
            .then(a.0.character_id.cmp(&b.0.character_id))
    });

    let mut guilds_recomputed = 0;
    if !dry_run {
        if !ids.is_empty() {
            diesel::sql_query(
                "UPDATE arena_match_results r \
                 SET classic_trophy_delta = COALESCE(r.classic_trophy_delta, r.trophy_delta), \
                     trophy_delta = u.d \
                 FROM UNNEST($1::uuid[], $2::bigint[]) AS u(id, d) \
                 WHERE r.id = u.id",
            )
            .bind::<Array<SqlUuid>, _>(&ids)
            .bind::<Array<BigInt>, _>(&new)
            .execute(conn)
            .await?;
        }
        for parked in [false, true] {
            let (row_ids, values): (Vec<Uuid>, Vec<i64>) = changes
                .iter()
                .filter(|(c, _)| c.parked == parked)
                .map(|(c, row)| (*row, c.after))
                .unzip();
            if row_ids.is_empty() {
                continue;
            }
            let table = if parked {
                "character_versions"
            } else {
                "characters"
            };
            diesel::sql_query(format!(
                "UPDATE {table} t \
                 SET character = jsonb_set(t.character, '{{pvpTrophies}}', to_jsonb(u.v)) \
                 FROM UNNEST($1::uuid[], $2::bigint[]) AS u(id, v) \
                 WHERE t.id = u.id"
            ))
            .bind::<Array<SqlUuid>, _>(&row_ids)
            .bind::<Array<BigInt>, _>(&values)
            .execute(conn)
            .await?;
        }
        guilds_recomputed = crate::guild::recompute_guild_trophies(conn, season).await?;
    }

    Ok(RecalcReport {
        rule,
        dry_run,
        season_id: season.id,
        season_name: season.name.clone(),
        matches: rows.len(),
        rows_rewritten,
        alts: totals.len(),
        counters_changed: changes.into_iter().map(|(c, _)| c).collect(),
        classic_mismatches,
        guilds_recomputed,
    })
}

#[derive(Deserialize)]
pub struct RecalcRequest {
    /// Default `true`: writing needs an explicit `"dry_run": false`.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Optional guard: refuse unless the ACTIVE config selects this rule, so a
    /// run never recalculates under a rule the caller did not mean.
    #[serde(default)]
    pub expect_rule: Option<CupsRule>,
}

fn default_true() -> bool {
    true
}

/// `POST /api/dev/v1/arena-ranking/cups-rule/recalculate` (X-Import-Token).
///
/// Body `{"dry_run": bool, "expect_rule": "classic"|"haudrauf_2500"}`. Reads the
/// ACTIVE ranking config fresh (not the 30 s cache) and recalculates the active
/// season under its `cups_rule`.
#[post("/blades.bgs.services/api/dev/v1/arena-ranking/cups-rule/recalculate")]
pub async fn post_cups_rule_recalculate(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<RecalcRequest>,
) -> Result<actix_web::HttpResponse, BladeApiError> {
    crate::admin::check_import_token(&app_state, &req)?;
    ranking::invalidate_cache();
    let config = ranking::load(&app_state.db_pool).await.config;
    if let Some(expected) = body.expect_rule
        && expected != config.cups_rule
    {
        return Ok(actix_web::HttpResponse::Conflict().json(serde_json::json!({
            "error": "rule_mismatch",
            "active": config.cups_rule,
            "expected": expected,
        })));
    }
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    match recalculate(&mut conn, &config, body.dry_run).await {
        Ok(report) => {
            log::warn!(
                "arena cups rule: recalculated season {} under {} (dry_run {}): {} of {} match rows, \
                 {} counters, {} classic mismatches, {} guilds",
                report.season_id,
                report.rule.as_str(),
                report.dry_run,
                report.rows_rewritten,
                report.matches,
                report.counters_changed.len(),
                report.classic_mismatches,
                report.guilds_recomputed,
            );
            Ok(actix_web::HttpResponse::Ok().json(report))
        }
        Err(RecalcError::NoActiveSeason) => Ok(actix_web::HttpResponse::Conflict()
            .json(serde_json::json!({"error": "no_active_season"}))),
        Err(RecalcError::Db(e)) => Err(BladeApiError::generic_internal_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn haudrauf() -> RankingConfig {
        RankingConfig {
            cups_rule: CupsRule::Haudrauf2500,
            ..RankingConfig::default()
        }
    }

    fn o(w: u8, l: u8) -> MatchOutcome {
        MatchOutcome::new(w, l)
    }

    /// The rule as the owner approved it (d54), case by case, on the live path.
    #[test]
    fn live_trophy_delta_follows_the_selected_rule() {
        let input = |outcome, own, h2h, pair| ranking::TrophyDeltaInput {
            outcome,
            own_trophies: own,
            opponent_trophies_for_pricing: own,
            h2h,
            h2h_pair_matches_today: pair,
        };
        let classic = RankingConfig::default();
        let h = haudrauf();
        let cases = [
            // (outcome, own before, h2h, pair games today, classic, haudrauf)
            (o(2, 0), 0, false, 0, 32, 32),
            (o(2, 1), 2499, false, 0, 28, 28),
            (o(1, 2), 100, false, 0, -16, 16),
            (o(0, 2), 2499, false, 0, -20, 20),
            (o(2, 0), 2500, false, 0, 32, 1),
            (o(2, 1), 9000, false, 0, 28, 1),
            (o(1, 2), 2500, false, 0, -16, 0),
            (o(0, 2), 9000, false, 0, -20, 0),
            // h2h with the +50 (first three games of the pair today)
            (o(2, 0), 0, true, 0, 90, 90),
            (o(2, 1), 100, true, 2, 85, 85),
            (o(1, 2), 100, true, 1, 40, 60),
            (o(0, 2), 2499, true, 0, 35, 65),
            // h2h after the daily cap: no award either way
            (o(2, 0), 100, true, 3, 40, 40),
            (o(0, 2), 100, true, 3, -15, 15),
            (o(1, 2), 100, true, 5, -10, 10),
            // from 2500: half the change without the award, rounded down
            (o(2, 0), 2500, true, 0, 90, 20),
            (o(2, 1), 2600, true, 0, 85, 17),
            (o(1, 2), 2500, true, 0, 40, 5),
            (o(0, 2), 2500, true, 3, -15, 7),
        ];
        for (outcome, own, h2h, pair, want_classic, want_h) in cases {
            let i = input(outcome, own, h2h, pair);
            assert_eq!(
                ranking::trophy_delta(&classic, i),
                want_classic,
                "classic {outcome:?} {own} h2h={h2h} pair={pair}"
            );
            assert_eq!(
                ranking::trophy_delta(&h, i),
                want_h,
                "haudrauf {outcome:?} {own} h2h={h2h} pair={pair}"
            );
            // The classic value is recorded whatever rule is selected...
            assert_eq!(ranking::classic_trophy_delta(&h, i), want_classic);
            // ...and the replay from it reproduces the live payout exactly.
            assert_eq!(
                replay_delta(&h, CupsRule::Haudrauf2500, want_classic, h2h, outcome, own),
                want_h,
                "replay {outcome:?} {own} h2h={h2h} pair={pair}"
            );
        }
    }

    #[test]
    fn a_voided_ai_match_stays_zero_under_every_rule() {
        let h = haudrauf();
        for before in [0, 2499, 2500, 9000] {
            for win in [false, true] {
                let outcome = MatchOutcome {
                    rounds_won: 2,
                    rounds_lost: 0,
                    win,
                };
                assert_eq!(
                    replay_delta(&h, CupsRule::Haudrauf2500, 0, false, outcome, before),
                    0
                );
                assert_eq!(
                    replay_delta(&h, CupsRule::Classic, 0, false, outcome, before),
                    0
                );
            }
        }
    }

    #[test]
    fn the_config_switch_defaults_to_classic_and_round_trips() {
        assert_eq!(RankingConfig::default().cups_rule, CupsRule::Classic);
        // A config row written before d54 has no `cups_rule` at all.
        let old: RankingConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(old.cups_rule, CupsRule::Classic);
        let v = serde_json::to_value(haudrauf()).unwrap();
        assert_eq!(v["cups_rule"], "haudrauf_2500");
        let back: RankingConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.cups_rule, CupsRule::Haudrauf2500);
        assert!(back.validate().is_ok());
        assert!(
            serde_json::from_value::<RankingConfig>(serde_json::json!({"cups_rule": "bogus"}))
                .is_err()
        );
    }

    struct Fixture {
        rows: Vec<ReplayInput<usize>>,
        /// alt -> (in-game counter now, sim515's HauDrauf total)
        expected: BTreeMap<usize, (i64, i64)>,
    }

    /// The 2026-11 season from prod (sim515's pull, alts anonymised to indices;
    /// `ticket-duty-files/2026-11-15/d54/make_fixture.py`).
    fn prod_fixture() -> Fixture {
        let rows = include_str!("testdata/d54_season_matches.csv")
            .lines()
            .skip(1)
            .map(|l| {
                let f: Vec<i64> = l.split(',').map(|x| x.parse().unwrap()).collect();
                ReplayInput {
                    key: f[0] as usize,
                    h2h: f[1] == 1,
                    outcome: MatchOutcome {
                        rounds_won: f[3] as u8,
                        rounds_lost: f[4] as u8,
                        win: f[2] == 1,
                    },
                    classic: f[5],
                }
            })
            .collect();
        let expected = include_str!("testdata/d54_season_expected.csv")
            .lines()
            .skip(1)
            .map(|l| {
                let f: Vec<i64> = l.split(',').map(|x| x.parse().unwrap()).collect();
                (f[0] as usize, (f[2], f[3]))
            })
            .collect();
        Fixture { rows, expected }
    }

    /// Classic replay of the stored season == every in-game counter today, so
    /// flipping back restores today's numbers exactly.
    #[test]
    fn classic_replay_of_the_prod_season_equals_the_in_game_counters() {
        let f = prod_fixture();
        assert_eq!(f.rows.len(), 2576);
        let (deltas, totals) = replay(&RankingConfig::default(), CupsRule::Classic, &f.rows);
        assert!(deltas.iter().zip(&f.rows).all(|(d, r)| *d == r.classic));
        assert_eq!(totals.len(), 39);
        for (alt, (in_game, _)) in &f.expected {
            assert_eq!(totals[alt], *in_game, "alt {alt}");
        }
    }

    /// HauDrauf replay of the same season == sim515, alt for alt (HauDrauf
    /// 15451 → 4689, Yaskrava 14686 → 3522, ...).
    #[test]
    fn haudrauf_replay_of_the_prod_season_equals_sim515() {
        let f = prod_fixture();
        let (_, totals) = replay(&haudrauf(), CupsRule::Haudrauf2500, &f.rows);
        for (alt, (_, sim)) in &f.expected {
            assert_eq!(totals[alt], *sim, "alt {alt}");
        }
        assert_eq!(totals.values().max(), Some(&4689));
        assert_eq!(totals.values().filter(|t| **t >= 2500).count(), 9);
    }

    mod db {
        use super::*;
        use diesel_async::SimpleAsyncConnection;

        async fn fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            conn.begin_test_transaction()
                .await
                .expect("test transaction");
            let schema = format!("t{}", Uuid::new_v4().simple());
            conn.batch_execute(&format!(
                "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}; \
                 CREATE TABLE characters (id UUID PRIMARY KEY, user_id UUID NOT NULL, \
                     character JSONB NOT NULL, server_state JSONB NOT NULL DEFAULT '{{}}', \
                     source_alt_uuid UUID); \
                 CREATE TABLE character_versions (id UUID PRIMARY KEY, character_id UUID NOT NULL, \
                     user_id UUID NOT NULL, source_alt_uuid UUID, character JSONB NOT NULL, \
                     server_state JSONB NOT NULL DEFAULT '{{}}', saved_at BIGINT NOT NULL); \
                 CREATE TABLE arena_matches (ticket_id UUID PRIMARY KEY, game_session_id UUID, \
                     paired BOOLEAN NOT NULL DEFAULT false);"
            ))
            .await
            .unwrap();
            for ddl in [
                include_str!("../../../migrations/2026-06-20-010000-0000_add_guilds/up.sql"),
                include_str!(
                    "../../../migrations/2026-07-25-000000-0000_add_arena_match_results/up.sql"
                ),
                include_str!(
                    "../../../migrations/2026-09-28-000000-0000_add_arena_match_results_h2h_flag/up.sql"
                ),
                include_str!("../../../migrations/2026-09-05-000000-0000_add_arena_seasons/up.sql"),
                include_str!(
                    "../../../migrations/2026-10-10-000000-0000_alts_own_arena_history/up.sql"
                ),
                include_str!(
                    "../../../migrations/2026-11-15-000000-0000_arena_match_results_classic_delta/up.sql"
                ),
            ] {
                conn.batch_execute(ddl).await.unwrap();
            }
            crate::testchar::apply_migration_for_tests(&mut conn).await;
            Some(conn)
        }

        macro_rules! db {
            () => {
                match fixture().await {
                    Some(c) => c,
                    None => {
                        eprintln!("SKIP: TEST_DATABASE_URL unset — d54 recalculation NOT verified");
                        return;
                    }
                }
            };
        }

        #[derive(QueryableByName, Debug, PartialEq, Eq, Clone)]
        struct Snap {
            #[diesel(sql_type = diesel::sql_types::Text)]
            what: String,
            #[diesel(sql_type = BigInt)]
            v: i64,
        }

        /// Everything a recalculation may touch, as one comparable list.
        async fn snapshot(c: &mut AsyncPgConnection) -> Vec<Snap> {
            diesel::sql_query(
                "SELECT 'm:' || id::text AS what, trophy_delta AS v FROM arena_match_results \
                 UNION ALL SELECT 'm_after:' || id::text, trophies_after FROM arena_match_results \
                 UNION ALL SELECT 'c:' || id::text, COALESCE((character->>'pvpTrophies')::bigint, -1) FROM characters \
                 UNION ALL SELECT 'c_mm:' || id::text, COALESCE((character->>'matchmakingPvpTrophies')::bigint, -1) FROM characters \
                 UNION ALL SELECT 'v:' || id::text, COALESCE((character->>'pvpTrophies')::bigint, -1) FROM character_versions \
                 UNION ALL SELECT 'g:' || id, trophies FROM guilds \
                 ORDER BY 1",
            )
            .get_results(c)
            .await
            .unwrap()
        }

        /// The Top 100 "all matches" board's `wins`, keyed by board id, sorted.
        async fn board_wins(c: &mut AsyncPgConnection) -> Vec<(Uuid, i64)> {
            let mut w: Vec<(Uuid, i64)> = crate::arena::leaderboards::ranked_entries(c, 100, 0)
                .await
                .unwrap()
                .into_iter()
                .map(|e| (e.character_id, e.number_of_matches_won))
                .collect();
            w.sort();
            w
        }

        async fn counter(c: &mut AsyncPgConnection, table: &str, id: Uuid) -> i64 {
            diesel::sql_query(format!(
                "SELECT 'x' AS what, (character->>'pvpTrophies')::bigint AS v FROM {table} WHERE id = '{id}'"
            ))
            .get_result::<Snap>(c)
            .await
            .unwrap()
            .v
        }

        /// One alt that crosses 2500, a parked alt with h2h matches, a guild,
        /// a voided AI row, and a row recorded before alts were tracked.
        /// classic → haudrauf → classic restores every stored number, each step
        /// is idempotent, and the original deltas survive in
        /// `classic_trophy_delta`.
        #[tokio::test]
        async fn recalculation_round_trips_losslessly() {
            let mut c = db!();
            let season = Uuid::new_v4();
            let (cid, uid, alt_a, alt_b) = (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            let (cid2, uid2) = (Uuid::new_v4(), Uuid::new_v4());
            let mut stmts = format!(
                "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at, status) \
                     VALUES ('{season}', 3, 'S', 1000, 4000000000, 'active'); \
                 INSERT INTO guilds (id, name, tag_id) VALUES ('g1', 'G', '0001'); \
                 INSERT INTO guild_members (guild_id, user_id, character_id) VALUES ('g1', '{uid}', '{cid}'); \
                 INSERT INTO characters (id, user_id, source_alt_uuid, character) VALUES \
                   ('{cid}', '{uid}', '{alt_a}', '{{\"pvpSeasonId\":\"{season}\",\"matchmakingPvpTrophies\":9999}}'), \
                   ('{cid2}', '{uid2}', NULL, '{{}}'); \
                 INSERT INTO character_versions (id, character_id, user_id, source_alt_uuid, character, saved_at) VALUES \
                   ('{}', '{cid}', '{uid}', '{alt_b}', '{{\"pvpTrophies\":1}}', 10), \
                   ('{}', '{cid}', '{uid}', '{alt_b}', '{{\"pvpSeasonId\":\"{season}\"}}', 20); ",
                Uuid::new_v4(),
                Uuid::new_v4(),
            );
            // alt A: 80 AI 2-0 wins (32 each → 2560 classic), then losses and a
            // voided row; one early row with no alt (= alt A, the live one).
            let mut t = 2000;
            let mut row = |cid: Uuid, alt: Option<Uuid>, h2h: bool, w: i32, l: i32, d: i64| {
                t += 1;
                let alt = alt.map_or("NULL".into(), |a| format!("'{a}'"));
                format!(
                    "INSERT INTO arena_match_results (id, character_id, win, rounds_won, rounds_lost, \
                       trophy_delta, recorded_at, is_h2h, source_alt_uuid) VALUES \
                       ('{}', '{cid}', {}, {w}, {l}, {d}, to_timestamp({t}), {h2h}, {alt}); ",
                    Uuid::new_v4(),
                    w > l,
                )
            };
            stmts += &row(cid, None, false, 0, 2, -20);
            for _ in 0..80 {
                stmts += &row(cid, Some(alt_a), false, 2, 0, 32);
            }
            stmts += &row(cid, Some(alt_a), false, 0, 2, -20);
            stmts += &row(cid, Some(alt_a), false, 0, 2, 0); // voided
            stmts += &row(cid, Some(alt_a), true, 2, 0, 90);
            stmts += &row(cid, Some(alt_a), true, 1, 2, -10);
            // alt B (parked): h2h only
            stmts += &row(cid, Some(alt_b), true, 0, 2, 35);
            stmts += &row(cid, Some(alt_b), true, 0, 2, -15);
            stmts += &row(cid, Some(alt_b), true, 2, 1, 85);
            // second player, never had an alt
            stmts += &row(cid2, None, false, 1, 2, -16);
            stmts += &row(cid2, None, false, 2, 1, 28);
            c.batch_execute(&stmts).await.unwrap();

            // Seed the live counters with the CLASSIC replay (= prod today).
            let classic_cfg = RankingConfig::default();
            let report = recalculate(&mut c, &classic_cfg, false).await.unwrap();
            assert_eq!(
                report.rows_rewritten, 0,
                "classic never rewrites a stored delta"
            );
            // alt A: 0 → 80*32=2560 → 2540 → 2540 → 2630 → 2620; B: 35 → 20 → 105; cid2: 0 → 28
            let live_a = counter(&mut c, "characters", cid).await;
            assert_eq!(live_a, 2620);
            let parked_b = diesel::sql_query(format!(
                "SELECT 'x' AS what, (character->>'pvpTrophies')::bigint AS v FROM character_versions \
                 WHERE source_alt_uuid = '{alt_b}' ORDER BY saved_at DESC LIMIT 1"
            ))
            .get_result::<Snap>(&mut c)
            .await
            .unwrap()
            .v;
            assert_eq!(parked_b, 105);
            assert_eq!(counter(&mut c, "characters", cid2).await, 28);
            let original = snapshot(&mut c).await;
            // Top 100 "all matches" wins, per alt: a count of win rows, which a
            // recalculation never touches. Pinned here and after each flip.
            let wins_before = board_wins(&mut c).await;
            assert_eq!(
                wins_before,
                {
                    let mut e = vec![(cid, 81), (cid2, 1), (alt_b, 1)];
                    e.sort();
                    e
                },
                "alt A 80 AI + 1 h2h, parked alt B 1 h2h, cid2 1"
            );
            let again = recalculate(&mut c, &classic_cfg, false).await.unwrap();
            assert_eq!(
                (
                    again.rows_rewritten,
                    again.counters_changed.len(),
                    again.classic_mismatches
                ),
                (0, 0, 0)
            );

            // Switch to HauDrauf.
            let h = haudrauf();
            let dry = recalculate(&mut c, &h, true).await.unwrap();
            assert_eq!(snapshot(&mut c).await, original, "a dry run writes nothing");
            let done = recalculate(&mut c, &h, false).await.unwrap();
            assert_eq!(done.rows_rewritten, dry.rows_rewritten);
            assert_eq!(done.counters_changed, dry.counters_changed);
            // alt A: +20 (|−20|) → 20 + 78*32 = 2516 → then 2 wins at +1 → 2518,
            // AI loss 0, voided 0, h2h 2-0 |40|/2=20 → 2538, h2h 1-2 |−10|/2=5 → 2543
            assert_eq!(counter(&mut c, "characters", cid).await, 2543);
            assert_eq!(
                board_wins(&mut c).await,
                wins_before,
                "a cups-rule recalculation (#614) must not move a single win"
            );
            // alt B: 15+50=65 → +15=80 → 35+50=85 → 165
            let b_now = diesel::sql_query(format!(
                "SELECT 'x' AS what, (character->>'pvpTrophies')::bigint AS v FROM character_versions \
                 WHERE source_alt_uuid = '{alt_b}' ORDER BY saved_at DESC LIMIT 1"
            ))
            .get_result::<Snap>(&mut c)
            .await
            .unwrap()
            .v;
            assert_eq!(b_now, 165);
            assert_eq!(counter(&mut c, "characters", cid2).await, 44);
            // The high-water mark the matchmaker pairs on is untouched.
            let mm = diesel::sql_query(format!(
                "SELECT 'x' AS what, (character->>'matchmakingPvpTrophies')::bigint AS v \
                 FROM characters WHERE id = '{cid}'"
            ))
            .get_result::<Snap>(&mut c)
            .await
            .unwrap()
            .v;
            assert_eq!(mm, 9999);
            // The season close reads what was rewritten.
            let s: SeasonRow = active_season(&mut c).await.unwrap().unwrap();
            let standings = crate::arena::season_store::freeze_standings(&mut c, &s)
                .await
                .unwrap();
            let a = standings
                .iter()
                .find(|r| r.source_alt_uuid == Some(alt_a))
                .unwrap();
            assert_eq!((a.rank, a.trophies), (1, 2543));
            // And the guild board was recomputed from it (alt A is live in g1).
            let g = snapshot(&mut c)
                .await
                .into_iter()
                .find(|s| s.what == "g:g1")
                .unwrap();
            assert_eq!(g.v, (2543.0f64 * 0.30).round() as i64);
            // Idempotent.
            let again = recalculate(&mut c, &h, false).await.unwrap();
            assert_eq!((again.rows_rewritten, again.counters_changed.len()), (0, 0));
            // Original deltas kept.
            let kept = diesel::sql_query(
                "SELECT 'x' AS what, COUNT(*)::bigint AS v FROM arena_match_results \
                 WHERE classic_trophy_delta IS NOT NULL AND classic_trophy_delta <> trophy_delta",
            )
            .get_result::<Snap>(&mut c)
            .await
            .unwrap()
            .v;
            assert!(kept > 0);

            // Flip back: every number is today's again.
            let back = recalculate(&mut c, &classic_cfg, false).await.unwrap();
            assert_eq!(back.rows_rewritten, done.rows_rewritten);
            assert_eq!(snapshot(&mut c).await, original);
            assert_eq!(board_wins(&mut c).await, wins_before);
        }

        /// A live result written under HauDrauf keeps its classic value, so a
        /// flip back pays it classic.
        #[tokio::test]
        async fn a_live_haudrauf_row_flips_back_to_its_classic_value() {
            let mut c = db!();
            let (season, cid, uid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            c.batch_execute(&format!(
                "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at, status) \
                     VALUES ('{season}', 3, 'S', 1000, 4000000000, 'active'); \
                 INSERT INTO characters (id, user_id, character) VALUES ('{cid}', '{uid}', '{{\"pvpTrophies\":16}}'); \
                 INSERT INTO arena_match_results (id, character_id, win, rounds_won, rounds_lost, \
                     trophy_delta, classic_trophy_delta, recorded_at) VALUES \
                     ('{}', '{cid}', false, 1, 2, 16, -16, to_timestamp(2000));",
                Uuid::new_v4()
            ))
            .await
            .unwrap();
            let r = recalculate(&mut c, &RankingConfig::default(), false)
                .await
                .unwrap();
            assert_eq!(r.rows_rewritten, 1);
            assert_eq!(counter(&mut c, "characters", cid).await, 0);
        }
    }
}
