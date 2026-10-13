//! The Abyss future-reward ladder — which reward the next score rung grants.
//!
//! THE BUG (#172). `deploy/static/abyss.json` carries ONE rung, score 35, so the
//! server advertised rung 1 of 10 forever: a player who passed 35 was shown the
//! same reward for the rest of the run, and the nine rungs above it did not
//! exist. The ladder itself had already been measured — 35, 50, 70, 95, 135,
//! 190, 260, 360, 490, 650 — but the CONTENTS of rungs 2-10 were written off as
//! "a server-side loot table nobody has". They were in the captures all along;
//! the earlier search looked for a key called `futureRewards` and the wire calls
//! it `abyssFutureRewards`.
//!
//! TWO KINDS OF RUNG, AND ONLY ONE NEEDS A TABLE
//!
//! * Rungs 50 and 360 grant a CHEST and are fully deterministic: the tier never
//!   varies within a rung (22 and 50 observations), and the chest's level is the
//!   character's own level in 72 of 72 observations. No table, just the rule.
//! * Every other rung grants a stackable item — or gear, at rung 260 — drawn
//!   from a pool. Those are whole observed results with counts, the same shape
//!   as `enemy_loot.json` and for the same reason: a result is what retail
//!   produced, kept whole.
//!
//! THE CORPUS IS THIN, and says so. One observation per retail run and rung —
//! 25 runs reached rung 35, 13 rung 70, and rungs 190-650 rest on four or fewer
//! (650 on one). Every observation carries the character level it was made at.
//!
//! BY LEVEL, NOT BY ONE POOL (#294). The pools are drawn per [`LEVEL_BANDS`]:
//! retail's rung 70 paid a level-3 or level-7 character Brass Ingot or Garlic and
//! a level 66-100 one Nightshade, Giant's Toe, Chaurus Chitin or Malachite Ingot
//! (27-35 of them). The first corpus dropped the level, so a level-100 character
//! at floor 149 could be handed six Garlic.
//!
//! THE `/end` PACKAGE. Retail's `/end` paid one stackable besides the per-floor
//! gold and XP — a material, or a stack of soul gems — see [`end_package`]. A band
//! with no captured gem package (everything below level 60) still pays gems
//! (report #371): at levels 39-48 as retail screenshots show (Grand, up to 7),
//! below that at the pooled retail rate, graded to the character's level. The
//! rungs either side of the golden chest can pay a Grand gem at 39-59.
//!
//! THE TOP BAND OPENS AT 49 (report #383). A character of level 49 or more draws
//! the top band's rewards — see [`TOP_BAND_FROM_LEVEL`].
//!
//! RETAIL END SCREENS (reports #371, #383). Rewards seen on screenshots of the
//! original game, not in a capture, are added beside the corpus by the slot they
//! were paid in: a rung ([`screenshot_results`]) or `/end` ([`screenshot_end_packages`]).

use std::collections::HashMap;

use serde::Deserialize;
use uuid::Uuid;

use crate::{
    economy::{RewardChest, RewardGrant, RewardItem},
    user_data::Item,
};

static ABYSS_FUTURE_REWARDS_RAW: &str = include_str!("../abyss_future_rewards.json");

/// The ten score rungs, in order. Public so the wire layer and the tests share
/// one definition instead of two that can drift.
pub const ABYSS_LADDER: [u32; 10] = [35, 50, 70, 95, 135, 190, 260, 360, 490, 650];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    rungs: Vec<Rung>,
    #[serde(default)]
    end_package: EndPackage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rung {
    score: u32,
    kind: String,
    /// Present only on a chest rung.
    #[serde(default)]
    tier: Option<u64>,
    #[serde(default)]
    observations: u64,
    /// One entry per retail RUN that was advertised this rung.
    #[serde(default)]
    results: Vec<LevelledResult>,
}

/// One retail observation, kept with the character level it was made at — the
/// level is what decides which pool a character draws from (#294).
#[derive(Deserialize)]
struct LevelledResult {
    level: u64,
    reward: ObservedReward,
}

#[derive(Deserialize, Default)]
struct EndPackage {
    #[serde(default)]
    observations: Vec<EndObservation>,
}

/// One retail `/end`. `package` is the stackable it paid on top of gold/XP, or
/// `None` for a run that paid none (both such runs scored under
/// [`END_PACKAGE_MIN_SCORE`]).
#[derive(Deserialize)]
struct EndObservation {
    level: u64,
    #[serde(default)]
    package: Option<ObservedReward>,
}

/// One observed reward body. `Item` deserializes straight from retail's own
/// shape; the corpus strips `items[].id` so a fresh one is minted below.
#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct ObservedReward {
    #[serde(default)]
    stackable_items: HashMap<Uuid, u64>,
    #[serde(default)]
    items: Vec<Item>,
}

fn corpus() -> &'static Corpus {
    static TABLE: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(ABYSS_FUTURE_REWARDS_RAW)
            .unwrap_or_else(|_| Corpus { rungs: Vec::new(), end_package: EndPackage::default() })
    })
}

/// splitmix64. The draw must be STABLE within a run: the client re-reads
/// `/abysses/current` while it plays and has to be advertised the same reward
/// each time, so this is seeded from the run and the rung, never random.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn instance_uuid(seed: u64, ordinal: usize) -> Uuid {
    let hi = mix(seed ^ (ordinal as u64).rotate_left(17));
    let lo = mix(hi ^ 0x5DEE_CE66_D1B2_4E35);
    let mut b = (((hi as u128) << 64) | lo as u128).to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

/// The character-level bands a reward pool is drawn from.
///
/// Retail's tables are keyed by level, not depth: a level-34 character at floor
/// 118 was advertised the same rung-70 kinds (Nightshade, Chaurus Chitin,
/// Quicksilver) as a level-40 one at floor 43, while level 3 and 7 at floor 1 got
/// Brass Ingot and Garlic. The observed levels cluster as 3-8, 34-57 and 64-100,
/// and the bands follow those clusters. The first corpus threw the level away, so
/// a level-100 character drew level-3 results (#294).
///
/// These bands sort the retail OBSERVATIONS. Which band a CHARACTER draws from is
/// [`character_band`]: the same, except that the top band opens at
/// [`TOP_BAND_FROM_LEVEL`].
pub const LEVEL_BANDS: [std::ops::RangeInclusive<u64>; 3] = [0..=19, 20..=59, 60..=u64::MAX];

/// The character level from which a character draws the top band's rewards — the
/// level 64-100 captures — rather than the level 34-57 ones (report #383).
///
/// NOT A CAPTURED THRESHOLD. The captures put no retail run between level 58 and
/// 63, so the 60 in [`LEVEL_BANDS`] only splits the observed clusters; nothing
/// measured where retail's own boundary was. Only two of the 26 captured runs
/// were made at level 49-59 (54 and 57), and both reached rung 35 alone, where
/// they were paid Fire Salts ×5 and Honeycomb ×6 — kinds the top band pays too
/// (Fire Salts ×5 at level 66, Honeycomb ×5 at 93). No capture shows a level 49-59
/// character paid what only the lower band pays (rung 70's Quicksilver, say).
/// HauDrauf, a trusted retail player: "From 45 a player is practically done, by
/// 50 completely. Many players stayed at 49. So everything must be obtainable by
/// level 49 at the latest." Where the data cannot decide, he is trusted.
///
/// The observations keep their bands, so a level-60+ character draws exactly the
/// pool it drew before, and a level 20-48 one too.
pub const TOP_BAND_FROM_LEVEL: u64 = 49;

fn band(level: u64) -> usize {
    LEVEL_BANDS
        .iter()
        .position(|b| b.contains(&level))
        .unwrap_or(LEVEL_BANDS.len() - 1)
}

/// The band a CHARACTER of `level` draws from — see [`TOP_BAND_FROM_LEVEL`].
fn character_band(level: u64) -> usize {
    if level >= TOP_BAND_FROM_LEVEL {
        LEVEL_BANDS.len() - 1
    } else {
        band(level)
    }
}

/// The observations a character of `level` draws from: those in its own band, or —
/// for a rung no retail run in that band reached — the band of the observation
/// nearest in level. Never empty unless `observations` is.
fn level_pool<T>(observations: &[T], level: u64, level_of: impl Fn(&T) -> u64) -> Vec<&T> {
    let own = character_band(level);
    let target = if observations.iter().any(|o| band(level_of(o)) == own) {
        own
    } else {
        match observations
            .iter()
            .map(&level_of)
            .min_by_key(|l| (l.abs_diff(level), *l))
        {
            Some(nearest) => band(nearest),
            None => return Vec::new(),
        }
    };
    observations
        .iter()
        .filter(|o| band(level_of(o)) == target)
        .collect()
}

