//! Arena ranking v2: configurable trophy deltas plus the human-only Elo board.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use diesel::OptionalExtension;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DbPool;
use crate::arena::arena_ladder::{ELO_LOGISTIC_SCALE, MatchOutcome};

const CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotPricing {
    Own,
    OwnJitter,
    Mimic,
}

impl Default for BotPricing {
    fn default() -> Self {
        Self::Own
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotMode {
    Fixed,
    Elo,
}

impl Default for BotMode {
    fn default() -> Self {
        Self::Fixed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanMode {
    Flat,
    Fixed,
    Elo,
}

impl Default for HumanMode {
    fn default() -> Self {
        Self::Flat
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedDeltas {
    pub win_2_0: i64,
    pub win_2_1: i64,
    pub loss_1_2: i64,
    pub loss_0_2: i64,
    pub tie: i64,
}

impl FixedDeltas {
    pub const fn bot_default() -> Self {
        Self { win_2_0: 32, win_2_1: 28, loss_1_2: -16, loss_0_2: -20, tie: 0 }
    }

    pub const fn human_default() -> Self {
        Self { win_2_0: 40, win_2_1: 35, loss_1_2: -10, loss_0_2: -15, tie: 0 }
    }

    pub fn delta(&self, outcome: MatchOutcome) -> i64 {
        match (outcome.rounds_won, outcome.rounds_lost) {
            (2, 0) => self.win_2_0,
            (2, 1) => self.win_2_1,
            (1, 2) => self.loss_1_2,
            (0, 2) => self.loss_0_2,
            _ if outcome.rounds_won == outcome.rounds_lost => self.tie,
            _ if outcome.win => self.win_2_1,
            _ => self.loss_1_2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreTable {
    pub win_2_0: f64,
    pub win_2_1: f64,
    pub loss_1_2: f64,
    pub loss_0_2: f64,
    pub tie: f64,
}

impl Default for ScoreTable {
    fn default() -> Self {
        Self { win_2_0: 1.0, win_2_1: 0.92, loss_1_2: 0.12, loss_0_2: 0.0, tie: 0.5 }
    }
}

impl ScoreTable {
    fn score(&self, outcome: MatchOutcome) -> f64 {
        match (outcome.rounds_won, outcome.rounds_lost) {
            (2, 0) => self.win_2_0,
            (2, 1) => self.win_2_1,
            (1, 2) => self.loss_1_2,
            (0, 2) => self.loss_0_2,
            _ if outcome.rounds_won == outcome.rounds_lost => self.tie,
            _ if outcome.win => self.win_2_1,
            _ => self.loss_1_2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EloConfig {
    pub k_table: Vec<[i64; 2]>,
    pub score: ScoreTable,
    pub min_win: i64,
    pub max_loss: i64,
}

impl Default for EloConfig {
    fn default() -> Self {
        Self {
            k_table: vec![[0, 100], [500, 90], [1000, 80], [1500, 70], [2000, 60], [2500, 50]],
            score: ScoreTable::default(),
            min_win: 1,
            max_loss: -1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotConfig {
    #[serde(default)]
    pub pricing: BotPricing,
    #[serde(default = "default_bot_jitter")]
    pub jitter: i64,
    #[serde(default)]
    pub mode: BotMode,
    #[serde(default = "FixedDeltas::bot_default")]
    pub fixed: FixedDeltas,
    #[serde(default = "default_avoid_recent")]
    pub avoid_recent_opponents: usize,
}

fn default_bot_jitter() -> i64 {
    50
}

fn default_avoid_recent() -> usize {
    3
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            pricing: BotPricing::Own,
            jitter: default_bot_jitter(),
            mode: BotMode::Fixed,
            fixed: FixedDeltas::bot_default(),
            avoid_recent_opponents: default_avoid_recent(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanConfig {
    #[serde(default)]
    pub mode: HumanMode,
    #[serde(default = "default_flat_award")]
    pub flat_award: i64,
    #[serde(default = "default_pair_daily_cap")]
    pub pair_daily_cap: usize,
    #[serde(default = "FixedDeltas::human_default")]
    pub fixed: FixedDeltas,
}

fn default_flat_award() -> i64 {
    50
}

fn default_pair_daily_cap() -> usize {
    3
}

impl Default for HumanConfig {
    fn default() -> Self {
        Self {
            mode: HumanMode::Flat,
            flat_award: default_flat_award(),
            pair_daily_cap: default_pair_daily_cap(),
            fixed: FixedDeltas::human_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct H2hRatingConfig {
    pub start: i64,
    pub k: i64,
    pub k_provisional: i64,
    pub provisional_games: i64,
    pub min_games_listed: i64,
}

impl Default for H2hRatingConfig {
    fn default() -> Self {
        Self { start: 1000, k: 32, k_provisional: 48, provisional_games: 10, min_games_listed: 3 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankingConfig {
    #[serde(default)]
    pub bot: BotConfig,
    #[serde(default)]
    pub human: HumanConfig,
    #[serde(default)]
    pub elo: EloConfig,
    #[serde(default)]
    pub h2h_rating: H2hRatingConfig,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            bot: BotConfig::default(),
            human: HumanConfig::default(),
            elo: EloConfig::default(),
            h2h_rating: H2hRatingConfig::default(),
        }
    }
}

pub type ValidationErrors = BTreeMap<String, String>;

impl RankingConfig {
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_fixed("bot.fixed", &self.bot.fixed, &mut errors);
        validate_fixed("human.fixed", &self.human.fixed, &mut errors);
        if !(0..=1_000).contains(&self.bot.jitter) {
            errors.insert("bot.jitter".into(), "must be between 0 and 1000".into());
        }
        if self.bot.avoid_recent_opponents > 50 {
            errors.insert("bot.avoid_recent_opponents".into(), "must be at most 50".into());
        }
        if self.human.flat_award < 0 {
            errors.insert("human.flat_award".into(), "must be non-negative".into());
        }
        if self.human.pair_daily_cap > 100 {
            errors.insert("human.pair_daily_cap".into(), "must be at most 100".into());
        }
        if self.elo.k_table.is_empty() {
            errors.insert("elo.k_table".into(), "must contain at least one band".into());
        }
        let mut last = None;
        for (i, [floor, k]) in self.elo.k_table.iter().copied().enumerate() {
            if floor < 0 {
                errors.insert(format!("elo.k_table[{i}][0]"), "must be non-negative".into());
            }
            if k <= 0 {
                errors.insert(format!("elo.k_table[{i}][1]"), "must be positive".into());
            }
            if let Some(prev) = last
                && floor <= prev
            {
                errors.insert("elo.k_table".into(), "floors must be strictly ascending".into());
            }
            last = Some(floor);
        }
        if self.elo.k_table.as_slice().first().is_some_and(|x| x[0] != 0) {
            errors.insert("elo.k_table[0][0]".into(), "must start at 0".into());
        }
        for (field, score) in [
            ("elo.score.win_2_0", self.elo.score.win_2_0),
            ("elo.score.win_2_1", self.elo.score.win_2_1),
            ("elo.score.loss_1_2", self.elo.score.loss_1_2),
            ("elo.score.loss_0_2", self.elo.score.loss_0_2),
            ("elo.score.tie", self.elo.score.tie),
        ] {
            if !(0.0..=1.0).contains(&score) || !score.is_finite() {
                errors.insert(field.into(), "must be a finite value between 0 and 1".into());
            }
        }
        if self.elo.min_win < 0 {
            errors.insert("elo.min_win".into(), "must be non-negative".into());
        }
        if self.elo.max_loss > 0 {
            errors.insert("elo.max_loss".into(), "must be non-positive".into());
        }
        if self.h2h_rating.start <= 0 {
            errors.insert("h2h_rating.start".into(), "must be positive".into());
        }
        if self.h2h_rating.k <= 0 {
            errors.insert("h2h_rating.k".into(), "must be positive".into());
        }
        if self.h2h_rating.k_provisional <= 0 {
            errors.insert("h2h_rating.k_provisional".into(), "must be positive".into());
        }
        if self.h2h_rating.provisional_games < 0 {
            errors.insert("h2h_rating.provisional_games".into(), "must be non-negative".into());
        }
        if self.h2h_rating.min_games_listed < 0 {
            errors.insert("h2h_rating.min_games_listed".into(), "must be non-negative".into());
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}

fn validate_fixed(prefix: &str, fixed: &FixedDeltas, errors: &mut ValidationErrors) {
    if fixed.win_2_0 < fixed.win_2_1 || fixed.win_2_1 < 0 {
        errors.insert(format!("{prefix}.win_2_0"), "wins must be non-negative and 2-0 >= 2-1".into());
    }
    if fixed.loss_0_2 > fixed.loss_1_2 || fixed.loss_1_2 > 0 {
        errors.insert(format!("{prefix}.loss_0_2"), "losses must be non-positive and 0-2 <= 1-2".into());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadedRankingConfig {
    pub version: i32,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_by: Option<String>,
    pub note: Option<String>,
    pub config: RankingConfig,
}

impl LoadedRankingConfig {
    pub fn defaults() -> Self {
        Self {
            version: 0,
            updated_at: None,
            updated_by: None,
            note: None,
            config: RankingConfig::default(),
        }
    }
}

#[derive(Clone)]
struct CachedConfig {
    loaded_at: Instant,
    loaded: LoadedRankingConfig,
}

static CACHE: LazyLock<Mutex<Option<CachedConfig>>> = LazyLock::new(|| Mutex::new(None));

#[derive(diesel::QueryableByName)]
struct ConfigRow {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    version: i32,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    config: serde_json::Value,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    updated_by: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    updated_at: DateTime<Utc>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    note: Option<String>,
}

pub async fn load(pool: &DbPool) -> LoadedRankingConfig {
    if let Some(cached) = CACHE.lock().unwrap().clone()
        && cached.loaded_at.elapsed() <= CACHE_TTL
    {
        return cached.loaded;
    }

    let loaded = match load_uncached(pool).await {
        Ok(Some(row)) => row,
        Ok(None) => LoadedRankingConfig::defaults(),
        Err(e) => {
            log::warn!("arena ranking: config load failed; using defaults: {e}");
            LoadedRankingConfig::defaults()
        }
    };
    *CACHE.lock().unwrap() = Some(CachedConfig {
        loaded_at: Instant::now(),
        loaded: loaded.clone(),
    });
    loaded
}

async fn load_uncached(pool: &DbPool) -> Result<Option<LoadedRankingConfig>, anyhow::Error> {
    let mut conn = pool.get().await?;
    let row: Option<ConfigRow> = diesel::sql_query(
        "SELECT version, config, updated_by, updated_at, note \
         FROM arena_ranking_config ORDER BY version DESC LIMIT 1",
    )
    .get_result(&mut conn)
    .await
    .optional()?;
    let Some(row) = row else { return Ok(None) };
    let config: RankingConfig = serde_json::from_value(row.config)?;
    if let Err(errors) = config.validate() {
        anyhow::bail!("active config v{} does not validate: {errors:?}", row.version);
    }
    Ok(Some(LoadedRankingConfig {
        version: row.version,
        updated_at: Some(row.updated_at),
        updated_by: row.updated_by,
        note: row.note,
        config,
    }))
}

pub fn invalidate_cache() {
    *CACHE.lock().unwrap() = None;
}

#[derive(Debug, Clone)]
pub struct MatchRankingContext {
    pub config: RankingConfig,
    pub h2h_pair_matches_today: usize,
}

impl Default for MatchRankingContext {
    fn default() -> Self {
        Self { config: RankingConfig::default(), h2h_pair_matches_today: 0 }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TrophyDeltaInput {
    pub outcome: MatchOutcome,
    pub own_trophies: i64,
    pub opponent_trophies_for_pricing: i64,
    pub h2h: bool,
    pub h2h_pair_matches_today: usize,
}

pub fn trophy_delta(config: &RankingConfig, input: TrophyDeltaInput) -> i64 {
    if input.h2h {
        return match config.human.mode {
            HumanMode::Flat => {
                if input.h2h_pair_matches_today < config.human.pair_daily_cap {
                    config.human.flat_award
                } else {
                    0
                }
            }
            HumanMode::Fixed => config.human.fixed.delta(input.outcome),
            HumanMode::Elo => elo_delta(&config.elo, input.outcome, input.own_trophies, input.opponent_trophies_for_pricing),
        };
    }
    match config.bot.mode {
        BotMode::Fixed => config.bot.fixed.delta(input.outcome),
        BotMode::Elo => {
            let delta = elo_delta(&config.elo, input.outcome, input.own_trophies, input.opponent_trophies_for_pricing);
            if config.bot.pricing == BotPricing::Mimic && delta < -80 {
                -80
            } else {
                delta
            }
        }
    }
}

pub fn elo_delta(config: &EloConfig, outcome: MatchOutcome, own_trophies: i64, opponent_trophies: i64) -> i64 {
    let expected =
        1.0 / (1.0 + 10f64.powf((opponent_trophies - own_trophies) as f64 / ELO_LOGISTIC_SCALE));
    let k = k_factor(&config.k_table, own_trophies) as f64;
    let rounded = (k * (config.score.score(outcome) - expected)).round() as i64;
    if outcome.rounds_won == outcome.rounds_lost {
        rounded
    } else if outcome.win {
        rounded.max(config.min_win)
    } else {
        rounded.min(config.max_loss)
    }
}

pub fn k_factor(table: &[[i64; 2]], trophies: i64) -> i64 {
    let mut k = table.first().map(|x| x[1]).unwrap_or(1);
    for [floor, band_k] in table {
        if trophies >= *floor {
            k = *band_k;
        }
    }
    k
}

pub fn bot_pricing_trophies(
    config: &RankingConfig,
    own_trophies: i64,
    mimic_matchmaking_trophies: i64,
    game_session_id: Uuid,
    slot: usize,
) -> i64 {
    match config.bot.pricing {
        BotPricing::Own => own_trophies,
        BotPricing::OwnJitter => {
            let jitter = config.bot.jitter.max(0);
            if jitter == 0 {
                return own_trophies;
            }
            (own_trophies + deterministic_jitter(game_session_id, slot, jitter)).max(0)
        }
        BotPricing::Mimic => {
            if mimic_matchmaking_trophies > 0 {
                mimic_matchmaking_trophies
            } else {
                own_trophies
            }
        }
    }
}

fn deterministic_jitter(game_session_id: Uuid, slot: usize, jitter: i64) -> i64 {
    let span = jitter.saturating_mul(2).saturating_add(1).max(1) as u64;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in game_session_id
        .as_bytes()
        .iter()
        .copied()
        .chain((slot as u64).to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % span) as i64 - jitter
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H2hRatingState {
    pub rating: i64,
    pub wins: i64,
    pub losses: i64,
    pub ties: i64,
    pub matches: i64,
}

impl H2hRatingState {
    pub fn new(start: i64) -> Self {
        Self { rating: start, wins: 0, losses: 0, ties: 0, matches: 0 }
    }
}

pub fn apply_h2h_rating(
    config: &H2hRatingConfig,
    a: H2hRatingState,
    b: H2hRatingState,
    a_outcome: MatchOutcome,
) -> (H2hRatingState, H2hRatingState) {
    let a_score = if a_outcome.rounds_won == a_outcome.rounds_lost {
        0.5
    } else if a_outcome.win {
        1.0
    } else {
        0.0
    };
    let b_score = 1.0 - a_score;
    let expected_a = 1.0 / (1.0 + 10f64.powf((b.rating - a.rating) as f64 / ELO_LOGISTIC_SCALE));
    let expected_b = 1.0 - expected_a;
    let ka = if a.matches < config.provisional_games { config.k_provisional } else { config.k } as f64;
    let kb = if b.matches < config.provisional_games { config.k_provisional } else { config.k } as f64;
    let mut na = a;
    let mut nb = b;
    na.rating = (a.rating as f64 + ka * (a_score - expected_a)).round() as i64;
    nb.rating = (b.rating as f64 + kb * (b_score - expected_b)).round() as i64;
    na.matches += 1;
    nb.matches += 1;
    match a_score {
        1.0 => {
            na.wins += 1;
            nb.losses += 1;
        }
        0.0 => {
            na.losses += 1;
            nb.wins += 1;
        }
        _ => {
            na.ties += 1;
            nb.ties += 1;
        }
    }
    (na, nb)
}

pub fn replay_h2h_ratings<I>(config: &H2hRatingConfig, rows: I) -> BTreeMap<Uuid, H2hRatingState>
where
    I: IntoIterator<Item = (Uuid, Uuid, MatchOutcome)>,
{
    let mut ratings = BTreeMap::new();
    for (a_id, b_id, outcome) in rows {
        apply_h2h_row(config, &mut ratings, a_id, b_id, outcome);
    }
    ratings
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeasonedH2hRatings {
    pub all_time: BTreeMap<Uuid, H2hRatingState>,
    pub seasons: BTreeMap<Uuid, BTreeMap<Uuid, H2hRatingState>>,
}

pub fn replay_h2h_ratings_by_season<I>(
    config: &H2hRatingConfig,
    rows: I,
) -> SeasonedH2hRatings
where
    I: IntoIterator<Item = (Option<Uuid>, Uuid, Uuid, MatchOutcome)>,
{
    let mut ratings = SeasonedH2hRatings::default();
    for (season_id, a_id, b_id, outcome) in rows {
        apply_h2h_row(config, &mut ratings.all_time, a_id, b_id, outcome);
        if let Some(season_id) = season_id {
            apply_h2h_row(
                config,
                ratings.seasons.entry(season_id).or_default(),
                a_id,
                b_id,
                outcome,
            );
        }
    }
    ratings
}

fn apply_h2h_row(
    config: &H2hRatingConfig,
    ratings: &mut BTreeMap<Uuid, H2hRatingState>,
    a_id: Uuid,
    b_id: Uuid,
    outcome: MatchOutcome,
) {
    let a = ratings
        .get(&a_id)
        .copied()
        .unwrap_or_else(|| H2hRatingState::new(config.start));
    let b = ratings
        .get(&b_id)
        .copied()
        .unwrap_or_else(|| H2hRatingState::new(config.start));
    let (a, b) = apply_h2h_rating(config, a, b, outcome);
    ratings.insert(a_id, a);
    ratings.insert(b_id, b);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H2hSeasonSelection {
    Current,
    AllTime,
    Season(Uuid),
}

pub fn parse_h2h_season_selection(value: Option<&str>) -> Result<H2hSeasonSelection, uuid::Error> {
    match value.unwrap_or("current") {
        "current" | "" => Ok(H2hSeasonSelection::Current),
        "all" => Ok(H2hSeasonSelection::AllTime),
        raw => Uuid::parse_str(raw).map(H2hSeasonSelection::Season),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn win20() -> MatchOutcome {
        MatchOutcome::new(2, 0)
    }

    fn loss02() -> MatchOutcome {
        MatchOutcome::new(0, 2)
    }

    #[test]
    fn defaults_serialize_to_contract_json() {
        let got = serde_json::to_value(RankingConfig::default()).unwrap();
        let expected = json!({
            "bot": {
                "pricing": "own",
                "jitter": 50,
                "mode": "fixed",
                "fixed": { "win_2_0": 32, "win_2_1": 28, "loss_1_2": -16, "loss_0_2": -20, "tie": 0 },
                "avoid_recent_opponents": 3
            },
            "human": {
                "mode": "flat",
                "flat_award": 50,
                "pair_daily_cap": 3,
                "fixed": { "win_2_0": 40, "win_2_1": 35, "loss_1_2": -10, "loss_0_2": -15, "tie": 0 }
            },
            "elo": {
                "k_table": [[0,100],[500,90],[1000,80],[1500,70],[2000,60],[2500,50]],
                "score": { "win_2_0": 1.0, "win_2_1": 0.92, "loss_1_2": 0.12, "loss_0_2": 0.0, "tie": 0.5 },
                "min_win": 1,
                "max_loss": -1
            },
            "h2h_rating": { "start": 1000, "k": 32, "k_provisional": 48, "provisional_games": 10, "min_games_listed": 3 }
        });
        assert_eq!(got, expected);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let bad = RankingConfig {
            bot: BotConfig { jitter: -1, fixed: FixedDeltas { win_2_0: 1, win_2_1: 2, loss_1_2: 1, loss_0_2: 0, tie: 0 }, ..BotConfig::default() },
            elo: EloConfig { k_table: vec![[500, 0], [100, 10]], min_win: -1, max_loss: 1, ..EloConfig::default() },
            h2h_rating: H2hRatingConfig { start: 0, k: 0, k_provisional: 0, provisional_games: -1, min_games_listed: -1 },
            ..RankingConfig::default()
        };
        let errors = bad.validate().unwrap_err();
        assert!(errors.contains_key("bot.jitter"));
        assert!(errors.contains_key("bot.fixed.win_2_0"));
        assert!(errors.contains_key("elo.k_table"));
        assert!(errors.contains_key("h2h_rating.start"));
    }

    #[test]
    fn bot_defaults_fix_plus_one_minus_eighty() {
        let cfg = RankingConfig::default();
        let win = trophy_delta(&cfg, TrophyDeltaInput {
            outcome: win20(),
            own_trophies: 800,
            opponent_trophies_for_pricing: bot_pricing_trophies(&cfg, 800, 50, Uuid::nil(), 0),
            h2h: false,
            h2h_pair_matches_today: 0,
        });
        let loss = trophy_delta(&cfg, TrophyDeltaInput {
            outcome: loss02(),
            own_trophies: 800,
            opponent_trophies_for_pricing: bot_pricing_trophies(&cfg, 800, 50, Uuid::nil(), 0),
            h2h: false,
            h2h_pair_matches_today: 0,
        });
        assert_eq!((win, loss), (32, -20));
    }

    #[test]
    fn mimic_elo_reproduces_old_numbers() {
        let mut cfg = RankingConfig::default();
        cfg.bot.pricing = BotPricing::Mimic;
        cfg.bot.mode = BotMode::Elo;
        let price = bot_pricing_trophies(&cfg, 800, 50, Uuid::nil(), 0);
        assert_eq!(price, 50);
        let win = trophy_delta(&cfg, TrophyDeltaInput {
            outcome: win20(),
            own_trophies: 800,
            opponent_trophies_for_pricing: price,
            h2h: false,
            h2h_pair_matches_today: 0,
        });
        let loss = trophy_delta(&cfg, TrophyDeltaInput {
            outcome: loss02(),
            own_trophies: 800,
            opponent_trophies_for_pricing: price,
            h2h: false,
            h2h_pair_matches_today: 0,
        });
        assert_eq!((win, loss), (1, -80));
    }

    #[test]
    fn h2h_flat_awards_both_sides_and_daily_cap_stops_it() {
        let cfg = RankingConfig::default();
        let before_cap = trophy_delta(&cfg, TrophyDeltaInput {
            outcome: loss02(),
            own_trophies: 100,
            opponent_trophies_for_pricing: 300,
            h2h: true,
            h2h_pair_matches_today: 2,
        });
        let after_cap = trophy_delta(&cfg, TrophyDeltaInput {
            outcome: win20(),
            own_trophies: 100,
            opponent_trophies_for_pricing: 300,
            h2h: true,
            h2h_pair_matches_today: 3,
        });
        assert_eq!(before_cap, 50);
        assert_eq!(after_cap, 0);
    }

    #[test]
    fn h2h_elo_uses_provisional_k() {
        let cfg = H2hRatingConfig::default();
        let a = H2hRatingState::new(1000);
        let b = H2hRatingState::new(1000);
        let (a, b) = apply_h2h_rating(&cfg, a, b, win20());
        assert_eq!(a.rating, 1024);
        assert_eq!(b.rating, 976);
        assert_eq!((a.wins, b.losses, a.matches, b.matches), (1, 1, 1, 1));

        let veteran = H2hRatingState { matches: 10, ..H2hRatingState::new(1000) };
        let (veteran, _) = apply_h2h_rating(&cfg, veteran, H2hRatingState::new(1000), win20());
        assert_eq!(veteran.rating, 1016);
    }

    #[test]
    fn rebuild_replays_in_supplied_order() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let cfg = H2hRatingConfig::default();
        let ratings = replay_h2h_ratings(&cfg, [(a, b, win20()), (b, a, win20())]);
        assert_eq!(ratings[&a].matches, 2);
        assert_eq!(ratings[&b].matches, 2);
        assert_eq!(ratings[&a].wins, 1);
        assert_eq!(ratings[&b].wins, 1);
    }

    #[test]
    fn h2h_replay_maps_match_to_season_and_all_time() {
        let season = Uuid::from_u128(10);
        let other_season = Uuid::from_u128(11);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let cfg = H2hRatingConfig::default();
        let ratings = replay_h2h_ratings_by_season(&cfg, [(Some(season), a, b, win20())]);
        assert_eq!(ratings.all_time[&a].matches, 1);
        assert_eq!(ratings.seasons[&season][&a].matches, 1);
        assert!(!ratings.seasons.contains_key(&other_season));
    }

    #[test]
    fn h2h_new_season_starts_fresh_and_old_stays_frozen() {
        let old = Uuid::from_u128(10);
        let new = Uuid::from_u128(11);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let cfg = H2hRatingConfig::default();
        let ratings = replay_h2h_ratings_by_season(
            &cfg,
            [(Some(old), a, b, win20()), (Some(new), b, a, win20())],
        );
        assert_eq!(ratings.seasons[&old][&a].rating, 1024);
        assert_eq!(ratings.seasons[&old][&b].rating, 976);
        assert_eq!(ratings.seasons[&new][&b].rating, 1024);
        assert_eq!(ratings.seasons[&new][&a].rating, 976);
        assert_eq!(ratings.seasons[&old][&a].matches, 1);
        assert_eq!(ratings.seasons[&new][&a].matches, 1);
        assert_eq!(ratings.all_time[&a].matches, 2);
    }

    #[test]
    fn h2h_season_selection_defaults_to_current() {
        let season = Uuid::from_u128(42);
        assert_eq!(
            parse_h2h_season_selection(None).unwrap(),
            H2hSeasonSelection::Current
        );
        assert_eq!(
            parse_h2h_season_selection(Some("all")).unwrap(),
            H2hSeasonSelection::AllTime
        );
        assert_eq!(
            parse_h2h_season_selection(Some(&season.to_string())).unwrap(),
            H2hSeasonSelection::Season(season)
        );
        assert!(parse_h2h_season_selection(Some("definitely-not-a-season")).is_err());
    }

    #[test]
    fn h2h_season_rebuild_matches_incremental_updates() {
        let season = Uuid::from_u128(10);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let cfg = H2hRatingConfig::default();
        let rows = [
            (Some(season), a, b, win20()),
            (Some(season), b, c, win20()),
            (None, c, a, win20()),
        ];
        let replayed = replay_h2h_ratings_by_season(&cfg, rows);

        let mut all_time = BTreeMap::new();
        let mut seasons: BTreeMap<Uuid, BTreeMap<Uuid, H2hRatingState>> = BTreeMap::new();
        for (season_id, a_id, b_id, outcome) in rows {
            apply_h2h_row(&cfg, &mut all_time, a_id, b_id, outcome);
            if let Some(season_id) = season_id {
                apply_h2h_row(&cfg, seasons.entry(season_id).or_default(), a_id, b_id, outcome);
            }
        }

        assert_eq!(replayed.all_time, all_time);
        assert_eq!(replayed.seasons, seasons);
    }

    #[test]
    fn sixty_percent_vs_ai_reaches_imperial_in_expected_window() {
        let cfg = RankingConfig::default();
        let mut trophies = 0;
        let mut reached = None;
        for i in 1..=260 {
            let outcome = if i % 5 < 3 { win20() } else { loss02() };
            let delta = trophy_delta(&cfg, TrophyDeltaInput {
                outcome,
                own_trophies: trophies,
                opponent_trophies_for_pricing: trophies,
                h2h: false,
                h2h_pair_matches_today: 0,
            });
            trophies = (trophies + delta).max(0);
            if trophies >= 2500 {
                reached = Some(i);
                break;
            }
        }
        let matches = reached.expect("should reach imperial");
        assert!((180..=260).contains(&matches), "reached in {matches}");
    }
}
