//! **Arena season lifecycle** — the season a character's PvP counters belong to,
//! and the rollover that closes one season and opens the next.
//!
//! # What retail did, and how we know
//!
//! Every captured character carries two related fields:
//!
//! * `pvpSeasonId` — the season its live PvP counters belong to. Already modelled
//!   on [`CompleteCharacter`], but until now nothing in this server ever read or
//!   wrote it.
//! * `pvpSeasonHistory` — a **map from season UUID to a frozen copy of the eleven
//!   live PvP counters**. Verbatim shape from the prod capture snapshot
//!   (`api_captures`, 5 088 responses carrying the field, 9 characters,
//!   61 distinct season UUIDs):
//!
//!   ```json
//!   "pvpSeasonHistory": {
//!     "ea65edb8-35e1-4328-b36c-e3138300208c": {
//!       "highestArenaReached": 3, "highestLevelArenaReached": 5,
//!       "highestLevelArenaReachedTimeSecs": 1748500000,
//!       "matchmakingPvpTrophies": 1215, "numberPvpMatchPlayed": 219,
//!       "pvpChestMeter": 2, "pvpExceptionEasierMatchRemaining": 0,
//!       "pvpExceptionHarderMatchRemaining": 0, "pvpTrophies": 1215,
//!       "pvpWinningStreak": 2, "trophyCountModifier": -40
//!     }
//!   }
//!   ```
//!
//! ## The cadence — `[Class 2, capture-derived]`
//!
//! The 61 archived seasons run from 2020-10 to 2026-05 and are **calendar
//! monthly**: taking `highestLevelArenaReachedTimeSecs` as a within-season
//! timestamp, no two seasons' observation windows overlap and every window ends
//! on the last day of a month or the first of the next. Recent examples:
//!
//! ```text
//! 4df8ebe3…  2026-02-18 .. 2026-03-01
//! 177b6c32…  2026-03-03 .. 2026-04-01
//! ca35eb60…  2026-04-23 .. 2026-04-28
//! ea65edb8…  2026-05-05 .. 2026-05-31
//! ```
//!
//! Day-level resolution only: the captures bound the boundary to the turn of the
//! UTC month, they do not pin the exact instant or Bethesda's chosen time zone.
//!
//! ## The reset — `[Class 2, capture-derived]`
//!
//! A rollover **zeroes everything**. Evidence, from the same snapshot:
//!
//! * `numberPvpMatchPlayed` is per-season, not lifetime — char `38c987fd` archives
//!   159 / 309 / 272 / 219 across four consecutive seasons and is at 45 in the
//!   live one.
//! * Two characters' first match of the new season leaves them at
//!   `pvpTrophies == matchmakingPvpTrophies` with `numberPvpMatchPlayed == 1`
//!   (`128f1c2a` at 49, `97cf5fa6` at 57) — only reachable from a zero start.
//! * `ee5b1920` archives season `ca35eb60` as an all-zero block and its live block
//!   is all-zero too, including `highestArenaReached: 1` / `highestLevelArenaReached: 1`
//!   / `highestLevelArenaReachedTimeSecs: 0` — so the ladder rung resets as well.
//!
//! No captured or shipped asset states retail's rule as a *rule* — the shipped
//! `PvpSeasonsData._trophyResetRule` (a string, per `dump.cs`) is not in any APK
//! bundle we hold. So [`TrophyResetRule`] carries exactly one variant, the one the
//! captures show; a soft/decayed reset is deliberately **not** offered rather than
//! invented.

use serde_json::{Map, Value, json};
use uuid::Uuid;

use blades_lib::static_data::Announcement;
use blades_lib::user_data::CompleteCharacter;

/// Which trophy-scoring model a season runs.
///
/// Retail's K-factor already varies with a player's own trophy count (see
/// [`super::pvp_tuning::ELO_FACTORS`]) — that is the "early days vs later days"
/// behaviour, and it is shipped data. This enum exists for the *other* reading of
/// that question: whether retail ALSO changed the algorithm between eras of the
/// live game. We have found no evidence either way (see
/// `docs/arena-season-model.md`), so the shipped model is the default and the
/// alternative is available per-season without a redeploy of the scoring code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoringVariant {
    /// The shipped tables: banded K-factor x round-score-weighted Elo.
    /// `[Class 1]`.
    Shipped,
    /// A single flat K-factor for every trophy band, round score ignored — what
    /// this server did before the shipped tables were recovered, kept switchable
    /// so a season can be run the old way for comparison. `[Class 3 — modelled]`.
    #[allow(dead_code)] // Deliberate opt-in comparator; no live season selects it yet.
    FlatK(i64),
}