/// The lowest run score that earns the `/end` package.
///
/// Retail paid a package on 16 of 18 captured `/end`s. The two that paid none
/// are the two lowest-scoring runs: no kill at all (level 8), and three kills on
/// floor 49 at initialPlayerLevel 59 — delta -10, one point a kill, at most 6 even
/// were all three bosses. The lowest-scoring run that WAS paid killed 38 enemies
/// at delta -13 and below, one point each, so at least 38 × 0.33 = 12.5. The
/// threshold lies in (6, 12.5]; 10 is one same-level kill.
pub const END_PACKAGE_MIN_SCORE: f64 = 10.0;

/// The `/end` package: one stackable on top of the per-floor gold and XP — a
/// crafting material, or a stack of soul gems (#294: "randomly up to 7 greater,
/// glorious or transcendent soul gems, but not every run").
///
/// Drawn whole from the retail packages paid in the character's level band. In
/// the 60+ band that is 5 soul-gem stacks (4-6 Greater/Grand/Elevated/Glorious)
/// and 5 enchanting ingredients (Glow Dust ×7 thrice, Fire Salts ×19, Imp Stool ×7)
/// in 10 captured runs, plus 3 Transcendent ×4 from retail end screens (report #383,
/// [`screenshot_end_packages`]). Seeded from the run, so a retried `/end` pays the
/// same thing.
/// Gear, which retail added to 3 of the 4 runs that passed rung 360, is not
/// modelled here.
///
/// A soul-gem stack is TRANSCENDENT, 4-7 of them, one time in
/// [`TRANSCENDENT_ONE_IN`] — see [`TRANSCENDENT_SOUL_GEM`] for why that rests on
/// players' reports and not on a capture.
///
/// BELOW LEVEL 60 (report #371). The six captured packages under level 60 are all
/// materials, and drawing only from them meant a level-43 character could never
/// be paid a soul gem at `/end` — while a level-60 one was paid them every other
/// run. Six draws are too few to say retail never did: at the level-blind rate
/// of all 16 paid packages (5 gem stacks) six materials in a row happen one time
/// in nine. And retail's own package table (`AbyssPackageRewardLootTable`, in
/// `dump.cs`) is keyed by score tier, then by LEVEL — a `_levelList` of loot
/// entries — so level decides what a stack holds, not whether one is paid. A band
/// with no gem package of its own therefore pays one at that pooled rate, a
/// retail stack re-graded to the character's level ([`regrade_soul_gems`]). The
/// rest of the time it pays its own band's material, as before. The 60+ band is
/// unchanged.
///
/// LEVELS 39-48 follow the retail screenshots the reporter later supplied: half
/// the runs pay 1-3 Grand (39-44) or 1-7 Greater/Elevated/Grand (45-48) — see
/// [`mid_level_soul_gems`]. The re-grading above now applies below level 39 only.
///
/// FROM LEVEL 49 the package is the top band's, drawn as at level 100 (report #383,
/// [`TOP_BAND_FROM_LEVEL`]): Glorious gems and the Transcendent upgrade included.
pub fn end_package(score: f64, character_level: u64, run_seed: i64) -> RewardGrant {
    let mut grant = RewardGrant::default();
    if score < END_PACKAGE_MIN_SCORE {
        return grant;
    }
    let paid: Vec<&EndObservation> = corpus()
        .end_package
        .observations
        .iter()
        .filter(|o| o.package.is_some())
        .collect();
    // The captures plus the #383 screenshots ([`screenshot_end_packages`]); `paid`
    // alone still sets the pooled capture rate below.
    let observed: Vec<&EndObservation> =
        paid.iter().copied().chain(screenshot_end_packages()).collect();
    let pool = level_pool(&observed, character_level, |o| o.level);
    if pool.is_empty() {
        return grant;
    }
    let seed = mix((run_seed as u64) ^ 0xE4D0_A55E_5EED_0294);
    if MID_LEVEL_GEMS.contains(&character_level) {
        // Report #371, from retail screenshots — see [`mid_level_soul_gems`].
        let roll = mix(seed ^ 0x50_017E_4D00_0371);
        if roll % MID_LEVEL_GEM_ONE_IN == 0 {
            grant.stackable_items = mid_level_soul_gems(character_level, roll >> 8);
        } else if let Some(package) = &pool[(seed % pool.len() as u64) as usize].package {
            grant.stackable_items = package.stackable_items.clone();
        }
        return grant;
    }
    let gem_packages: Vec<&&EndObservation> =
        paid.iter().filter(|o| is_soul_gem_package(o)).collect();
    if !pool.iter().any(|o| is_soul_gem_package(o)) && !gem_packages.is_empty() {
        // A band retail was never seen paying gems in (see the doc above).
        let roll = mix(seed ^ 0x50_017E_4D00_0371);
        if roll % paid.len() as u64 >= gem_packages.len() as u64 {
            if let Some(package) = &pool[(seed % pool.len() as u64) as usize].package {
                grant.stackable_items = package.stackable_items.clone();
            }
            return grant;
        }
        let drawn = gem_packages[((roll >> 32) % gem_packages.len() as u64) as usize];
        if let Some(package) = &drawn.package {
            grant.stackable_items =
                regrade_soul_gems(&package.stackable_items, drawn.level, character_level);
        }
        return grant;
    }
    if let Some(package) = &pool[(seed % pool.len() as u64) as usize].package {
        grant.stackable_items = package.stackable_items.clone();
    }
    let is_soul_gems = is_soul_gem_stack(&grant.stackable_items);
    let upgrade = mix(seed ^ 0x7EA5_CE4D_E400_0294);
    if is_soul_gems && upgrade % TRANSCENDENT_ONE_IN == 0 {
        let count = 4 + (upgrade >> 32) % 4; // 4..=7
        grant.stackable_items =
            HashMap::from([(Uuid::parse_str(TRANSCENDENT_SOUL_GEM).unwrap(), count)]);
    }
    grant
}

/// The character levels whose `/end` soul gems follow retail screenshots rather
/// than the captures (report #371) — see [`mid_level_soul_gems`]. It ended at 59;
/// from [`TOP_BAND_FROM_LEVEL`] the top band's captured packages apply instead
/// (report #383). The level-57/58 screenshots' single Grand is still paid there,
/// by the rungs beside the golden chest ([`GRAND_BESIDE_THE_GOLDEN_CHEST`]).
pub const MID_LEVEL_GEMS: std::ops::RangeInclusive<u64> = 39..=TOP_BAND_FROM_LEVEL - 1;

/// A level in [`MID_LEVEL_GEMS`] is paid soul gems at `/end` one run in this many
/// — retail's measured rate at level 60+ (5 gem stacks in 10 paid packages). The
/// screenshots below show gems on three of four end screens at levels 47-58, so
/// the pooled capture rate (5/16) used further down is too low here.
pub const MID_LEVEL_GEM_ONE_IN: u64 = 2;

/// The soul-gem stack `/end` pays a level-39..59 character (report #371).
///
/// EVIDENCE. The captures hold no gem package below level 60 (six materials).
/// The reporter, a trusted retail player, supplied seven screenshots of the
/// original game (community Discord, 2022-2023; tracker #371):
///
/// * Abyss end, level 47, floor 53: Golden Chest, Wooden Chest, **Grand Soul Gem ×6**.
/// * Abyss end, level 58, floor 80: Golden Chest, Wooden Chest, **Grand Soul Gem**.
/// * Abyss end, level 57, floor 100: Wooden Chest, **Grand Soul Gem**, Fire Salts ×6.
/// * Abyss end, level 58, floor 91: Golden Chest, Wooden Chest, Stalhrim Shield.
/// * Abyss end, level 63, floor 68: Golden Chest, Wooden Chest, Giant's Toe ×29.
///
/// and, where the screenshots are silent, his account: "from level 39 to 45, 3
/// Grand at the end; from 45, up to 7 of every kind".
///
/// So: levels 39-44 get 1-3 Grand; 45-59 get 1-7 of Greater, Elevated or Grand —
/// the kinds retail's lowest gem-paying level (66) was paid, of which Grand is the
/// one every screenshot shows. Never Glorious or Transcendent below 60.
/// The `/end` stack used to be re-graded down one step per ten levels from the
/// level-66 stacks, which gave a level-47 character Exceptional at best; the
/// screenshot shows six Grand.
fn mid_level_soul_gems(level: u64, roll: u64) -> HashMap<Uuid, u64> {
    let grand = Uuid::parse_str(SOUL_GEMS[GRAND]).unwrap();
    if level < 45 {
        return HashMap::from([(grand, 1 + roll % 3)]);
    }
    let kind = [GREATER, ELEVATED, GRAND][(roll % 3) as usize];
    let count = 1 + (roll >> 16) % 7; // 1..=7
    HashMap::from([(Uuid::parse_str(SOUL_GEMS[kind]).unwrap(), count)])
}

