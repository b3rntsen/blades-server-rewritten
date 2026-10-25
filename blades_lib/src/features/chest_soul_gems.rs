//! Chest soul gems for a level 45-100 opener (tracker #421, #429).
//!
//! OWNER-APPROVED DEVIATION FROM RETAIL. Retail's chests at these levels paid
//! mostly Grand in Gold chests (about 70%, fork #556) and only Glorious and
//! Transcendent in Elder and Legendary ones. HauDrauf's rule, chosen instead:
//!
//! - #421 (decision d21, 2026-10-25), Silver and Gold chests.
//! - #429 (2026-10-25, the owner's standing rule for HauDrauf's reversible
//!   common-good rules), Elder and Legendary chests: each enchant needs 4 Grand,
//!   2 Glorious and 1 Transcendent, so Grand is the bottleneck.
//!
//! For every one of these chests:
//!
//! - HOW OFTEN it holds soul gems is unchanged: the chest is rolled as before,
//!   and only a chest that already holds soul gems is touched.
//! - WHICH soul gem: Grand 60%, Glorious 20%, Transcendent 20% ([`GRADES`]).
//! - HOW MANY ([`counts`]): 1-2 in a Silver chest and 3-7 in a Gold chest, one
//!   stack a chest; an Elder or Legendary chest keeps the count of every stack
//!   it rolled, and each stack is re-graded on its own.
//!
//! Wooden chests, and every opener below level 45, are left as rolled.
//!
//! To revert all of it, empty [`LEVELS`] (or drop the calls to
//! [`pay_chest_soul_gems`]); to revert one tier, return `None` for it from
//! [`counts`].

use std::collections::HashMap;
use std::ops::RangeInclusive;

use uuid::Uuid;

use crate::features::beginner_economy::{
    GLORIOUS_SOUL_GEM, GRAND_SOUL_GEM, SOUL_GEMS, TRANSCENDENT_SOUL_GEM,
};
use crate::features::store_bundles::mix;

/// The opener's character levels the rule applies to (#421, #429).
pub const LEVELS: RangeInclusive<i64> = 45..=100;

/// Silver chest tier.
pub const SILVER: u64 = 2;
/// Gold chest tier.
pub const GOLD: u64 = 3;
/// Elder chest tier.
pub const ELDER: u64 = 4;
/// Legendary chest tier.
pub const LEGENDARY: u64 = 5;

/// The soul gem a chest pays, with its weight in percent (#421, #429).
pub const GRADES: [(Uuid, u64); 3] = [
    (GRAND_SOUL_GEM, 60),
    (GLORIOUS_SOUL_GEM, 20),
    (TRANSCENDENT_SOUL_GEM, 20),
];

/// How many soul gems a chest pays under the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Counts {
    /// One stack of this many, drawn evenly (Silver and Gold, #421).
    OneStack(RangeInclusive<u64>),
    /// Every stack keeps its rolled count; each is re-graded (Elder and
    /// Legendary, #429).
    AsRolled,
}

/// The rule's counts for a chest of `tier`, or `None` for a tier the rule
/// leaves alone (#421, #429).
pub fn counts(tier: u64) -> Option<Counts> {
    match tier {
        SILVER => Some(Counts::OneStack(1..=2)),
        GOLD => Some(Counts::OneStack(3..=7)),
        ELDER | LEGENDARY => Some(Counts::AsRolled),
        _ => None,
    }
}

fn grade(seed: u64) -> Uuid {
    let total: u64 = GRADES.iter().map(|(_, w)| w).sum();
    let mut at = mix(seed) % total;
    GRADES
        .iter()
        .find(|(_, w)| {
            if at < *w {
                true
            } else {
                at -= w;
                false
            }
        })
        .map_or(GRAND_SOUL_GEM, |(k, _)| *k)
}

