//! Arena ranking v2: configurable trophy deltas plus the human-only Elo board.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use diesel::OptionalExtension;
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DbPool;
use crate::arena::arena_ladder::{ELO_LOGISTIC_SCALE, MatchOutcome};

const CACHE_TTL: Duration = Duration::from_secs(30);
const H2H_RATING_ADVISORY_LOCK_ID: i64 = 7_202_409_280_001;

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
    /// Human-vs-human pays the configured normal human fixed delta plus a flat
    /// participation award while the pair is under `pair_daily_cap`.
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
        Self {
            win_2_0: 32,
            win_2_1: 28,
            loss_1_2: -16,
            loss_0_2: -20,
            tie: 0,
        }
    }

    pub const fn human_default() -> Self {
        Self {
            win_2_0: 40,
            win_2_1: 35,
            loss_1_2: -10,
            loss_0_2: -15,
            tie: 0,
        }
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
        Self {
            win_2_0: 1.0,
            win_2_1: 0.92,
            loss_1_2: 0.12,
            loss_0_2: 0.0,
            tie: 0.5,
        }
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
            k_table: vec![
                [0, 100],
                [500, 90],
                [1000, 80],
                [1500, 70],
                [2000, 60],
                [2500, 50],
            ],
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
    /// Extra h2h trophies added in `Flat` mode while the pair is under
    /// `pair_daily_cap`; the normal match delta still applies.
    #[serde(default = "default_flat_award")]
    pub flat_award: i64,
    /// Number of daily h2h matches per pair that receive `flat_award` in `Flat`
    /// mode. At and after this count, `Flat` mode is plain `fixed`.
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
        Self {
            start: 1000,
            k: 32,
            k_provisional: 48,
            provisional_games: 10,
            min_games_listed: 3,
        }
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
    #[serde(default = "default_h2h_reconcile_minutes")]
    pub h2h_reconcile_minutes: i64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            bot: BotConfig::default(),
            human: HumanConfig::default(),
            elo: EloConfig::default(),
            h2h_rating: H2hRatingConfig::default(),
            h2h_reconcile_minutes: default_h2h_reconcile_minutes(),
        }
    }
}

fn default_h2h_reconcile_minutes() -> i64 {
    15
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
            errors.insert(
                "bot.avoid_recent_opponents".into(),
                "must be at most 50".into(),
            );
        }
        if self.human.flat_award < 0 {
            errors.insert("human.flat_award".into(), "must be non-negative".into());
        }
        if self.human.pair_daily_cap > 100 {
            errors.insert("human.pair_daily_cap".into(), "must be at most 100".into());
        }
        if self.elo.k_table.is_empty() {
            errors.insert(
                "elo.k_table".into(),
                "must contain at least one band".into(),
            );
        }
        let mut last = None;
        for (i, [floor, k]) in self.elo.k_table.iter().copied().enumerate() {
            if floor < 0 {
                errors.insert(
                    format!("elo.k_table[{i}][0]"),
                    "must be non-negative".into(),
                );
            }
            if k <= 0 {
                errors.insert(format!("elo.k_table[{i}][1]"), "must be positive".into());
            }
            if let Some(prev) = last
                && floor <= prev
            {
                errors.insert(
                    "elo.k_table".into(),
                    "floors must be strictly ascending".into(),
                );
            }
            last = Some(floor);
        }
        if self
            .elo
            .k_table
            .as_slice()
            .first()
            .is_some_and(|x| x[0] != 0)
        {
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
                errors.insert(
                    field.into(),
                    "must be a finite value between 0 and 1".into(),
                );
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
            errors.insert(
                "h2h_rating.provisional_games".into(),
                "must be non-negative".into(),
            );
        }
        if self.h2h_rating.min_games_listed < 0 {
            errors.insert(
                "h2h_rating.min_games_listed".into(),
                "must be non-negative".into(),
            );
        }
        if self.h2h_reconcile_minutes != 0 && !(1..=1440).contains(&self.h2h_reconcile_minutes) {
            errors.insert(
                "h2h_reconcile_minutes".into(),
                "must be 0 or between 1 and 1440".into(),
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn validate_fixed(prefix: &str, fixed: &FixedDeltas, errors: &mut ValidationErrors) {
    if fixed.win_2_0 < fixed.win_2_1 || fixed.win_2_1 < 0 {
        errors.insert(
            format!("{prefix}.win_2_0"),
            "wins must be non-negative and 2-0 >= 2-1".into(),
        );
    }
    if fixed.loss_0_2 > fixed.loss_1_2 || fixed.loss_1_2 > 0 {
        errors.insert(
            format!("{prefix}.loss_0_2"),
            "losses must be non-positive and 0-2 <= 1-2".into(),
        );
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
        anyhow::bail!(
            "active config v{} does not validate: {errors:?}",
            row.version
        );
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
        Self {
            config: RankingConfig::default(),
            h2h_pair_matches_today: 0,
        }
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
                let award = if input.h2h_pair_matches_today < config.human.pair_daily_cap {
                    config.human.flat_award
                } else {
                    0
                };
                config.human.fixed.delta(input.outcome) + award
            }
            HumanMode::Fixed => config.human.fixed.delta(input.outcome),
            HumanMode::Elo => elo_delta(
                &config.elo,
                input.outcome,
                input.own_trophies,
                input.opponent_trophies_for_pricing,
            ),
        };
    }
    match config.bot.mode {
        BotMode::Fixed => config.bot.fixed.delta(input.outcome),
        BotMode::Elo => {
            let delta = elo_delta(
                &config.elo,
                input.outcome,
                input.own_trophies,
                input.opponent_trophies_for_pricing,
            );
            if config.bot.pricing == BotPricing::Mimic && delta < -80 {
                -80
            } else {
                delta
            }
        }
    }
}

pub fn elo_delta(
    config: &EloConfig,
    outcome: MatchOutcome,
    own_trophies: i64,
    opponent_trophies: i64,
) -> i64 {
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
        Self {
            rating: start,
            wins: 0,
            losses: 0,
            ties: 0,
            matches: 0,
        }
    }
}

/// Who a row on the head-to-head boards belongs to: the character row plus the
/// ALT that played (owner decision 2026-10-10: every alt is its own character).
///
/// A user's alts take turns occupying one `characters` row, so the character id
/// alone credited one alt's human-vs-human results to whichever alt was live
/// (tracker #373). `alt_key` is the match row's `source_alt_uuid`; for a row
/// recorded before alts were tracked, the alt live now; for a character that has
/// never had an alt id, the character id itself — see [`H2H_REPLAY_ROWS_CTE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct H2hKey {
    pub character_id: Uuid,
    pub alt_key: Uuid,
}

