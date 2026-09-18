//! Arena leaderboard — `GET /characters/{id}/leaderboards/{leaderboard_id}`.
//!
//! This used to be a stub that answered `{"totalEntries":0,"entries":[]}` to
//! everyone, so the arena's Leaderboard tab was permanently empty. It now ranks
//! real characters by their PvP trophy count.
//!
//! # Wire shape
//!
//! Taken verbatim from a retail capture (`api_captures` #1229, Flappety's own
//! request against `blades.bgs.services`), not invented:
//!
//! ```json
//! {
//!   "playerEntry": {
//!     "userId": "...", "characterId": "...", "characterName": "Flappety",
//!     "guildName": "The Moderately Formidables",
//!     "rank": 92, "score": 361, "numberOfMatchesWon": 6, "streak": 2
//!   },
//!   "leaderboard": {
//!     "totalEntries": 890, "currentPage": 1, "totalPages": 9,
//!     "entries": [ { ...same shape... } ]
//!   }
//! }
//! ```
//!
//! `score` is the trophy count (`pvpTrophies`), `streak` is `pvpWinningStreak`
//! (negative on a losing run — retail ships `-1` happily), and the page size is
//! 100 (`890 entries / 9 pages`). The query string retail sends is
//! `?page=1&includePlayerEntry=True&groups=`.
//!
//! # Ranking source
//!
//! `score` is the player's own `character.pvpTrophies` — the same field the player
//! screen and the social card serve, so the board cannot disagree with what a
//! player sees. It is NOT reconstructed from `arena_match_results`; that
//! reconstruction applied a retroactive zero-floor where the live counter clamps
//! per match, and the two answers diverged for anyone who had ever bottomed out.
//!
//! `wins` is a windowed count over the active season, as a LEFT join, so a
//! character with trophies but no matches this season still appears at zero wins
//! rather than vanishing. Bot opponents have no result row and cannot enter.
//!
//! # Both numbers must describe the SAME season
//!
//! `pvpTrophies` is a live counter that the season rollover zeroes
//! ([`crate::arena::arena_season::roll_character_into`]). A character still
//! stamped with a different `pvpSeasonId` never had that rollover applied, so its
//! balance was earned in another season — or, for a retail import, on Bethesda's
//! servers. Serving it beside a count of THIS season's wins is what produces the
//! shape this module has now had twice: hundreds of cups against zero wins.
//!
//! So membership is gated on the character's season stamp. The nil stamp is
//! admitted on purpose: it means "never been through a rollover", so any cups such
//! a character holds were necessarily earned here and now. Excluding it would
//! repeat the mistake that once hid the server's highest scorer — narrowing
//! membership until the board stops describing the server.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{
    get,
    web::{self, Json},
};
use diesel::sql_types::{BigInt, Nullable, Text, Uuid as SqlUuid};
use diesel::{QueryableByName, sql_query};
use diesel_async::RunQueryDsl;
use log::warn;
use serde::Serialize;
use uuid::Uuid;

use crate::ServerGlobal;

/// Entries per page. `890 total / 9 pages` in the retail capture.
const PAGE_SIZE: i64 = 100;

/// `totalPages` for a given `totalEntries`. Written out rather than `div_ceil`,
/// which is still unstable for `i64` on this toolchain.
fn page_count(total: i64) -> i64 {
    if total <= 0 { 0 } else { (total + PAGE_SIZE - 1) / PAGE_SIZE }
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardEntry {
    pub user_id: Uuid,
    pub character_id: Uuid,
    pub character_name: String,
    pub guild_name: String,
    pub rank: i64,
    pub score: i64,
    pub number_of_matches_won: i64,
    pub streak: i64,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct InnerLeaderboardResult {
    total_entries: i64,
    current_page: i64,
    total_pages: i64,
    entries: Vec<LeaderboardEntry>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LeaderboardResult {
    /// Omitted unless the client asked for it (`includePlayerEntry=True`), which
    /// is what retail's own request does.
    #[serde(skip_serializing_if = "Option::is_none")]
    player_entry: Option<LeaderboardEntry>,
    leaderboard: InnerLeaderboardResult,
}

/// One ranked row straight out of Postgres. `rank` is a window function so the
/// numbering is global, not per-page.
#[derive(QueryableByName, Debug)]
struct RankedRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Nullable<Text>)]
    guild_name: Option<String>,
    #[diesel(sql_type = BigInt)]
    score: i64,
    #[diesel(sql_type = BigInt)]
    streak: i64,
    #[diesel(sql_type = BigInt)]
    rank: i64,
    #[diesel(sql_type = BigInt)]
    wins: i64,
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    total: i64,
}

