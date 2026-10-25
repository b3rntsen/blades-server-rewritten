//! Silver and Gold chest soul gems for a level 45-100 opener (tracker #421).
//!
//! OWNER-APPROVED DEVIATION FROM RETAIL, #421 (decision d21, 2026-10-25). Retail's
//! Gold chests at 61-100 paid Grand about 70%, Glorious and Transcendent about 15%
//! each (fork #556 reproduced that); the owner chose a richer rule instead:
//!
//! - HOW OFTEN a Silver or Gold chest holds soul gems is unchanged (about one in
//!   three): the chest is rolled as before, and only a chest that already holds
//!   soul gems is touched.
//! - WHICH soul gem: Grand 60%, Glorious 20%, Transcendent 20% ([`GRADES`]).
//! - HOW MANY: 1-2 in a Silver chest, 3-7 in a Gold chest ([`count_range`]).
//!
//! Wooden, Elder and Legendary chests, and every opener below level 45, are left
//! as rolled; the owner explicitly declined Elder and Legendary.
//!
//! To revert, drop the calls to [`pay_chest_soul_gems`] (or empty [`LEVELS`]).

use std::collections::HashMap;
use std::ops::RangeInclusive;

use uuid::Uuid;

use crate::features::beginner_economy::{GLORIOUS_SOUL_GEM, GRAND_SOUL_GEM, SOUL_GEMS, TRANSCENDENT_SOUL_GEM};
use crate::features::store_bundles::mix;

/// The opener's character levels the rule applies to (#421).
pub const LEVELS: RangeInclusive<i64> = 45..=100;

/// Silver chest tier.
pub const SILVER: u64 = 2;
/// Gold chest tier.
pub const GOLD: u64 = 3;

/// The soul gem a Silver or Gold chest pays, with its weight in percent (#421).
pub const GRADES: [(Uuid, u64); 3] = [(GRAND_SOUL_GEM, 60), (GLORIOUS_SOUL_GEM, 20), (TRANSCENDENT_SOUL_GEM, 20)];

/// How many soul gems a chest of `tier` pays under the rule, or `None` for a tier
/// the rule leaves alone (#421).
pub fn count_range(tier: u64) -> Option<RangeInclusive<u64>> {
    match tier {
        SILVER => Some(1..=2),
        GOLD => Some(3..=7),
        _ => None,
    }
}

/// Pay the soul gems of one opened chest of `tier` by the #421 rule for an opener
/// of `level`: if `stacks` holds any soul gem, every soul-gem stack is replaced by
/// one stack of a [`GRADES`] kind and a [`count_range`] count. A chest without soul
/// gems, another tier, or an opener outside [`LEVELS`] is left as it is.
/// Deterministic in `seed`.
pub fn pay_chest_soul_gems(stacks: &mut HashMap<Uuid, u64>, tier: u64, level: i64, seed: u64) {
    let Some(counts) = count_range(tier) else { return };
    if !LEVELS.contains(&level) || !stacks.keys().any(|id| SOUL_GEMS.contains(id)) {
        return;
    }
    stacks.retain(|id, _| !SOUL_GEMS.contains(id));
    let total: u64 = GRADES.iter().map(|(_, w)| w).sum();
    let mut at = mix(seed ^ 0x0421_6EAD_E5C4_E570) % total;
    let kind = GRADES
        .iter()
        .find(|(_, w)| if at < *w { true } else { at -= w; false })
        .map_or(GRAND_SOUL_GEM, |(k, _)| *k);
    let count = counts.start() + mix(seed ^ 0x0421_C0E7_5C4E_5701) % (counts.end() - counts.start() + 1);
    stacks.insert(kind, count);
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
        for tier in [SILVER, GOLD] {
            let mut s = HashMap::from([(crate::economy::GOLD, 500u64)]);
            pay_chest_soul_gems(&mut s, tier, 70, 9);
            assert_eq!(s, HashMap::from([(crate::economy::GOLD, 500u64)]));
        }
    }

    #[test]
    fn other_tiers_and_levels_below_45_are_untouched() {
        for (tier, level) in [(1, 70), (4, 70), (5, 100), (SILVER, 44), (GOLD, 44), (GOLD, 1)] {
            let mut s = with_gem(SOUL_GEMS[3], 2);
            pay_chest_soul_gems(&mut s, tier, level, 7);
            assert_eq!(s, with_gem(SOUL_GEMS[3], 2), "tier {tier} level {level}");
        }
    }

    /// Two kinds in, one kind out, never anything else touched.
    #[test]
    fn several_kinds_become_one_stack() {
        let mut s = HashMap::from([(SOUL_GEMS[2], 1u64), (SOUL_GEMS[7], 3), (crate::economy::GOLD, 500)]);
        pay_chest_soul_gems(&mut s, GOLD, 45, 1);
        assert_eq!(s.keys().filter(|id| SOUL_GEMS.contains(id)).count(), 1);
        assert_eq!(s[&crate::economy::GOLD], 500);
    }
}