impl H2hKey {
    pub fn new(character_id: Uuid, alt_key: Uuid) -> Self {
        Self {
            character_id,
            alt_key,
        }
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
    let ka = if a.matches < config.provisional_games {
        config.k_provisional
    } else {
        config.k
    } as f64;
    let kb = if b.matches < config.provisional_games {
        config.k_provisional
    } else {
        config.k
    } as f64;
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

pub fn is_h2h_match(fighter_count: usize, expected_peers: usize, any_bot: bool) -> bool {
    fighter_count == 2 && expected_peers == 2 && !any_bot
}

pub fn apply_h2h_match_by_season<K: Ord + Copy>(
    config: &H2hRatingConfig,
    ratings: &mut SeasonedH2hRatings<K>,
    season_id: Option<Uuid>,
    a_id: K,
    b_id: K,
    outcome: MatchOutcome,
) {
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

/// Unseasoned replay. Live reconcile uses [`replay_h2h_ratings_by_season`]
/// (whose `all_time` map is this); kept for the replay-order test.
#[cfg(test)]
pub fn replay_h2h_ratings<K, I>(config: &H2hRatingConfig, rows: I) -> BTreeMap<K, H2hRatingState>
where
    K: Ord + Copy,
    I: IntoIterator<Item = (K, K, MatchOutcome)>,
{
    let mut ratings = BTreeMap::new();
    for (a_id, b_id, outcome) in rows {
        apply_h2h_row(config, &mut ratings, a_id, b_id, outcome);
    }
    ratings
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonedH2hRatings<K: Ord = Uuid> {
    pub all_time: BTreeMap<K, H2hRatingState>,
    pub seasons: BTreeMap<Uuid, BTreeMap<K, H2hRatingState>>,
}

impl<K: Ord> Default for SeasonedH2hRatings<K> {
    fn default() -> Self {
        Self {
            all_time: BTreeMap::new(),
            seasons: BTreeMap::new(),
        }
    }
}

pub fn replay_h2h_ratings_by_season<K, I>(
    config: &H2hRatingConfig,
    rows: I,
) -> SeasonedH2hRatings<K>
where
    K: Ord + Copy,
    I: IntoIterator<Item = (Option<Uuid>, K, K, MatchOutcome)>,
{
    let mut ratings = SeasonedH2hRatings::default();
    for (season_id, a_id, b_id, outcome) in rows {
        apply_h2h_match_by_season(config, &mut ratings, season_id, a_id, b_id, outcome);
    }
    ratings
}

fn apply_h2h_row<K: Ord + Copy>(
    config: &H2hRatingConfig,
    ratings: &mut BTreeMap<K, H2hRatingState>,
    a_id: K,
    b_id: K,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct H2hReconcileReport {
    pub characters: usize,
    pub matches: usize,
}

#[derive(Debug, Clone, diesel::QueryableByName)]
struct H2hReplayRow {
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    season_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    character_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    alt_key: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    opponent_character_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    opponent_alt_key: Uuid,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    rounds_won: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    rounds_lost: i32,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    recorded_at: DateTime<Utc>,
}

/// Every h2h match exactly once, canonically oriented, with the ALT on each side.
///
/// Ends in a `replay_rows` relation: `season_id, character_id, alt_key,
/// opponent_character_id, opponent_alt_key, rounds_won, rounds_lost, recorded_at,
/// id`, with `character_id < opponent_character_id`. The reconcile replays it in
/// `recorded_at, id` order; the live increment counts it to tell whether a match
/// was already absorbed. One definition so the two can never disagree.
///
/// Each side's alt comes from that side's OWN audit row (the alt stamped when the
/// match was recorded), so both of a match's rows give the same answer. A legacy
/// row (`source_alt_uuid` NULL) counts for the alt live now, as every other arena
/// reader treats it; a character with no alt id is keyed by its own id. When the
/// opponent's row is missing (one-sided audit), the opponent's live alt is used.
///
/// A match with an operator TEST alt on either side (`crate::testchar`) is
/// not a row at all: the test alt gets no rating, and its opponent's rating is not
/// moved by a fight against a character whose level was set by hand. Only the
/// alt STAMPED on an audit row decides that, never the live-alt fallback: while
/// a test alt is live, an old unstamped or one-sided match of the same account
/// must still count for the real alt that fought it. (Every match recorded since
/// alts were tracked is stamped at record time, so a test alt's fights are.)
pub(crate) const H2H_REPLAY_ROWS_CTE: &str = "\
    WITH sided AS ( \
         SELECT r.id, r.game_session_id, r.recorded_at, r.character_id, r.opponent_character_id, \
                r.rounds_won, r.rounds_lost, \
                COALESCE(r.source_alt_uuid, rc.source_alt_uuid, r.character_id) AS alt_key, \
                COALESCE( \
                    (SELECT COALESCE(o.source_alt_uuid, oc.source_alt_uuid, o.character_id) \
                       FROM arena_match_results o \
                       LEFT JOIN characters oc ON oc.id = o.character_id \
                      WHERE o.game_session_id = r.game_session_id \
                        AND o.character_id = r.opponent_character_id \
                        AND o.opponent_character_id = r.character_id \
                        AND o.is_h2h = true \
                      ORDER BY o.recorded_at, o.id \
                      LIMIT 1), \
                    (SELECT COALESCE(oc.source_alt_uuid, oc.id) \
                       FROM characters oc WHERE oc.id = r.opponent_character_id), \
                    r.opponent_character_id) AS opponent_alt_key, \
                r.source_alt_uuid AS stamped_alt, \
                (SELECT o.source_alt_uuid \
                   FROM arena_match_results o \
                  WHERE o.game_session_id = r.game_session_id \
                    AND o.character_id = r.opponent_character_id \
                    AND o.opponent_character_id = r.character_id \
                    AND o.is_h2h = true \
                  ORDER BY o.recorded_at, o.id \
                  LIMIT 1) AS opponent_stamped_alt \
         FROM arena_match_results r \
         LEFT JOIN characters rc ON rc.id = r.character_id \
         WHERE r.opponent_character_id IS NOT NULL \
           AND r.is_h2h = true \
     ), candidates AS ( \
         SELECT s.id AS season_id, \
                LEAST(r.character_id, r.opponent_character_id) AS character_id, \
                GREATEST(r.character_id, r.opponent_character_id) AS opponent_character_id, \
                CASE WHEN r.character_id = LEAST(r.character_id, r.opponent_character_id) \
                     THEN r.alt_key ELSE r.opponent_alt_key END AS alt_key, \
                CASE WHEN r.character_id = LEAST(r.character_id, r.opponent_character_id) \
                     THEN r.opponent_alt_key ELSE r.alt_key END AS opponent_alt_key, \
                CASE WHEN r.character_id = LEAST(r.character_id, r.opponent_character_id) \
                     THEN r.rounds_won ELSE r.rounds_lost END AS rounds_won, \
                CASE WHEN r.character_id = LEAST(r.character_id, r.opponent_character_id) \
                     THEN r.rounds_lost ELSE r.rounds_won END AS rounds_lost, \
                r.recorded_at, r.id, r.game_session_id, r.stamped_alt, r.opponent_stamped_alt \
         FROM sided r \
         LEFT JOIN LATERAL ( \
             SELECT id FROM arena_seasons s \
             WHERE r.recorded_at >= to_timestamp(s.starts_at) \
               AND r.recorded_at < to_timestamp(s.ends_at) \
             ORDER BY s.starts_at DESC LIMIT 1 \
         ) s ON true \
     ), replay_rows AS ( \
         SELECT DISTINCT ON (game_session_id, character_id, opponent_character_id) \
                season_id, character_id, alt_key, opponent_character_id, opponent_alt_key, \
                rounds_won, rounds_lost, recorded_at, id \
         FROM candidates \
         WHERE NOT EXISTS (SELECT 1 FROM test_characters tc \
                            WHERE tc.alt_uuid IN (candidates.stamped_alt, \
                                                  candidates.opponent_stamped_alt)) \
         ORDER BY game_session_id, character_id, opponent_character_id, recorded_at, id \
     )";

/// The public human-vs-human player board: one row per ALT, named as that alt.
///
/// `{RATINGS}` / `{SEASON_FILTER}` select the season or all-time table (see
/// [`h2h_board_sql`]). `$1` is `min_games_listed`, `$2` the limit and, for a
/// season, `$3` the season id.
///
/// The live alt is named from the `characters` row; an alt that has been switched
/// out is named from its newest saved version — exactly how the in-game
/// leaderboard (`leaderboards::RANKED_CTE`) names it. A rating whose alt is
/// neither live nor saved any more (deleted) is not listed. The guild is the one
/// the alt itself is in: the live membership, or the one parked in its newest
/// version while that guild exists. An operator test alt is never listed (the
/// replay already gives it no rating; this keeps it off even if one existed).
const H2H_BOARD_ENTRIES_SQL: &str = "\
    WITH entries AS ( \
         SELECT r.character_id, r.alt_key, r.rating, r.wins, r.losses, r.ties, r.matches, \
                r.last_match_at, \
                (r.alt_key = COALESCE(c.source_alt_uuid, c.id)) AS live, \
                CASE WHEN r.alt_key = COALESCE(c.source_alt_uuid, c.id) \
                     THEN COALESCE(c.character ->> 'name', '') \
                     ELSE COALESCE(saved.name, '') END AS name, \
                CASE WHEN r.alt_key = COALESCE(c.source_alt_uuid, c.id) \
                     THEN COALESCE((c.character ->> 'level')::bigint, 0) \
                     ELSE COALESCE(saved.level, 0) END AS level, \
                CASE WHEN r.alt_key = COALESCE(c.source_alt_uuid, c.id) \
                     THEN gm.guild_id::text \
                     ELSE pg.id::text END AS guild_id, \
                CASE WHEN r.alt_key = COALESCE(c.source_alt_uuid, c.id) \
                     THEN g.name \
                     ELSE pg.name END AS guild_name, \
                saved.name IS NOT NULL AS saved \
         FROM {RATINGS} r \
         INNER JOIN characters c ON c.id = r.character_id \
         LEFT JOIN LATERAL ( \
             SELECT v.character ->> 'name' AS name, \
                    (v.character ->> 'level')::bigint AS level, \
                    v.server_state -> 'parkedGuildMembers' -> 0 ->> 'guild_id' AS parked_guild_id \
             FROM character_versions v \
             WHERE v.character_id = r.character_id \
               AND v.source_alt_uuid = r.alt_key \
               AND r.alt_key <> COALESCE(c.source_alt_uuid, c.id) \
             ORDER BY v.saved_at DESC \
             LIMIT 1 \
         ) saved ON true \
         LEFT JOIN guild_members gm ON gm.character_id = c.id \
         LEFT JOIN guilds g ON g.id = gm.guild_id \
         LEFT JOIN guilds pg ON pg.id::text = saved.parked_guild_id \
         WHERE r.matches >= $1 {SEASON_FILTER} \
           AND NOT EXISTS (SELECT 1 FROM test_characters tc WHERE tc.alt_uuid = r.alt_key) \
     ) \
     SELECT row_number() OVER (ORDER BY rating DESC, matches DESC, last_match_at ASC NULLS LAST, \
                                        character_id, alt_key) AS rank, \
            character_id, alt_key, name, level, guild_id, guild_name, \
            rating, wins, losses, ties, matches, last_match_at \
     FROM entries \
     WHERE live OR saved \
     ORDER BY rating DESC, matches DESC, last_match_at ASC NULLS LAST, character_id, alt_key \
     LIMIT $2";

/// [`H2H_BOARD_ENTRIES_SQL`] for one season (`$3`) or all-time.
pub fn h2h_board_sql(season: bool) -> String {
    if season {
        H2H_BOARD_ENTRIES_SQL
            .replace("{RATINGS}", "arena_h2h_season_ratings")
            .replace("{SEASON_FILTER}", "AND r.season_id = $3")
    } else {
        H2H_BOARD_ENTRIES_SQL
            .replace("{RATINGS}", "arena_h2h_ratings")
            .replace("{SEASON_FILTER}", "")
    }
}

/// One row of the public h2h player board (and the input to the h2h guild board).
#[derive(Debug, Clone, Serialize, diesel::QueryableByName)]
pub struct H2hBoardEntry {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub rank: i64,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub character_id: Uuid,
    /// The alt this row belongs to (see [`H2hKey`]). Unique together with
    /// `character_id`; the website keys its rows on the pair.
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub alt_key: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub level: i64,
    #[serde(skip)]
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub guild_id: Option<String>,
    #[serde(skip)]
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub guild_name: Option<String>,
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
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
    pub last_match_at: Option<DateTime<Utc>>,
}

/// The h2h player board: `season` = `Some(id)` for a season, `None` for all-time.
pub async fn h2h_board(
    conn: &mut AsyncPgConnection,
    season: Option<Uuid>,
    min_games: i32,
    limit: i64,
) -> Result<Vec<H2hBoardEntry>, diesel::result::Error> {
    let query = diesel::sql_query(h2h_board_sql(season.is_some()))
        .bind::<diesel::sql_types::Integer, _>(min_games)
        .bind::<diesel::sql_types::BigInt, _>(limit);
    match season {
        Some(season_id) => {
            query
                .bind::<diesel::sql_types::Uuid, _>(season_id)
                .get_results(conn)
                .await
        }
        None => query.get_results(conn).await,
    }
}

pub async fn reconcile_h2h_ratings(
    pool: &DbPool,
    dry_run: bool,
) -> Result<H2hReconcileReport, anyhow::Error> {
    let cfg = load(pool).await.config.h2h_rating;
    let mut conn = pool.get().await?;
    reconcile_h2h_ratings_with_config(&mut conn, &cfg, dry_run).await
}

pub async fn reconcile_h2h_ratings_with_config(
    conn: &mut AsyncPgConnection,
    cfg: &H2hRatingConfig,
    dry_run: bool,
) -> Result<H2hReconcileReport, anyhow::Error> {
    conn.transaction::<_, anyhow::Error, _>(|mut conn| {
        async move {
            lock_h2h_rating_transaction(&mut conn).await?;
            let rows = load_h2h_replay_rows(&mut conn).await?;
            let last_seen = h2h_last_seen(&rows);
            let season_last_seen = h2h_season_last_seen(&rows);
            let ratings = replay_h2h_ratings_by_season(
                cfg,
                rows.iter().map(|r| {
                    (
                        r.season_id,
                        r.key(),
                        r.opponent_key(),
                        MatchOutcome::new(r.rounds_won.max(0) as u8, r.rounds_lost.max(0) as u8),
                    )
                }),
            );
            let report = H2hReconcileReport {
                characters: ratings.all_time.len(),
                matches: rows.len(),
            };

            if !dry_run {
                write_h2h_ratings(&mut conn, ratings, last_seen, season_last_seen).await?;
            }

            Ok(report)
        }
        .scope_boxed()
    })
    .await
}

pub async fn lock_h2h_rating_transaction(
    conn: &mut AsyncPgConnection,
) -> Result<(), anyhow::Error> {
    diesel::sql_query("SELECT pg_advisory_xact_lock($1)")
        .bind::<diesel::sql_types::BigInt, _>(H2H_RATING_ADVISORY_LOCK_ID)
        .execute(conn)
        .await?;
    Ok(())
}

async fn load_h2h_replay_rows(
    conn: &mut AsyncPgConnection,
) -> Result<Vec<H2hReplayRow>, anyhow::Error> {
    let rows = diesel::sql_query(format!(
        "{H2H_REPLAY_ROWS_CTE} \
         SELECT season_id, character_id, alt_key, opponent_character_id, opponent_alt_key, \
                rounds_won, rounds_lost, recorded_at \
         FROM replay_rows \
         ORDER BY recorded_at, id"
    ))
    .get_results(conn)
    .await?;
    Ok(rows)
}

impl H2hReplayRow {
    fn key(&self) -> H2hKey {
        H2hKey::new(self.character_id, self.alt_key)
    }

    fn opponent_key(&self) -> H2hKey {
        H2hKey::new(self.opponent_character_id, self.opponent_alt_key)
    }
}

fn h2h_last_seen(rows: &[H2hReplayRow]) -> HashMap<H2hKey, DateTime<Utc>> {
    rows.iter()
        .flat_map(|r| [(r.key(), r.recorded_at), (r.opponent_key(), r.recorded_at)])
        .fold(
            HashMap::<H2hKey, DateTime<Utc>>::new(),
            |mut acc, (id, at)| {
                acc.entry(id)
                    .and_modify(|old| *old = (*old).max(at))
                    .or_insert(at);
                acc
            },
        )
}

fn h2h_season_last_seen(rows: &[H2hReplayRow]) -> HashMap<(Uuid, H2hKey), DateTime<Utc>> {
    rows.iter()
        .filter_map(|r| r.season_id.map(|season_id| (season_id, r)))
        .flat_map(|(season_id, r)| {
            [
                ((season_id, r.key()), r.recorded_at),
                ((season_id, r.opponent_key()), r.recorded_at),
            ]
        })
        .fold(
            HashMap::<(Uuid, H2hKey), DateTime<Utc>>::new(),
            |mut acc, (key, at)| {
                acc.entry(key)
                    .and_modify(|old| *old = (*old).max(at))
                    .or_insert(at);
                acc
            },
        )
}

async fn write_h2h_ratings(
    conn: &mut AsyncPgConnection,
    ratings: SeasonedH2hRatings<H2hKey>,
    last_seen: HashMap<H2hKey, DateTime<Utc>>,
    season_last_seen: HashMap<(Uuid, H2hKey), DateTime<Utc>>,
) -> Result<(), anyhow::Error> {
    diesel::sql_query("DELETE FROM arena_h2h_ratings")
        .execute(&mut *conn)
        .await?;
    diesel::sql_query("DELETE FROM arena_h2h_season_ratings")
        .execute(&mut *conn)
        .await?;
    for (key, state) in ratings.all_time {
        diesel::sql_query(
            "INSERT INTO arena_h2h_ratings \
             (character_id, alt_key, rating, wins, losses, ties, matches, last_match_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind::<diesel::sql_types::Uuid, _>(key.character_id)
        .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
        .bind::<diesel::sql_types::Integer, _>(state.rating as i32)
        .bind::<diesel::sql_types::Integer, _>(state.wins as i32)
        .bind::<diesel::sql_types::Integer, _>(state.losses as i32)
        .bind::<diesel::sql_types::Integer, _>(state.ties as i32)
        .bind::<diesel::sql_types::Integer, _>(state.matches as i32)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>, _>(
            last_seen.get(&key).copied(),
        )
        .execute(&mut *conn)
        .await?;
    }
    for (season_id, season_ratings) in ratings.seasons {
        for (key, state) in season_ratings {
            diesel::sql_query(
                "INSERT INTO arena_h2h_season_ratings \
                 (season_id, character_id, alt_key, rating, wins, losses, ties, matches, last_match_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind::<diesel::sql_types::Uuid, _>(season_id)
            .bind::<diesel::sql_types::Uuid, _>(key.character_id)
            .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
            .bind::<diesel::sql_types::Integer, _>(state.rating as i32)
            .bind::<diesel::sql_types::Integer, _>(state.wins as i32)
            .bind::<diesel::sql_types::Integer, _>(state.losses as i32)
            .bind::<diesel::sql_types::Integer, _>(state.ties as i32)
            .bind::<diesel::sql_types::Integer, _>(state.matches as i32)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>, _>(
                season_last_seen.get(&(season_id, key)).copied(),
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

pub fn spawn_h2h_reconcile_task(pool: DbPool) {
    actix_web::rt::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            let loaded = load(&pool).await;
            let minutes = loaded.config.h2h_reconcile_minutes;
            if minutes == 0 {
                log::info!("arena h2h reconcile: disabled by ranking config");
                return;
            }

            match reconcile_h2h_ratings(&pool, false).await {
                Ok(report) => log::info!(
                    "arena h2h reconcile: rebuilt {} character(s) from {} match(es)",
                    report.characters,
                    report.matches
                ),
                Err(e) => log::warn!("arena h2h reconcile: failed: {e}"),
            }

            tokio::time::sleep(Duration::from_secs((minutes as u64).saturating_mul(60))).await;
        }
    });
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
            "h2h_rating": { "start": 1000, "k": 32, "k_provisional": 48, "provisional_games": 10, "min_games_listed": 3 },
            "h2h_reconcile_minutes": 15
        });
        assert_eq!(got, expected);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let bad = RankingConfig {
            bot: BotConfig {
                jitter: -1,
                fixed: FixedDeltas {
                    win_2_0: 1,
                    win_2_1: 2,
                    loss_1_2: 1,
                    loss_0_2: 0,
                    tie: 0,
                },
                ..BotConfig::default()
            },
            elo: EloConfig {
                k_table: vec![[500, 0], [100, 10]],
                min_win: -1,
                max_loss: 1,
                ..EloConfig::default()
            },
            h2h_rating: H2hRatingConfig {
                start: 0,
                k: 0,
                k_provisional: 0,
                provisional_games: -1,
                min_games_listed: -1,
            },
            h2h_reconcile_minutes: 1441,
            ..RankingConfig::default()
        };
        let errors = bad.validate().unwrap_err();
        assert!(errors.contains_key("bot.jitter"));
        assert!(errors.contains_key("bot.fixed.win_2_0"));
        assert!(errors.contains_key("elo.k_table"));
        assert!(errors.contains_key("h2h_rating.start"));
        assert!(errors.contains_key("h2h_reconcile_minutes"));
    }

    #[test]
    fn h2h_reconcile_minutes_accepts_disabled_and_bounded_values() {
        for minutes in [0, 1, 15, 1440] {
            let cfg = RankingConfig {
                h2h_reconcile_minutes: minutes,
                ..RankingConfig::default()
            };
            assert!(cfg.validate().is_ok(), "{minutes} should validate");
        }
        for minutes in [-1, 1441] {
            let cfg = RankingConfig {
                h2h_reconcile_minutes: minutes,
                ..RankingConfig::default()
            };
            assert!(
                cfg.validate()
                    .unwrap_err()
                    .contains_key("h2h_reconcile_minutes"),
                "{minutes} should be rejected"
            );
        }
    }

    #[test]
    fn bot_defaults_fix_plus_one_minus_eighty() {
        let cfg = RankingConfig::default();
        let win = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: win20(),
                own_trophies: 800,
                opponent_trophies_for_pricing: bot_pricing_trophies(&cfg, 800, 50, Uuid::nil(), 0),
                h2h: false,
                h2h_pair_matches_today: 0,
            },
        );
        let loss = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: loss02(),
                own_trophies: 800,
                opponent_trophies_for_pricing: bot_pricing_trophies(&cfg, 800, 50, Uuid::nil(), 0),
                h2h: false,
                h2h_pair_matches_today: 0,
            },
        );
        assert_eq!((win, loss), (32, -20));
    }

    #[test]
    fn mimic_elo_reproduces_old_numbers() {
        let mut cfg = RankingConfig::default();
        cfg.bot.pricing = BotPricing::Mimic;
        cfg.bot.mode = BotMode::Elo;
        let price = bot_pricing_trophies(&cfg, 800, 50, Uuid::nil(), 0);
        assert_eq!(price, 50);
        let win = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: win20(),
                own_trophies: 800,
                opponent_trophies_for_pricing: price,
                h2h: false,
                h2h_pair_matches_today: 0,
            },
        );
        let loss = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: loss02(),
                own_trophies: 800,
                opponent_trophies_for_pricing: price,
                h2h: false,
                h2h_pair_matches_today: 0,
            },
        );
        assert_eq!((win, loss), (1, -80));
    }

    #[test]
    fn h2h_flat_adds_award_to_normal_win_and_loss_under_daily_cap() {
        let cfg = RankingConfig::default();
        let winner = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: win20(),
                own_trophies: 100,
                opponent_trophies_for_pricing: 300,
                h2h: true,
                h2h_pair_matches_today: 2,
            },
        );
        let loser = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: loss02(),
                own_trophies: 100,
                opponent_trophies_for_pricing: 300,
                h2h: true,
                h2h_pair_matches_today: 2,
            },
        );
        assert_eq!(winner, cfg.human.fixed.win_2_0 + cfg.human.flat_award);
        assert_eq!(loser, cfg.human.fixed.loss_0_2 + cfg.human.flat_award);
        assert!(winner > cfg.human.flat_award);
        assert!(loser < winner);
        assert!(loser > 0);
    }

    #[test]
    fn h2h_flat_daily_cap_falls_back_to_plain_normal_delta() {
        let cfg = RankingConfig::default();
        let after_cap = trophy_delta(
            &cfg,
            TrophyDeltaInput {
                outcome: win20(),
                own_trophies: 100,
                opponent_trophies_for_pricing: 300,
                h2h: true,
                h2h_pair_matches_today: 3,
            },
        );
        assert_eq!(after_cap, cfg.human.fixed.win_2_0);
    }

    #[test]
    fn h2h_flat_zero_award_is_identical_to_plain_normal_delta() {
        let cfg = RankingConfig {
            human: HumanConfig {
                flat_award: 0,
                ..HumanConfig::default()
            },
            ..RankingConfig::default()
        };
        for outcome in [win20(), loss02()] {
            let got = trophy_delta(
                &cfg,
                TrophyDeltaInput {
                    outcome,
                    own_trophies: 100,
                    opponent_trophies_for_pricing: 300,
                    h2h: true,
                    h2h_pair_matches_today: 0,
                },
            );
            assert_eq!(got, cfg.human.fixed.delta(outcome));
        }
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

        let veteran = H2hRatingState {
            matches: 10,
            ..H2hRatingState::new(1000)
        };
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
    fn h2h_detection_requires_two_human_peers_and_no_bot() {
        assert!(is_h2h_match(2, 2, false));
        assert!(
            !is_h2h_match(2, 1, false),
            "solo plus unmarked extra fighter is not paired h2h"
        );
        assert!(!is_h2h_match(2, 2, true), "bot copies are never h2h");
        assert!(!is_h2h_match(3, 3, false), "only 1v1 h2h feeds this board");
    }

    #[test]
    fn incremental_random_h2h_matches_equal_full_rebuild() {
        let cfg = H2hRatingConfig::default();
        let seasons = [Some(Uuid::from_u128(100)), Some(Uuid::from_u128(200)), None];
        let chars: Vec<Uuid> = (1..=8).map(Uuid::from_u128).collect();
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rows = Vec::new();
        let mut incremental = SeasonedH2hRatings::default();

        for _ in 0..160 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a_idx = (seed as usize) % chars.len();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut b_idx = (seed as usize) % chars.len();
            if b_idx == a_idx {
                b_idx = (b_idx + 1) % chars.len();
            }
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let outcome = match seed % 5 {
                0 => MatchOutcome::new(2, 0),
                1 => MatchOutcome::new(2, 1),
                2 => MatchOutcome::new(1, 2),
                3 => MatchOutcome::new(0, 2),
                _ => MatchOutcome::new(1, 1),
            };
            let season_id = seasons[(seed as usize / 5) % seasons.len()];
            let row = (season_id, chars[a_idx], chars[b_idx], outcome);
            apply_h2h_match_by_season(&cfg, &mut incremental, row.0, row.1, row.2, row.3);
            rows.push(row);
        }

        let rebuilt = replay_h2h_ratings_by_season(&cfg, rows);
        assert_eq!(incremental, rebuilt);
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
    fn h2h_season_rebuild_has_independent_expected_ratings() {
        let season = Uuid::from_u128(10);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let cfg = H2hRatingConfig::default();
        let replayed = replay_h2h_ratings_by_season(
            &cfg,
            [
                (Some(season), a, b, win20()),
                (Some(season), b, c, win20()),
                (None, c, a, win20()),
            ],
        );

        assert_eq!(
            replayed.all_time[&a],
            H2hRatingState {
                rating: 997,
                wins: 1,
                losses: 1,
                ties: 0,
                matches: 2
            }
        );
        assert_eq!(
            replayed.all_time[&b],
            H2hRatingState {
                rating: 1002,
                wins: 1,
                losses: 1,
                ties: 0,
                matches: 2
            }
        );
        assert_eq!(
            replayed.all_time[&c],
            H2hRatingState {
                rating: 1001,
                wins: 1,
                losses: 1,
                ties: 0,
                matches: 2
            }
        );
        assert_eq!(
            replayed.seasons[&season][&a],
            H2hRatingState {
                rating: 1024,
                wins: 1,
                losses: 0,
                ties: 0,
                matches: 1
            }
        );
        assert_eq!(
            replayed.seasons[&season][&b],
            H2hRatingState {
                rating: 1002,
                wins: 1,
                losses: 1,
                ties: 0,
                matches: 2
            }
        );
        assert_eq!(
            replayed.seasons[&season][&c],
            H2hRatingState {
                rating: 974,
                wins: 0,
                losses: 1,
                ties: 0,
                matches: 1
            }
        );
    }

    mod db_reconcile_tests {
        use super::*;
        use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

        const H2H_FLAG_MIGRATION_UP: &str = include_str!(
            "../../../migrations/2026-09-28-000000-0000_add_arena_match_results_h2h_flag/up.sql"
        );

        const H2H_PER_ALT_MIGRATION_UP: &str =
            include_str!("../../../migrations/2026-10-11-000000-0000_h2h_ratings_per_alt/up.sql");

        const SCHEMA: [&str; 9] = [
            "CREATE TABLE arena_seasons ( \
                 id UUID PRIMARY KEY, number INTEGER NOT NULL, name TEXT NOT NULL, \
                 starts_at BIGINT NOT NULL, ends_at BIGINT NOT NULL, status TEXT NOT NULL DEFAULT 'scheduled', \
                 scoring TEXT NOT NULL DEFAULT 'shipped', reset_rule TEXT NOT NULL DEFAULT 'hard_reset', \
                 created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint, ended_at BIGINT)",
            "CREATE TABLE arena_matches ( \
                 ticket_id UUID PRIMARY KEY, user_id UUID NOT NULL, status TEXT NOT NULL, \
                 game_session_id UUID, paired BOOLEAN NOT NULL DEFAULT FALSE, \
                 recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), resolved_at TIMESTAMPTZ)",
            "CREATE TABLE arena_match_results ( \
                 id UUID PRIMARY KEY, character_id UUID NOT NULL, opponent_character_id UUID, \
                 game_session_id UUID, win BOOLEAN NOT NULL, rounds_won INTEGER NOT NULL DEFAULT 0, \
                 rounds_lost INTEGER NOT NULL DEFAULT 0, gold BIGINT NOT NULL DEFAULT 0, \
                 character_xp BIGINT NOT NULL DEFAULT 0, trophy_delta BIGINT NOT NULL DEFAULT 0, \
                 trophies_after BIGINT NOT NULL DEFAULT 0, matchmaking_trophies_after BIGINT NOT NULL DEFAULT 0, \
                 arena INTEGER NOT NULL DEFAULT 1, arena_level INTEGER NOT NULL DEFAULT 1, \
                 chest_meter BIGINT NOT NULL DEFAULT 0, recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 is_h2h BOOLEAN NOT NULL DEFAULT false, source_alt_uuid UUID)",
            "CREATE TABLE arena_h2h_ratings ( \
                 character_id UUID PRIMARY KEY, rating INTEGER NOT NULL, wins INTEGER NOT NULL DEFAULT 0, \
                 losses INTEGER NOT NULL DEFAULT 0, ties INTEGER NOT NULL DEFAULT 0, \
                 matches INTEGER NOT NULL DEFAULT 0, last_match_at TIMESTAMPTZ)",
            "CREATE TABLE arena_h2h_season_ratings ( \
                 season_id UUID NOT NULL, character_id UUID NOT NULL, rating INTEGER NOT NULL, \
                 wins INTEGER NOT NULL DEFAULT 0, losses INTEGER NOT NULL DEFAULT 0, \
                 ties INTEGER NOT NULL DEFAULT 0, matches INTEGER NOT NULL DEFAULT 0, \
                 last_match_at TIMESTAMPTZ, PRIMARY KEY (season_id, character_id))",
            "CREATE TABLE characters (id UUID PRIMARY KEY, character JSONB NOT NULL, \
                 source_alt_uuid UUID)",
            "CREATE TABLE character_versions (id UUID PRIMARY KEY, character_id UUID NOT NULL, \
                 source_alt_uuid UUID, character JSONB NOT NULL, server_state JSONB, \
                 saved_at BIGINT NOT NULL)",
            "CREATE TABLE guilds (id TEXT PRIMARY KEY, name TEXT NOT NULL)",
            "CREATE TABLE guild_members (guild_id TEXT NOT NULL, character_id UUID PRIMARY KEY)",
        ];

        #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
        struct RatingRow {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            character_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            rating: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            wins: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            losses: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            ties: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            matches: i32,
        }

        #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
        struct SeasonRatingRow {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            season_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            character_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            rating: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            matches: i32,
        }

        #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
        struct H2hFlagRow {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            character_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Bool)]
            is_h2h: bool,
        }

        async fn fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            conn.begin_test_transaction()
                .await
                .expect("test transaction");
            let schema = format!("t{}", Uuid::new_v4().simple());
            diesel::sql_query(format!("CREATE SCHEMA {schema}"))
                .execute(&mut conn)
                .await
                .unwrap();
            diesel::sql_query(format!("SET LOCAL search_path TO {schema}"))
                .execute(&mut conn)
                .await
                .unwrap();
            for stmt in SCHEMA {
                diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
            }
            // The rating tables as production has them: the pre-alt shape above,
            // migrated by the real migration.
            let sql: String = H2H_PER_ALT_MIGRATION_UP
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");
            for stmt in sql.split(';') {
                if !stmt.trim().is_empty() {
                    diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
                }
            }
            crate::testchar::apply_migration_for_tests(&mut conn).await;
            Some(conn)
        }

        async fn pre_h2h_flag_fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            conn.begin_test_transaction()
                .await
                .expect("test transaction");
            let schema = format!("t{}", Uuid::new_v4().simple());
            diesel::sql_query(format!("CREATE SCHEMA {schema}"))
                .execute(&mut conn)
                .await
                .unwrap();
            diesel::sql_query(format!("SET LOCAL search_path TO {schema}"))
                .execute(&mut conn)
                .await
                .unwrap();
            for stmt in [
                "CREATE TABLE arena_matches ( \
                     ticket_id UUID PRIMARY KEY, user_id UUID NOT NULL, status TEXT NOT NULL, \
                     game_session_id UUID, paired BOOLEAN NOT NULL DEFAULT FALSE, \
                     recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), resolved_at TIMESTAMPTZ)",
                "CREATE TABLE arena_match_results ( \
                     id UUID PRIMARY KEY, character_id UUID NOT NULL, opponent_character_id UUID, \
                     game_session_id UUID, win BOOLEAN NOT NULL, rounds_won INTEGER NOT NULL DEFAULT 0, \
                     rounds_lost INTEGER NOT NULL DEFAULT 0, recorded_at TIMESTAMPTZ NOT NULL DEFAULT now())",
                "CREATE TABLE characters (id UUID PRIMARY KEY, character JSONB NOT NULL)",
            ] {
                diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
            }
            crate::testchar::apply_migration_for_tests(&mut conn).await;
            Some(conn)
        }

        async fn run_h2h_flag_migration(conn: &mut AsyncPgConnection) {
            for stmt in H2H_FLAG_MIGRATION_UP.split(';') {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    diesel::sql_query(stmt).execute(&mut *conn).await.unwrap();
                }
            }
        }

        async fn seed_character(conn: &mut AsyncPgConnection, id: Uuid) {
            diesel::sql_query("INSERT INTO characters (id, character) VALUES ($1, $2)")
                .bind::<diesel::sql_types::Uuid, _>(id)
                .bind::<diesel::sql_types::Jsonb, _>(
                    serde_json::json!({"name":"fighter","level":1}),
                )
                .execute(conn)
                .await
                .unwrap();
        }

        async fn seed_season(conn: &mut AsyncPgConnection, id: Uuid, start: i64, end: i64) {
            diesel::sql_query(
                "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at) \
                 VALUES ($1, 1, 'test', $2, $3)",
            )
            .bind::<diesel::sql_types::Uuid, _>(id)
            .bind::<diesel::sql_types::BigInt, _>(start)
            .bind::<diesel::sql_types::BigInt, _>(end)
            .execute(conn)
            .await
            .unwrap();
        }

        async fn seed_match(
            conn: &mut AsyncPgConnection,
            gsid: Uuid,
            a: Uuid,
            b: Uuid,
            at: &str,
            paired: bool,
            is_h2h: bool,
            score: (i32, i32),
        ) {
            diesel::sql_query(
                "INSERT INTO arena_matches (ticket_id, user_id, status, game_session_id, paired) \
                 VALUES ($1, $2, 'matched', $3, $4)",
            )
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(gsid)
            .bind::<diesel::sql_types::Bool, _>(paired)
            .execute(&mut *conn)
            .await
            .unwrap();
            for (character_id, opponent_character_id, rounds_won, rounds_lost, win) in [
                (a, b, score.0, score.1, score.0 > score.1),
                (b, a, score.1, score.0, score.1 > score.0),
            ] {
                diesel::sql_query(
                    "INSERT INTO arena_match_results \
                     (id, character_id, opponent_character_id, game_session_id, win, rounds_won, rounds_lost, recorded_at, is_h2h) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9)",
                )
                .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
                .bind::<diesel::sql_types::Uuid, _>(character_id)
                .bind::<diesel::sql_types::Uuid, _>(opponent_character_id)
                .bind::<diesel::sql_types::Uuid, _>(gsid)
                .bind::<diesel::sql_types::Bool, _>(win)
                .bind::<diesel::sql_types::Integer, _>(rounds_won)
                .bind::<diesel::sql_types::Integer, _>(rounds_lost)
                .bind::<diesel::sql_types::Text, _>(at)
                .bind::<diesel::sql_types::Bool, _>(is_h2h)
                .execute(&mut *conn)
                .await
                .unwrap();
            }
        }

        async fn seed_one_sided_match_result(
            conn: &mut AsyncPgConnection,
            gsid: Uuid,
            character_id: Uuid,
            opponent_character_id: Uuid,
            at: &str,
            paired: bool,
            is_h2h: bool,
            score: (i32, i32),
        ) {
            diesel::sql_query(
                "INSERT INTO arena_matches (ticket_id, user_id, status, game_session_id, paired) \
                 VALUES ($1, $2, 'matched', $3, $4)",
            )
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(gsid)
            .bind::<diesel::sql_types::Bool, _>(paired)
            .execute(&mut *conn)
            .await
            .unwrap();
            diesel::sql_query(
                "INSERT INTO arena_match_results \
                 (id, character_id, opponent_character_id, game_session_id, win, rounds_won, rounds_lost, recorded_at, is_h2h) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9)",
            )
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(character_id)
            .bind::<diesel::sql_types::Uuid, _>(opponent_character_id)
            .bind::<diesel::sql_types::Uuid, _>(gsid)
            .bind::<diesel::sql_types::Bool, _>(score.0 > score.1)
            .bind::<diesel::sql_types::Integer, _>(score.0)
            .bind::<diesel::sql_types::Integer, _>(score.1)
            .bind::<diesel::sql_types::Text, _>(at)
            .bind::<diesel::sql_types::Bool, _>(is_h2h)
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        async fn all_time(conn: &mut AsyncPgConnection) -> Vec<RatingRow> {
            diesel::sql_query(
                "SELECT character_id, rating, wins, losses, ties, matches \
                 FROM arena_h2h_ratings ORDER BY character_id",
            )
            .get_results(conn)
            .await
            .unwrap()
        }

        async fn seasonal(conn: &mut AsyncPgConnection) -> Vec<SeasonRatingRow> {
            diesel::sql_query(
                "SELECT season_id, character_id, rating, matches \
                 FROM arena_h2h_season_ratings ORDER BY season_id, character_id",
            )
            .get_results(conn)
            .await
            .unwrap()
        }

        async fn h2h_flags(conn: &mut AsyncPgConnection) -> Vec<H2hFlagRow> {
            diesel::sql_query(
                "SELECT character_id, is_h2h FROM arena_match_results ORDER BY character_id",
            )
            .get_results(conn)
            .await
            .unwrap()
        }

        async fn seed_match_ticket_count(
            conn: &mut AsyncPgConnection,
            gsid: Uuid,
            paired: bool,
            tickets: usize,
        ) {
            for _ in 0..tickets {
                diesel::sql_query(
                    "INSERT INTO arena_matches (ticket_id, user_id, status, game_session_id, paired) \
                     VALUES ($1, $2, 'matched', $3, $4)",
                )
                .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
                .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
                .bind::<diesel::sql_types::Uuid, _>(gsid)
                .bind::<diesel::sql_types::Bool, _>(paired)
                .execute(&mut *conn)
                .await
                .unwrap();
            }
        }

        async fn seed_pre_flag_audit(
            conn: &mut AsyncPgConnection,
            gsid: Uuid,
            character_id: Uuid,
            opponent_character_id: Uuid,
            score: (i32, i32),
        ) {
            diesel::sql_query(
                "INSERT INTO arena_match_results \
                 (id, character_id, opponent_character_id, game_session_id, win, rounds_won, rounds_lost) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(character_id)
            .bind::<diesel::sql_types::Uuid, _>(opponent_character_id)
            .bind::<diesel::sql_types::Uuid, _>(gsid)
            .bind::<diesel::sql_types::Bool, _>(score.0 > score.1)
            .bind::<diesel::sql_types::Integer, _>(score.0)
            .bind::<diesel::sql_types::Integer, _>(score.1)
            .execute(&mut *conn)
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn startup_reconcile_fills_empty_tables_and_is_idempotent() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — h2h reconcile SQL not verified");
                return;
            };
            let (old, new) = (Uuid::from_u128(1000), Uuid::from_u128(2000));
            seed_season(&mut conn, old, 1_700_000_000, 1_700_086_400).await;
            seed_season(&mut conn, new, 1_700_086_400, 1_700_172_800).await;
            let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
            for id in [a, b, c] {
                seed_character(&mut conn, id).await;
            }

            seed_match(
                &mut conn,
                Uuid::from_u128(10),
                a,
                b,
                "2023-11-14 22:13:20+00",
                true,
                true,
                (2, 0),
            )
            .await;
            seed_match(
                &mut conn,
                Uuid::from_u128(11),
                b,
                c,
                "2023-11-15 22:13:20+00",
                true,
                true,
                (1, 1),
            )
            .await;
            seed_match(
                &mut conn,
                Uuid::from_u128(12),
                a,
                c,
                "2023-11-16 22:13:20+00",
                false,
                false,
                (2, 0),
            )
            .await;

            let cfg = H2hRatingConfig::default();
            let report = reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();
            assert_eq!(
                report,
                H2hReconcileReport {
                    characters: 3,
                    matches: 2
                }
            );
            let first_all = all_time(&mut conn).await;
            let first_seasons = seasonal(&mut conn).await;
            assert_eq!(
                first_all.len(),
                3,
                "paired h2h history should populate empty all-time"
            );
            assert_eq!(
                first_all.iter().map(|r| r.matches).sum::<i32>(),
                4,
                "two matches touch two humans each; the AI match is excluded"
            );
            assert_eq!(
                first_seasons.len(),
                4,
                "one old-season and one new-season match"
            );
            assert!(
                first_seasons
                    .iter()
                    .any(|r| r.season_id == new && r.character_id == b),
                "the boundary match belongs to the season starting at that second"
            );
            assert!(
                first_seasons
                    .iter()
                    .all(|r| !(r.season_id == new && r.character_id == a)),
                "the false paired=false AI match must not leak onto a season board"
            );

            let second = reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();
            assert_eq!(second, report);
            assert_eq!(all_time(&mut conn).await, first_all);
            assert_eq!(seasonal(&mut conn).await, first_seasons);
        }

        #[tokio::test]
        async fn replay_uses_durable_h2h_flag_not_paired_marker() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — h2h reconcile SQL not verified");
                return;
            };
            let season = Uuid::from_u128(1000);
            seed_season(&mut conn, season, 1_700_000_000, 1_700_086_400).await;
            let (a, b, c, d) = (
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
                Uuid::from_u128(4),
            );
            for id in [a, b, c, d] {
                seed_character(&mut conn, id).await;
            }

            seed_match(
                &mut conn,
                Uuid::from_u128(20),
                a,
                b,
                "2023-11-14 22:13:20+00",
                false,
                true,
                (2, 0),
            )
            .await;
            seed_match(
                &mut conn,
                Uuid::from_u128(21),
                c,
                d,
                "2023-11-14 22:14:20+00",
                true,
                false,
                (2, 0),
            )
            .await;

            let cfg = H2hRatingConfig::default();
            let report = reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();
            assert_eq!(
                report,
                H2hReconcileReport {
                    characters: 2,
                    matches: 1
                }
            );
            let rows = all_time(&mut conn).await;
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().any(|r| r.character_id == a));
            assert!(rows.iter().any(|r| r.character_id == b));
            assert!(
                rows.iter()
                    .all(|r| r.character_id != c && r.character_id != d),
                "paired=true cannot make an AI/non-h2h audit row enter replay"
            );
        }

        #[tokio::test]
        async fn h2h_flag_migration_backfills_paired_and_reciprocal_human_rows_only() {
            let Some(mut conn) = pre_h2h_flag_fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — h2h migration SQL not verified");
                return;
            };
            let paired_a = Uuid::from_u128(1);
            let paired_b = Uuid::from_u128(2);
            let reciprocal_a = Uuid::from_u128(3);
            let reciprocal_b = Uuid::from_u128(4);
            let ai_human = Uuid::from_u128(5);
            let ai_copy = Uuid::from_u128(6);
            for id in [
                paired_a,
                paired_b,
                reciprocal_a,
                reciprocal_b,
                ai_human,
                ai_copy,
            ] {
                seed_character(&mut conn, id).await;
            }

            let paired_gsid = Uuid::from_u128(30);
            seed_match_ticket_count(&mut conn, paired_gsid, true, 2).await;
            seed_pre_flag_audit(&mut conn, paired_gsid, paired_a, paired_b, (2, 0)).await;

            let reciprocal_gsid = Uuid::from_u128(31);
            seed_match_ticket_count(&mut conn, reciprocal_gsid, false, 2).await;
            seed_pre_flag_audit(
                &mut conn,
                reciprocal_gsid,
                reciprocal_a,
                reciprocal_b,
                (2, 0),
            )
            .await;
            seed_pre_flag_audit(
                &mut conn,
                reciprocal_gsid,
                reciprocal_b,
                reciprocal_a,
                (0, 2),
            )
            .await;

            let ai_gsid = Uuid::from_u128(32);
            seed_match_ticket_count(&mut conn, ai_gsid, false, 1).await;
            seed_pre_flag_audit(&mut conn, ai_gsid, ai_human, ai_copy, (2, 0)).await;
            seed_pre_flag_audit(&mut conn, ai_gsid, ai_copy, ai_human, (0, 2)).await;

            run_h2h_flag_migration(&mut conn).await;

            assert_eq!(
                h2h_flags(&mut conn).await,
                vec![
                    H2hFlagRow {
                        character_id: paired_a,
                        is_h2h: true
                    },
                    H2hFlagRow {
                        character_id: reciprocal_a,
                        is_h2h: true
                    },
                    H2hFlagRow {
                        character_id: reciprocal_b,
                        is_h2h: true
                    },
                    H2hFlagRow {
                        character_id: ai_human,
                        is_h2h: false
                    },
                    H2hFlagRow {
                        character_id: ai_copy,
                        is_h2h: false
                    },
                ],
                "paired history and reciprocal two-ticket human history are h2h; solo AI stays out"
            );
        }

        #[tokio::test]
        async fn reconcile_replays_surviving_higher_uuid_audit_row_in_canonical_order() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — h2h reconcile SQL not verified");
                return;
            };
            let season = Uuid::from_u128(1000);
            let lower = Uuid::from_u128(1);
            let higher = Uuid::from_u128(2);
            seed_season(&mut conn, season, 1_700_000_000, 1_700_086_400).await;
            for id in [lower, higher] {
                seed_character(&mut conn, id).await;
            }

            seed_one_sided_match_result(
                &mut conn,
                Uuid::from_u128(20),
                higher,
                lower,
                "2023-11-14 22:13:20+00",
                true,
                true,
                (0, 2),
            )
            .await;

            let cfg = H2hRatingConfig::default();
            let report = reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();
            assert_eq!(
                report,
                H2hReconcileReport {
                    characters: 2,
                    matches: 1
                }
            );
            assert_eq!(
                all_time(&mut conn).await,
                vec![
                    RatingRow {
                        character_id: lower,
                        rating: 1024,
                        wins: 1,
                        losses: 0,
                        ties: 0,
                        matches: 1,
                    },
                    RatingRow {
                        character_id: higher,
                        rating: 976,
                        wins: 0,
                        losses: 1,
                        ties: 0,
                        matches: 1,
                    },
                ]
            );
            assert_eq!(seasonal(&mut conn).await.len(), 2);
        }

        // ---- One row per alt (tracker #373) --------------------------------

        async fn seed_alt_character(
            conn: &mut AsyncPgConnection,
            id: Uuid,
            live_alt: Option<Uuid>,
            live_name: &str,
        ) {
            diesel::sql_query(
                "INSERT INTO characters (id, character, source_alt_uuid) VALUES ($1, $2, $3)",
            )
            .bind::<diesel::sql_types::Uuid, _>(id)
            .bind::<diesel::sql_types::Jsonb, _>(
                serde_json::json!({"name": live_name, "level": 40}),
            )
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(live_alt)
            .execute(conn)
            .await
            .unwrap();
        }

        async fn seed_saved_alt(
            conn: &mut AsyncPgConnection,
            character_id: Uuid,
            alt: Uuid,
            name: &str,
            saved_at: i64,
        ) {
            diesel::sql_query(
                "INSERT INTO character_versions (id, character_id, source_alt_uuid, character, saved_at) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(character_id)
            .bind::<diesel::sql_types::Uuid, _>(alt)
            .bind::<diesel::sql_types::Jsonb, _>(serde_json::json!({"name": name, "level": 30}))
            .bind::<diesel::sql_types::BigInt, _>(saved_at)
            .execute(conn)
            .await
            .unwrap();
        }

        /// Both audit rows of one h2h match, each stamped with the alt that played
        /// on that side (`None` = recorded before alts were tracked).
        async fn seed_alt_match(
            conn: &mut AsyncPgConnection,
            gsid: Uuid,
            (a, a_alt): (Uuid, Option<Uuid>),
            (b, b_alt): (Uuid, Option<Uuid>),
            at: &str,
            score: (i32, i32),
        ) {
            for (character_id, alt, opponent, rounds_won, rounds_lost) in [
                (a, a_alt, b, score.0, score.1),
                (b, b_alt, a, score.1, score.0),
            ] {
                diesel::sql_query(
                    "INSERT INTO arena_match_results \
                     (id, character_id, opponent_character_id, game_session_id, win, rounds_won, \
                      rounds_lost, recorded_at, is_h2h, source_alt_uuid) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, true, $9)",
                )
                .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
                .bind::<diesel::sql_types::Uuid, _>(character_id)
                .bind::<diesel::sql_types::Uuid, _>(opponent)
                .bind::<diesel::sql_types::Uuid, _>(gsid)
                .bind::<diesel::sql_types::Bool, _>(rounds_won > rounds_lost)
                .bind::<diesel::sql_types::Integer, _>(rounds_won)
                .bind::<diesel::sql_types::Integer, _>(rounds_lost)
                .bind::<diesel::sql_types::Text, _>(at)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(alt)
                .execute(&mut *conn)
                .await
                .unwrap();
            }
        }

        #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
        struct AltRatingRow {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            character_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            alt_key: Uuid,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            wins: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            losses: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            matches: i32,
        }

        async fn season_alt_rows(conn: &mut AsyncPgConnection, season: Uuid) -> Vec<AltRatingRow> {
            diesel::sql_query(
                "SELECT character_id, alt_key, wins, losses, matches \
                 FROM arena_h2h_season_ratings WHERE season_id = $1 \
                 ORDER BY character_id, alt_key",
            )
            .bind::<diesel::sql_types::Uuid, _>(season)
            .get_results(conn)
            .await
            .unwrap()
        }

        /// NBS-2, measured: Ivan (an alt switched out of the live row, whose live
        /// occupant is Ísdís) beat Sephoris twice. The board rated those wins as
        /// the shared character id and named them "Ísdís". spacemunk's win over
        /// Sephoris was likewise listed as newmunk's. Each alt is its own row.
        #[tokio::test]
        async fn reconcile_rates_and_names_each_alt_separately() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — per-alt h2h SQL not verified");
                return;
            };
            let season = Uuid::from_u128(1000);
            seed_season(&mut conn, season, 1_700_000_000, 1_700_086_400).await;
            // Ivan's account: one live row, three alts; Ísdís is live.
            let (ivan_row, isdis, ivan, princess) = (
                Uuid::from_u128(0x7e),
                Uuid::from_u128(0xd8),
                Uuid::from_u128(0xf1),
                Uuid::from_u128(0x26),
            );
            seed_alt_character(&mut conn, ivan_row, Some(isdis), "Ísdís").await;
            seed_saved_alt(&mut conn, ivan_row, ivan, "Ivan (old)", 1_700_000_050).await;
            seed_saved_alt(&mut conn, ivan_row, ivan, "Ivan", 1_700_000_900).await;
            seed_saved_alt(&mut conn, ivan_row, princess, "Princess", 1_700_000_950).await;
            let sephoris = Uuid::from_u128(0xf7);
            seed_alt_character(&mut conn, sephoris, Some(Uuid::from_u128(0x0b)), "Sephoris").await;

            // Ivan beats Sephoris twice, then Ísdís beats her once. A legacy row
            // (no stamp) counts for the alt live now, Ísdís.
            let ivan_side = (ivan_row, Some(ivan));
            let seph = (sephoris, Some(Uuid::from_u128(0x0b)));
            seed_alt_match(
                &mut conn,
                Uuid::from_u128(1),
                ivan_side,
                seph,
                "2023-11-14 22:20:00+00",
                (1, 0),
            )
            .await;
            seed_alt_match(
                &mut conn,
                Uuid::from_u128(2),
                ivan_side,
                seph,
                "2023-11-14 22:22:00+00",
                (2, 0),
            )
            .await;
            seed_alt_match(
                &mut conn,
                Uuid::from_u128(3),
                (ivan_row, None),
                seph,
                "2023-11-14 22:30:00+00",
                (2, 0),
            )
            .await;

            let cfg = H2hRatingConfig::default();
            reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();

            let row = |character_id, alt_key, wins, losses| AltRatingRow {
                character_id,
                alt_key,
                wins,
                losses,
                matches: wins + losses,
            };
            assert_eq!(
                season_alt_rows(&mut conn, season).await,
                vec![
                    row(ivan_row, isdis, 1, 0),
                    row(ivan_row, ivan, 2, 0),
                    row(sephoris, Uuid::from_u128(0x0b), 0, 3),
                ],
                "the shared character id must not merge Ivan's and Ísdís's results"
            );

            let board = h2h_board(&mut conn, Some(season), 1, 100).await.unwrap();
            let named: Vec<(String, Uuid, i32)> = board
                .iter()
                .map(|e| (e.name.clone(), e.alt_key, e.matches))
                .collect();
            assert_eq!(
                named,
                vec![
                    ("Ivan".to_string(), ivan, 2),
                    ("Ísdís".to_string(), isdis, 1),
                    ("Sephoris".to_string(), Uuid::from_u128(0x0b), 3),
                ],
                "a switched-out alt is named from its newest saved version, the live alt from the row"
            );
            assert_eq!(
                board.iter().map(|e| e.rank).collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            assert_eq!(board[0].level, 30, "the switched-out alt's own level");

            // The listing gate is per alt, too: at min 2 matches Ísdís (1) drops out
            // while Ivan (2) stays — the shared id would have listed "Ísdís" at 3.
            let gated = h2h_board(&mut conn, Some(season), 2, 100).await.unwrap();
            assert_eq!(
                gated.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
                vec!["Ivan", "Sephoris"]
            );
            let all_time = h2h_board(&mut conn, None, 1, 100).await.unwrap();
            assert_eq!(all_time.len(), 3, "the all-time board is per alt as well");
        }

        /// An operator test alt (`crate::testchar`) is never rated: a fight
        /// with one leaves both sides off the ratings, so it neither appears on the
        /// Top 100 nor moves its opponent's rating. Other fights are untouched.
        #[tokio::test]
        async fn a_test_alt_is_not_rated_and_does_not_move_its_opponent() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — test-alt h2h exclusion not verified");
                return;
            };
            let season = Uuid::from_u128(1000);
            seed_season(&mut conn, season, 1_700_000_000, 1_700_086_400).await;
            let (row_t, test_alt) = (Uuid::from_u128(0x10), Uuid::from_u128(0x1a));
            let (row_r, real_alt) = (Uuid::from_u128(0x20), Uuid::from_u128(0x2a));
            let (row_o, other_alt) = (Uuid::from_u128(0x30), Uuid::from_u128(0x3a));
            seed_alt_character(&mut conn, row_t, Some(test_alt), "Tester (Test)").await;
            seed_alt_character(&mut conn, row_r, Some(real_alt), "Real").await;
            seed_alt_character(&mut conn, row_o, Some(other_alt), "Other").await;
            // The test alt beats Real; Real beats Other.
            seed_alt_match(
                &mut conn,
                Uuid::from_u128(1),
                (row_t, Some(test_alt)),
                (row_r, Some(real_alt)),
                "2023-11-14 22:20:00+00",
                (2, 0),
            )
            .await;
            seed_alt_match(
                &mut conn,
                Uuid::from_u128(2),
                (row_r, Some(real_alt)),
                (row_o, Some(other_alt)),
                "2023-11-14 22:30:00+00",
                (2, 0),
            )
            .await;
            let cfg = H2hRatingConfig::default();

            // The control: unmarked, all three are rated.
            reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();
            assert_eq!(
                h2h_board(&mut conn, Some(season), 1, 100)
                    .await
                    .unwrap()
                    .len(),
                3
            );
            let all_time = h2h_board(&mut conn, None, 1, 100).await.unwrap();
            let real_unmarked = all_time
                .iter()
                .find(|e| e.alt_key == real_alt)
                .unwrap()
                .clone();

            diesel::sql_query(
                "INSERT INTO test_characters (alt_uuid, user_id, marked_by) VALUES ($1, $1, 't')",
            )
            .bind::<diesel::sql_types::Uuid, _>(test_alt)
            .execute(&mut conn)
            .await
            .unwrap();
            // Even before the rebuild, the board does not list the stale row.
            let stale = h2h_board(&mut conn, Some(season), 1, 100).await.unwrap();
            assert!(stale.iter().all(|e| e.alt_key != test_alt), "never listed");

            reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
                .await
                .unwrap();
            let rows = season_alt_rows(&mut conn, season).await;
            assert!(
                rows.iter().all(|r| r.alt_key != test_alt),
                "no rating row at all"
            );
            let real = rows.iter().find(|r| r.alt_key == real_alt).unwrap();
            assert_eq!(
                (real.wins, real.losses, real.matches),
                (1, 0, 1),
                "only the real fight counts"
            );
            let all_time = h2h_board(&mut conn, None, 1, 100).await.unwrap();
            assert_eq!(
                all_time.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
                vec!["Real", "Other"]
            );
            let real_marked = all_time.iter().find(|e| e.alt_key == real_alt).unwrap();
            assert!(
                real_marked.rating > real_unmarked.rating,
                "Real's loss to the test alt no longer costs rating ({} vs {})",
                real_marked.rating,
                real_unmarked.rating
            );
        }

        /// Each side's alt comes from ITS OWN audit row: an opponent who has since
        /// switched alts keeps the result on the alt that actually fought.
        #[tokio::test]
        async fn the_opponent_is_rated_as_the_alt_that_fought() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — per-alt h2h SQL not verified");
                return;
            };
            let season = Uuid::from_u128(1000);
            seed_season(&mut conn, season, 1_700_000_000, 1_700_086_400).await;
            let (low, high) = (Uuid::from_u128(1), Uuid::from_u128(2));
            let (spacemunk, newmunk) = (Uuid::from_u128(0x2e), Uuid::from_u128(0x47));
            // The higher-id character fought as spacemunk, and is newmunk now.
            seed_alt_character(&mut conn, low, None, "Sephoris").await;
            seed_alt_character(&mut conn, high, Some(newmunk), "newmunk").await;
            seed_saved_alt(&mut conn, high, spacemunk, "spacemunk", 1_700_000_900).await;
            seed_alt_match(
                &mut conn,
                Uuid::from_u128(9),
                (low, None),
                (high, Some(spacemunk)),
                "2023-11-14 22:20:00+00",
                (0, 2),
            )
            .await;

            reconcile_h2h_ratings_with_config(&mut conn, &H2hRatingConfig::default(), false)
                .await
                .unwrap();
            let rows = season_alt_rows(&mut conn, season).await;
            assert_eq!(
                rows,
                vec![
                    // No alt id at all: keyed by the character id itself.
                    AltRatingRow {
                        character_id: low,
                        alt_key: low,
                        wins: 0,
                        losses: 1,
                        matches: 1
                    },
                    AltRatingRow {
                        character_id: high,
                        alt_key: spacemunk,
                        wins: 1,
                        losses: 0,
                        matches: 1
                    },
                ],
                "the canonical (lower-id) row must take the opponent's alt from the opponent's row"
            );
            let board = h2h_board(&mut conn, Some(season), 1, 100).await.unwrap();
            assert_eq!(board[0].name, "spacemunk");
        }

        /// A rating whose alt is neither live nor saved (deleted) is not listed,
        /// and the guild is the alt's own: live membership or the parked one.
        #[tokio::test]
        async fn board_guilds_follow_the_alt_and_deleted_alts_are_not_listed() {
            let Some(mut conn) = fixture().await else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — per-alt h2h SQL not verified");
                return;
            };
            let season = Uuid::from_u128(1000);
            seed_season(&mut conn, season, 1_700_000_000, 1_700_086_400).await;
            let (row, live, parked, gone) = (
                Uuid::from_u128(1),
                Uuid::from_u128(0xa),
                Uuid::from_u128(0xb),
                Uuid::from_u128(0xc),
            );
            let opp = Uuid::from_u128(2);
            seed_alt_character(&mut conn, row, Some(live), "Live").await;
            seed_alt_character(&mut conn, opp, None, "Opp").await;
            diesel::sql_query(
                "INSERT INTO character_versions (id, character_id, source_alt_uuid, character, server_state, saved_at) \
                 VALUES ($1, $2, $3, '{\"name\":\"Parked\",\"level\":7}', \
                         '{\"parkedGuildMembers\":[{\"guild_id\":\"g-parked\"}]}', 1700000500)",
            )
            .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
            .bind::<diesel::sql_types::Uuid, _>(row)
            .bind::<diesel::sql_types::Uuid, _>(parked)
            .execute(&mut conn)
            .await
            .unwrap();
            diesel::sql_query(
                "INSERT INTO guilds VALUES ('g-live', 'Live Guild'), ('g-parked', 'Parked Guild')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            diesel::sql_query("INSERT INTO guild_members VALUES ('g-live', $1)")
                .bind::<diesel::sql_types::Uuid, _>(row)
                .execute(&mut conn)
                .await
                .unwrap();
            for (n, alt) in [(1u128, live), (2, parked), (3, gone)] {
                seed_alt_match(
                    &mut conn,
                    Uuid::from_u128(n),
                    (row, Some(alt)),
                    (opp, None),
                    "2023-11-14 22:20:00+00",
                    (2, 0),
                )
                .await;
            }
            reconcile_h2h_ratings_with_config(&mut conn, &H2hRatingConfig::default(), false)
                .await
                .unwrap();
            let board = h2h_board(&mut conn, Some(season), 1, 100).await.unwrap();
            let mut seen: Vec<(String, Option<String>)> = board
                .iter()
                .filter(|e| e.character_id == row)
                .map(|e| (e.name.clone(), e.guild_name.clone()))
                .collect();
            seen.sort();
            assert_eq!(
                seen,
                vec![
                    ("Live".to_string(), Some("Live Guild".to_string())),
                    ("Parked".to_string(), Some("Parked Guild".to_string())),
                ],
                "the deleted alt is not listed; each alt carries its own guild"
            );
        }
    }

    #[test]
    fn sixty_percent_vs_ai_reaches_imperial_in_expected_window() {
        let cfg = RankingConfig::default();
        let mut trophies = 0;
        let mut reached = None;
        for i in 1..=260 {
            let outcome = if i % 5 < 3 { win20() } else { loss02() };
            let delta = trophy_delta(
                &cfg,
                TrophyDeltaInput {
                    outcome,
                    own_trophies: trophies,
                    opponent_trophies_for_pricing: trophies,
                    h2h: false,
                    h2h_pair_matches_today: 0,
                },
            );
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
