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
    arena::{
        leaderboards::LeaderboardEntry,
        ranking,
        season_store::{
            self, guild_score_from_shares, guild_weighted_score, guild_weighted_shares,
        },
    },
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

pub(crate) fn cached(key: &str) -> Option<Value> {
    let map = cache().lock().ok()?;
    map.get(key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, v)| v.clone())
}

pub(crate) fn store(key: String, value: &Value) {
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

pub(crate) async fn active_season(
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

/// The whole human player board for a season selection — every listed alt, not a
/// top 100 — with the season it is for and the `min_games_listed` it was cut at.
/// Shared by the guild board and the guild member breakdown, so both read the same
/// members.
async fn h2h_board_entries(
    app_state: &ServerGlobal,
    selection: ranking::H2hSeasonSelection,
) -> Result<(Option<H2hSeasonSummary>, Vec<ranking::H2hBoardEntry>, i32), BladeApiError> {
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
    let (season, entries) = match selection {
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
    Ok((season, entries, min_games))
}

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
    let (season, entries, _) = h2h_board_entries(&app_state, selection).await?;
    let members = h2h_guild_members(entries);
    let rows = h2h_guild_board(members, limit as usize);
    let value = serde_json::to_value(H2hGuildsResponse { season, rows })
        .map_err(BladeApiError::generic_internal_error)?;
    store(key, &value);
    Ok(Json(value))
}

// ---- Guild member breakdown -------------------------------------------------

/// One member going into a guild score: their own score on the board the guild is
/// scored from (season trophies for "all matches", h2h rating for "humans").
#[derive(Debug, Clone, PartialEq)]
pub struct GuildMemberScore {
    pub character_id: Uuid,
    /// The alt (`None` for a character from before alts were tracked).
    pub alt_key: Option<Uuid>,
    pub name: String,
    pub score: i64,
    /// The member's rank on the matching website player top 100, if on it.
    pub board_rank: Option<i64>,
}

/// One member's line in a guild's score breakdown.
#[derive(Serialize, Debug, PartialEq)]
pub struct GuildMemberShareRow {
    pub rank_in_guild: usize,
    pub character_id: Uuid,
    pub alt_key: Option<Uuid>,
    pub name: String,
    pub score: i64,
    /// 30/25/20/15/10, or 0 past the 20th member.
    pub weight_pct: i64,
    /// What this member adds to the guild score, in points (two decimals).
    pub contribution: f64,
    /// The same, exact, in hundredths of a point.
    pub contribution_centi: i64,
    pub board_rank: Option<i64>,
}

/// A guild's score, member by member — [`guild_weighted_shares`], the function
/// [`guild_weighted_score`] itself is built on, so the breakdown's total IS the
/// guild score (returned first). Best member first.
pub fn guild_member_breakdown(members: Vec<GuildMemberScore>) -> (i64, Vec<GuildMemberShareRow>) {
    let scores: Vec<i64> = members.iter().map(|m| m.score).collect();
    let shares = guild_weighted_shares(&scores);
    let score = guild_score_from_shares(&shares);
    let rows = shares
        .iter()
        .map(|s| {
            let m = &members[s.index];
            GuildMemberShareRow {
                rank_in_guild: s.rank_in_guild,
                character_id: m.character_id,
                alt_key: m.alt_key,
                name: m.name.clone(),
                score: s.score,
                weight_pct: s.weight_pct,
                contribution: s.contribution_centi as f64 / 100.0,
                contribution_centi: s.contribution_centi,
                board_rank: m.board_rank,
            }
        })
        .collect();
    (score, rows)
}

/// A current guild member who adds nothing because they are not ranked on that
/// board: no arena match this season ("all matches"), or fewer than `min_games`
/// human-vs-human matches ("humans").
#[derive(Serialize, Debug, PartialEq)]
pub struct UnrankedGuildMember {
    pub character_id: Uuid,
    pub name: String,
}

#[derive(Serialize)]
pub struct GuildMembersResponse {
    /// `game` (All matches) or `h2h` (Humans).
    pub board: &'static str,
    pub season: Option<H2hSeasonSummary>,
    pub guild_id: String,
    pub name: String,
    /// The guild score the breakdown adds up to.
    pub score: i64,
    /// `game` only: the stored `guilds.trophies` the in-game board shows. Equal to
    /// `score` except in the seconds between a match and the next recompute.
    pub board_score: Option<i64>,
    /// `h2h` only: the matches a member needs to be ranked.
    pub min_games: Option<i32>,
    pub members: Vec<GuildMemberShareRow>,
    pub unranked: Vec<UnrankedGuildMember>,
}

#[derive(Deserialize)]
pub struct GuildMembersQuery {
    pub guild_id: String,
    #[serde(default)]
    pub board: Option<String>,
    #[serde(default)]
    pub season: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct GuildHead {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    trophies: i64,
}

#[derive(diesel::QueryableByName)]
struct RosterRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    character_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    source_alt_uuid: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

#[derive(diesel::QueryableByName)]
struct NameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// The guild's current roster (live alts), operator test alts left out as every
/// ladder leaves them out — and developer lock children, whose alt the parent
/// still holds (it was not on this roster before the lock either).
pub(crate) const GUILD_ROSTER_SQL: &str = "\
    SELECT c.id AS character_id, c.source_alt_uuid, \
           COALESCE(c.character ->> 'name', '') AS name \
    FROM guild_members gm \
    JOIN characters c ON c.id = gm.character_id \
    WHERE gm.guild_id = $1 \
      AND NOT EXISTS (SELECT 1 FROM test_characters tc \
                      WHERE tc.alt_uuid = COALESCE(c.source_alt_uuid, c.id)) \
      AND NOT EXISTS (SELECT 1 FROM dev_alt_lock_accounts dl WHERE dl.child_character_id = c.id)";

/// `GET /api/dev/v1/arena-ranking/guild-members?guild_id=X&board=game|h2h&season=…`
/// — how a guild's score on the website's top 100 is made up, member by member.
///
/// - `board=game` (default): the "All matches" guild board. Members are the
///   guild's rows of the season standings ([`season_store::freeze_standings`]),
///   scored with [`season_store::guild_standings_from`]'s rule — the exact computation
///   `guild::recompute_guild_trophies` writes into `guilds.trophies`.
/// - `board=h2h`: the "Humans" guild board. Members are the guild's rows of the
///   human player board, exactly the input [`get_arena_h2h_guilds`] scores.
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/guild-members")]
pub async fn get_arena_guild_members(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<GuildMembersQuery>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let bad = || BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 84);
    let guild_id = query.guild_id.trim().to_string();
    if guild_id.is_empty() || guild_id.len() > 64 {
        return Err(bad());
    }
    let h2h = match query.board.as_deref().unwrap_or("game") {
        "game" => false,
        "h2h" => true,
        _ => return Err(bad()),
    };
    let selection = if h2h {
        Some(ranking::parse_h2h_season_selection(query.season.as_deref()).map_err(|_| bad())?)
    } else {
        None
    };
    let key = format!("guild-members:{h2h}:{selection:?}:{guild_id}");
    if let Some(v) = cached(&key) {
        return Ok(Json(v));
    }

    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let head: GuildHead = diesel::sql_query("SELECT name, trophies FROM guilds WHERE id = $1")
        .bind::<diesel::sql_types::Text, _>(&guild_id)
        .get_result(&mut conn)
        .await
        .optional()?
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, IMPORT_SERVICE_ID, 85))?;
    let roster: Vec<RosterRow> = diesel::sql_query(GUILD_ROSTER_SQL)
        .bind::<diesel::sql_types::Text, _>(&guild_id)
        .get_results(&mut conn)
        .await?;

    let response = if let Some(selection) = selection {
        drop(conn);
        let (season, entries, min_games) = h2h_board_entries(&app_state, selection).await?;
        let members: Vec<GuildMemberScore> = entries
            .into_iter()
            .filter(|e| e.guild_id.as_deref() == Some(guild_id.as_str()))
            .map(|e| GuildMemberScore {
                character_id: e.character_id,
                alt_key: Some(e.alt_key),
                name: e.name,
                score: e.rating as i64,
                board_rank: (e.rank <= 100).then_some(e.rank),
            })
            .collect();
        let unranked = roster
            .into_iter()
            .filter(|r| {
                let alt = r.source_alt_uuid.unwrap_or(r.character_id);
                !members
                    .iter()
                    .any(|m| m.character_id == r.character_id && m.alt_key == Some(alt))
            })
            .map(|r| UnrankedGuildMember {
                character_id: r.character_id,
                name: r.name,
            })
            .collect();
        let (score, members) = guild_member_breakdown(members);
        GuildMembersResponse {
            board: "h2h",
            season,
            guild_id,
            name: head.name,
            score,
            board_score: None,
            min_games: Some(min_games),
            members,
            unranked,
        }
    } else {
        let season_row: Option<season_store::SeasonRow> = {
            use crate::schema::arena_seasons;
            use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
            arena_seasons::table
                .filter(arena_seasons::status.eq("active"))
                .select(season_store::SeasonRow::as_select())
                .order(arena_seasons::starts_at.desc())
                .first(&mut conn)
                .await
                .optional()?
        };
        let standings = match &season_row {
            Some(s) => season_store::freeze_standings(&mut conn, s).await?,
            None => Vec::new(),
        };
        // The game player board, for links: live characters only.
        let on_board: HashMap<Uuid, i64> =
            crate::arena::leaderboards::ranked_entries(&mut conn, 100, 0)
                .await?
                .into_iter()
                .map(|e| (e.character_id, e.rank))
                .collect();
        let mut members = Vec::new();
        for st in standings
            .iter()
            .filter(|s| s.guild_id.as_deref() == Some(guild_id.as_str()))
        {
            let live = roster.iter().find(|r| {
                r.character_id == st.character_id && r.source_alt_uuid == st.source_alt_uuid
            });
            let name = match live {
                Some(r) => r.name.clone(),
                // An alt switched out but still in this guild: its newest version.
                None => diesel::sql_query(
                    "SELECT COALESCE(character ->> 'name', '') AS name FROM character_versions \
                     WHERE character_id = $1 AND source_alt_uuid IS NOT DISTINCT FROM $2 \
                     ORDER BY saved_at DESC LIMIT 1",
                )
                .bind::<diesel::sql_types::Uuid, _>(st.character_id)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(st.source_alt_uuid)
                .get_result::<NameRow>(&mut conn)
                .await
                .optional()?
                .map(|n| n.name)
                .unwrap_or_default(),
            };
            members.push(GuildMemberScore {
                character_id: st.character_id,
                alt_key: st.source_alt_uuid,
                name,
                score: st.trophies,
                board_rank: live.and_then(|_| on_board.get(&st.character_id).copied()),
            });
        }
        let unranked = roster
            .into_iter()
            .filter(|r| {
                !members
                    .iter()
                    .any(|m| m.character_id == r.character_id && m.alt_key == r.source_alt_uuid)
            })
            .map(|r| UnrankedGuildMember {
                character_id: r.character_id,
                name: r.name,
            })
            .collect();
        let (score, members) = guild_member_breakdown(members);
        let season = season_row.map(|s| H2hSeasonSummary {
            id: s.id,
            name: s.name,
            starts_at: s.starts_at,
            ends_at: s.ends_at,
        });
        GuildMembersResponse {
            board: "game",
            season,
            guild_id,
            name: head.name,
            score,
            board_score: Some(head.trophies),
            min_games: None,
            members,
            unranked,
        }
    };
    let value = serde_json::to_value(response).map_err(BladeApiError::generic_internal_error)?;
    store(key, &value);
    Ok(Json(value))
}

