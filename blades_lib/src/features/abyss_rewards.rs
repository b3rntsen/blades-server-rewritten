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
//! THE CORPUS IS THIN, and says so. 71 distinct observations across ten rungs,
//! against 49,602 for enemy loot; rungs 190, 260, 490 and 650 rest on fewer than
//! five each, and rung 650 on a single distinct result. That is weak data. It is
//! still strictly better than advertising rung 1 of 10 forever, and every rung
//! carries its own observation count so nobody has to guess how thin it is.

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
struct Corpus {
    rungs: Vec<Rung>,
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
    #[serde(default)]
    results: Vec<DrawnResult>,
}

#[derive(Deserialize)]
struct DrawnResult {
    reward: ObservedReward,
    n: u64,
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
            .unwrap_or_else(|_| Corpus { rungs: Vec::new() })
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

    let total: u64 = rung.results.iter().map(|r| r.n).sum();
    if total == 0 {
        // A rung we somehow hold no observation for advertises nothing rather
        // than inventing a reward. The score still shows, which is what the
        // player is climbing towards.
        return grant;
    }

    let seed = mix((run_seed as u64) ^ (u64::from(rung.score)).rotate_left(29));
    let mut pick = seed % total;
    let drawn = rung
        .results
        .iter()
        .find(|r| {
            if pick < r.n {
                true
            } else {
                pick -= r.n;
                false
            }
        })
        .unwrap_or(&rung.results[rung.results.len() - 1]);

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
        assert!(rung_observations(35) >= 21, "rung 35 had 21 observations");
        assert!(rung_observations(650) >= 2, "rung 650 had 2 observations");
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
}
