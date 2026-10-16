//! The beginner economy (tracker #388/#389/#390).
//!
//! OWNER-APPROVED DEVIATION FROM RETAIL, #388, 2026-10-16. HauDrauf proposed a
//! package to help new and returning players rebuild their characters, and the
//! owner approved it as a deliberate departure from retail's numbers. Every number
//! the package sets is a named constant here, so each one can be tuned or reverted
//! on its own.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use uuid::Uuid;

use crate::economy::{RewardGrant, GEMS};
use crate::features::store_bundles::mix;

// ---------------------------------------------------------------------------
// 1. Containers: loot stacks per breakable
// ---------------------------------------------------------------------------

/// The small breakable's loot table (`Breakable_T1`, urns and small barrels).
pub const SMALL_CONTAINER_TABLE: Uuid = Uuid::from_u128(0x7ee92f4b_18e7_4d15_91c6_ed0436180b81);

/// The medium breakable's loot table (`Breakable_T2`).
pub const MEDIUM_CONTAINER_TABLE: Uuid = Uuid::from_u128(0x49173c2c_e34c_4664_b2cd_d1bae46c39fa);

/// The large breakable's loot table (`Breakable_T3`).
pub const LARGE_CONTAINER_TABLE: Uuid = Uuid::from_u128(0x6cb97b90_6ab2_4ddc_bc81_4f7db6db3e7f);

/// The job dungeon's one breakable table (`JobSpawnGroupsReference`). Its size
/// class is in the spawn group's name instead: "... common rarity" (small),
/// "... uncommon rarity" (medium), "... rare rarity" (large).
pub const JOB_CONTAINER_TABLE: Uuid = Uuid::from_u128(0x37ef9ab6_d7e3_454f_b9a6_e244c8d08657);

/// Loot stacks in a small container.
///
/// Owner-approved deviation from retail, #388, 2026-10-16. Retail's small
/// breakable held 0-1 stacks four times in five.
pub const SMALL_CONTAINER_STACKS: usize = 2;

/// Loot stacks in a medium container.
///
/// Owner-approved deviation from retail, #388, 2026-10-16. Retail's held mostly 2-4.
pub const MEDIUM_CONTAINER_STACKS: usize = 4;

/// Loot stacks in a large container.
///
/// Owner-approved deviation from retail, #388, 2026-10-16. Retail's held mostly 4-6.
pub const LARGE_CONTAINER_STACKS: usize = 6;

/// How many further draws of a container's table may be made to reach its stack
/// count. A table whose level band holds fewer distinct stacks stops short.
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub const CONTAINER_FILL_MAX_DRAWS: usize = 64;

/// A container's size class: the loot stacks it holds and the breakable table
/// its further stacks are drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerClass {
    /// [`SMALL_CONTAINER_STACKS`], [`MEDIUM_CONTAINER_STACKS`] or
    /// [`LARGE_CONTAINER_STACKS`].
    pub stacks: usize,
    /// [`SMALL_CONTAINER_TABLE`], [`MEDIUM_CONTAINER_TABLE`] or
    /// [`LARGE_CONTAINER_TABLE`]. A job's own table pays one kind of stack only,
    /// so its breakables are topped up from the dungeon breakable of their size.
    pub fill_table: Uuid,
}