/// How a season rollover treats a character's trophies.
///
/// One variant on purpose: see the module docs. Adding a second means finding
/// evidence for it first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrophyResetRule {
    /// Archive the live block verbatim, then zero every PvP counter and drop the
    /// character back to arena 1 level 1. Capture-derived `[Class 2]`.
    HardReset,
}

/// One arena season.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonConfig {
    /// The season's UUID — the key it occupies in `pvpSeasonHistory` once closed,
    /// and the value `character.pvpSeasonId` carries while it is live.
    pub id: Uuid,
    /// Human-facing season number, for `UI.Arena.Loadout.SeasonNumber` ("Season {0}").
    pub number: u32,
    /// Inclusive start, unix seconds UTC.
    pub start_unix: i64,
    /// Exclusive end, unix seconds UTC.
    pub end_unix: i64,
    /// Which scoring model this season runs.
    pub scoring: ScoringVariant,
    /// How the rollover INTO this season treated the previous one.
    pub reset: TrophyResetRule,
}

/// `2026-09-01T00:00:00Z`.
pub const SEASON_2026_09_START: i64 = 1_788_220_800;
/// `2026-10-01T00:00:00Z` — the retail cadence is calendar-monthly.
pub const SEASON_2026_09_END: i64 = 1_790_812_800;

/// The season this build opens on 2026-09-01.
///
/// The UUID is **ours**, freshly minted — it is not one of the 61 retail season
/// UUIDs, so a transferred retail character's `pvpSeasonHistory` can never
/// collide with it.
///
/// `number` is a local count, not retail's. Retail's own season numbering is not
/// recoverable: `pvpSeasonHistory` is an unordered map and no captured response
/// carries a season index, so any number we printed for a retail season would be
/// a guess. This is season 1 *of this server*.
pub const SEASON_2026_09: SeasonConfig = SeasonConfig {
    id: Uuid::from_u128(0x9b3f_1c74_5a20_4f8e_9d61_2c07_ab54_e319),
    number: 1,
    start_unix: SEASON_2026_09_START,
    end_unix: SEASON_2026_09_END,
    scoring: ScoringVariant::Shipped,
    reset: TrophyResetRule::HardReset,
};

/// Every season this build knows about, oldest first.
pub const SEASONS: [SeasonConfig; 1] = [SEASON_2026_09];

/// Wall-clock unix seconds. Saturates to 0 before the epoch rather than panicking.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The scoring model in force right now.
///
/// Falls back to [`ScoringVariant::Shipped`] outside any configured season, so
/// the arena keeps paying retail-shaped trophies between seasons instead of
/// silently changing behaviour at midnight.
pub fn active_scoring() -> ScoringVariant {
    season_at(now_unix()).map(|s| s.scoring).unwrap_or(ScoringVariant::Shipped)
}

/// The season live at `now` (unix seconds), if any.
///
/// Returns `None` before the first season opens, so the arena runs unseasoned
/// rather than silently back-dating players into a season that has not started.
pub fn season_at(now: i64) -> Option<&'static SeasonConfig> {
    SEASONS.iter().find(|s| now >= s.start_unix && now < s.end_unix)
}

/// The eleven-key PvP counter block, exactly as retail archives it.
///
/// Key set and order verified against the capture snapshot; see the module docs.
fn snapshot(ch: &CompleteCharacter) -> Value {
    json!({
        "highestArenaReached": ch.highest_arena_reached,
        "highestLevelArenaReached": ch.highest_level_arena_reached,
        "highestLevelArenaReachedTimeSecs": ch.highest_level_arena_reached_time_secs,
        "matchmakingPvpTrophies": ch.matchmaking_pvp_trophies,
        "numberPvpMatchPlayed": ch.number_pvp_match_played,
        "pvpChestMeter": ch.pvp_chest_meter,
        "pvpExceptionEasierMatchRemaining": ch.pvp_exception_easier_match_remaining,
        "pvpExceptionHarderMatchRemaining": ch.pvp_exception_harder_match_remaining,
        "pvpTrophies": ch.pvp_trophies,
        "pvpWinningStreak": ch.pvp_winning_streak,
        "trophyCountModifier": ch.trophy_count_modifier,
    })
}