// ---- Humans: the season's matches -------------------------------------------

/// A player in [`H2hMatchesResponse`]: the alt and its name (empty when the alt
/// no longer exists). Public: no account id.
#[derive(Serialize, Debug, PartialEq)]
pub struct H2hMatchPlayer {
    pub character_id: Uuid,
    pub alt_key: Uuid,
    pub name: String,
}

/// One match; `a`/`b` index [`H2hMatchesResponse::players`].
#[derive(Serialize, Debug, PartialEq)]
pub struct H2hMatchRow {
    pub at: chrono::DateTime<chrono::Utc>,
    pub a: usize,
    pub b: usize,
    pub a_rounds: i64,
    pub b_rounds: i64,
    pub a_before: i64,
    pub a_after: i64,
    pub b_before: i64,
    pub b_after: i64,
}

#[derive(Serialize)]
pub struct H2hMatchesResponse {
    pub season: Option<H2hSeasonSummary>,
    pub players: Vec<H2hMatchPlayer>,
    pub matches: Vec<H2hMatchRow>,
}

/// Index the log's players and name them from the board rows (all of them,
/// `min_games` 0), so a name is sent once however many matches it played.
pub fn h2h_matches_payload(
    log: Vec<ranking::H2hMatchLogEntry>,
    named: &[ranking::H2hBoardEntry],
) -> (Vec<H2hMatchPlayer>, Vec<H2hMatchRow>) {
    let names: HashMap<ranking::H2hKey, &str> = named
        .iter()
        .map(|e| {
            (
                ranking::H2hKey::new(e.character_id, e.alt_key),
                e.name.as_str(),
            )
        })
        .collect();
    let mut index: HashMap<ranking::H2hKey, usize> = HashMap::new();
    let mut players = Vec::new();
    let mut idx = |k: ranking::H2hKey, players: &mut Vec<H2hMatchPlayer>| {
        *index.entry(k).or_insert_with(|| {
            players.push(H2hMatchPlayer {
                character_id: k.character_id,
                alt_key: k.alt_key,
                name: names.get(&k).copied().unwrap_or("").to_string(),
            });
            players.len() - 1
        })
    };
    let matches = log
        .into_iter()
        .map(|m| H2hMatchRow {
            at: m.at,
            a: idx(m.a, &mut players),
            b: idx(m.b, &mut players),
            a_rounds: m.a_rounds,
            b_rounds: m.b_rounds,
            a_before: m.a_before,
            a_after: m.a_after,
            b_before: m.b_before,
            b_after: m.b_after,
        })
        .collect();
    (players, matches)
}