const GREATER: usize = 5;
const ELEVATED: usize = 6;
const GRAND: usize = 7;

/// The rungs that sit either side of the golden chest (rung 360): the stackable
/// rungs before (190) and after (490) it.
const RUNGS_BESIDE_THE_GOLDEN_CHEST: [u32; 2] = [190, 490];

/// The character levels a rung beside the golden chest can pay a Grand gem at —
/// see [`grand_gem_beside_the_golden_chest`]. Unchanged by #383: it ADDS a result
/// to the top band's pool these levels already drew at those rungs, and it is how
/// the level-57/58 screenshots' single Grand is reached.
const GRAND_BESIDE_THE_GOLDEN_CHEST: std::ops::RangeInclusive<u64> = 39..=59;

/// A rung beside the golden chest can pay one Grand Soul Gem to a level-39..59
/// character (report #371): "there is a Grand one before a golden chest, and one
/// after", and the level-57/58 end screens above show a single Grand among the
/// rung rewards. Retail's captures have no observation at those rungs below
/// level 60 — the pool a mid-level character draws from there is borrowed from
/// level 66-81 (Emerald, Malachite, Elevated ×1) — so the Grand gem is added to
/// that pool as one more equally likely result, not as a certainty ("mostly
/// after the golden chest").
fn grand_gem_beside_the_golden_chest(rung: u32, level: u64) -> Option<ObservedReward> {
    let beside = RUNGS_BESIDE_THE_GOLDEN_CHEST.contains(&rung);
    (beside && GRAND_BESIDE_THE_GOLDEN_CHEST.contains(&level)).then(|| ObservedReward {
        stackable_items: HashMap::from([(Uuid::parse_str(SOUL_GEMS[GRAND]).unwrap(), 1)]),
        items: Vec::new(),
    })
}

fn is_soul_gem_stack(stacks: &HashMap<Uuid, u64>) -> bool {
    !stacks.is_empty() && stacks.keys().all(|id| soul_gem_grade(id).is_some())
}

fn is_soul_gem_package(o: &EndObservation) -> bool {
    o.package.as_ref().is_some_and(|p| is_soul_gem_stack(&p.stackable_items))
}

/// 0 for Petty (SoulGem1) .. 9 for Transcendent (SoulGem10).
fn soul_gem_grade(id: &Uuid) -> Option<usize> {
    let id = id.to_string();
    SOUL_GEMS.iter().position(|g| *g == id)
}

/// A retail soul-gem stack paid at `observed_level`, moved to `level`: one grade
/// down per ten levels below the observation (rounded), never below Petty, same
/// count. Never moved UP — it is only used for characters below every gem
/// observation.
///
/// The slope is retail's own: its `/end` stacks went Greater-Grand at level 66 and
/// Glorious at 79 and 93, and its enemy corpses dropped Common to Greater gems at
/// enemy level 40-45 (`enemy_loot.json`, tables 871c2e9b and 9217b4cd) — which is
/// what this gives a level-43 character from those five stacks.
fn regrade_soul_gems(
    stacks: &HashMap<Uuid, u64>,
    observed_level: u64,
    level: u64,
) -> HashMap<Uuid, u64> {
    let shift = (observed_level.saturating_sub(level) + 5) / 10;
    let mut out = HashMap::new();
    for (id, count) in stacks {
        let to = match soul_gem_grade(id) {
            Some(grade) => Uuid::parse_str(SOUL_GEMS[grade.saturating_sub(shift as usize)]).unwrap(),
            None => *id,
        };
        *out.entry(to).or_default() += count;
    }
    out
}

/// The ten soul gems, Petty (SoulGem1) to Transcendent (SoulGem10).
const SOUL_GEMS: [&str; 10] = [
    "19ce1a65-057f-4f34-a0ed-27de7c085662",
    "790a188b-3fa0-4f38-99d9-bc8d3675bc46",
    "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029",
    "1ba210b4-8cca-4f2f-b942-8fab80a52fd8",
    "3932e499-441e-4c6d-b671-9a03131ebe6f",
    "a1d41da0-51e0-4a80-ba9a-b8e9046be27e",
    "a3351353-f613-4368-bac7-05783f857b07",
    "68d7941e-8c8d-47bf-9f66-becb058f1817",
    "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb",
    TRANSCENDENT_SOUL_GEM,
];

/// Transcendent Soul Gem (SoulGem10).
///
/// NOT FROM A CAPTURE. None of the 18 captured retail `/end`s paid one (their gem
/// stacks were 4-6 Greater, Grand, Elevated or Glorious). Three retail end screens
/// at level 82-95 show it paid ×4 (report #383, [`screenshot_end_packages`]), and
/// those join the top band's pool as observations; the upgrade below is kept for
/// the counts above 4 ("up to 7"). #294's reporter, a
/// level-100 player, reports retail's `/end` paying "up to 7 greater, glorious or
/// transcendent soul gems, but not every run", backed by a screenshot, and that
/// players on Discord put the highest count at 7. The APK's server-only
/// `AbyssScaling_Backend.PackageRewardTier` has an `_upgradeChance` the captures
/// cannot measure. So: the retail gem frequency is kept as it was, and a gem stack
/// is upgraded to 4-7 Transcendent one time in [`TRANSCENDENT_ONE_IN`] — an
/// assumption sized so that the 5 captured high-level gem stacks containing none is
/// unremarkable (0.8^5 = 0.33).
pub const TRANSCENDENT_SOUL_GEM: &str = "d94bab85-53d5-4c9c-a637-acd94fc66c98";

/// One soul-gem stack in this many is Transcendent — see [`TRANSCENDENT_SOUL_GEM`].
pub const TRANSCENDENT_ONE_IN: u64 = 5;

/// The number of observations behind a rung, for tests and diagnostics.
pub fn rung_observations(score: u32) -> u64 {
    corpus()
        .rungs
        .iter()
        .find(|r| r.score == score)
        .map(|r| r.observations)
        .unwrap_or(0)
}

/// The next rung a run has NOT yet reached, and what it grants.
///
/// Retail sends exactly one entry — the next unreached rung — in 707 of 707
/// observed responses, so this returns one rung and not the ladder. `None` once
/// every rung has been passed, which serialises as an empty list, matching a run
/// that has nothing left to advertise.
pub fn next_future_reward(
    score: f64,
    character_level: u64,
    run_seed: i64,
) -> Option<(u32, RewardGrant)> {
    let rung = corpus()
        .rungs
        .iter()
        .find(|r| f64::from(r.score) > score)?;
    Some((rung.score, rung_reward(rung, character_level, run_seed)))
}

/// Every rung a score has reached (`score >= rung`), lowest first.
///
/// The complement of [`next_future_reward`]'s selection: a rung stops being
/// advertised at exactly the score at which it counts as reached, so a rung is
/// never both advertised and paid, and never neither.
pub fn reached_rungs(score: f64) -> impl Iterator<Item = u32> {
    corpus()
        .rungs
        .iter()
        .map(|r| r.score)
        .filter(move |rung| f64::from(*rung) <= score)
}

/// What one rung grants this run — the very reward [`next_future_reward`]
/// advertised for it, because both resolve through [`rung_reward`] with the same
/// seed. `None` for a score that is not a rung.
pub fn future_reward_for_rung(
    rung_score: u32,
    character_level: u64,
    run_seed: i64,
) -> Option<RewardGrant> {
    let rung = corpus().rungs.iter().find(|r| r.score == rung_score)?;
    Some(rung_reward(rung, character_level, run_seed))
}

