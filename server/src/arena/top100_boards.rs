//! Read-only boards for the website's `/arena/top100` page (dev routes,
//! `X-Import-Token`).
//!
//! The page has four tabs. The two "Humans" tabs come from the head-to-head rating
//! (`admin::get_arena_h2h_top` for players, [`get_arena_h2h_guilds`] here for
//! guilds). The two "All matches" tabs must show EXACTLY what the game shows a
//! player in-game, so they do not re-derive anything:
//!
//! - [`get_arena_game_top`] reads [`crate::arena::leaderboards::ranked_entries`],
//!   the same function `GET /characters/{id}/leaderboards/{id}` pages through.
//! - [`get_arena_game_guilds`] reads [`crate::guild::guild_board`], built on the
//!   same ordered load `GET /guilds/leaderboard` pages through.
//!
//! Each answer is cached in-process for [`CACHE_TTL`]: the page is public and the
//! player board's query scans the season's match results.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    web::{self, Json},
};
use diesel::OptionalExtension;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    ServerGlobal,
    admin::{H2hSeasonSummary, IMPORT_SERVICE_ID, check_import_token},
    arena::{leaderboards::LeaderboardEntry, ranking, season_store::guild_weighted_score},
    error::BladeApiError,
};

/// How long one computed board is served before it is recomputed.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// Bound on distinct cached answers (season ids are caller-supplied).
const CACHE_MAX_KEYS: usize = 64;

fn cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(key: &str) -> Option<Value> {
    let map = cache().lock().ok()?;
    map.get(key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, v)| v.clone())
}

fn store(key: String, value: &Value) {
    if let Ok(mut map) = cache().lock() {
        map.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if map.len() >= CACHE_MAX_KEYS {
            map.clear();
        }
        map.insert(key, (Instant::now(), value.clone()));
    }
}

#[derive(Deserialize)]
pub struct BoardQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub season: Option<String>,
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(100).clamp(1, 100)
}

async fn active_season(
    conn: &mut diesel_async::AsyncPgConnection,
) -> Result<Option<H2hSeasonSummary>, BladeApiError> {
    Ok(diesel::sql_query(
        "SELECT id, name, starts_at, ends_at \
         FROM arena_seasons WHERE status = 'active' ORDER BY starts_at DESC LIMIT 1",
    )
    .get_result(conn)
    .await
    .optional()?)
}

// ---- All matches: players ---------------------------------------------------

/// One row of the in-game player leaderboard. Everything the client shows, minus
/// the account id (`userId`), which has no business on a public page.
#[derive(Serialize, Debug, PartialEq)]
pub struct GameTopRow {
    pub rank: i64,
    pub character_id: Uuid,
    pub name: String,
    pub guild_name: String,
    /// The in-game `score`: the character's own `pvpTrophies`.
    pub trophies: i64,
    /// `numberOfMatchesWon` this season.
    pub wins: i64,
    pub streak: i64,
}

impl From<LeaderboardEntry> for GameTopRow {
    fn from(e: LeaderboardEntry) -> Self {
        GameTopRow {
            rank: e.rank,
            character_id: e.character_id,
            name: e.character_name,
            guild_name: e.guild_name,
            trophies: e.score,
            wins: e.number_of_matches_won,
            streak: e.streak,
        }
    }
}

#[derive(Serialize)]
pub struct GameTopResponse {
    pub season: Option<H2hSeasonSummary>,
    pub total: i64,
    pub rows: Vec<GameTopRow>,
}

/// `GET /api/dev/v1/arena-ranking/game-top?limit=N` — the first N rows of the
/// in-game arena leaderboard (current season, bot and human matches alike).
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/game-top")]
pub async fn get_arena_game_top(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<BoardQuery>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let limit = clamp_limit(query.limit);
    let key = format!("game-top:{limit}");
    if let Some(v) = cached(&key) {
        return Ok(Json(v));
    }
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let season = active_season(&mut conn).await?;
    let total = crate::arena::leaderboards::count_entries(&mut conn).await?;
    let rows = crate::arena::leaderboards::ranked_entries(&mut conn, limit, 0)
        .await?
        .into_iter()
        .map(GameTopRow::from)
        .collect();
    let value = serde_json::to_value(GameTopResponse {
        season,
        total,
        rows,
    })
    .map_err(BladeApiError::generic_internal_error)?;
    store(key, &value);
    Ok(Json(value))
}

// ---- All matches: guilds ----------------------------------------------------

#[derive(Serialize)]
pub struct GameGuildsResponse {
    pub season: Option<H2hSeasonSummary>,
    pub total: i64,
    pub rows: Vec<crate::guild::GuildBoardEntry>,
}

