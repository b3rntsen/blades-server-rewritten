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
//! gold and XP — a material, or a stack of soul gems — see [`end_package`].

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
pub const LEVEL_BANDS: [std::ops::RangeInclusive<u64>; 3] = [0..=19, 20..=59, 60..=u64::MAX];

fn band(level: u64) -> usize {
    LEVEL_BANDS
        .iter()
        .position(|b| b.contains(&level))
        .unwrap_or(LEVEL_BANDS.len() - 1)
}

/// The observations a character of `level` draws from: those in its own band, or —
/// for a rung no retail run in that band reached — the band of the observation
/// nearest in level. Never empty unless `observations` is.
fn level_pool<T>(observations: &[T], level: u64, level_of: impl Fn(&T) -> u64) -> Vec<&T> {
    let own = band(level);
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
/// in 10 runs; below level 60 retail paid none in 6. Seeded from the run, so a
/// retried `/end` pays the same thing. Gear, which retail added to 3 of the 4
/// runs that passed rung 360, is not modelled here.
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
    let pool = level_pool(&paid, character_level, |o| o.level);
    if pool.is_empty() {
        return grant;
    }
    let seed = mix((run_seed as u64) ^ 0xE4D0_A55E_5EED_0294);
    if let Some(package) = &pool[(seed % pool.len() as u64) as usize].package {
        grant.stackable_items = package.stackable_items.clone();
    }
    grant
}

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

    let pool = level_pool(&rung.results, character_level, |r| r.level);
    if pool.is_empty() {
        // A rung we somehow hold no observation for advertises nothing rather
        // than inventing a reward. The score still shows, which is what the
        // player is climbing towards.
        return grant;
    }

    let seed = mix((run_seed as u64) ^ (u64::from(rung.score)).rotate_left(29));
    let drawn = pool[(seed % pool.len() as u64) as usize];

    grant.stackable_items = drawn.reward.stackable_items.clone();
    for (ordinal, item) in drawn.reward.items.iter().enumerate() {
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
    /// paid packages were a soul-gem stack of 4-6 (Greater, Grand, Elevated,
    /// Glorious x2); below level 60, 0 of 6.
    #[test]
    fn the_end_soul_gem_roll_matches_retails_frequency_and_range() {
        let paid_kinds = SOUL_GEMS_PAID.map(uuid);
        let all_gems = ALL_SOUL_GEMS.map(uuid);
        let runs = 20_000;
        let mut gems = 0;
        let mut kinds = std::collections::HashSet::new();
        for seed in 0..runs {
            let grant = end_package(100.0, 100, seed);
            let (item, count) = single_stack(&grant);
            if all_gems.contains(&item) {
                gems += 1;
                assert!(paid_kinds.contains(&item), "seed {seed}: a gem retail never paid: {item}");
                assert!((4..=6).contains(&count), "seed {seed}: {count} gems, retail paid 4-6");
                kinds.insert(item);
            }
        }
        let rate = f64::from(gems) / runs as f64;
        assert!((0.4..=0.6).contains(&rate), "soul gems on {rate:.3} of level-100 /ends, retail 5/10");
        assert_eq!(kinds.len(), 4, "all four retail gem kinds come up");

        for level in [3u64, 7, 34, 57] {
            for seed in 0..2_000i64 {
                let (item, _) = single_stack(&end_package(100.0, level, seed));
                assert!(!all_gems.contains(&item), "level {level} seed {seed}: retail paid no gems below 60");
            }
        }
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
