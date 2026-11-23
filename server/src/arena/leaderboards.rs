//! Arena leaderboard — `GET /characters/{id}/leaderboards/{leaderboard_id}`.
//!
//! This used to be a stub that answered `{"totalEntries":0,"entries":[]}` to
//! everyone, so the arena's Leaderboard tab was permanently empty. It now ranks
//! real characters by their PvP trophy count, including the newest saved snapshot
//! for an alt that has been switched out of the live account slot.
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
//! `score` is the player's own `character.pvpTrophies` — from the live row for an
//! active alt, or from the latest `character_versions` snapshot for a switched-out
//! alt. It is NOT reconstructed from `arena_match_results`; that reconstruction
//! applied a retroactive zero-floor where the live counter clamps per match, and
//! the two answers diverged for anyone who had ever bottomed out.
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
use std::time::{Duration, Instant};

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
    if total <= 0 {
        0
    } else {
        (total + PAGE_SIZE - 1) / PAGE_SIZE
    }
}

#[derive(Serialize, Debug, Default, Clone, PartialEq)]
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
pub(crate) const RANKED_CTE: &str = "
    WITH active_season AS (
        SELECT id,
               starts_at,
               LEAST(ends_at, EXTRACT(EPOCH FROM now())::bigint) AS cutoff
        FROM arena_seasons
        WHERE status = 'active'
        ORDER BY starts_at DESC
        LIMIT 1
    ),
    -- Per ALT, not per character id: a user's alts share the id, and grouping by
    -- it alone credited every alt's wins to whichever one was live (owner
    -- decision 2026-10-10). A match from before alts were tracked counts for the
    -- alt live now, as it always did.
    season_wins AS (
        SELECT r.character_id,
               COALESCE(r.source_alt_uuid, rc.source_alt_uuid) AS alt,
               COUNT(*) FILTER (WHERE r.win) AS wins
        FROM arena_match_results r
        JOIN characters rc ON rc.id = r.character_id
        JOIN active_season s
          ON r.recorded_at >= to_timestamp(s.starts_at)
         AND r.recorded_at < to_timestamp(s.cutoff)
        -- A fight with a developer's (TST) character is not a win for anyone.
        WHERE NOT EXISTS (
              SELECT 1 FROM dev_alt_lock_accounts dl
              WHERE dl.child_character_id IN (r.character_id, r.opponent_character_id)
        )
        GROUP BY r.character_id, COALESCE(r.source_alt_uuid, rc.source_alt_uuid)
    ),
    ranked AS (
        SELECT e.id,
               e.user_id,
               e.name,
               e.guild_name,
               e.score,
               e.streak,
               e.wins,
               ROW_NUMBER() OVER (
                   ORDER BY e.score DESC,
                            e.wins DESC,
                            e.id
               ) AS rank
        FROM (
            SELECT c.id,
                   c.user_id,
                   c.source_alt_uuid,
                   COALESCE(c.character ->> 'name', '') AS name,
                   g.name AS guild_name,
                   COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) AS score,
                   COALESCE((c.character ->> 'pvpWinningStreak')::bigint, 0) AS streak,
                   COALESCE(w.wins, 0) AS wins
            FROM characters c
            CROSS JOIN active_season s
            LEFT JOIN season_wins w
                   ON w.character_id = c.id
                  AND w.alt IS NOT DISTINCT FROM c.source_alt_uuid
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
              -- An operator test character never ranks (`crate::testchar`).
              AND NOT EXISTS (
                    SELECT 1 FROM test_characters tc WHERE tc.alt_uuid = c.source_alt_uuid
              )
              -- A developer lock child holds a copy of a parent alt that the
              -- parent still lists as a kept version (`crate::dev_alt_lock`).
              AND NOT EXISTS (
                    SELECT 1 FROM dev_alt_lock_accounts dl WHERE dl.child_character_id = c.id
              )
            UNION ALL
            -- A switched-out alt keeps its own wins and its own guild: the
            -- membership parked in its newest version, while that guild exists.
            SELECT saved.id,
                   saved.user_id,
                   saved.source_alt_uuid,
                   saved.name,
                   pg.name AS guild_name,
                   saved.score,
                   saved.streak,
                   COALESCE(pw.wins, 0) AS wins
            FROM (
                SELECT DISTINCT ON (v.user_id, v.source_alt_uuid)
                       COALESCE(v.source_alt_uuid, v.character_id) AS id,
                       v.character_id,
                       v.user_id,
                       v.source_alt_uuid,
                       COALESCE(v.character ->> 'name', '') AS name,
                       (SELECT s.guild_id FROM guild_alt_seats s WHERE s.alt_key = v.source_alt_uuid) AS parked_guild_id,
                       COALESCE((v.character ->> 'pvpTrophies')::bigint, 0) AS score,
                       COALESCE((v.character ->> 'pvpWinningStreak')::bigint, 0) AS streak
                FROM character_versions v
                CROSS JOIN active_season s
                WHERE v.source_alt_uuid IS NOT NULL
                  AND NOT EXISTS (
                        SELECT 1
                        FROM characters live
                        WHERE live.user_id = v.user_id
                          AND live.source_alt_uuid = v.source_alt_uuid
                  )
                  AND (
                        v.character ->> 'pvpSeasonId' IS NULL
                     OR v.character ->> 'pvpSeasonId' = '00000000-0000-0000-0000-000000000000'
                     OR v.character ->> 'pvpSeasonId' = s.id::text
                  )
                  AND NOT EXISTS (
                        SELECT 1 FROM test_characters tc WHERE tc.alt_uuid = v.source_alt_uuid
                  )
                -- The NEWEST snapshot, not the best: an alt that lost cups after
                -- its peak ranks on what it holds now. The > 0 filter is applied
                -- after the pick for the same reason.
                ORDER BY v.user_id,
                         v.source_alt_uuid,
                         v.saved_at DESC
            ) saved
            LEFT JOIN season_wins pw
                   ON pw.character_id = saved.character_id
                  AND pw.alt = saved.source_alt_uuid
            LEFT JOIN guilds pg ON pg.id = saved.parked_guild_id
            WHERE saved.score > 0
        ) e
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

