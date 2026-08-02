//! **Arena ladder + match-end economy tables** (Phase 5).
//!
//! Two things live here, kept apart on purpose:
//!
//! 1. **[`ARENA_LADDER`] — shipped game data, Class 1.** The 6-arena trophy
//!    ladder verbatim from the client's own `loot.json`
//!    (`matchmaking.arenas[].levels[]`): `required_trophy_count`,
//!    `rewards_once_reached` (chest rarity) and the per-tier loot-table name,
//!    plus [`CHEST_METER_CAPACITY`] from `pvp_match_rewards`.
//!    Source: `blades-capture/reference/game-defs/loot.json`
//!    sha256 `b68d2d46aa1d2a95836238faa2b7068056b45d5b4d571a268b132a6952b6f245`.
//!
//! 2. **[`match_reward`] / [`trophy_delta`] — `[Class 3 calibration]`.** The gold
//!    and XP magnitudes on the victory card. Retail computed these server-side;
//!    the formula is in no capture and never will be (the service shut down
//!    2026-06-30). What we *do* have is the retail **outputs**: 108 op49
//!    ResultsJSON cards reassembled from the prod `arena_udp_frames` ENet
//!    fragments across 43 sessions and 5 characters (levels 5/6/7/8/56/72/86/93).
//!    The model below is fitted to those and reproduces every distinct observed
//!    gold value within ~5%. See [`calibration`] for the anchors and provenance.
//!
//! ## How the ladder actually works (capture-proven, not inferred)
//!
//! Promotion is driven by **`matchmakingPvpTrophies`**, which the cards show is a
//! per-season **high-water mark** (monotone non-decreasing, `= max(pvpTrophies)`),
//! *not* by the live `pvpTrophies` count which goes up and down with each match.
//! A character's `highestArenaReached` / `highestLevelArenaReached` are exactly the
//! tier whose `required_trophy_count` is the greatest `<=` that high-water mark —
//! verified against every one of the 108 cards:
//!
//! ```text
//! flapdroid  mtro  51 -> arena 1 level 2 (req   50)   card says 1 / 2  OK
//! flapdroid  mtro 142 -> arena 1 level 3 (req  100)   card says 1 / 3  OK
//! flapdroid  mtro 181 -> arena 1 level 4 (req  150)   card says 1 / 4  OK
//! flapdroid  mtro 200 -> arena 1 level 5 (req  200)   card says 1 / 5  OK
//! simi       mtro 256 -> arena 1 level 6 (req  250)   card says 1 / 6  OK
//! Taheen     mtro 502 -> arena 2 level 1 (req  500)   card says 2 / 1  OK
//! Taheen     mtro 579 -> arena 2 level 2 (req  550)   card says 2 / 2  OK
//! Shoyr      mtro 725 -> arena 2 level 5 (req  700)   card says 2 / 5  OK
//! Shoyr      mtro 760 -> arena 2 level 6 (req  750)   card says 2 / 6  OK
//! Flappety   mtro 817 -> arena 2 level 7 (req  800)   card says 2 / 7  OK
//! ```
//!
//! `rewards_once_reached` fires **once**, when the high-water mark crosses the
//! threshold — the shape of the resulting `rewardNewLevelArena` block is also
//! capture-proven (3 populated examples, see [`PromotionRewards`]).

#![allow(dead_code)]

/// One rung of the arena ladder — one `(arena, level)` pair from `loot.json`
/// `matchmaking.arenas[].levels[]`. Class 1 (shipped data, verbatim).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaTier {
    /// 1-based arena index (`arena_01` -> 1). The card's `highestArenaReached`.
    pub arena: u8,
    /// 1-based level within the arena. The card's `highestLevelArenaReached`.
    pub level: u8,
    /// `required_trophy_count` — the `matchmakingPvpTrophies` high-water mark at
    /// or above which this tier is reached.
    pub required_trophies: i64,
    /// `rewards_once_reached[].chest_rarity` — chest tiers granted exactly once,
    /// the first time this rung is reached. Empty for rungs with no reward.
    pub chests_once_reached: &'static [u8],
    /// `is_high_arena` (only `arena_06`).
    pub is_high_arena: bool,
    /// `can_drop_out` (only `arena_06`) — whether a player can fall out of it.
    pub can_drop_out: bool,
    /// The per-tier loot-table name (`LootTable_ArenaN_ArenaLevelM`). Recorded for
    /// provenance; the shipped tables carry no gold, see [`calibration`].
    pub loot_table: &'static str,
}