/// Rung results seen on retail screenshots rather than in a capture, by rung.
///
/// Rung 70, level 63: Giant's Toe ×29 — the end screen of a level-63, floor-68 run
/// (#371's screenshots, see [`mid_level_soul_gems`]). It is a rung-70 stack: rung 70
/// paid 27-35 at level 66-100 in every capture, while `/end` packages were 4-19.
/// The captures alone hold one Giant's Toe in seven level-60+ rung-70 draws against
/// four Nightshade, which report #381 calls too much Nightshade.
///
/// Rungs 35 and 70, levels 82-95 — three more retail end screens ("INTO THE ABYSS —
/// REWARDS", players' Discord posts of 2024-10-11 and 2025-02-27, tracker #383):
///
/// * level 92, floor 150, 3 kills: Wooden Chest, Transcendent Soul Gem ×4,
///   **Frost Salts ×6**, and a tile cut off at the right edge whose name starts
///   "M". It is not encoded: the screenshot does not say what it is.
/// * level 95, floor 150, 3 kills: Transcendent Soul Gem ×4, **Nightshade ×30**,
///   **Imp Stool ×6** (the left edge may hide a tile: the rung-50 Wooden Chest).
/// * level 82, floor 136, 1 kill: Wooden Chest, Transcendent Soul Gem ×4,
///   **Honeycomb ×5**.
///
/// WHICH SLOT EACH TILE IS. An end screen lists every rung the run reached plus the
/// `/end` package, and `/end` pays ONE stack (16 of 16 paid captures). The Wooden
/// Chest is rung 50 (tier 1, every capture), so each run also reached rung 35 and
/// its reward is on the screen. The level-82 screen has three tiles: the chest, the
/// gems and the Honeycomb. Rung 35 never paid a soul gem (25 captures), so the
/// Honeycomb ×5 is rung 35 and the Transcendent ×4 is `/end` — see
/// [`screenshot_end_packages`]. The same holds for the Frost Salts ×6 and Imp Stool ×6,
/// which are also rung-35 sized (top-band rung 35 paid Fire Salts, Imp Stool, Void
/// Salts, Honeycomb and Glow Dust ×5-6). Nightshade ×30 is a rung-70 stack (27-35;
/// `/end` packages were 4-19). The kills agree: a deep-floor kill scores 30, ×2 for
/// a boss, so one boss reaches rung 50 but not 70 (the level-82 screen: rungs 35 and
/// 50), and three kills reach 70 but not 95 (the level-92/95 screens: rungs 35, 50
/// and 70 — so the cut-off "M…" tile is most likely the level-92 run's rung 70).
fn screenshot_results(rung: u32) -> &'static [LevelledResult] {
    static RUNG_35: std::sync::OnceLock<Vec<LevelledResult>> = std::sync::OnceLock::new();
    static RUNG_70: std::sync::OnceLock<Vec<LevelledResult>> = std::sync::OnceLock::new();
    let seen = |level: u64, item: &str, count: u64| LevelledResult { level, reward: one_stack(item, count) };
    match rung {
        35 => RUNG_35.get_or_init(|| {
            vec![
                seen(92, FROST_SALTS, 6), // #383, level 92, floor 150, 3 kills
                seen(95, IMP_STOOL, 6),   // #383, level 95, floor 150, 3 kills
                seen(82, HONEYCOMB, 5),   // #383, level 82, floor 136, 1 kill
            ]
        }),
        70 => RUNG_70.get_or_init(|| {
            vec![
                seen(63, GIANTS_TOE, 29), // #371, level 63, floor 68
                seen(95, NIGHTSHADE, 30), // #383, level 95, floor 150, 3 kills
            ]
        }),
        _ => &[],
    }
}

/// `/end` packages seen on retail screenshots rather than in a capture: the three
/// #383 end screens above each paid **Transcendent Soul Gem ×4** at `/end` (levels 92,
/// 95 and 82). They join the top band's captured packages as three more observations,
/// so a level-49+ `/end` is drawn from 13 retail packages instead of 10. They are
/// NOT counted in the pooled capture rate a band without gem packages of its own is
/// paid gems at (below level 39): that rate is the captures', and these three are
/// all top-band.
fn screenshot_end_packages() -> &'static [EndObservation] {
    static END: std::sync::OnceLock<Vec<EndObservation>> = std::sync::OnceLock::new();
    END.get_or_init(|| {
        [92u64, 95, 82]
            .into_iter()
            .map(|level| EndObservation { level, package: Some(one_stack(TRANSCENDENT_SOUL_GEM, 4)) })
            .collect()
    })
}

fn one_stack(item: &str, count: u64) -> ObservedReward {
    ObservedReward {
        stackable_items: HashMap::from([(Uuid::parse_str(item).unwrap(), count)]),
        items: Vec::new(),
    }
}

const FROST_SALTS: &str = "05b4dd6b-796f-4088-a772-0e33ea3db976";
const IMP_STOOL: &str = "07f380dd-f123-4ef7-9b12-8b9fdb03c3e1";
const HONEYCOMB: &str = "7116a2a8-ac2d-4cd9-8b7c-b80c397d3f50";
const NIGHTSHADE: &str = "4d7420db-8042-4946-a43f-e0b3bd9bec81";
const GIANTS_TOE: &str = "a4d5e792-5a27-4bb3-851f-e0917c0962db";