/// How long one computed board is served before it is ranked again.
///
/// The ranking query costs ~0.5 s on prod (it has to detoast every saved alt's
/// character JSON), and `build` used to run it THREE times per request — count,
/// page, player row — so every arena menu open waited 1.6 s at the median and up
/// to 2.9 s (#588 side finding, 2026-11-21). The board is cosmetic; a few seconds
/// of staleness after a match is invisible, a two-second spinner is not.
const BOARD_TTL: Duration = Duration::from_secs(20);

/// The whole ranked board, as of `at`.
struct CachedBoard {
    at: Instant,
    board: Arc<Vec<LeaderboardEntry>>,
}

/// A single-flight TTL cache: concurrent misses wait on one load instead of each
/// running the query (the lock is held across the load on purpose).
struct BoardCache {
    slot: tokio::sync::Mutex<Option<CachedBoard>>,
    ttl: Duration,
}

impl BoardCache {
    const fn new(ttl: Duration) -> Self {
        Self {
            slot: tokio::sync::Mutex::const_new(None),
            ttl,
        }
    }

    async fn get_or_load<F, Fut, E>(&self, load: F) -> Result<Arc<Vec<LeaderboardEntry>>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<LeaderboardEntry>, E>>,
    {
        let mut slot = self.slot.lock().await;
        if let Some(c) = slot.as_ref() {
            if c.at.elapsed() < self.ttl {
                return Ok(c.board.clone());
            }
        }
        // A failed load leaves the previous board (if any) in place for the next
        // caller to retry; the handler serves an empty board for this request.
        let board = Arc::new(load().await?);
        *slot = Some(CachedBoard {
            at: Instant::now(),
            board: board.clone(),
        });
        Ok(board)
    }
}