/// Zero every live PvP counter — the [`TrophyResetRule::HardReset`] body.
fn zero_live_counters(ch: &mut CompleteCharacter) {
    ch.pvp_trophies = 0;
    ch.matchmaking_pvp_trophies = 0;
    ch.number_pvp_match_played = 0;
    ch.pvp_winning_streak = 0;
    ch.pvp_chest_meter = 0;
    ch.trophy_count_modifier = 0;
    ch.pvp_exception_easier_match_remaining = 0;
    ch.pvp_exception_harder_match_remaining = 0;
    ch.highest_arena_reached = 1;
    ch.highest_level_arena_reached = 1;
    ch.highest_level_arena_reached_time_secs = 0;
}

/// What a rollover did to one character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloverOutcome {
    /// The character's counters were zeroed and its `pvpSeasonId` re-stamped.
    pub reset: bool,
    /// The closing season the previous standings were filed under, if there was
    /// one to file. `None` when the character had never been stamped with a
    /// season (`pvpSeasonId` nil) — there is no sane key to archive it under, and
    /// inventing one would put junk in the client's end-of-season screen.
    pub archived_under: Option<Uuid>,
}

/// Roll one character into `season`, archiving the season it was in.
///
/// **Idempotent**: a character already stamped with `season.id` is left
/// untouched, so re-running the rollover cannot wipe a player who has since
/// scored. Returns what it did.
pub fn roll_character_into(ch: &mut CompleteCharacter, season: &SeasonConfig) -> RolloverOutcome {
    if ch.pvp_season_id == season.id {
        return RolloverOutcome { reset: false, archived_under: None };
    }

    let closing = ch.pvp_season_id;
    let archived_under = if closing.is_nil() {
        None
    } else {
        let mut hist: Map<String, Value> = match ch.pvp_season_history.take() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        hist.insert(closing.to_string(), snapshot(ch));
        ch.pvp_season_history = Value::Object(hist);
        Some(closing)
    };

    match season.reset {
        TrophyResetRule::HardReset => zero_live_counters(ch),
    }
    ch.pvp_season_id = season.id;

    RolloverOutcome { reset: true, archived_under }
}

/// The in-game news entry announcing a season.
///
/// Shape is the captured `announcements.json` record verbatim — the five keys
/// `assetUrl` / `id` / `startTime` / `ttl` / `type`, nothing more. All 156
/// captured retail records are `"type": "BASIC"`, and 155 of them run for exactly
/// `86_340` seconds (one day less a minute), which is what this reproduces.
///
/// The `assetUrl` follows retail's `/{YYYY}/{MM}/{DD}/{id}` path on
/// `announcements.blades.bgs.services` — a host this server already answers on
/// (see `status.rs`). **No banner image is shipped**: like the 156 replayed retail
/// entries, the client will show the news item and quietly fail to load its
/// artwork until someone drops a PNG at that path.
pub fn season_announcement(season: &SeasonConfig) -> Announcement {
    let (year, month, day) = ymd_from_unix(season.start_unix);
    let id = announcement_id(season);
    Announcement {
        asset_url: format!(
            "https://announcements.blades.bgs.services/{year:04}/{month:02}/{day:02}/{id}"
        ),
        id: id.to_string(),
        start_time: season.start_unix,
        ttl: season.start_unix + ANNOUNCEMENT_TTL_SECS,
        r#type: "BASIC".to_string(),
    }
}

/// Civil `(year, month, day)` in UTC for a unix timestamp.
///
/// Howard Hinnant's `civil_from_days`. Written out rather than pulling in a date
/// crate for one call — and covered by a test against the two season boundaries,
/// which are the only timestamps this is ever asked about.
pub fn ymd_from_unix(unix: i64) -> (i32, u32, u32) {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + i64::from(m <= 2)) as i32, m as u32, d as u32)
}

/// `ttl - startTime` on 155 of the 156 captured retail announcements.
pub const ANNOUNCEMENT_TTL_SECS: i64 = 86_340;