/// `GET /api/dev/v1/arena-ranking/game-guilds?limit=N` — the first N rows of the
/// in-game guild leaderboard.
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/game-guilds")]
pub async fn get_arena_game_guilds(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<BoardQuery>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let limit = clamp_limit(query.limit);
    let key = format!("game-guilds:{limit}");
    if let Some(v) = cached(&key) {
        return Ok(Json(v));
    }
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let season = active_season(&mut conn).await?;
    let (total, rows) = crate::guild::guild_board(&mut conn, limit as usize).await?;
    let value = serde_json::to_value(GameGuildsResponse {
        season,
        total,
        rows,
    })
    .map_err(BladeApiError::generic_internal_error)?;
    store(key, &value);
    Ok(Json(value))
}

// ---- Humans: guilds ---------------------------------------------------------

/// One guild member's head-to-head standing.
#[derive(Debug, Clone, diesel::QueryableByName)]
pub struct H2hGuildMemberRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub guild_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub guild_name: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub rating: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub wins: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub losses: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub ties: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub matches: i32,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct H2hGuildRow {
    pub rank: i64,
    pub guild_id: String,
    pub name: String,
    pub score: i64,
    /// Members who are on the human player board (and so count toward `score`).
    pub members: i64,
    pub wins: i64,
    pub losses: i64,
    pub ties: i64,
    pub matches: i64,
}

/// THE human-vs-human guild rule.
///
/// A guild is scored the way the game scores guilds — [`guild_weighted_score`], the
/// in-guild rank brackets (30/25/20/15/10%, 20 members) — applied to its members'
/// head-to-head ratings instead of their trophies. So:
///
/// - only human-vs-human results count (the h2h rating never sees a bot match);
/// - a member counts only once they would appear on the human PLAYER board
///   (`matches >= min_games_listed`, filtered by the caller's query), so the two
///   Humans tabs agree about who is ranked;
/// - membership is the guild's CURRENT roster, for every season shown (the server
///   keeps no history of who was in which guild when).
///
/// Order: score, then total member h2h matches, then guild id — highest first, and
/// deterministic, as the player board is.
pub fn h2h_guild_board(members: Vec<H2hGuildMemberRow>, limit: usize) -> Vec<H2hGuildRow> {
    struct Acc {
        name: String,
        ratings: Vec<i64>,
        wins: i64,
        losses: i64,
        ties: i64,
        matches: i64,
    }
    let mut by_guild: HashMap<String, Acc> = HashMap::new();
    for m in members {
        let acc = by_guild.entry(m.guild_id).or_insert_with(|| Acc {
            name: m.guild_name.clone(),
            ratings: Vec::new(),
            wins: 0,
            losses: 0,
            ties: 0,
            matches: 0,
        });
        acc.ratings.push(m.rating as i64);
        acc.wins += m.wins as i64;
        acc.losses += m.losses as i64;
        acc.ties += m.ties as i64;
        acc.matches += m.matches as i64;
    }
    let mut rows: Vec<H2hGuildRow> = by_guild
        .into_iter()
        .map(|(guild_id, a)| H2hGuildRow {
            rank: 0,
            guild_id,
            name: a.name,
            members: a.ratings.len() as i64,
            score: guild_weighted_score(a.ratings),
            wins: a.wins,
            losses: a.losses,
            ties: a.ties,
            matches: a.matches,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.matches.cmp(&a.matches))
            .then_with(|| a.guild_id.cmp(&b.guild_id))
    });
    rows.truncate(limit);
    for (i, r) in rows.iter_mut().enumerate() {
        r.rank = i as i64 + 1;
    }
    rows
}

#[derive(Serialize)]
pub struct H2hGuildsResponse {
    pub season: Option<H2hSeasonSummary>,
    pub rows: Vec<H2hGuildRow>,
}

/// The guild members on the human PLAYER board: the same per-alt rows
/// ([`ranking::h2h_board`]), so the two Humans tabs agree about who is ranked and
/// in which guild — a switched-out alt counts for the guild it is parked in, not
/// for the live alt's guild.
pub fn h2h_guild_members(entries: Vec<ranking::H2hBoardEntry>) -> Vec<H2hGuildMemberRow> {
    entries
        .into_iter()
        .filter_map(|e| {
            let guild_id = e.guild_id?;
            Some(H2hGuildMemberRow {
                guild_id,
                guild_name: e.guild_name.unwrap_or_default(),
                rating: e.rating,
                wins: e.wins,
                losses: e.losses,
                ties: e.ties,
                matches: e.matches,
            })
        })
        .collect()
}

/// Every board row, not a top 100: a guild's score needs all its ranked members.
const ALL_BOARD_ROWS: i64 = i64::MAX;