static BOARD: BoardCache = BoardCache::new(BOARD_TTL);

async fn build(
    app_state: &ServerGlobal,
    character_id: Uuid,
    page: i64,
    include_player: bool,
) -> Result<LeaderboardResult, anyhow::Error> {
    let board = BOARD
        .get_or_load(|| async {
            let mut conn = app_state.db_pool.get().await?;
            // The whole board in one pass of the SAME function the website's top
            // 100 reads; count, page and the player's row are all cut from it.
            Ok::<_, anyhow::Error>(ranked_entries(&mut conn, i64::MAX, 0).await?)
        })
        .await?;
    Ok(page_of(&board, character_id, page, include_player))
}

/// Cut one page (and optionally the player's own row) out of the full board.
fn page_of(
    board: &[LeaderboardEntry],
    character_id: Uuid,
    page: i64,
    include_player: bool,
) -> LeaderboardResult {
    let total = board.len() as i64;
    let start = usize::try_from((page - 1).saturating_mul(PAGE_SIZE)).unwrap_or(usize::MAX);
    let entries: Vec<LeaderboardEntry> = board
        .iter()
        .skip(start)
        .take(PAGE_SIZE as usize)
        .cloned()
        .collect();

    // The player's own row may be on any page (retail's capture showed rank 92
    // returned alongside page 1), so it is looked up across the whole board.
    // No row in the active season answers rank 0 / score 0.
    let player_entry = include_player.then(|| {
        board
            .iter()
            .find(|e| e.character_id == character_id)
            .cloned()
            .unwrap_or_else(|| LeaderboardEntry {
                character_id,
                ..Default::default()
            })
    });

    LeaderboardResult {
        player_entry,
        leaderboard: InnerLeaderboardResult {
            total_entries: total,
            current_page: page,
            total_pages: page_count(total),
            entries,
        },
    }
}

/// How many characters are on the board. Same projection as [`ranked_entries`].
pub(crate) async fn count_entries(
    conn: &mut diesel_async::AsyncPgConnection,
) -> Result<i64, diesel::result::Error> {
    Ok(
        sql_query(format!("{RANKED_CTE} SELECT COUNT(*) AS total FROM ranked"))
            .get_result::<CountRow>(conn)
            .await?
            .total,
    )
}