/// The ranked projection every query below selects from. Kept in one place so the
/// page query, the count and the player's own row can never disagree about who is
/// on the board or in what order.
const RANKED_CTE: &str = "
    WITH active_season AS (
        SELECT id,
               starts_at,
               LEAST(ends_at, EXTRACT(EPOCH FROM now())::bigint) AS cutoff
        FROM arena_seasons
        WHERE status = 'active'
        ORDER BY starts_at DESC
        LIMIT 1
    ),
    season_wins AS (
        SELECT r.character_id,
               COUNT(*) FILTER (WHERE r.win) AS wins
        FROM arena_match_results r
        JOIN active_season s
          ON r.recorded_at >= to_timestamp(s.starts_at)
         AND r.recorded_at < to_timestamp(s.cutoff)
        GROUP BY r.character_id
    ),
    ranked AS (
        SELECT c.id,
               c.user_id,
               COALESCE(c.character ->> 'name', '') AS name,
               g.name AS guild_name,
               COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) AS score,
               COALESCE((c.character ->> 'pvpWinningStreak')::bigint, 0) AS streak,
               COALESCE(w.wins, 0) AS wins,
               ROW_NUMBER() OVER (
                   ORDER BY COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) DESC,
                            COALESCE(w.wins, 0) DESC,
                            c.id
               ) AS rank
        FROM characters c
        CROSS JOIN active_season s
        LEFT JOIN season_wins w ON w.character_id = c.id
        LEFT JOIN guild_members gm ON gm.character_id = c.id
        LEFT JOIN guilds g ON g.id = gm.guild_id
        WHERE COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) > 0
          -- Cups this character holds FOR THIS SEASON. `pvpTrophies` is a live
          -- counter the season rollover zeroes; a character still stamped with
          -- another season's id never had that rollover applied, so its balance
          -- was earned somewhere else. Counting it here is what puts hundreds of
          -- cups against zero wins. The nil stamp is admitted deliberately — see
          -- `an_unstamped_character_is_not_treated_as_off_season`.
          AND (
                c.character ->> 'pvpSeasonId' IS NULL
             OR c.character ->> 'pvpSeasonId' = '00000000-0000-0000-0000-000000000000'
             OR c.character ->> 'pvpSeasonId' = s.id::text
          )
    )
";