pub const ARENA_LADDER: [ArenaTier; 46] = [
    ArenaTier { arena: 1, level: 1, required_trophies: 0, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel1" },
    ArenaTier { arena: 1, level: 2, required_trophies: 50, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel2" },
    ArenaTier { arena: 1, level: 3, required_trophies: 100, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel3" },
    ArenaTier { arena: 1, level: 4, required_trophies: 150, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel4" },
    ArenaTier { arena: 1, level: 5, required_trophies: 200, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel5" },
    ArenaTier { arena: 1, level: 6, required_trophies: 250, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel6" },
    ArenaTier { arena: 1, level: 7, required_trophies: 300, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel7" },
    ArenaTier { arena: 1, level: 8, required_trophies: 350, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel8" },
    ArenaTier { arena: 1, level: 9, required_trophies: 400, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena1_ArenaLevel9" },
    ArenaTier { arena: 2, level: 1, required_trophies: 500, chests_once_reached: &[4], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel1" },
    ArenaTier { arena: 2, level: 2, required_trophies: 550, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel2" },
    ArenaTier { arena: 2, level: 3, required_trophies: 600, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel3" },
    ArenaTier { arena: 2, level: 4, required_trophies: 650, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel4" },
    ArenaTier { arena: 2, level: 5, required_trophies: 700, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel5" },
    ArenaTier { arena: 2, level: 6, required_trophies: 750, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel6" },
    ArenaTier { arena: 2, level: 7, required_trophies: 800, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel7" },
    ArenaTier { arena: 2, level: 8, required_trophies: 850, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel8" },
    ArenaTier { arena: 2, level: 9, required_trophies: 900, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena2_ArenaLevel9" },
    ArenaTier { arena: 3, level: 1, required_trophies: 1000, chests_once_reached: &[4], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel1" },
    ArenaTier { arena: 3, level: 2, required_trophies: 1050, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel2" },
    ArenaTier { arena: 3, level: 3, required_trophies: 1100, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel3" },
    ArenaTier { arena: 3, level: 4, required_trophies: 1150, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel4" },
    ArenaTier { arena: 3, level: 5, required_trophies: 1200, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel5" },
    ArenaTier { arena: 3, level: 6, required_trophies: 1250, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel6" },
    ArenaTier { arena: 3, level: 7, required_trophies: 1300, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel7" },
    ArenaTier { arena: 3, level: 8, required_trophies: 1350, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel8" },
    ArenaTier { arena: 3, level: 9, required_trophies: 1400, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena3_ArenaLevel9" },
    ArenaTier { arena: 4, level: 1, required_trophies: 1500, chests_once_reached: &[4, 4], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel1" },
    ArenaTier { arena: 4, level: 2, required_trophies: 1550, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel2" },
    ArenaTier { arena: 4, level: 3, required_trophies: 1600, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel3" },
    ArenaTier { arena: 4, level: 4, required_trophies: 1650, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel4" },
    ArenaTier { arena: 4, level: 5, required_trophies: 1700, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel5" },
    ArenaTier { arena: 4, level: 6, required_trophies: 1750, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel6" },
    ArenaTier { arena: 4, level: 7, required_trophies: 1800, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel7" },
    ArenaTier { arena: 4, level: 8, required_trophies: 1850, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel8" },
    ArenaTier { arena: 4, level: 9, required_trophies: 1900, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena4_ArenaLevel9" },
    ArenaTier { arena: 5, level: 1, required_trophies: 2000, chests_once_reached: &[4, 4], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel1" },
    ArenaTier { arena: 5, level: 2, required_trophies: 2050, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel2" },
    ArenaTier { arena: 5, level: 3, required_trophies: 2100, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel3" },
    ArenaTier { arena: 5, level: 4, required_trophies: 2150, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel4" },
    ArenaTier { arena: 5, level: 5, required_trophies: 2200, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel5" },
    ArenaTier { arena: 5, level: 6, required_trophies: 2250, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel6" },
    ArenaTier { arena: 5, level: 7, required_trophies: 2300, chests_once_reached: &[], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel7" },
    ArenaTier { arena: 5, level: 8, required_trophies: 2350, chests_once_reached: &[3], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel8" },
    ArenaTier { arena: 5, level: 9, required_trophies: 2400, chests_once_reached: &[2], is_high_arena: false, can_drop_out: false, loot_table: "LootTable_Arena5_ArenaLevel9" },
    ArenaTier { arena: 6, level: 1, required_trophies: 2500, chests_once_reached: &[5], is_high_arena: true, can_drop_out: true, loot_table: "LootTable_Arena6_ArenaLevel1" },
];


/// `pvp_match_rewards.chest_meter_capacity` from `loot.json` (Class 1).
///
/// The victory card's `character.pvpChestMeter` counts **rounds won**, not
/// matches: capture-proven by diffing consecutive cards of the same character
/// against `numberPvpMatchPlayed` (e.g. Flappety s503 `3 -> 5 -> 6 -> 0 -> 2 -> 4`
/// over win / loss-1-round / win / win / win, i.e. `+2 / +1 / +2 / +2 / +2`, and
/// Taheen s486 `4 -> 4` over a 2-0 loss, i.e. `+0`). It wraps at capacity.
pub const CHEST_METER_CAPACITY: i64 = 8;

/// `pvp_match_rewards.winner_loot_table` / `.loser_loot_table` — in the shipped
/// data **both** point at `LootTable_PvpWinner`.
pub const PVP_MATCH_LOOT_TABLE: &str = "LootTable_PvpWinner";

/// The outcome of one finished match from ONE player's point of view — the input
/// to [`match_reward`]. Best-of-3, so `rounds_won + rounds_lost <= 3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchOutcome {
    /// Rounds this player won (`MatchCombat::rounds_won[slot]`).
    pub rounds_won: u8,
    /// Rounds the opponent won.
    pub rounds_lost: u8,
    /// Whether this player won the match.
    pub win: bool,
}

impl MatchOutcome {
    pub fn new(rounds_won: u8, rounds_lost: u8) -> Self {
        MatchOutcome { rounds_won, rounds_lost, win: rounds_won > rounds_lost }
    }
}

/// The gold + XP granted for one match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchReward {
    pub gold: i64,
    pub character_xp: i64,
}

/// `[Class 3 calibration]` — the fitted match-reward model and its provenance.
///
/// # Where the numbers come from
///
/// 108 retail op49 `ResultsJSON` victory cards, recovered by reassembling the
/// fragmented ENet frames in the prod capture DB (`arena_udp_frames`,
/// `game_message_id = 49`) across 43 sessions / 5 characters. Crucially this
/// includes **session pair s615/s616** — the same match captured from BOTH
/// players' sockets at the same wall-clock second (2026-06-27 21:18:21,
/// Flappety L86 vs Taheen L72). That pair is the first and only direct
/// observation of a WINNER-side card next to its LOSER-side twin, and it is what
/// pins the multipliers below:
///
/// ```text
///   Flappety L86  WINNER  gold 14961  xp 691   (pvpWinningStreak +1)
///   Taheen   L72  LOSER   gold  3964  xp 256   (pvpWinningStreak -1)
/// ```
///
/// Taheen's 3964 is his *0-rounds-won* loss value (established independently
/// from his chest-meter deltas), so that match was a **2-0**, which is what makes
/// Flappety's 14961 / 691 identifiable as the 2-0 win multiplier rather than 2-1.
///
/// # The shape of the model
///
/// Both currencies are `base(level) * multiplier(rounds_won, rounds_lost)`.
///
/// * The **base** is the 0-rounds-won LOSS value, which the captures show is a
///   pure function of character level: it is byte-identical across dozens of
///   matches, opponents, trophy counts and dates for the same character
///   (Taheen 3723 x9, Flappety 4047 x6, flapdroid 302 x14).
/// * The **multiplier** is a function of the round score only. Derived from the
///   L86 pair above, then cross-checked at every other level.
///
/// # Fit quality (gold — every distinct observed value)
///
/// ```text
///  level  score  predicted  observed   err
///     86    0-2       4047      4047   anchor
///     86    1-2       5763      5764   0.0%
///     86    2-1      12999     12999   anchor
///     86    2-0      14961     14961   anchor (two-sided s615/s616)
///     72    0-2       3723      3723   anchor
///     72    1-2       5301      5405   1.9%
///     72    2-1      11958     12492   4.3%
///     72    2-0      13764     14413   4.5%   (also 14654 -> 6.1%)
///     93    1-2       5820      5822   0.0%
///     93    2-1      13128     13131   0.0%
///      8    1-2        464       474   2.1%
///      8    2-1       1047      1095   4.4%
///      8    2-0       1205      1263   4.6%
///      7    2-0       1179      1238   4.8%
///      5    2-0       1116      1170   4.6%
/// ```
///
/// Worst case 6.1%, inside the +/-10% target.
///
/// # What is NOT modelled
///
/// `characterXp` carries a residual term the captures cannot explain: the same
/// character, level and gold value sometimes ships two different XP numbers
/// (Flappety `14961` with xp `489` *and* `691`; `4047` with `280` *and* `482`).
/// Most likely a per-match performance component (damage dealt / duration). Gold
/// never does this. The XP multipliers below reproduce the common case; the
/// outliers are left unmodelled rather than guessed at.
pub mod calibration {
    /// `(character_level, base_gold, base_xp)` — the 0-rounds-won LOSS reward,
    /// read directly off retail cards. Interpolated linearly between anchors and
    /// clamped outside them.
    ///
    /// The L56 row is the one soft anchor: `simi`'s only captured card is a WIN
    /// (`5274` / `252`) with no companion card to fix the round score, so it is
    /// back-solved assuming a 2-0 (the more common win in the dataset, 18 vs 10).
    /// A 2-1 reading would put it at `1642` / `141` instead — so treat L56 as
    /// +/-15% rather than the +/-5% the other anchors carry.
    pub const BASE_ANCHORS: &[(u16, i64, i64)] = &[
        (5, 302, 16),   // flapdroid, 14 cards
        (6, 312, 18),   // flapdroid
        (7, 319, 20),   // flapdroid, 6 cards
        (8, 326, 22),   // flapdroid
        (56, 1426, 102), // simi, back-solved from a single 2-0 win  [soft]
        (72, 3723, 226), // Taheen, 9 cards
        (86, 4047, 280), // Flappety, 6 cards
        (93, 4087, 291), // Shoyr, 5 cards
    ];

    /// Gold multiplier for a LOSS in which the player won 1 round (2-1 down).
    /// `5764 / 4047` (Flappety L86); confirmed `5822 / 4087` (Shoyr L93, 0.0%).
    pub const GOLD_MUL_LOSS_1: f64 = 1.4243;
    /// Gold multiplier for a 2-1 WIN. `12999 / 4047` (Flappety L86).
    pub const GOLD_MUL_WIN_2_1: f64 = 3.2119;
    /// Gold multiplier for a 2-0 WIN. `14961 / 4047` (Flappety L86) — the
    /// two-sided s615/s616 pair is what proves this row is the 2-0 and not the 2-1.
    pub const GOLD_MUL_WIN_2_0: f64 = 3.6968;

    /// XP multiplier for a LOSS in which the player won 1 round. `342 / 280`
    /// (Flappety L86); confirmed exactly by `355 / 291` (Shoyr L93).
    pub const XP_MUL_LOSS_1: f64 = 1.2214;
    /// XP multiplier for a 2-1 WIN. `499 / 280`; confirmed by `520 / 291` (0.2%).
    pub const XP_MUL_WIN_2_1: f64 = 1.7821;
    /// XP multiplier for a 2-0 WIN. `691 / 280` — from the two-sided pair.
    pub const XP_MUL_WIN_2_0: f64 = 2.4679;

    /// `[Class 3]` Elo-style trophy swing. Retail's exact curve is not in the
    /// captures; the observed envelope across 108 cards is roughly `+6..+74` on a
    /// win and `-7..-51` on a loss, centred near `+/-30` for evenly-matched
    /// players, which is what a K-factor of 60 produces.
    pub const TROPHY_K: f64 = 60.0;
    /// `matchmaking.tuning.trophy_gain_floor` from `loot.json` (Class 1) — a win
    /// never awards less than this, and a loss never costs less than this.
    pub const TROPHY_FLOOR: i64 = 1;
}

/// The `rewardNewLevelArena` payload — populated **only** when this match pushed
/// the player's `matchmakingPvpTrophies` high-water mark across one or more
/// `required_trophy_count` thresholds; otherwise `{}`.
///
/// The populated shape was previously believed uncaptured. It is not — three
/// retail examples were recovered from the op49 reassembly:
///
/// ```text
/// s168 flapdroid L5  mtro ->  51 (arena 1 lvl 2, chest_rarity 3)
///      {"chests":[{"id":"1","tier":3,"level":5}],"characterXp":0}
/// s460 flapdroid L8  mtro -> 181 (arena 1 lvl 3 + lvl 4, chest_rarity 2 + 2)
///      {"stackableItems":{...},"chests":[{"id":"2","tier":2,"level":8},
///                                        {"id":"3","tier":2,"level":8}],"characterXp":0}
/// s607 simi     L56  mtro -> 256 (arena 1 lvl 6, chest_rarity 3)
///      {"chests":[{"id":"4","tier":3,"level":56}],"characterXp":0}
/// ```
///
/// So: `tier` is the ladder's `chest_rarity`, `level` is the CHARACTER level, `id`
/// is the treasury chest id assigned at grant time, and `characterXp` is always 0
/// (the match XP rides the separate `reward` block). s460 also shows that a
/// promotion missed on a LOSS is deferred and paid out with the next one — two
/// rungs' chests arrived together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromotionRewards {
    /// `(chest_rarity, character_level)` for each rung crossed, in ladder order.
    pub chests: Vec<(u8, u16)>,
    /// The tier the player ends up on, if it changed.
    pub new_tier: Option<ArenaTier>,
}

impl PromotionRewards {
    pub fn is_empty(&self) -> bool {
        self.chests.is_empty() && self.new_tier.is_none()
    }
}

/// The ladder rung for a `matchmakingPvpTrophies` high-water mark: the tier with
/// the greatest `required_trophies <= high_water`. Always `Some` for
/// `high_water >= 0` (arena 1 level 1 requires 0).
pub fn tier_for_trophies(high_water: i64) -> &'static ArenaTier {
    ARENA_LADDER
        .iter()
        .rev()
        .find(|t| high_water >= t.required_trophies)
        .unwrap_or(&ARENA_LADDER[0])
}

/// Every rung crossed when the high-water mark moves `old -> new` (exclusive of
/// `old`, inclusive of `new`), in ascending order. Empty when the mark did not
/// move or moved without crossing a threshold.
pub fn tiers_crossed(old_high_water: i64, new_high_water: i64) -> Vec<&'static ArenaTier> {
    if new_high_water <= old_high_water {
        return Vec::new();
    }
    ARENA_LADDER
        .iter()
        .filter(|t| t.required_trophies > old_high_water && t.required_trophies <= new_high_water)
        .collect()
}

/// The `rewardNewLevelArena` content for a high-water move — the chests from every
/// rung crossed, at the player's current character level.
pub fn promotion_rewards(old_high_water: i64, new_high_water: i64, character_level: u16) -> PromotionRewards {
    let crossed = tiers_crossed(old_high_water, new_high_water);
    let chests = crossed
        .iter()
        .flat_map(|t| t.chests_once_reached.iter().map(move |&r| (r, character_level)))
        .collect::<Vec<_>>();
    PromotionRewards {
        chests,
        new_tier: crossed.last().copied().copied(),
    }
}

/// `[Class 3 calibration]` The base (0-rounds-won LOSS) reward at a character
/// level — linear interpolation over [`calibration::BASE_ANCHORS`], clamped at
/// both ends.
fn base_reward(level: u16) -> (f64, f64) {
    let a = calibration::BASE_ANCHORS;
    let first = a[0];
    let last = a[a.len() - 1];
    if level <= first.0 {
        return (first.1 as f64, first.2 as f64);
    }
    if level >= last.0 {
        return (last.1 as f64, last.2 as f64);
    }
    for w in a.windows(2) {
        let (l0, g0, x0) = w[0];
        let (l1, g1, x1) = w[1];
        if level >= l0 && level <= l1 {
            let t = (level - l0) as f64 / (l1 - l0) as f64;
            return (
                g0 as f64 + t * (g1 - g0) as f64,
                x0 as f64 + t * (x1 - x0) as f64,
            );
        }
    }
    (last.1 as f64, last.2 as f64)
}

/// `[Class 3 calibration]` The gold + XP for one finished match.
///
/// `base(level) * multiplier(round score)` — see [`calibration`] for the anchors,
/// the two-sided s615/s616 derivation and the measured fit error (worst 6.1%).
pub fn match_reward(level: u16, outcome: MatchOutcome) -> MatchReward {
    let (base_gold, base_xp) = base_reward(level);
    let (gm, xm) = match (outcome.win, outcome.rounds_won, outcome.rounds_lost) {
        // WIN 2-0 / any clean sweep.
        (true, _, 0) => (calibration::GOLD_MUL_WIN_2_0, calibration::XP_MUL_WIN_2_0),
        // WIN 2-1.
        (true, _, _) => (calibration::GOLD_MUL_WIN_2_1, calibration::XP_MUL_WIN_2_1),
        // LOSS having won no rounds (0-2) — the base itself.
        (false, 0, _) => (1.0, 1.0),
        // LOSS having won at least one round (1-2).
        (false, _, _) => (calibration::GOLD_MUL_LOSS_1, calibration::XP_MUL_LOSS_1),
    };
    MatchReward {
        gold: (base_gold * gm).round() as i64,
        character_xp: (base_xp * xm).round() as i64,
    }
}

/// `[Class 3 calibration]` The trophy swing for one match — an Elo expectation
/// against the opponent's trophy count, scaled by [`calibration::TROPHY_K`] and
/// floored by the shipped `trophy_gain_floor`.
///
/// Positive on a win, negative on a loss; monotone in the rating gap (beating a
/// stronger opponent is worth more, losing to a weaker one costs more).
/// `pvpTrophies` never goes below 0 — that clamp is applied by the caller which
/// knows the pre-match total.
pub fn trophy_delta(win: bool, own_trophies: i64, opponent_trophies: i64) -> i64 {
    let expected = 1.0 / (1.0 + 10f64.powf((opponent_trophies - own_trophies) as f64 / 400.0));
    let raw = calibration::TROPHY_K * (if win { 1.0 - expected } else { -expected });
    let rounded = raw.round() as i64;
    if win {
        rounded.max(calibration::TROPHY_FLOOR)
    } else {
        rounded.min(-calibration::TROPHY_FLOOR)
    }
}

/// Advance the chest meter by the rounds won this match, wrapping at
/// [`CHEST_METER_CAPACITY`]. Returns `(new_meter, chests_filled)`.
pub fn advance_chest_meter(meter: i64, rounds_won: u8) -> (i64, i64) {
    let total = meter.max(0) + rounds_won as i64;
    (total % CHEST_METER_CAPACITY, total / CHEST_METER_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_the_shipped_46_rung_table() {
        assert_eq!(ARENA_LADDER.len(), 46, "5 arenas x 9 levels + the 1-level high arena");
        assert_eq!(ARENA_LADDER[0].required_trophies, 0);
        assert_eq!(ARENA_LADDER[ARENA_LADDER.len() - 1].required_trophies, 2500);
        // Strictly ascending thresholds — tier_for_trophies relies on this.
        for w in ARENA_LADDER.windows(2) {
            assert!(w[1].required_trophies > w[0].required_trophies, "{:?} !> {:?}", w[1], w[0]);
        }
        // Only arena 6 is the high arena, and only it can be dropped out of.
        for t in ARENA_LADDER.iter() {
            assert_eq!(t.is_high_arena, t.arena == 6);
            assert_eq!(t.can_drop_out, t.arena == 6);
        }
    }

    /// Every `(arena, level)` the 108 reassembled retail op49 cards report for a
    /// given `matchmakingPvpTrophies`. If `tier_for_trophies` disagrees with any
    /// of these, the ladder wiring is wrong.
    #[test]
    fn tier_matches_every_retail_card() {
        // (matchmakingPvpTrophies, highestArenaReached, highestLevelArenaReached)
        let cards: &[(i64, u8, u8)] = &[
            (0, 1, 1),     // flapdroid s167
            (51, 1, 2),    // flapdroid s168
            (142, 1, 3),   // flapdroid s394
            (181, 1, 4),   // flapdroid s460
            (200, 1, 5),   // flapdroid s127
            (256, 1, 6),   // simi s607
            (502, 2, 1),   // Taheen s398..s517
            (506, 2, 1),   // Taheen s517
            (518, 2, 1),   // Taheen s517
            (579, 2, 2),   // Taheen s616
            (725, 2, 5),   // Shoyr s544
            (760, 2, 6),   // Shoyr s551..s709
            (817, 2, 7),   // Flappety s470..s601
            (847, 2, 7),   // Flappety s615
        ];
        for &(mtro, arena, level) in cards {
            let t = tier_for_trophies(mtro);
            assert_eq!(
                (t.arena, t.level),
                (arena, level),
                "mtro {mtro} should be arena {arena} level {level}, got {}/{}",
                t.arena,
                t.level
            );
        }
    }

    #[test]
    fn promotion_chests_match_the_retail_reward_new_level_arena() {
        // s168: flapdroid L5 crossed 50 -> arena 1 level 2, chest_rarity 3.
        let p = promotion_rewards(0, 51, 5);
        assert_eq!(p.chests, vec![(3, 5)]);
        assert_eq!(p.new_tier.map(|t| (t.arena, t.level)), Some((1, 2)));

        // s460: flapdroid L8 crossed 100 AND 150 in one payout -> two rarity-2
        // chests, exactly the two the retail card carried.
        let p = promotion_rewards(51, 181, 8);
        assert_eq!(p.chests, vec![(2, 8), (2, 8)]);
        assert_eq!(p.new_tier.map(|t| (t.arena, t.level)), Some((1, 4)));

        // s607: simi L56 crossed 250 -> arena 1 level 6, chest_rarity 3.
        let p = promotion_rewards(200, 256, 56);
        assert_eq!(p.chests, vec![(3, 56)]);

        // No crossing -> empty (the card ships `rewardNewLevelArena: {}`).
        assert!(promotion_rewards(817, 847, 86).is_empty());
        assert!(promotion_rewards(847, 800, 86).is_empty());
    }

    /// The whole point of Phase 5.3: reproduce the retail gold numbers. Every
    /// distinct observed gold value, with the round score that produced it.
    #[test]
    fn gold_reproduces_every_observed_retail_card_within_10_percent() {
        // (level, rounds_won, rounds_lost, observed_gold, session)
        let obs: &[(u16, u8, u8, i64, &str)] = &[
            (86, 0, 2, 4047, "s487/s503/s572"),
            (86, 1, 2, 5764, "s503/s605/s615"),
            (86, 2, 1, 12999, "s470/s490/s503"),
            (86, 2, 0, 14961, "s615 (two-sided vs s616)"),
            (72, 0, 2, 3723, "s414/s447/s486"),
            (72, 1, 2, 5405, "s414/s447/s517"),
            (72, 2, 1, 12492, "s398/s433/s447"),
            (72, 2, 0, 14413, "s414/s464/s486"),
            (72, 2, 0, 14654, "s517"),
            (72, 0, 2, 3964, "s616 (arena 2 lvl 2)"),
            (93, 0, 2, 4087, "s544/s551/s593"),
            (93, 1, 2, 5822, "s581/s593"),
            (93, 2, 1, 13131, "s544"),
            (8, 0, 2, 326, "s399/s460"),
            (8, 1, 2, 474, "s399"),
            (8, 2, 1, 1095, "s460"),
            (8, 2, 0, 1263, "s460"),
            (7, 0, 2, 319, "s390/s394"),
            (7, 2, 0, 1238, "s385"),
            (6, 0, 2, 312, "s293"),
            (5, 0, 2, 302, "s127/s167/s168/s203/s223/s277"),
            (5, 2, 0, 1170, "s168"),
            (56, 2, 0, 5274, "s607"),
        ];
        let mut worst = 0.0f64;
        for &(level, w, l, observed, src) in obs {
            let got = match_reward(level, MatchOutcome::new(w, l)).gold;
            let err = (got - observed).abs() as f64 / observed as f64;
            assert!(
                err <= 0.10,
                "L{level} {w}-{l} [{src}]: predicted {got}, retail {observed} ({:.1}% off)",
                err * 100.0
            );
            worst = worst.max(err);
        }
        assert!(worst < 0.07, "worst-case gold error crept up to {:.1}%", worst * 100.0);
    }

    /// The XP side. Looser than gold on purpose — retail's `characterXp` carries an
    /// unmodelled per-match performance term (see [`calibration`]), so this pins
    /// the common case only.
    #[test]
    fn xp_reproduces_the_common_retail_cards() {
        let obs: &[(u16, u8, u8, i64)] = &[
            (86, 0, 2, 280),
            (86, 1, 2, 342),
            (86, 2, 1, 499),
            (86, 2, 0, 691), // two-sided s615/s616
            (72, 0, 2, 226),
            (72, 1, 2, 282),
            (72, 2, 1, 417),
            (72, 2, 0, 602),
            (93, 0, 2, 291),
            (93, 1, 2, 355),
            (93, 2, 1, 520),
            (8, 0, 2, 22),
            (8, 1, 2, 28),
            (8, 2, 0, 58),
            (7, 0, 2, 20),
            (7, 2, 0, 53),
            (5, 0, 2, 16),
            (5, 2, 0, 42),
            (56, 2, 0, 252),
        ];
        for &(level, w, l, observed) in obs {
            let got = match_reward(level, MatchOutcome::new(w, l)).character_xp;
            let err = (got - observed).abs() as f64 / observed as f64;
            assert!(
                err <= 0.11,
                "L{level} {w}-{l}: predicted xp {got}, retail {observed} ({:.1}% off)",
                err * 100.0
            );
        }
    }

    #[test]
    fn reward_is_monotone_in_level_and_in_outcome() {
        let mut prev = 0;
        for level in 1..=100u16 {
            let g = match_reward(level, MatchOutcome::new(0, 2)).gold;
            assert!(g >= prev, "base gold dipped at level {level}: {g} < {prev}");
            prev = g;
        }
        for level in [5u16, 30, 56, 72, 86, 100] {
            let loss0 = match_reward(level, MatchOutcome::new(0, 2)).gold;
            let loss1 = match_reward(level, MatchOutcome::new(1, 2)).gold;
            let win21 = match_reward(level, MatchOutcome::new(2, 1)).gold;
            let win20 = match_reward(level, MatchOutcome::new(2, 0)).gold;
            assert!(loss0 < loss1 && loss1 < win21 && win21 < win20, "L{level} not ordered");
        }
    }

    #[test]
    fn chest_meter_counts_rounds_won_and_wraps_at_capacity() {
        // Flappety s503: 3 -> (win 2-0) 5 -> (loss 1-2) 6 -> (win) 8 wraps to 0 -> 2 -> 4.
        assert_eq!(advance_chest_meter(3, 2), (5, 0));
        assert_eq!(advance_chest_meter(5, 1), (6, 0));
        assert_eq!(advance_chest_meter(6, 2), (0, 1));
        assert_eq!(advance_chest_meter(0, 2), (2, 0));
        assert_eq!(advance_chest_meter(2, 2), (4, 0));
        // Taheen s486: a 0-2 loss does not move the meter.
        assert_eq!(advance_chest_meter(4, 0), (4, 0));
    }

    #[test]
    fn trophy_delta_is_elo_shaped_and_inside_the_observed_envelope() {
        // Even match -> the classic +/- K/2.
        assert_eq!(trophy_delta(true, 800, 800), 30);
        assert_eq!(trophy_delta(false, 800, 800), -30);
        // Beating a much stronger opponent is worth more than beating a weaker one.
        assert!(trophy_delta(true, 400, 900) > trophy_delta(true, 900, 400));
        // Losing to a weaker opponent costs more than losing to a stronger one.
        assert!(trophy_delta(false, 900, 400) < trophy_delta(false, 400, 900));
        // Never zero, and inside the retail envelope seen across 108 cards.
        for own in [0i64, 100, 500, 900, 2500] {
            for opp in [0i64, 100, 500, 900, 2500] {
                let w = trophy_delta(true, own, opp);
                let l = trophy_delta(false, own, opp);
                assert!((1..=60).contains(&w), "win delta {w} out of envelope");
                assert!((-60..=-1).contains(&l), "loss delta {l} out of envelope");
            }
        }
    }
}