/// Pay the soul gems of one opened chest of `tier` by the #421/#429 rule for an
/// opener of `level`. A chest without soul gems, a tier without [`counts`], or an
/// opener outside [`LEVELS`] is left as it is. Deterministic in `seed`.
pub fn pay_chest_soul_gems(stacks: &mut HashMap<Uuid, u64>, tier: u64, level: i64, seed: u64) {
    let Some(counts) = counts(tier) else { return };
    if !LEVELS.contains(&level) || !stacks.keys().any(|id| SOUL_GEMS.contains(id)) {
        return;
    }
    let mut rolled: Vec<(Uuid, u64)> = stacks
        .iter()
        .filter(|(id, _)| SOUL_GEMS.contains(id))
        .map(|(id, n)| (*id, *n))
        .collect();
    rolled.sort_unstable();
    stacks.retain(|id, _| !SOUL_GEMS.contains(id));
    match counts {
        Counts::OneStack(range) => {
            let count = range.start()
                + mix(seed ^ 0x0421_C0E7_5C4E_5701) % (range.end() - range.start() + 1);
            stacks.insert(grade(seed ^ 0x0421_6EAD_E5C4_E570), count);
        }
        Counts::AsRolled => {
            for (i, (_, n)) in rolled.into_iter().enumerate() {
                let kind = grade(seed ^ 0x0429_6EAD_E5C4_E570 ^ (i as u64 + 1).rotate_left(29));
                *stacks.entry(kind).or_insert(0) += n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_gem(id: Uuid, n: u64) -> HashMap<Uuid, u64> {
        HashMap::from([(id, n), (crate::economy::GOLD, 500)])
    }

    #[test]
    fn grades_sum_to_one_hundred() {
        assert_eq!(GRADES.iter().map(|(_, w)| w).sum::<u64>(), 100);
    }

    #[test]
    fn a_chest_without_soul_gems_stays_without() {
        for tier in [SILVER, GOLD, ELDER, LEGENDARY] {
            let mut s = HashMap::from([(crate::economy::GOLD, 500u64)]);
            pay_chest_soul_gems(&mut s, tier, 70, 9);
            assert_eq!(s, HashMap::from([(crate::economy::GOLD, 500u64)]));
        }
    }

    #[test]
    fn wooden_chests_and_levels_below_45_are_untouched() {
        for (tier, level) in [
            (1, 70),
            (0, 70),
            (6, 70),
            (SILVER, 44),
            (GOLD, 44),
            (ELDER, 44),
            (LEGENDARY, 44),
            (GOLD, 1),
        ] {
            let mut s = with_gem(SOUL_GEMS[3], 2);
            pay_chest_soul_gems(&mut s, tier, level, 7);
            assert_eq!(s, with_gem(SOUL_GEMS[3], 2), "tier {tier} level {level}");
        }
    }

    /// Silver and Gold: two kinds in, one kind out, nothing else touched.
    #[test]
    fn several_kinds_become_one_stack_in_silver_and_gold() {
        let mut s = HashMap::from([
            (SOUL_GEMS[2], 1u64),
            (SOUL_GEMS[7], 3),
            (crate::economy::GOLD, 500),
        ]);
        pay_chest_soul_gems(&mut s, GOLD, 45, 1);
        assert_eq!(s.keys().filter(|id| SOUL_GEMS.contains(id)).count(), 1);
        assert_eq!(s[&crate::economy::GOLD], 500);
    }

    /// Elder and Legendary: every gem is kept, each stack re-graded.
    #[test]
    fn elder_and_legendary_keep_every_gem() {
        for tier in [ELDER, LEGENDARY] {
            for seed in 0..200 {
                let mut s = HashMap::from([
                    (SOUL_GEMS[8], 2u64),
                    (SOUL_GEMS[9], 1),
                    (crate::economy::GOLD, 500),
                ]);
                pay_chest_soul_gems(&mut s, tier, 70, seed);
                let gems: u64 = s
                    .iter()
                    .filter(|(id, _)| SOUL_GEMS.contains(id))
                    .map(|(_, n)| n)
                    .sum();
                assert_eq!(gems, 3);
                assert!(s.keys().all(|id| !SOUL_GEMS[..7].contains(id)));
                assert_eq!(s[&crate::economy::GOLD], 500);
            }
        }
    }
}