/// One slice of the board, in board order — THE ranking the client's arena
/// Leaderboard tab shows. The game handler pages through it; the website's
/// "all matches" top 100 (`arena::top100_boards::get_arena_game_top`) reads its first slice, so
/// both answer from one query and cannot disagree about who is where.
pub(crate) async fn ranked_entries(
    conn: &mut diesel_async::AsyncPgConnection,
    limit: i64,
    offset: i64,
) -> Result<Vec<LeaderboardEntry>, diesel::result::Error> {
    let rows: Vec<RankedRow> = sql_query(format!(
        "{RANKED_CTE} SELECT id, user_id, name, guild_name, score, streak, rank, wins \
         FROM ranked ORDER BY rank LIMIT $1 OFFSET $2"
    ))
    .bind::<BigInt, _>(limit)
    .bind::<BigInt, _>(offset)
    .get_results(conn)
    .await?;
    Ok(rows.into_iter().map(row_to_entry).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rank: i64) -> LeaderboardEntry {
        LeaderboardEntry {
            character_id: Uuid::from_u128(rank as u128),
            rank,
            score: 1000 - rank,
            ..Default::default()
        }
    }

    #[test]
    fn page_of_cuts_pages_and_finds_the_player_anywhere() {
        let board: Vec<_> = (1..=250).map(entry).collect();
        let r = page_of(&board, Uuid::from_u128(192), 1, true);
        assert_eq!(r.leaderboard.total_entries, 250);
        assert_eq!(r.leaderboard.total_pages, 3);
        assert_eq!(r.leaderboard.entries.len(), 100);
        assert_eq!(r.leaderboard.entries[0].rank, 1);
        assert_eq!(
            r.player_entry.as_ref().unwrap().rank,
            192,
            "player row from page 2"
        );

        let p3 = page_of(&board, Uuid::nil(), 3, false);
        assert_eq!(p3.leaderboard.entries.len(), 50);
        assert_eq!(p3.leaderboard.entries[0].rank, 201);
        assert_eq!(p3.leaderboard.current_page, 3);
        assert!(p3.player_entry.is_none());

        // Past the end: empty page, not a panic. Unranked player: rank 0 row.
        let far = page_of(&board, Uuid::from_u128(9999), i64::MAX, true);
        assert!(far.leaderboard.entries.is_empty());
        let me = far.player_entry.unwrap();
        assert_eq!(
            (me.character_id, me.rank, me.score),
            (Uuid::from_u128(9999), 0, 0)
        );
    }

    /// The latency fix itself: N menu opens inside the TTL pay for ONE ranking
    /// query, not 3N. The loader sleeps like the prod query (~0.5 s measured);
    /// the cached requests must come back without paying it again.
    #[tokio::test]
    async fn board_cache_runs_the_query_once_per_ttl() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = BoardCache::new(Duration::from_millis(400));
        let loads = AtomicUsize::new(0);
        let load = || async {
            loads.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, ()>(vec![entry(1)])
        };

        let cold = Instant::now();
        cache.get_or_load(load).await.unwrap();
        let cold = cold.elapsed();

        let warm = Instant::now();
        for _ in 0..50 {
            assert_eq!(cache.get_or_load(load).await.unwrap()[0].rank, 1);
        }
        let warm = warm.elapsed();
        assert_eq!(
            AtomicUsize::load(&loads, Ordering::SeqCst),
            1,
            "50 warm requests ran no query"
        );
        assert!(
            warm < cold,
            "50 cached requests ({warm:?}) beat one cold load ({cold:?})"
        );

        // Concurrent misses share one load (single flight).
        let fresh = BoardCache::new(Duration::from_secs(60));
        let before = AtomicUsize::load(&loads, Ordering::SeqCst);
        let (a, b, c) = tokio::join!(
            fresh.get_or_load(load),
            fresh.get_or_load(load),
            fresh.get_or_load(load)
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(AtomicUsize::load(&loads, Ordering::SeqCst) - before, 1);

        // After the TTL the board is ranked again, so new cups do show up.
        tokio::time::sleep(Duration::from_millis(450)).await;
        cache.get_or_load(load).await.unwrap();
        assert_eq!(AtomicUsize::load(&loads, Ordering::SeqCst), 3);
    }

    /// A failed load must not poison the cache: the next request retries.
    #[tokio::test]
    async fn board_cache_does_not_cache_a_failure() {
        let cache = BoardCache::new(Duration::from_secs(60));
        assert!(
            cache
                .get_or_load(|| async { Err::<Vec<LeaderboardEntry>, _>("db down") })
                .await
                .is_err()
        );
        let ok = cache
            .get_or_load(|| async { Ok::<_, &str>(vec![entry(7)]) })
            .await
            .unwrap();
        assert_eq!(ok[0].rank, 7);
    }

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
            RANKED_CTE.contains("v.character ->> 'pvpTrophies'"),
            "switched-out alts must keep ranking from their saved trophy count"
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

    /// Owner design: a player may compete on multiple alts in one season, so a
    /// saved alt that is no longer the live row still belongs on the season board.
    #[test]
    fn saved_current_season_alts_stay_on_the_board() {
        assert!(
            RANKED_CTE.contains("FROM character_versions v"),
            "the board must read saved switched-out alts"
        );
        assert!(
            RANKED_CTE.contains("DISTINCT ON (v.user_id, v.source_alt_uuid)"),
            "one switched-out alt should contribute its best saved season row"
        );
        assert!(
            RANKED_CTE.contains("NOT EXISTS")
                && RANKED_CTE.contains("live.source_alt_uuid = v.source_alt_uuid"),
            "do not duplicate the alt currently occupying the live character row"
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