fn row_to_entry(r: RankedRow) -> LeaderboardEntry {
    LeaderboardEntry {
        user_id: r.user_id,
        character_id: r.id,
        character_name: r.name,
        guild_name: r.guild_name.unwrap_or_default(),
        rank: r.rank,
        score: r.score,
        number_of_matches_won: r.wins,
        streak: r.streak,
    }
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/leaderboards/{unk}")]
pub async fn get_leaderboard(
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    query: web::Query<HashMap<String, String>>,
) -> Json<LeaderboardResult> {
    let (character_id, _leaderboard_id) = path.into_inner();
    let page = query
        .get("page")
        .and_then(|p| p.parse::<i64>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    let include_player = query
        .get("includePlayerEntry")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    match build(app_state.get_ref(), character_id, page, include_player).await {
        Ok(result) => Json(result),
        Err(e) => {
            // The leaderboard is cosmetic — never fail the arena menu over it.
            warn!("leaderboard: query failed, serving an empty board: {e}");
            Json(LeaderboardResult {
                player_entry: include_player.then(|| LeaderboardEntry {
                    character_id,
                    ..Default::default()
                }),
                leaderboard: InnerLeaderboardResult {
                    total_entries: 0,
                    current_page: page,
                    total_pages: 0,
                    entries: Vec::new(),
                },
            })
        }
    }
}

async fn build(
    app_state: &ServerGlobal,
    character_id: Uuid,
    page: i64,
    include_player: bool,
) -> Result<LeaderboardResult, anyhow::Error> {
    let mut conn = app_state.db_pool.get().await?;

    let total: i64 = sql_query(format!("{RANKED_CTE} SELECT COUNT(*) AS total FROM ranked"))
        .get_result::<CountRow>(&mut conn)
        .await?
        .total;
    let total_pages = page_count(total);

    let rows: Vec<RankedRow> = sql_query(format!(
        "{RANKED_CTE} SELECT id, user_id, name, guild_name, score, streak, rank, wins \
         FROM ranked ORDER BY rank LIMIT $1 OFFSET $2"
    ))
    .bind::<BigInt, _>(PAGE_SIZE)
    .bind::<BigInt, _>((page - 1) * PAGE_SIZE)
    .get_results(&mut conn)
    .await?;

    // The player's own row may be on any page (retail's capture showed rank 92
    // returned alongside page 1), so it is fetched separately.
    let player_row: Option<RankedRow> = if include_player {
        sql_query(format!(
            "{RANKED_CTE} SELECT id, user_id, name, guild_name, score, streak, rank, wins \
             FROM ranked WHERE id = $1"
        ))
        .bind::<SqlUuid, _>(character_id)
        .get_results::<RankedRow>(&mut conn)
        .await?
        .into_iter()
        .next()
    } else {
        None
    };

    let player_entry = if include_player {
        Some(match player_row {
            Some(r) => row_to_entry(r),
            // No match in the active season — answer rank 0 / score 0.
            None => LeaderboardEntry {
                character_id,
                ..Default::default()
            },
        })
    } else {
        None
    };

    Ok(LeaderboardResult {
        player_entry,
        leaderboard: InnerLeaderboardResult {
            total_entries: total,
            current_page: page,
            total_pages,
            entries: rows.into_iter().map(row_to_entry).collect(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board is cosmetic but its SHAPE is load-bearing — the client parses it
    /// strictly. Pin the camelCase keys against the retail capture (#1229).
    #[test]
    fn serialized_shape_matches_the_retail_capture() {
        let result = LeaderboardResult {
            player_entry: Some(LeaderboardEntry {
                user_id: Uuid::nil(),
                character_id: Uuid::nil(),
                character_name: "Flappety".into(),
                guild_name: "The Moderately Formidables".into(),
                rank: 92,
                score: 361,
                number_of_matches_won: 6,
                streak: 2,
            }),
            leaderboard: InnerLeaderboardResult {
                total_entries: 890,
                current_page: 1,
                total_pages: 9,
                entries: vec![LeaderboardEntry {
                    character_name: "Snake".into(),
                    guild_name: "Akatosh Empire".into(),
                    rank: 1,
                    score: 1062,
                    number_of_matches_won: 72,
                    streak: 41,
                    ..Default::default()
                }],
            },
        };
        let v: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert_eq!(v["playerEntry"]["characterName"], "Flappety");
        assert_eq!(v["playerEntry"]["guildName"], "The Moderately Formidables");
        assert_eq!(v["playerEntry"]["rank"], 92);
        assert_eq!(v["playerEntry"]["score"], 361);
        assert_eq!(v["playerEntry"]["numberOfMatchesWon"], 6);
        assert_eq!(v["playerEntry"]["streak"], 2);
        assert_eq!(v["leaderboard"]["totalEntries"], 890);
        assert_eq!(v["leaderboard"]["currentPage"], 1);
        assert_eq!(v["leaderboard"]["totalPages"], 9);
        assert_eq!(v["leaderboard"]["entries"][0]["characterName"], "Snake");
        assert_eq!(
            v["leaderboard"]["entries"][0]["userId"],
            Uuid::nil().to_string()
        );
        // playerEntry is omitted entirely when the client did not ask for it.
        let no_player = LeaderboardResult {
            player_entry: None,
            leaderboard: InnerLeaderboardResult {
                total_entries: 0,
                current_page: 1,
                total_pages: 0,
                entries: vec![],
            },
        };
        let v2: serde_json::Value = serde_json::to_value(&no_player).unwrap();
        assert!(v2.get("playerEntry").is_none());
    }

    /// The board must show the SAME number the player sees on their own screen.
    ///
    /// It did not. The board RECONSTRUCTED each score by summing
    /// `arena_match_results.trophy_delta` over the season and then applying a
    /// single retroactive zero-floor:
    ///
    /// ```sql
    /// raw_score - LEAST(0, MIN(raw_score) OVER (...))
    /// ```
    ///
    /// The live counter in `arena_economy` clamps at zero on EVERY match
    /// (`(pre + delta).max(0)`), discarding each shortfall as it happens. Two
    /// different algorithms for one number, and they disagree for anyone who has
    /// ever bottomed out.
    ///
    /// Measured on production before the change (character 30581f3e, Flappety):
    /// 126 season matches, deltas summing to 426, running minimum -123. The board
    /// served 426 - (-123) = 549 while his screen showed 534. Worse, the board's
    /// rank 1 "Deathbell" showed 725 against 321 actually held, and the highest
    /// scorer on the server (1061) was absent entirely — a character with trophies
    /// but no match rows this season never reached the reconstruction at all.
    ///
    /// This is a SOURCE assertion, and deliberately so: the projection is raw SQL
    /// and the tests in this module have no database. It cannot prove the query
    /// returns the right rows — the production measurement above does that — but
    /// it does catch a silent revert to the reconstruction.
    #[test]
    fn the_board_scores_from_the_player_s_own_trophy_count() {
        assert!(
            RANKED_CTE.contains("c.character ->> 'pvpTrophies'"),
            "score must come from the character's live trophy count"
        );
        assert!(
            !RANKED_CTE.contains("trophy_delta"),
            "the board must not reconstruct scores from match history: {RANKED_CTE}"
        );
        assert!(
            !RANKED_CTE.contains("raw_score"),
            "the retroactive zero-floor reconstruction must not come back"
        );
    }

    /// Wins are still season-scoped, and a character with NO matches this season
    /// still appears — with zero wins — rather than dropping off the board.
    ///
    /// That second half is the control: the obvious way to write this fix is to
    /// keep the inner join to `arena_match_results`, which is exactly what hid the
    /// server's highest scorer.
    #[test]
    fn wins_are_season_scoped_but_never_gate_membership() {
        assert!(
            RANKED_CTE.contains("LEFT JOIN season_wins"),
            "wins must be a LEFT join — an inner join drops trophied characters \
             who have not played this season"
        );
        assert!(
            RANKED_CTE.contains("COALESCE(w.wins, 0)"),
            "a character with no season matches must read as 0 wins, not NULL"
        );
        assert!(
            RANKED_CTE.contains("active_season"),
            "the win count is still scoped to the active season"
        );
    }

    /// 890 entries at 100 per page is 9 pages — the exact arithmetic the captured
    /// response reports.
    #[test]
    fn paging_matches_the_capture() {
        assert_eq!(PAGE_SIZE, 100);
        assert_eq!(page_count(890), 9);
        assert_eq!(page_count(100), 1);
        assert_eq!(page_count(101), 2);
        assert_eq!(page_count(0), 0);
    }
}

/// Cups and wins must describe the same season.
///
/// This module has produced "hundreds of cups, zero wins" twice. The first time,
/// late imports carried a retail trophy balance into their first local result;
/// `admin::align_import_with_active_season` fixed that at the entry point and its
/// doc comment names this exact symptom. The second time, the board switched from
/// reconstructing scores out of `arena_match_results` to reading `pvpTrophies`
/// directly — a correct fix for a different bug — and in doing so stopped
/// season-scoping the cups at all, which put every character still holding an
/// unrolled balance straight onto the board.
///
/// Measured on production when this was written: 10 characters carried
/// `pvpSeasonId = 7a3985ae-…`, an id that has no row in `arena_seasons` at all,
/// and between them held 3,877 cups — more than the entire real season's 2,403
/// across 102 properly-stamped characters. Seven of them ranked, one at 1,061 cups
/// with 3 wins and six at 396–440 cups having never played a single match.
#[cfg(test)]
mod cups_and_wins_describe_one_season {
    use super::*;

    #[test]
    fn membership_is_gated_on_the_characters_season_stamp() {
        assert!(
            RANKED_CTE.contains("'pvpSeasonId'") && RANKED_CTE.contains("s.id::text"),
            "the board must compare the character's season stamp with the active \
             season, or an unrolled balance counts as this season's cups: {RANKED_CTE}"
        );
        assert!(
            RANKED_CTE.contains("CROSS JOIN active_season"),
            "the comparison needs the active season in scope on the character join"
        );
        assert!(
            RANKED_CTE.contains("SELECT id,"),
            "active_season must project its id, or the comparison cannot be made"
        );
    }

    /// THE CONTROL, and the half most likely to be "tidied" away.
    ///
    /// A character that has never been through a rollover carries the nil stamp.
    /// Any cups it holds were therefore earned here, this season, and it belongs on
    /// the board. Tightening this to a bare equality would drop those players —
    /// the same failure mode as the inner join that once hid the top scorer.
    #[test]
    fn an_unstamped_character_is_not_treated_as_off_season() {
        assert!(
            RANKED_CTE.contains("'pvpSeasonId' IS NULL"),
            "a character with no season field must still rank"
        );
        assert!(
            RANKED_CTE.contains("00000000-0000-0000-0000-000000000000"),
            "the nil uuid is the 'never rolled over' stamp and must still rank"
        );
    }

    /// The gate is a WHERE clause, not a join that could also drop rows for an
    /// unrelated reason — guilds and wins stay LEFT joins.
    #[test]
    fn the_gate_does_not_disturb_the_optional_joins() {
        assert!(RANKED_CTE.contains("LEFT JOIN season_wins"));
        assert!(RANKED_CTE.contains("LEFT JOIN guild_members"));
        assert!(RANKED_CTE.contains("LEFT JOIN guilds"));
        assert!(
            !RANKED_CTE.contains("INNER JOIN"),
            "nothing about this gate justifies narrowing membership by a join"
        );
    }
}