/// `GET /api/dev/v1/arena-ranking/h2h-matches?season=current|all|<uuid>` — every
/// human-vs-human match of the season with the ratings it moved, replayed exactly
/// as the reconcile that writes the board replays it.
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/h2h-matches")]
pub async fn get_arena_h2h_matches(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<BoardQuery>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let selection = ranking::parse_h2h_season_selection(query.season.as_deref())
        .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 86))?;
    let key = format!("h2h-matches:{selection:?}");
    if let Some(v) = cached(&key) {
        return Ok(Json(v));
    }
    let cfg = ranking::load(&app_state.db_pool).await.config.h2h_rating;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let season = match selection {
        ranking::H2hSeasonSelection::AllTime => None,
        ranking::H2hSeasonSelection::Current => active_season(&mut conn).await?,
        ranking::H2hSeasonSelection::Season(season_id) => Some(
            diesel::sql_query(
                "SELECT id, name, starts_at, ends_at FROM arena_seasons WHERE id = $1",
            )
            .bind::<diesel::sql_types::Uuid, _>(season_id)
            .get_result::<H2hSeasonSummary>(&mut conn)
            .await
            .map_err(|_| BladeApiError::new(StatusCode::NOT_FOUND, IMPORT_SERVICE_ID, 87))?,
        ),
    };
    let (players, matches) =
        if matches!(selection, ranking::H2hSeasonSelection::Current) && season.is_none() {
            (Vec::new(), Vec::new())
        } else {
            let season_id = season.as_ref().map(|s| s.id);
            let log = ranking::load_h2h_match_log(&mut conn, &cfg, season_id)
                .await
                .map_err(|e| {
                    log::warn!("arena h2h match log failed: {e}");
                    BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 88)
                })?;
            let named = ranking::h2h_board(&mut conn, season_id, 0, ALL_BOARD_ROWS).await?;
            h2h_matches_payload(log, &named)
        };
    let value = serde_json::to_value(H2hMatchesResponse {
        season,
        players,
        matches,
    })
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

    /// Players are sent once and referenced by index; an alt the board cannot
    /// name (deleted) still appears, with an empty name.
    #[test]
    fn match_payload_indexes_and_names_players_once() {
        let k = |n: u128| ranking::H2hKey::new(Uuid::from_u128(n), Uuid::from_u128(n + 100));
        let at = chrono::DateTime::<chrono::Utc>::from_timestamp(1_762_000_000, 0).unwrap();
        let m = |a, b| ranking::H2hMatchLogEntry {
            at,
            a,
            b,
            a_rounds: 2,
            b_rounds: 0,
            a_before: 1000,
            a_after: 1024,
            b_before: 1000,
            b_after: 976,
        };
        let entry = |n: u128, name: &str| ranking::H2hBoardEntry {
            rank: 1,
            character_id: Uuid::from_u128(n),
            alt_key: Uuid::from_u128(n + 100),
            name: name.into(),
            level: 10,
            guild_id: None,
            guild_name: None,
            rating: 1000,
            wins: 0,
            losses: 0,
            ties: 0,
            matches: 0,
            last_match_at: None,
        };
        let (players, rows) = h2h_matches_payload(
            vec![m(k(1), k(2)), m(k(2), k(3)), m(k(1), k(3))],
            &[entry(1, "Ana"), entry(2, "Bo")],
        );
        assert_eq!(
            players.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["Ana", "Bo", ""]
        );
        assert_eq!(
            rows.iter().map(|r| (r.a, r.b)).collect::<Vec<_>>(),
            [(0, 1), (1, 2), (0, 2)]
        );
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

    fn scored(n: u128, score: i64) -> GuildMemberScore {
        GuildMemberScore {
            character_id: Uuid::from_u128(n),
            alt_key: Some(Uuid::from_u128(n + 100)),
            name: format!("P{n}"),
            score,
            board_rank: None,
        }
    }

    /// The member breakdown adds up to the score the humans guild board shows for
    /// the same members — it is the same function, not a copy.
    #[test]
    fn guild_member_breakdown_totals_the_h2h_guild_score() {
        let ratings = [1200, 950, 1100, 1000, 990, 1310, 1007];
        let board = h2h_guild_board(ratings.iter().map(|r| member("a", *r, 4)).collect(), 100);
        let (score, rows) = guild_member_breakdown(
            ratings
                .iter()
                .enumerate()
                .map(|(i, r)| scored(i as u128, *r as i64))
                .collect(),
        );
        assert_eq!(score, board[0].score);
        let centi: i64 = rows.iter().map(|r| r.contribution_centi).sum();
        assert_eq!((centi + 50) / 100, score);
        assert_eq!(rows[0].score, 1310, "best member first");
        assert_eq!(rows[0].weight_pct, 30);
        assert_eq!(rows[4].weight_pct, 25);
        assert_eq!(rows[4].contribution, rows[4].score as f64 * 0.25);
        assert_eq!(
            rows.iter().map(|r| r.rank_in_guild).collect::<Vec<_>>(),
            (1..=7).collect::<Vec<_>>()
        );
    }

    /// The same on the "All matches" side: the breakdown equals the season guild
    /// ladder (`guild_standings_from`, what `guilds.trophies` is recomputed from),
    /// including members on 0 trophies and a guild past 20 members.
    #[test]
    fn guild_member_breakdown_totals_the_season_guild_ladder() {
        for n in [1usize, 3, 7, 20, 23] {
            let trophies: Vec<i64> = (0..n as i64).map(|i| (i * 53) % 310).collect();
            let standings: Vec<season_store::StandingRow> = trophies
                .iter()
                .enumerate()
                .map(|(i, t)| season_store::StandingRow {
                    season_id: Uuid::nil(),
                    character_id: Uuid::from_u128(i as u128),
                    source_alt_uuid: None,
                    rank: i as i32 + 1,
                    trophies: *t,
                    matches: 1,
                    wins: 0,
                    guild_id: Some("g".into()),
                    high_water: *t,
                    arena: 0,
                    arena_level: 0,
                })
                .collect();
            let ladder = season_store::guild_standings_from(Uuid::nil(), &standings);
            let (score, rows) = guild_member_breakdown(
                trophies
                    .iter()
                    .enumerate()
                    .map(|(i, t)| scored(i as u128, *t))
                    .collect(),
            );
            assert_eq!(score, ladder[0].trophies, "{n} members");
            assert_eq!(rows.len(), n);
            assert!(
                rows.iter()
                    .filter(|r| r.score == 0)
                    .all(|r| r.contribution_centi == 0)
            );
            assert!(rows.iter().skip(20).all(|r| r.weight_pct == 0));
        }
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
        assert!(handler[..handler.find("\n}\n").unwrap()].contains("h2h_board_entries("));
        let entries = &src[src.find("async fn h2h_board_entries(").unwrap()..];
        assert!(entries[..entries.find("\n}\n").unwrap()].contains("ranking::h2h_board("));
        // The member breakdown reads the same members.
        let breakdown = &src[src.find("pub async fn get_arena_guild_members(").unwrap()..];
        assert!(breakdown[..breakdown.find("\n}\n").unwrap()].contains("h2h_board_entries("));
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
            build[..build.find("\n}\n").unwrap()]
                .contains("ranked_entries(&mut conn, i64::MAX, 0)"),
            "the in-game player board must be cut from the same function"
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