/// Report #381: "in the Abyss, instead of Pearls, there should be Diamonds and
/// Atronite". Rungs 190 and 490 were never captured for a level 20-59 character,
/// and [`level_pool`] lent a level 20-36 one the nearest observations in level —
/// the level-3/7 runs: one Pearl, seven Steel Ingots, one Petty Soul Gem. Lending
/// from the level-66/81 runs instead pays Emerald, Malachite and a soul gem, the
/// gem moved down to the character's level ([`regrade_soul_gems`]).
///
/// Only for a pool [`level_pool`] borrowed from a band BELOW the character's, so
/// every character it already served from its own band or from above (37-100,
/// the level-43 and level-100 runs) draws exactly as before. The rung-260 gear is
/// not lent upwards: a level-66 weapon is no fitter for a level-21 character than
/// a level-7 one is. Returns the pool and the lowest level in it.
fn borrow_from_above<'a>(
    observations: &[&'a LevelledResult],
    pool: &[&'a LevelledResult],
    level: u64,
) -> Option<(Vec<&'a LevelledResult>, u64)> {
    let own = character_band(level);
    let pool_band = band(pool.first()?.level);
    if pool_band >= own {
        return None;
    }
    let above = observations.iter().map(|o| band(o.level)).filter(|b| *b > own).min()?;
    let lent: Vec<&LevelledResult> =
        observations.iter().copied().filter(|o| band(o.level) == above).collect();
    let lowest = lent.iter().map(|o| o.level).min()?;
    Some((lent, lowest))
}

/// The single resolution of a rung's reward, shared by the advertisement and the
/// grant. The draw is a function of the run seed and the rung only — never of
/// the score that selected it — so advertising and paying cannot disagree.
fn rung_reward(rung: &Rung, character_level: u64, run_seed: i64) -> RewardGrant {
    let mut grant = RewardGrant::default();

    if rung.kind == "chest" {
        // Deterministic: fixed tier, and the chest takes the character's own
        // level. `id` stays None so the treasury assigns its own, and so the
        // wire reads {"level":N,"tier":T} exactly as retail sent it.
        grant.chests.push(RewardChest {
            id: None,
            tier: rung.tier.unwrap_or(1),
            level: character_level,
        });
        return grant;
    }

    let screenshots = screenshot_results(rung.score);
    let observations: Vec<&LevelledResult> = rung.results.iter().chain(screenshots).collect();
    let mut pool: Vec<&LevelledResult> =
        level_pool(&observations, character_level, |r| r.level).into_iter().copied().collect();
    // A material rung the character's band never showed us borrows from the band
    // ABOVE rather than below (report #381) — see [`borrow_from_above`].
    let mut regrade_from: Option<u64> = None;
    if rung.kind == "stackableItems" {
        if let Some((above, lowest)) = borrow_from_above(&observations, &pool, character_level) {
            pool = above;
            regrade_from = Some(lowest);
        }
    }
    if pool.is_empty() {
        // A rung we somehow hold no observation for advertises nothing rather
        // than inventing a reward. The score still shows, which is what the
        // player is climbing towards.
        return grant;
    }

    let seed = mix((run_seed as u64) ^ (u64::from(rung.score)).rotate_left(29));
    let extra = grand_gem_beside_the_golden_chest(rung.score, character_level);
    let mut candidates: Vec<&ObservedReward> = pool.iter().map(|r| &r.reward).collect();
    if let Some(grand) = &extra {
        candidates.push(grand);
    }
    let drawn = candidates[(seed % candidates.len() as u64) as usize];

    grant.stackable_items = match regrade_from {
        Some(observed) => regrade_soul_gems(&drawn.stackable_items, observed, character_level),
        None => drawn.stackable_items.clone(),
    };
    for (ordinal, item) in drawn.items.iter().enumerate() {
        grant.items.push(RewardItem {
            id: instance_uuid(seed, ordinal),
            item: item.clone(),
        });
    }
    grant
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus must be compiled in and hold the ladder that was measured.
    /// Everything below is vacuous without it.
    #[test]
    fn the_ladder_is_the_one_that_was_measured() {
        let scores: Vec<u32> = corpus().rungs.iter().map(|r| r.score).collect();
        assert_eq!(scores, ABYSS_LADDER.to_vec());
        assert_eq!(corpus().rungs.len(), 10);
    }

    /// THE BUG: every rung above the first advertised nothing, because static
    /// data held one rung. Each rung must now produce a reward.
    #[test]
    fn every_rung_advertises_something() {
        for (i, score) in ABYSS_LADDER.iter().enumerate() {
            // a score just below this rung selects it
            let below = f64::from(*score) - 1.0;
            let (picked, grant) = next_future_reward(below, 40, 12345)
                .unwrap_or_else(|| panic!("rung {score} advertised nothing"));
            assert_eq!(picked, *score, "rung {i} selected the wrong score");
            assert!(
                !grant.stackable_items.is_empty()
                    || !grant.items.is_empty()
                    || !grant.chests.is_empty(),
                "rung {score} selected but granted nothing"
            );
        }
    }

    /// Retail sends the NEXT unreached rung, one entry. A run past every rung
    /// has nothing left to advertise.
    #[test]
    fn the_next_unreached_rung_is_selected() {
        assert_eq!(next_future_reward(0.0, 40, 1).unwrap().0, 35);
        assert_eq!(next_future_reward(35.0, 40, 1).unwrap().0, 50);
        assert_eq!(next_future_reward(36.0, 40, 1).unwrap().0, 50);
        assert_eq!(next_future_reward(649.0, 40, 1).unwrap().0, 650);
        assert!(next_future_reward(650.0, 40, 1).is_none());
        assert!(next_future_reward(9999.0, 40, 1).is_none());
    }

    /// The chest rungs are the deterministic ones: fixed tier, and the chest is
    /// cut to the character's own level. 72 of 72 observations.
    #[test]
    fn chest_rungs_take_the_characters_level() {
        for (rung, tier) in [(50u32, 1u64), (360, 3)] {
            for level in [1u64, 7, 40, 66, 100] {
                let (picked, grant) =
                    next_future_reward(f64::from(rung) - 1.0, level, 999).unwrap();
                assert_eq!(picked, rung);
                assert_eq!(grant.chests.len(), 1, "rung {rung} must grant one chest");
                assert_eq!(grant.chests[0].tier, tier, "rung {rung} tier");
                assert_eq!(
                    grant.chests[0].level, level,
                    "rung {rung} must cut the chest to the character's level"
                );
                assert!(grant.chests[0].id.is_none(), "the treasury assigns the id");
                assert!(grant.stackable_items.is_empty() && grant.items.is_empty());
            }
        }
    }

    /// The client re-reads the run while it plays and must be advertised the
    /// same reward every time. A random draw per request would change it.
    #[test]
    fn the_draw_is_stable_within_a_run() {
        for seed in [1i64, 77, -9000, i64::MAX] {
            for rung in ABYSS_LADDER {
                let a = next_future_reward(f64::from(rung) - 1.0, 50, seed).unwrap();
                let b = next_future_reward(f64::from(rung) - 1.0, 50, seed).unwrap();
                assert_eq!(a.0, b.0);
                assert_eq!(a.1.stackable_items, b.1.stackable_items, "rung {rung}");
                assert_eq!(
                    a.1.items.iter().map(|i| i.id).collect::<Vec<_>>(),
                    b.1.items.iter().map(|i| i.id).collect::<Vec<_>>(),
                    "rung {rung} gear instance ids must not move between reads"
                );
            }
        }
    }

    /// Different runs must not all be shown the same thing, or the pool is
    /// decorative. Checked on the rung with the most distinct results.
    #[test]
    fn different_runs_draw_different_rewards() {
        let mut seen = std::collections::HashSet::new();
        for seed in 0..200i64 {
            let (_, grant) = next_future_reward(34.0, 50, seed).unwrap();
            let mut key: Vec<_> = grant
                .stackable_items
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect();
            key.sort();
            seen.insert(format!("{key:?}"));
        }
        assert!(
            seen.len() > 5,
            "rung 35 has 17 distinct observed results but 200 runs drew only {}",
            seen.len()
        );
    }

    /// Gear at rung 260 must come out as a real item with a fresh instance id.
    #[test]
    fn gear_rungs_mint_an_instance_id() {
        let mut ids = std::collections::HashSet::new();
        let mut with_gear = 0;
        for seed in 0..60i64 {
            let (score, grant) = next_future_reward(259.0, 50, seed).unwrap();
            assert_eq!(score, 260);
            for item in &grant.items {
                with_gear += 1;
                assert!(!item.item.item_template_id.is_nil());
                assert_eq!(item.id.get_version_num(), 4);
                ids.insert(item.id);
            }
        }
        assert!(with_gear > 0, "rung 260 granted no gear at all");
        assert!(ids.len() > 1, "every run was handed the same instance id");
    }

    /// The thin rungs are recorded as thin. If an observation count silently
    /// went to zero the corpus was rebuilt wrong.
    #[test]
    fn every_rung_records_how_thin_it_is() {
        for score in ABYSS_LADDER {
            assert!(
                rung_observations(score) > 0,
                "rung {score} claims zero observations"
            );
        }
        assert_eq!(rung_observations(35), 25, "25 retail runs reached rung 35");
        assert_eq!(rung_observations(70), 13, "13 retail runs reached rung 70");
        assert_eq!(rung_observations(650), 1, "one retail run reached rung 650");
    }

    /// The grant and the advertisement resolve through one function: whatever a
    /// rung was advertised as is exactly what it pays.
    #[test]
    fn a_rung_pays_what_it_advertised() {
        for seed in [1i64, 77, -9000, i64::MAX] {
            for rung in ABYSS_LADDER {
                let (score, advertised) =
                    next_future_reward(f64::from(rung) - 1.0, 42, seed).unwrap();
                assert_eq!(score, rung);
                let paid = future_reward_for_rung(rung, 42, seed).unwrap();
                assert_eq!(paid, advertised, "rung {rung} seed {seed}");
            }
        }
        assert!(
            future_reward_for_rung(36, 42, 1).is_none(),
            "36 is not a rung"
        );
    }

    /// A rung is reached at exactly the score where it stops being advertised.
    #[test]
    fn reached_rungs_complement_the_advertised_one() {
        assert_eq!(reached_rungs(34.9).count(), 0);
        assert_eq!(reached_rungs(35.0).collect::<Vec<_>>(), vec![35]);
        assert_eq!(reached_rungs(69.0).collect::<Vec<_>>(), vec![35, 50]);
        assert_eq!(
            reached_rungs(10_000.0).collect::<Vec<_>>(),
            ABYSS_LADDER.to_vec()
        );
        for score in [0.0, 34.0, 35.0, 49.9, 50.0, 300.0, 650.0] {
            let next = next_future_reward(score, 1, 1).map(|(s, _)| s);
            let last_reached = reached_rungs(score).last();
            if let Some(next) = next {
                assert!(f64::from(next) > score);
                assert!(last_reached.is_none_or(|r| r < next));
            }
        }
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    const NIGHTSHADE: &str = "4d7420db-8042-4946-a43f-e0b3bd9bec81";
    const GIANTS_TOE: &str = "a4d5e792-5a27-4bb3-851f-e0917c0962db";
    const CHAURUS_CHITIN: &str = "8ef9f10c-3c46-492c-9a00-29fd1626d85e";
    const MALACHITE_INGOT: &str = "85ed5500-3581-4699-8095-4b5ff6514355";
    const GARLIC: &str = "fbf96b07-e9aa-4157-8761-10179fa05138";
    const BRASS_INGOT: &str = "92b77b2f-bd33-469f-8aad-8a228b9537eb";
    /// Greater, Elevated, Grand, Glorious — the four retail `/end` paid.
    const SOUL_GEMS_PAID: [&str; 4] = [
        "a1d41da0-51e0-4a80-ba9a-b8e9046be27e",
        "a3351353-f613-4368-bac7-05783f857b07",
        "68d7941e-8c8d-47bf-9f66-becb058f1817",
        "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb",
    ];
    /// Every soul gem, Petty to Transcendent.
    const ALL_SOUL_GEMS: [&str; 10] = [
        "19ce1a65-057f-4f34-a0ed-27de7c085662", "790a188b-3fa0-4f38-99d9-bc8d3675bc46",
        "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029", "1ba210b4-8cca-4f2f-b942-8fab80a52fd8",
        "3932e499-441e-4c6d-b671-9a03131ebe6f", "a1d41da0-51e0-4a80-ba9a-b8e9046be27e",
        "a3351353-f613-4368-bac7-05783f857b07", "68d7941e-8c8d-47bf-9f66-becb058f1817",
        "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", "d94bab85-53d5-4c9c-a637-acd94fc66c98",
    ];

    fn single_stack(grant: &RewardGrant) -> (Uuid, u64) {
        assert_eq!(grant.stackable_items.len(), 1, "one stack per draw: {:?}", grant.stackable_items);
        let (k, v) = grant.stackable_items.iter().next().unwrap();
        (*k, *v)
    }

    /// #294: "at floor 149 ... then either Nightshade, Giant's Toe or Moon Sugar".
    /// Retail's rung 70 for a level 66-100 character: Nightshade x4, Giant's Toe,
    /// Chaurus Chitin, Malachite Ingot — 27-35 of them. Never the level-3/7 results.
    #[test]
    fn a_level_100_character_draws_rung_70_from_the_high_level_table() {
        let allowed = [NIGHTSHADE, GIANTS_TOE, CHAURUS_CHITIN, MALACHITE_INGOT].map(uuid);
        let mut seen = std::collections::HashSet::new();
        for seed in 0..500i64 {
            let grant = future_reward_for_rung(70, 100, seed).unwrap();
            let (item, count) = single_stack(&grant);
            assert!(allowed.contains(&item), "seed {seed}: level 100 drew {item}");
            assert!((27..=35).contains(&count), "seed {seed}: {count} is outside retail's 27-35");
            seen.insert(item);
        }
        assert!(seen.contains(&uuid(NIGHTSHADE)) && seen.contains(&uuid(GIANTS_TOE)));
    }

    /// Report #381: "the share of Nightshade is too high, it should alternate more
    /// with Giant's Toe". The captures alone gave a level-60+ character Nightshade
    /// 4 times in 7 and Giant's Toe once; with the level-63 screenshot it is 4 in 8
    /// and 2 in 8, and with #383's level-95 Nightshade ×30 screenshot 5 in 9 and 2 in
    /// 9. The level-43 (20-59) pool, which has no Giant's Toe in retail, is untouched.
    #[test]
    fn rung_70_alternates_nightshade_with_giants_toe_at_high_level() {
        let draws = 4000i64;
        let share = |level: u64, id: &str| {
            (0..draws)
                .filter(|seed| single_stack(&future_reward_for_rung(70, level, *seed).unwrap()).0 == uuid(id))
                .count() as f64
                / draws as f64
        };
        let (nightshade, toe) = (share(100, NIGHTSHADE), share(100, GIANTS_TOE));
        println!("rung 70, level 100: Nightshade {nightshade:.3}, Giant's Toe {toe:.3}");
        assert!((0.50..=0.61).contains(&nightshade), "Nightshade {nightshade}, retail 5/9");
        assert!((0.18..=0.27).contains(&toe), "Giant's Toe {toe}, retail 2/9");
        assert_eq!(share(43, GIANTS_TOE), 0.0, "level 43 keeps its own retail pool");
        assert!((0.20..=0.30).contains(&share(43, NIGHTSHADE)));
    }

    /// Report #381: "in the Abyss, instead of Pearls, Diamonds and Atronite". Rungs
    /// 190 and 490 have no capture between level 20 and 59; a level 20-36 character
    /// was lent the level-3/7 results (Pearl, Steel Ingot, Petty Soul Gem) and is now
    /// lent the level-66/81 ones (Emerald, Malachite, a soul gem graded to its level).
    /// Level 7 keeps its own retail Pearl; level 43 and 100 draw exactly as before.
    #[test]
    fn a_mid_level_rung_190_and_490_are_never_a_pearl() {
        const PEARL: &str = "89ece62c-9ff6-470a-a152-d45ef4e0c222";
        const STEEL: &str = "b81952e0-c3c8-4a5c-92c0-8215d3eb71af";
        const EMERALD: &str = "4d1231be-d3fa-4282-81b2-0f7bed4aabff";
        let low = [PEARL, STEEL, ALL_SOUL_GEMS[0]].map(uuid);
        for level in [20u64, 25, 30, 36] {
            let mut seen = std::collections::HashSet::new();
            for seed in 0..400i64 {
                for rung in [190, 490] {
                    let (item, _) = single_stack(&future_reward_for_rung(rung, level, seed).unwrap());
                    assert!(!low.contains(&item), "level {level} rung {rung} drew {item}");
                    if let Some(grade) = soul_gem_grade(&item) {
                        assert!(grade <= 4, "level {level}: soul gem grade {grade} is not regraded");
                    }
                    seen.insert(item);
                }
            }
            assert!(seen.contains(&uuid(EMERALD)), "level {level}: {seen:?}");
        }
        // Control: retail's own level-3/7 results still serve a level-7 character.
        let level_7: std::collections::HashSet<Uuid> = (0..200i64)
            .map(|seed| single_stack(&future_reward_for_rung(190, 7, seed).unwrap()).0)
            .collect();
        assert!(level_7.contains(&uuid(PEARL)));
        // Level 37+ was already lent the level-66/81 results; unchanged, gems ungraded.
        for level in [43u64, 100] {
            for seed in 0..200i64 {
                let (item, _) = single_stack(&future_reward_for_rung(490, level, seed).unwrap());
                assert!(!low.contains(&item));
                if let Some(grade) = soul_gem_grade(&item) {
                    assert!(grade >= 6, "level {level}: {item} was regraded");
                }
            }
        }
    }

    /// The low-level control: a level-7 character keeps retail's level 3-7 results.
    #[test]
    fn a_low_level_character_draws_rung_70_from_the_low_level_table() {
        let allowed = [GARLIC, BRASS_INGOT].map(uuid);
        for seed in 0..200i64 {
            let (item, count) = single_stack(&future_reward_for_rung(70, 7, seed).unwrap());
            assert!(allowed.contains(&item), "seed {seed}: level 7 drew {item}");
            assert!(count <= 6);
        }
    }

    /// Every rung resolves to something at every level, including the rungs a band
    /// has no retail observation for (rung 650 has one, at level 7).
    #[test]
    fn every_rung_pays_at_every_level() {
        for level in [1u64, 7, 19, 20, 40, 59, 60, 84, 100] {
            for rung in ABYSS_LADDER {
                let g = future_reward_for_rung(rung, level, 3).unwrap();
                assert!(
                    !g.stackable_items.is_empty() || !g.items.is_empty() || !g.chests.is_empty(),
                    "rung {rung} at level {level} paid nothing"
                );
            }
        }
        // A band without its own observation borrows the nearest one in level: a
        // level-50 character's rung 260 is the level-66/81 Glass gear, not Hide.
        let gear = future_reward_for_rung(260, 50, 1).unwrap();
        let hide_helmet = uuid("4a9fd901-aaf8-40bd-941e-971febf4daf8");
        assert!(gear.items.iter().all(|i| i.item.item_template_id != hide_helmet));
    }

    /// #294: "at the end of a run, randomly up to 7 greater, glorious or
    /// transcendent soul gems, but not every run". Retail, level 60 and up: 5 of 10
    /// captured packages were a soul-gem stack of 4-6 (Greater, Grand, Elevated,
    /// Glorious x2), and #383's three end screens add Transcendent ×4 — 8 gem stacks
    /// in 13. Transcendent above 4 (up to 7) is the reporter's and Discord's account
    /// — see [`TRANSCENDENT_SOUL_GEM`].
    #[test]
    fn the_end_soul_gem_roll_matches_retails_frequency_and_range() {
        let paid_kinds = SOUL_GEMS_PAID.map(uuid);
        let all_gems = ALL_SOUL_GEMS.map(uuid);
        let runs = 20_000;
        let mut gems = 0;
        let mut transcendent = 0;
        let mut counts = std::collections::BTreeSet::new();
        let mut kinds = std::collections::HashSet::new();
        for seed in 0..runs {
            let grant = end_package(100.0, 100, seed);
            let (item, count) = single_stack(&grant);
            if all_gems.contains(&item) {
                gems += 1;
                if item == uuid(TRANSCENDENT_SOUL_GEM) {
                    transcendent += 1;
                    assert!((4..=7).contains(&count), "seed {seed}: {count} transcendent, reported up to 7");
                    counts.insert(count);
                } else {
                    assert!(paid_kinds.contains(&item), "seed {seed}: a gem retail never paid: {item}");
                    assert!((4..=6).contains(&count), "seed {seed}: {count} gems, retail paid 4-6");
                }
                kinds.insert(item);
            }
        }
        let rate = f64::from(gems) / runs as f64;
        assert!((0.57..=0.66).contains(&rate), "soul gems on {rate:.3} of level-100 /ends, retail 8/13");
        assert_eq!(kinds.len(), 5, "the four retail gem kinds and Transcendent come up");
        // 3 of 8 gem stacks are #383's Transcendent ×4, and one of the other five in
        // five is upgraded to 4-7 Transcendent: 3/8 + 5/8 × 1/5 = 1/2.
        let upgraded = f64::from(transcendent) / f64::from(gems);
        assert!((0.46..=0.54).contains(&upgraded), "transcendent on {upgraded:.3} of gem stacks");
        assert_eq!(counts, (4..=7).collect(), "4 to 7 transcendent gems");
    }

    /// Every gem stack `/end` pays a character of `level` over `runs` seeds, as
    /// `(kind, count)`, plus how many runs paid one.
    fn end_gems(level: u64, runs: i64) -> (Vec<(Uuid, u64)>, i64) {
        let all_gems = ALL_SOUL_GEMS.map(uuid);
        let mut out = Vec::new();
        for seed in 0..runs {
            let (item, count) = single_stack(&end_package(100.0, level, seed));
            if all_gems.contains(&item) {
                out.push((item, count));
            }
        }
        let n = out.len() as i64;
        (out, n)
    }

    /// Report #371: a level-43 character (Sephoris) was never paid a soul gem at
    /// `/end`, because the six retail packages below level 60 are all materials.
    /// The reporter: "from level 39 to 45, 3 Grand at the end". Levels 39-44 now
    /// get 1-3 Grand on half their runs, and their band's retail material otherwise.
    #[test]
    fn levels_39_to_44_are_paid_up_to_three_grand_soul_gems() {
        let grand = uuid(ALL_SOUL_GEMS[GRAND]);
        let materials: std::collections::HashSet<Uuid> = corpus()
            .end_package
            .observations
            .iter()
            .filter(|o| LEVEL_BANDS[1].contains(&o.level))
            .filter_map(|o| o.package.as_ref())
            .flat_map(|p| p.stackable_items.keys().copied())
            .collect();
        for level in [39u64, 43, 44] {
            let runs = 10_000;
            let (gems, n) = end_gems(level, runs);
            let counts: std::collections::BTreeSet<u64> = gems.iter().map(|g| g.1).collect();
            assert!(gems.iter().all(|g| g.0 == grand), "level {level}: {gems:?}");
            assert_eq!(counts, (1..=3).collect(), "level {level}: 1 to 3 Grand");
            let rate = n as f64 / runs as f64;
            assert!((0.45..=0.55).contains(&rate), "level {level}: gems on {rate:.3} of runs");
            for seed in 0..2_000 {
                let (item, _) = single_stack(&end_package(100.0, level, seed));
                assert!(item == grand || materials.contains(&item), "level {level} seed {seed}: {item}");
            }
        }
    }

    /// The screenshot cases: retail's end screen paid Grand ×6 at level 47, and a
    /// single Grand at levels 57 and 58. From 45 to 48 the `/end` stack is 1-7 of
    /// Greater, Elevated or Grand ("up to 7 of every kind"), never Glorious or
    /// Transcendent. From 49 `/end` is the top band's (report #383), and the
    /// level-57/58 single Grand is paid by the rungs beside the golden chest.
    #[test]
    fn levels_45_to_48_are_paid_up_to_seven_and_the_screenshots_are_reachable() {
        let grand = uuid(ALL_SOUL_GEMS[GRAND]);
        let kinds = [GREATER, ELEVATED, GRAND].map(|g| uuid(ALL_SOUL_GEMS[g]));
        for level in [45u64, 47, 48] {
            let (gems, n) = end_gems(level, 10_000);
            assert!(gems.iter().all(|g| kinds.contains(&g.0)), "level {level}: {gems:?}");
            let counts: std::collections::BTreeSet<u64> = gems.iter().map(|g| g.1).collect();
            assert_eq!(counts, (1..=7).collect(), "level {level}: 1 to 7 gems");
            let seen: std::collections::HashSet<Uuid> = gems.iter().map(|g| g.0).collect();
            assert_eq!(seen.len(), 3, "level {level}: all three kinds come up");
            assert!((4_500..=5_500).contains(&n), "level {level}: {n} gem runs in 10000");
        }
        let (at47, _) = end_gems(47, 10_000);
        assert!(at47.contains(&(grand, 6)), "level 47: retail's Grand ×6 never comes up");
        for level in [57u64, 58] {
            let single_grand = (0..400i64).any(|seed| {
                [190u32, 490].iter().any(|rung| {
                    future_reward_for_rung(*rung, level, seed).unwrap().stackable_items
                        == HashMap::from([(grand, 1)])
                })
            });
            assert!(single_grand, "level {level}: retail's single Grand never comes up");
        }
    }

    /// Report #383: from level 49 every rung and the `/end` package draw what a
    /// level-100 character draws for the same seed — Giant's Toe at rung 70, the top
    /// band's soul gems at `/end` (Glorious, and the Transcendent upgrade). Chests
    /// take the character's level, and rungs 190/490 keep #371's extra Grand
    /// (39-59). Level 48 is the control: it keeps the level 34-57 pool.
    #[test]
    fn from_level_49_every_reward_is_the_top_bands() {
        let giants_toe = uuid(GIANTS_TOE);
        let top_end: std::collections::HashSet<Uuid> = (0..2_000i64)
            .flat_map(|seed| end_package(100.0, 100, seed).stackable_items.into_keys())
            .collect();
        for level in [49u64, 50, 54, 57, 59] {
            for seed in 0..500i64 {
                for rung in [35u32, 70, 95, 135, 260, 650] {
                    assert_eq!(
                        future_reward_for_rung(rung, level, seed),
                        future_reward_for_rung(rung, 100, seed),
                        "level {level} rung {rung} seed {seed}"
                    );
                }
                assert_eq!(end_package(100.0, level, seed), end_package(100.0, 100, seed));
            }
            let ends: std::collections::HashSet<Uuid> = (0..2_000i64)
                .flat_map(|seed| end_package(100.0, level, seed).stackable_items.into_keys())
                .collect();
            assert_eq!(ends, top_end, "level {level}: /end reaches every top-band package");
        }
        for glorious_or_better in [ALL_SOUL_GEMS[8], TRANSCENDENT_SOUL_GEM] {
            assert!(top_end.contains(&uuid(glorious_or_better)), "{glorious_or_better} unreachable");
        }
        let toe = |level: u64| {
            (0..500i64).any(|seed| single_stack(&future_reward_for_rung(70, level, seed).unwrap()).0 == giants_toe)
        };
        assert!(toe(49), "level 49 never draws Giant's Toe at rung 70");
        assert!(!toe(48), "level 48 keeps the level 34-57 rung-70 pool");
        let quicksilver = uuid("e80bee76-f92c-4005-9eff-20d1e8c64d24");
        let quick = |level: u64| {
            (0..500i64).any(|seed| single_stack(&future_reward_for_rung(70, level, seed).unwrap()).0 == quicksilver)
        };
        assert!(quick(48) && !quick(49), "Quicksilver at rung 70 is the level 20-48 pool's");
    }

    /// Below 39 the re-graded retail stack still applies: a level-3 character is
    /// paid Petty or Lesser, a level-34 one no better than Common — one grade per
    /// ten levels below the retail stack, never below Petty, never Grand.
    #[test]
    fn soul_gems_below_39_follow_the_level() {
        for (level, allowed) in [(3u64, &ALL_SOUL_GEMS[0..2]), (34, &ALL_SOUL_GEMS[1..5])] {
            let allowed: Vec<Uuid> = allowed.iter().map(|s| uuid(s)).collect();
            let (gems, n) = end_gems(level, 5_000);
            assert!(n > 1_000, "level {level}: only {n} gem packages in 5000");
            for (item, _) in gems {
                assert!(allowed.contains(&item), "level {level}: {item}");
            }
        }
        // Re-grading is downward only, and leaves a material alone.
        let greater = HashMap::from([(uuid(ALL_SOUL_GEMS[5]), 4)]);
        assert_eq!(regrade_soul_gems(&greater, 66, 43), HashMap::from([(uuid(ALL_SOUL_GEMS[3]), 4)]));
        assert_eq!(regrade_soul_gems(&greater, 66, 80), greater);
        let lumber = HashMap::from([(uuid("f2ba4bb2-6a4a-4b4b-9d8e-2c2d4c5e6f70"), 9)]);
        assert_eq!(regrade_soul_gems(&lumber, 66, 3), lumber);
    }

    /// Report #371: "a Grand one before a golden chest, and one after". At levels
    /// 39-59 the rungs either side of the golden chest (190 and 490) can pay one
    /// Grand Soul Gem — one more result in the pool, so not every run — and the
    /// other rungs, and every other level, are untouched.
    #[test]
    fn a_grand_gem_can_come_beside_the_golden_chest_at_mid_levels() {
        let grand = uuid(ALL_SOUL_GEMS[GRAND]);
        assert_eq!(corpus().rungs.iter().find(|r| r.score == 360).map(|r| r.tier), Some(Some(3)));
        for level in [39u64, 47, 58] {
            for rung in RUNGS_BESIDE_THE_GOLDEN_CHEST {
                let mut hits = 0;
                for seed in 0..3_000i64 {
                    let g = future_reward_for_rung(rung, level, seed).unwrap();
                    if g.stackable_items.get(&grand) == Some(&1) {
                        assert_eq!(g.stackable_items.len(), 1);
                        hits += 1;
                    }
                }
                assert!((700..=1300).contains(&hits), "rung {rung} level {level}: Grand on {hits}/3000");
            }
        }
        for (rung, level) in [(190u32, 38u64), (490, 60), (490, 100), (135, 47), (650, 47)] {
            for seed in 0..1_000i64 {
                let g = future_reward_for_rung(rung, level, seed).unwrap();
                assert!(!g.stackable_items.contains_key(&grand), "rung {rung} level {level} paid Grand");
            }
        }
    }

    /// "The order should be random" (report #371: at level 100 the reporter only
    /// ever saw Glorious, then Transcendent). The display order of one end screen
    /// is the client's: `AbyssRewardsMenu.OnRewardsReady` sorts the rewards by
    /// `ItemRarityManager.GetItemRarityLevel` (`dump.cs`, the `<OnRewardsReady>b__1`
    /// comparer). What the server decides — which stack a run is paid — is a fresh
    /// draw per run, so consecutive runs at level 100 follow no fixed sequence:
    /// every gem kind comes up, and comes up after each of the others.
    #[test]
    fn the_end_gem_kind_follows_no_fixed_sequence_at_level_100() {
        let all_gems = ALL_SOUL_GEMS.map(uuid);
        let sequence: Vec<Uuid> = (0..4_000i64)
            .map(|seed| single_stack(&end_package(100.0, 100, seed)).0)
            .filter(|item| all_gems.contains(item))
            .collect();
        let kinds: std::collections::HashSet<Uuid> = sequence.iter().copied().collect();
        assert_eq!(kinds.len(), 5);
        let pairs: std::collections::HashSet<(Uuid, Uuid)> =
            sequence.windows(2).map(|w| (w[0], w[1])).collect();
        for a in &kinds {
            for b in &kinds {
                assert!(pairs.contains(&(*a, *b)), "{a} is never followed by {b}");
            }
        }
    }

    /// The share of `/end` packages paid to a character of `level` that are an
    /// enchanting ingredient ([`crate::util::dungeon::ENCHANTING_INGREDIENTS`]).
    fn end_enchanting_share(level: u64, runs: i64) -> f64 {
        let enchanting = (0..runs)
            .filter(|seed| {
                let (item, _) = single_stack(&end_package(100.0, level, *seed));
                crate::util::dungeon::ENCHANTING_INGREDIENTS.contains(&item.as_u128())
            })
            .count();
        enchanting as f64 / runs as f64
    }

    /// Report #383: from level 49 `/end` is the top band's, and that band pays an
    /// enchanting ingredient (Glow Dust, Fire Salts, Imp Stool) on 5 of its 13 retail
    /// packages — the 10 captured, which had 5, plus the three Transcendent ×4 end
    /// screens. Levels 49, 70 and 100 all draw it.
    #[test]
    fn from_level_49_end_sometimes_pays_an_enchanting_ingredient() {
        for level in [49u64, 59, 70, 100] {
            let share = end_enchanting_share(level, 20_000);
            println!("/end enchanting share, level {level}: {share:.4}");
            assert!((0.35..=0.42).contains(&share), "level {level}: {share}, retail 5/13");
        }
    }

    /// The three #383 end screens are each producible, whole, by one run of a
    /// character of the screenshot's level: the rung-35 stack, the rung-50 Wooden
    /// Chest at the character's level, the rung-70 Nightshade ×30 where the screen
    /// shows it, and Transcendent ×4 at `/end`.
    #[test]
    fn the_383_end_screens_are_producible_at_their_level() {
        let transcendent = HashMap::from([(uuid(TRANSCENDENT_SOUL_GEM), 4)]);
        let screens: [(u64, f64, &str, u64, Option<(&str, u64)>); 3] = [
            (92, 90.0, FROST_SALTS, 6, None),
            (95, 90.0, IMP_STOOL, 6, Some((NIGHTSHADE, 30))),
            (82, 60.0, HONEYCOMB, 5, None),
        ];
        for (level, score, item, count, seventy) in screens {
            let hit = (0..200_000i64).find(|seed| {
                let rung35 = future_reward_for_rung(35, level, *seed).unwrap();
                let chest = future_reward_for_rung(50, level, *seed).unwrap();
                let rung70 = future_reward_for_rung(70, level, *seed).unwrap();
                rung35.stackable_items == HashMap::from([(uuid(item), count)])
                    && chest.chests.iter().any(|c| (c.tier, c.level) == (1, level))
                    && seventy.is_none_or(|(i, n)| rung70.stackable_items == HashMap::from([(uuid(i), n)]))
                    && end_package(score, level, *seed).stackable_items == transcendent
            });
            assert!(hit.is_some(), "level {level}: the retail end screen is never produced");
        }
        // Frost Salts is new to rung 35, and like Imp Stool and Honeycomb it is a
        // top-band result: the level 20-48 band does not draw it.
        for item in [FROST_SALTS, IMP_STOOL, HONEYCOMB] {
            let at = |level: u64| {
                (0..2_000i64).any(|seed| single_stack(&future_reward_for_rung(35, level, seed).unwrap()).0 == uuid(item))
            };
            assert!(at(49) && at(100), "{item} unreachable at rung 35 from level 49");
        }
        assert!(!(0..2_000i64).any(|seed| {
            single_stack(&future_reward_for_rung(35, 45, seed).unwrap()).0 == uuid(FROST_SALTS)
        }));
    }

    /// No package below the threshold; the same run always pays the same package.
    #[test]
    fn the_end_package_needs_a_score_and_is_stable_per_run() {
        assert_eq!(end_package(0.0, 100, 1), RewardGrant::default());
        assert_eq!(end_package(END_PACKAGE_MIN_SCORE - 0.01, 100, 1), RewardGrant::default());
        assert_ne!(end_package(END_PACKAGE_MIN_SCORE, 100, 1), RewardGrant::default());
        for seed in [1i64, -7, i64::MAX] {
            assert_eq!(end_package(50.0, 100, seed), end_package(50.0, 100, seed));
        }
    }
}