/// A deterministic announcement id derived from the season id, so regenerating
/// the news file cannot produce a second entry for the same season.
fn announcement_id(season: &SeasonConfig) -> Uuid {
    // Flip the top nibble of the season id: stable, collision-free against the
    // season id itself, and reproducible without a random source.
    Uuid::from_u128(season.id.as_u128() ^ (0xF << 124))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A character carrying a finished retail season, shaped like the capture.
    fn retail_character(season: Uuid) -> CompleteCharacter {
        let mut ch = CompleteCharacter::default();
        ch.pvp_season_id = season;
        ch.pvp_trophies = 1215;
        ch.matchmaking_pvp_trophies = 1250;
        ch.number_pvp_match_played = 219;
        ch.pvp_winning_streak = 2;
        ch.pvp_chest_meter = 2;
        ch.trophy_count_modifier = -40;
        ch.pvp_exception_easier_match_remaining = 1;
        ch.pvp_exception_harder_match_remaining = 0;
        ch.highest_arena_reached = 3;
        ch.highest_level_arena_reached = 5;
        ch.highest_level_arena_reached_time_secs = 1_748_500_000;
        ch
    }

    #[test]
    fn rollover_archives_the_closing_season_under_its_own_uuid() {
        let closing = Uuid::parse_str("ea65edb8-35e1-4328-b36c-e3138300208c").unwrap();
        let mut ch = retail_character(closing);

        // Precondition: there is something to archive and nowhere it is archived yet.
        assert_eq!(ch.pvp_trophies, 1215, "fixture must start with real standings");
        assert!(ch.pvp_season_history.is_null(), "fixture must start with no history");

        let out = roll_character_into(&mut ch, &SEASON_2026_09);
        assert!(out.reset);
        assert_eq!(out.archived_under, Some(closing));

        let hist = ch.pvp_season_history.as_object().expect("history is a map");
        let blk = hist.get(&closing.to_string()).expect("closing season archived");

        // The exact eleven-key retail block, no more and no less.
        let mut keys: Vec<&str> = blk.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "highestArenaReached",
                "highestLevelArenaReached",
                "highestLevelArenaReachedTimeSecs",
                "matchmakingPvpTrophies",
                "numberPvpMatchPlayed",
                "pvpChestMeter",
                "pvpExceptionEasierMatchRemaining",
                "pvpExceptionHarderMatchRemaining",
                "pvpTrophies",
                "pvpWinningStreak",
                "trophyCountModifier",
            ]
        );
        assert_eq!(blk["pvpTrophies"], 1215);
        assert_eq!(blk["matchmakingPvpTrophies"], 1250);
        assert_eq!(blk["numberPvpMatchPlayed"], 219);
        assert_eq!(blk["highestArenaReached"], 3);
    }

    #[test]
    fn rollover_zeroes_every_live_counter() {
        let closing = Uuid::parse_str("ea65edb8-35e1-4328-b36c-e3138300208c").unwrap();
        let mut ch = retail_character(closing);
        roll_character_into(&mut ch, &SEASON_2026_09);

        assert_eq!(ch.pvp_trophies, 0);
        assert_eq!(ch.matchmaking_pvp_trophies, 0);
        assert_eq!(ch.number_pvp_match_played, 0);
        assert_eq!(ch.pvp_winning_streak, 0);
        assert_eq!(ch.pvp_chest_meter, 0);
        assert_eq!(ch.trophy_count_modifier, 0);
        assert_eq!(ch.pvp_exception_easier_match_remaining, 0);
        assert_eq!(ch.pvp_exception_harder_match_remaining, 0);
        assert_eq!(ch.highest_arena_reached, 1);
        assert_eq!(ch.highest_level_arena_reached, 1);
        assert_eq!(ch.highest_level_arena_reached_time_secs, 0);
        assert_eq!(ch.pvp_season_id, SEASON_2026_09.id);
    }

    #[test]
    fn rollover_is_idempotent_and_cannot_wipe_a_player_twice() {
        let closing = Uuid::parse_str("ea65edb8-35e1-4328-b36c-e3138300208c").unwrap();
        let mut ch = retail_character(closing);
        assert!(roll_character_into(&mut ch, &SEASON_2026_09).reset);

        // Player scores in the new season…
        ch.pvp_trophies = 640;
        ch.matchmaking_pvp_trophies = 700;
        ch.number_pvp_match_played = 40;

        // …and the rollover is run again by mistake.
        let out = roll_character_into(&mut ch, &SEASON_2026_09);
        assert!(!out.reset, "a second rollover into the same season must be a no-op");
        assert_eq!(out.archived_under, None);
        assert_eq!(ch.pvp_trophies, 640, "in-season progress must survive");
        assert_eq!(ch.number_pvp_match_played, 40);
        assert_eq!(
            ch.pvp_season_history.as_object().unwrap().len(),
            1,
            "no second archive entry"
        );
    }

    #[test]
    fn a_character_that_never_had_a_season_is_stamped_but_not_archived() {
        let mut ch = CompleteCharacter::default();
        assert!(ch.pvp_season_id.is_nil(), "precondition: fresh char has a nil season");

        let out = roll_character_into(&mut ch, &SEASON_2026_09);
        assert!(out.reset);
        assert_eq!(out.archived_under, None, "nil season must not become a history key");
        assert!(
            ch.pvp_season_history.as_object().map(|m| m.is_empty()).unwrap_or(true),
            "no junk key in pvpSeasonHistory"
        );
        assert_eq!(ch.pvp_season_id, SEASON_2026_09.id);
    }

    #[test]
    fn existing_season_history_is_preserved_across_a_rollover() {
        let older = Uuid::parse_str("ca35eb60-6988-4fa9-a42c-f932dc863ec4").unwrap();
        let closing = Uuid::parse_str("ea65edb8-35e1-4328-b36c-e3138300208c").unwrap();
        let mut ch = retail_character(closing);
        ch.pvp_season_history = json!({ older.to_string(): { "pvpTrophies": 1599 } });

        roll_character_into(&mut ch, &SEASON_2026_09);
        let hist = ch.pvp_season_history.as_object().unwrap();
        assert_eq!(hist.len(), 2, "the older season must not be dropped");
        assert_eq!(hist[&older.to_string()]["pvpTrophies"], 1599);
        assert_eq!(hist[&closing.to_string()]["pvpTrophies"], 1215);
    }

    #[test]
    fn season_window_is_the_calendar_month_of_september_2026() {
        // 2026-09-01T00:00:00Z .. 2026-10-01T00:00:00Z, i.e. exactly 30 days.
        assert_eq!(SEASON_2026_09.start_unix, 1_788_220_800);
        assert_eq!(SEASON_2026_09.end_unix, 1_790_812_800);
        assert_eq!(SEASON_2026_09.end_unix - SEASON_2026_09.start_unix, 30 * 86_400);

        assert!(season_at(SEASON_2026_09.start_unix - 1).is_none(), "not live before it opens");
        assert_eq!(season_at(SEASON_2026_09.start_unix).map(|s| s.id), Some(SEASON_2026_09.id));
        assert_eq!(season_at(SEASON_2026_09.end_unix - 1).map(|s| s.id), Some(SEASON_2026_09.id));
        assert!(season_at(SEASON_2026_09.end_unix).is_none(), "end is exclusive");
    }

    #[test]
    fn ymd_from_unix_agrees_with_the_season_boundaries() {
        assert_eq!(ymd_from_unix(SEASON_2026_09.start_unix), (2026, 9, 1));
        assert_eq!(ymd_from_unix(SEASON_2026_09.end_unix), (2026, 10, 1));
        assert_eq!(ymd_from_unix(SEASON_2026_09.start_unix - 1), (2026, 8, 31));
        assert_eq!(ymd_from_unix(0), (1970, 1, 1));
        // A leap day, which the naive "days / 365" version gets wrong.
        assert_eq!(ymd_from_unix(1_709_164_800), (2024, 2, 29));
    }

    #[test]
    fn announcement_matches_the_captured_record_shape() {
        let a = season_announcement(&SEASON_2026_09);
        let v = serde_json::to_value(&a).unwrap();

        // Exactly the five keys every one of the 156 captured records carries.
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["assetUrl", "id", "startTime", "ttl", "type"]);

        assert_eq!(v["type"], "BASIC");
        assert_eq!(v["startTime"], SEASON_2026_09.start_unix);
        assert_eq!(v["ttl"], SEASON_2026_09.start_unix + 86_340);
        // Retail's own path layout: host/YYYY/MM/DD/<the record's own id>.
        assert_eq!(
            v["assetUrl"],
            format!(
                "https://announcements.blades.bgs.services/2026/09/01/{}",
                v["id"].as_str().unwrap()
            )
        );
        // …and the id is a UUID, like every captured record's.
        assert!(Uuid::parse_str(v["id"].as_str().unwrap()).is_ok());
        assert_ne!(v["id"].as_str().unwrap(), SEASON_2026_09.id.to_string());
    }
}