/// `GET /api/dev/v1/arena-ranking/h2h-guilds?season=current|all|<uuid>&limit=N`.
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/h2h-guilds")]
pub async fn get_arena_h2h_guilds(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<BoardQuery>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let limit = clamp_limit(query.limit);
    let selection = ranking::parse_h2h_season_selection(query.season.as_deref())
        .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 84))?;
    let key = format!("h2h-guilds:{selection:?}:{limit}");
    if let Some(v) = cached(&key) {
        return Ok(Json(v));
    }
    let min_games = ranking::load(&app_state.db_pool)
        .await
        .config
        .h2h_rating
        .min_games_listed as i32;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let (season, entries): (Option<H2hSeasonSummary>, Vec<ranking::H2hBoardEntry>) = match selection
    {
        ranking::H2hSeasonSelection::AllTime => (
            None,
            ranking::h2h_board(&mut conn, None, min_games, ALL_BOARD_ROWS).await?,
        ),
        ranking::H2hSeasonSelection::Current => match active_season(&mut conn).await? {
            Some(season) => {
                let entries =
                    ranking::h2h_board(&mut conn, Some(season.id), min_games, ALL_BOARD_ROWS)
                        .await?;
                (Some(season), entries)
            }
            None => (None, Vec::new()),
        },
        ranking::H2hSeasonSelection::Season(season_id) => {
            let season: H2hSeasonSummary = diesel::sql_query(
                "SELECT id, name, starts_at, ends_at FROM arena_seasons WHERE id = $1",
            )
            .bind::<diesel::sql_types::Uuid, _>(season_id)
            .get_result(&mut conn)
            .await
            .map_err(|_| BladeApiError::new(StatusCode::NOT_FOUND, IMPORT_SERVICE_ID, 85))?;
            let entries =
                ranking::h2h_board(&mut conn, Some(season.id), min_games, ALL_BOARD_ROWS).await?;
            (Some(season), entries)
        }
    };
    let members = h2h_guild_members(entries);
    let rows = h2h_guild_board(members, limit as usize);
    let value = serde_json::to_value(H2hGuildsResponse { season, rows })
        .map_err(BladeApiError::generic_internal_error)?;
    store(key, &value);
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(guild: &str, rating: i32, matches: i32) -> H2hGuildMemberRow {
        H2hGuildMemberRow {
            guild_id: guild.into(),
            guild_name: format!("Guild {guild}"),
            rating,
            wins: matches / 2,
            losses: matches - matches / 2,
            ties: 0,
            matches,
        }
    }

    /// The humans guild score is the game's guild rule on h2h ratings: the SAME
    /// function the season ladder uses, not a look-alike.
    #[test]
    fn humans_guild_score_is_the_game_guild_rule_on_h2h_ratings() {
        let ratings = [1200, 1100, 1000, 990, 950];
        let rows = h2h_guild_board(ratings.iter().map(|r| member("a", *r, 4)).collect(), 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].score,
            guild_weighted_score(ratings.iter().map(|r| *r as i64).collect())
        );
        // 0.30 * (1200 + 1100 + 1000 + 990) + 0.25 * 950 = 1287 + 237.5
        assert_eq!(rows[0].score, 1525);
        assert_eq!(rows[0].members, 5);
        assert_eq!(rows[0].matches, 20);
    }

    /// CONTROL: a flat sum would rank the bigger, weaker guild first; the rank-
    /// weighted rule must not.
    #[test]
    fn a_weak_tail_does_not_outscore_strong_members() {
        let mut m = vec![member("strong", 1400, 5), member("strong", 1400, 5)];
        m.extend((0..4).map(|_| member("wide", 700, 5)));
        let rows = h2h_guild_board(m, 100);
        // strong: 0.3 * 2800 = 840; wide: 0.3 * 2800 = 840 — a tie on score; the
        // wider guild played more matches, so it ranks first on the tie-break.
        assert_eq!(rows[0].guild_id, "wide");
        assert_eq!(rows[0].score, rows[1].score);
        let rows = h2h_guild_board(
            vec![
                member("strong", 1500, 5),
                member("strong", 1500, 5),
                member("wide", 700, 5),
                member("wide", 700, 5),
                member("wide", 700, 5),
                member("wide", 700, 5),
            ],
            100,
        );
        assert_eq!(rows[0].guild_id, "strong", "900 beats 840");
    }

    #[test]
    fn order_is_score_then_matches_then_guild_id_and_ranks_are_dense() {
        let rows = h2h_guild_board(
            vec![
                member("b", 1000, 3),
                member("a", 1000, 3),
                member("c", 1000, 9),
                member("d", 1300, 3),
            ],
            100,
        );
        let order: Vec<_> = rows.iter().map(|r| (r.rank, r.guild_id.as_str())).collect();
        assert_eq!(order, vec![(1, "d"), (2, "c"), (3, "a"), (4, "b")]);
    }

    #[test]
    fn limit_truncates_after_ranking() {
        let rows = h2h_guild_board(
            vec![
                member("a", 900, 3),
                member("b", 1100, 3),
                member("c", 1000, 3),
            ],
            2,
        );
        assert_eq!(
            rows.iter().map(|r| r.guild_id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    /// The humans guild board must admit exactly the rows the human player board
    /// admits — it is built from them — and count each alt for its own guild.
    #[test]
    fn humans_guild_membership_is_the_player_board() {
        let entry = |alt: u128, guild: Option<&str>| ranking::H2hBoardEntry {
            rank: 1,
            character_id: Uuid::from_u128(1),
            alt_key: Uuid::from_u128(alt),
            name: "x".into(),
            level: 1,
            guild_id: guild.map(str::to_string),
            guild_name: guild.map(|g| format!("Guild {g}")),
            rating: 1000,
            wins: 2,
            losses: 1,
            ties: 0,
            matches: 3,
            last_match_at: None,
        };
        let members = h2h_guild_members(vec![
            entry(1, Some("a")),
            entry(2, Some("b")),
            entry(3, None),
        ]);
        assert_eq!(
            members
                .iter()
                .map(|m| m.guild_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "two alts of one character count for their own guilds; guildless rows drop out"
        );
        let src = include_str!("top100_boards.rs");
        let handler = &src[src.find("pub async fn get_arena_h2h_guilds(").unwrap()..];
        assert!(handler[..handler.find("\n}\n").unwrap()].contains("ranking::h2h_board("));
    }

    /// The website's "All matches" player rows are the in-game entries, field for
    /// field, minus the account id.
    #[test]
    fn game_top_row_is_the_in_game_entry() {
        let e = LeaderboardEntry {
            user_id: Uuid::from_u128(7),
            character_id: Uuid::from_u128(9),
            character_name: "Snake".into(),
            guild_name: "Akatosh Empire".into(),
            rank: 1,
            score: 1062,
            number_of_matches_won: 72,
            streak: 41,
        };
        let row = GameTopRow::from(e);
        assert_eq!(
            row,
            GameTopRow {
                rank: 1,
                character_id: Uuid::from_u128(9),
                name: "Snake".into(),
                guild_name: "Akatosh Empire".into(),
                trophies: 1062,
                wins: 72,
                streak: 41,
            }
        );
        let v = serde_json::to_value(&row).unwrap();
        assert!(v.get("user_id").is_none() && v.get("userId").is_none());
    }

    /// The "All matches" boards must read the game's own ranking functions. A
    /// source check, because the queries need a database: it catches a later
    /// "simplification" that re-derives the ranking here.
    #[test]
    fn all_matches_boards_read_the_in_game_ranking_functions() {
        let src = include_str!("top100_boards.rs");
        let body = |name: &str| {
            let start = src.find(&format!("pub async fn {name}(")).unwrap();
            let rest = &src[start..];
            &rest[..rest.find("\n}\n").unwrap()]
        };
        assert!(body("get_arena_game_top").contains("leaderboards::ranked_entries("));
        assert!(body("get_arena_game_top").contains("leaderboards::count_entries("));
        assert!(body("get_arena_game_guilds").contains("guild::guild_board("));

        let lb = include_str!("leaderboards.rs");
        let build = &lb[lb.find("async fn build(").unwrap()..];
        assert!(
            build[..build.find("\n}\n").unwrap()].contains("ranked_entries(&mut conn, PAGE_SIZE"),
            "the in-game player board must page through the same function"
        );
        let guild = include_str!("../guild.rs");
        let handler = &guild[guild.find("pub async fn guild_leaderboard(").unwrap()..];
        assert!(
            handler[..handler.find("\n}\n").unwrap()].contains("ranked_guild_rows(&mut conn)"),
            "the in-game guild board must load through the same function"
        );
        let board = &guild[guild.find("pub(crate) async fn guild_board(").unwrap()..];
        assert!(board[..board.find("\n}\n").unwrap()].contains("ranked_guild_rows(conn)"));
    }

    #[test]
    fn limit_is_clamped_to_a_top_100() {
        assert_eq!(clamp_limit(None), 100);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(5000)), 100);
    }

    #[test]
    fn cache_serves_within_ttl_and_is_bounded() {
        store("test:a".into(), &serde_json::json!({"x": 1}));
        assert_eq!(cached("test:a"), Some(serde_json::json!({"x": 1})));
        assert_eq!(cached("test:missing"), None);
        for i in 0..(CACHE_MAX_KEYS + 5) {
            store(format!("test:k{i}"), &serde_json::json!(i));
        }
        assert!(cache().lock().unwrap().len() <= CACHE_MAX_KEYS);
    }
}
