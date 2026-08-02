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
//! `characters.character->>'pvpTrophies'`, descending — the same field the ladder
//! promotes on, and the one every op49 victory card reports. Characters that have
//! never scored are left off the board entirely (retail did the same: a player
//! with 0 cups comes back as `"rank": 0` in `playerEntry` and does not appear in
//! `entries`).
//!
//! `numberOfMatchesWon` comes from the Phase-5.4 `arena_match_results` audit
//! table. That table is created by a migration that has to be applied by hand on
//! the box, so a missing table degrades to `0` wins rather than failing the
//! request.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{
    get,
    web::{self, Json},
};
use diesel::sql_types::{Array, BigInt, Nullable, Text, Uuid as SqlUuid};
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
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    total: i64,
}

#[derive(QueryableByName, Debug)]
struct WinRow {
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = BigInt)]
    wins: i64,
}

/// The ranked projection every query below selects from. Kept in one place so the
/// page query, the count and the player's own row can never disagree about who is
/// on the board or in what order.
const RANKED_CTE: &str = "
    WITH ranked AS (
        SELECT c.id,
               c.user_id,
               COALESCE(c.character ->> 'name', '') AS name,
               g.name AS guild_name,
               COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) AS score,
               COALESCE((c.character ->> 'pvpWinningStreak')::bigint, 0) AS streak,
               ROW_NUMBER() OVER (
                   ORDER BY COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) DESC,
                            COALESCE(c.character ->> 'name', '') ASC
               ) AS rank
        FROM characters c
        LEFT JOIN guild_members gm ON gm.character_id = c.id
        LEFT JOIN guilds g ON g.id = gm.guild_id
        WHERE COALESCE((c.character ->> 'pvpTrophies')::bigint, 0) > 0
    )
";

fn row_to_entry(r: RankedRow, wins: &HashMap<Uuid, i64>) -> LeaderboardEntry {
    LeaderboardEntry {
        user_id: r.user_id,
        character_id: r.id,
        character_name: r.name,
        guild_name: r.guild_name.unwrap_or_default(),
        rank: r.rank,
        score: r.score,
        number_of_matches_won: wins.get(&r.id).copied().unwrap_or(0),
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
        "{RANKED_CTE} SELECT id, user_id, name, guild_name, score, streak, rank \
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
            "{RANKED_CTE} SELECT id, user_id, name, guild_name, score, streak, rank \
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

    // Win counts for exactly the characters we are about to serialize. Best
    // effort: the audit table is created by a hand-applied migration, so a
    // missing table means "0 wins", not a 500.
    let mut ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    if let Some(p) = &player_row {
        ids.push(p.id);
    }
    let wins: HashMap<Uuid, i64> = if ids.is_empty() {
        HashMap::new()
    } else {
        match sql_query(
            "SELECT character_id, COUNT(*) AS wins FROM arena_match_results \
             WHERE win AND character_id = ANY($1) GROUP BY character_id",
        )
        .bind::<Array<SqlUuid>, _>(ids)
        .get_results::<WinRow>(&mut conn)
        .await
        {
            Ok(rs) => rs.into_iter().map(|r| (r.character_id, r.wins)).collect(),
            Err(e) => {
                warn!(
                    "leaderboard: arena_match_results unavailable ({e}); \
                     reporting 0 matches won (apply the Phase-5.4 migration)"
                );
                HashMap::new()
            }
        }
    };

    let player_entry = if include_player {
        Some(match player_row {
            Some(r) => row_to_entry(r, &wins),
            // Unranked (never scored a trophy) — retail answers rank 0 / score 0.
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
            entries: rows.into_iter().map(|r| row_to_entry(r, &wins)).collect(),
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