/// The size class of a container rolling `table` in the spawn group named
/// `group_name`; `None` for a table that is not a breakable's (a key table, a pot
/// of Lumber), which keeps retail's draw.
///
/// Owner-approved deviation from retail, #388, 2026-10-16: "small containers hold
/// 2 items, medium 4, large 6", in event and quest/job dungeons.
pub fn container_class(table: &Uuid, group_name: Option<&str>) -> Option<ContainerClass> {
    let small = ContainerClass { stacks: SMALL_CONTAINER_STACKS, fill_table: SMALL_CONTAINER_TABLE };
    let medium = ContainerClass { stacks: MEDIUM_CONTAINER_STACKS, fill_table: MEDIUM_CONTAINER_TABLE };
    let large = ContainerClass { stacks: LARGE_CONTAINER_STACKS, fill_table: LARGE_CONTAINER_TABLE };
    match *table {
        SMALL_CONTAINER_TABLE => Some(small),
        MEDIUM_CONTAINER_TABLE => Some(medium),
        LARGE_CONTAINER_TABLE => Some(large),
        JOB_CONTAINER_TABLE => {
            let name = group_name?.to_ascii_lowercase();
            if name.contains("uncommon rarity") {
                Some(medium)
            } else if name.contains("common rarity") {
                Some(small)
            } else if name.contains("rare rarity") {
                Some(large)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The number of loot stacks a container of [`container_class`] holds.
pub fn container_stacks(table: &Uuid, group_name: Option<&str>) -> Option<usize> {
    container_class(table, group_name).map(|c| c.stacks)
}

// ---------------------------------------------------------------------------
// 2. Gems (the green premium currency)
// ---------------------------------------------------------------------------

/// Percent added to every gem reward: quests, jobs, events, dungeon corpses and
/// containers (so the Abyss's too). Chests use [`WOODEN_CHEST_GEMS`] and
/// [`SILVER_CHEST_GEMS`] instead; Gold, Elder and Legendary chests paid none and
/// still pay none. Arena rewards and challenges are not touched here.
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub const GEM_BONUS_PERCENT: u64 = 20;

/// Gems a Wooden chest pays, every opening, evenly drawn (retail paid 1-2).
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub const WOODEN_CHEST_GEMS: RangeInclusive<u64> = 3..=5;

/// Gems a Silver chest pays, every opening, evenly drawn (retail paid 0, 2 or 3).
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub const SILVER_CHEST_GEMS: RangeInclusive<u64> = 6..=8;

/// A fixed gem reward with [`GEM_BONUS_PERCENT`] added, ROUNDED UP so every
/// reward visibly grows: 4 -> 5, 12 -> 15, 36 -> 44.
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub fn with_gem_bonus(gems: u64) -> u64 {
    (gems * (100 + GEM_BONUS_PERCENT)).div_ceil(100)
}

/// A rolled gem drop with [`GEM_BONUS_PERCENT`] added, the fraction paid as one
/// more gem that often: a corpse's or container's single gem is 2 one time in
/// five, so drops pay exactly 20% more on average (rounding up would double them).
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub fn with_gem_bonus_drawn(gems: u64, seed: u64) -> u64 {
    let scaled = gems * (100 + GEM_BONUS_PERCENT);
    scaled / 100 + u64::from(mix(seed) % 100 < scaled % 100)
}

/// [`with_gem_bonus`] on a reward's gems, wherever it carries them: `currencies`,
/// or `stackableItems`, where an event's ladder lists them.
pub fn add_gem_bonus(reward: &mut RewardGrant) {
    for stacks in [&mut reward.currencies, &mut reward.stackable_items] {
        if let Some(gems) = stacks.get_mut(&GEMS) {
            *gems = with_gem_bonus(*gems);
        }
    }
}

/// [`with_gem_bonus_drawn`] on a dungeon loot result's gems.
pub fn add_drawn_gem_bonus(stacks: &mut HashMap<Uuid, u64>, seed: u64) {
    if let Some(gems) = stacks.get_mut(&GEMS) {
        *gems = with_gem_bonus_drawn(*gems, seed);
    }
}

/// An event's milestones (`rewards[]`) as minted for a character of `level`: gems
/// +20% ([`add_gem_bonus`]).
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub fn event_rewards(tiers: &mut [RewardGrant], _level: i64) {
    for tier in tiers.iter_mut() {
        add_gem_bonus(tier);
    }
}

/// An event's `finalReward` as minted for a character of `level`: gems +20%
/// ([`add_gem_bonus`]).
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub fn event_final_reward(reward: &mut RewardGrant, _level: i64) {
    add_gem_bonus(reward);
}

/// The gems a treasury chest of `tier` pays: [`WOODEN_CHEST_GEMS`] for tier 1,
/// [`SILVER_CHEST_GEMS`] for tier 2, drawn from `seed`; `None` for any other tier.
pub fn chest_gems(tier: u64, seed: u64) -> Option<u64> {
    let range = match tier {
        1 => WOODEN_CHEST_GEMS,
        2 => SILVER_CHEST_GEMS,
        _ => return None,
    };
    Some(range.start() + mix(seed) % (range.end() - range.start() + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_classes_are_2_4_6() {
        assert_eq!(container_stacks(&SMALL_CONTAINER_TABLE, None), Some(2));
        assert_eq!(container_stacks(&MEDIUM_CONTAINER_TABLE, Some("Breakable_T2")), Some(4));
        assert_eq!(container_stacks(&LARGE_CONTAINER_TABLE, None), Some(6));
        assert_eq!(container_stacks(&JOB_CONTAINER_TABLE, Some("Not in secret, common rarity")), Some(2));
        assert_eq!(container_stacks(&JOB_CONTAINER_TABLE, Some("In secret, uncommon rarity")), Some(4));
        assert_eq!(container_stacks(&JOB_CONTAINER_TABLE, Some("In secret, rare rarity")), Some(6));
        assert_eq!(container_stacks(&JOB_CONTAINER_TABLE, None), None);
        assert_eq!(container_stacks(&Uuid::nil(), Some("Breakable_T1")), None);
        let job = container_class(&JOB_CONTAINER_TABLE, Some("In secret, rare rarity")).unwrap();
        assert_eq!(job.fill_table, LARGE_CONTAINER_TABLE);
    }

    #[test]
    fn gems_pay_20_percent_more() {
        for (before, after) in [(0, 0), (1, 2), (2, 3), (4, 5), (5, 6), (9, 11), (10, 12), (12, 15), (15, 18), (36, 44), (50, 60)] {
            assert_eq!(with_gem_bonus(before), after, "{before}");
        }
        // Drawn: a single gem pays 1.2 on average, never less than 1 nor more than 2.
        let paid: Vec<u64> = (0..10_000).map(|s| with_gem_bonus_drawn(1, s)).collect();
        assert!(paid.iter().all(|g| (1..=2).contains(g)));
        let mean = paid.iter().sum::<u64>() as f64 / paid.len() as f64;
        assert!((mean - 1.2).abs() < 0.02, "{mean}");
        assert_eq!(with_gem_bonus_drawn(5, 7), 6, "an exact 20% is never rounded");
        let mut r = RewardGrant::default();
        r.currencies.insert(GEMS, 4);
        r.stackable_items.insert(GEMS, 36);
        add_gem_bonus(&mut r);
        assert_eq!((r.currencies[&GEMS], r.stackable_items[&GEMS]), (5, 44));
    }

    #[test]
    fn wooden_chests_pay_3_to_5_gems_and_silver_6_to_8() {
        for (tier, range) in [(1, 3..=5), (2, 6..=8)] {
            let seen: std::collections::BTreeSet<u64> = (0..300).filter_map(|s| chest_gems(tier, s)).collect();
            assert_eq!(seen, range.collect(), "tier {tier}");
        }
        assert_eq!(chest_gems(3, 1), None);
    }
}
