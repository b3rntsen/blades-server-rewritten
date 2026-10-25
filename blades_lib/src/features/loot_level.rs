//! The level a character's LOOT is rolled at (report #398).
//!
//! HauDrauf (#398, 2026-10-16), who played retail to the end: "From level 49, is
//! everything at the highest level? From then on there should be no difference to
//! level 100." His standing rule for the server: Blades is effectively finished at
//! 45 and complete by 50, many players stopped at 49, so everything must be
//! obtainable by 49.
//!
//! So from [`TOP_LOOT_FROM_LEVEL`] every level-keyed loot and reward source rolls as
//! for a level-[`TOP_LOOT_LEVEL`] character: treasury and store chests, enemy and
//! container loot, event rewards, job rewards, daily rewards and the Abyss. Only
//! rewards move. Character stats, experience, enemy levels and difficulty, and gear
//! required-level checks still read the real level.
//!
//! Below 49 nothing changes: [`effective_loot_level`] is the identity there.
//!
//! One exception, also HauDrauf's: EVENT soul gems at 49-60 stay Grand
//! ([`EVENT_GRAND_SOUL_GEM_LEVELS`], [`cap_event_soul_gems`]).

use std::collections::HashMap;
use std::ops::RangeInclusive;

use uuid::Uuid;

use crate::static_data::QuestLevelScaling;
use crate::user_data::{DungeonGeneratedData, LootTableResult};

/// From this character level on, loot is rolled as for [`TOP_LOOT_LEVEL`] (#398).
pub const TOP_LOOT_FROM_LEVEL: i64 = 49;

/// The level a character at or above [`TOP_LOOT_FROM_LEVEL`] is paid as.
pub const TOP_LOOT_LEVEL: i64 = 100;

/// The character level a loot or reward generator is given for a character of
/// `level`: `level` itself below 49, 100 from 49 on.
///
/// HauDrauf, #398: "From level 49 ... there should be no difference to level 100."
pub fn effective_loot_level(level: i64) -> i64 {
    if level >= TOP_LOOT_FROM_LEVEL {
        TOP_LOOT_LEVEL.max(level)
    } else {
        level
    }
}

/// [`effective_loot_level`] for an unsigned level.
pub fn effective_loot_level_u64(level: u64) -> u64 {
    effective_loot_level(i64::try_from(level).unwrap_or(i64::MAX)) as u64
}

/// [`effective_loot_level`] for a `u16` character level (the character row's type).
pub fn effective_loot_level_u16(level: u16) -> u16 {
    effective_loot_level(i64::from(level)).min(i64::from(u16::MAX)) as u16
}

/// How many levels above `dungeon_level` the loot of a dungeon minted for a
/// character of `character_level` is rolled: the gap between the dungeon level a
/// level-100 character is served ([`QuestLevelScaling::enemy_level`]) and this
/// dungeon's own. Zero below level 49, and never negative, so a dungeon that
/// already stands above the level-100 one keeps its own (better) loot.
///
/// Enemies keep their real level and experience; only what they and the
/// dungeon's containers and chests hold moves. A story quest or event served to a
/// level-49 character stands at enemy level 47 and a level-100 one at 78, so its
/// loot is rolled 31 levels up, at 78 — exactly the level-100 roll, delta for
/// delta (an event boss at 47 + 6 drops what one at 78 + 6 does).
pub fn dungeon_loot_shift(
    scaling: &QuestLevelScaling,
    character_level: i64,
    dungeon_level: i64,
) -> i64 {
    let paid_as = effective_loot_level(character_level);
    if paid_as == character_level {
        return 0;
    }
    (scaling.enemy_level(paid_as) - dungeon_level).max(0)
}

/// The character levels whose EVENT soul gems are Grand, never better (#398).
///
/// HauDrauf (#398, 2026-10-16), after level 49 was made to pay as level 100:
/// "At the end of events you get Grand soul gems instead of Glorious. Within the
/// events themselves you get Grand from level 49. Otherwise there were no big
/// differences between 49 and 60." (translated from German: "mächtige" instead of
/// "herrliche"). It matches the retail captures, which saw events pay Grand at
/// 56-58 and Glorious from 61 (`event_material_ladders.json`).
///
/// So an event's milestones, final reward and dungeon (corpse and container loot)
/// pay a 49-60 character level 100's rewards with every soul gem above Grand
/// (Glorious, Transcendent) paid as Grand, same count. Nothing else moves: not the
/// rest of the event's rewards, not other sources, not 61+.
pub const EVENT_GRAND_SOUL_GEM_LEVELS: RangeInclusive<i64> = TOP_LOOT_FROM_LEVEL..=60;

/// Grand Soul Gem (`Items.Name.SoulGem8`).
pub const GRAND_SOUL_GEM: Uuid = Uuid::from_u128(0x68d7941e_8c8d_47bf_9f66_becb058f1817);

/// The soul gems above Grand: Glorious (`SoulGem9`) and Transcendent (`SoulGem10`).
pub const SOUL_GEMS_ABOVE_GRAND: [Uuid; 2] = [
    Uuid::from_u128(0xbafe6ed5_6473_4a4c_aef5_421d3af5c8cb),
    Uuid::from_u128(0xd94bab85_53d5_4c9c_a637_acd94fc66c98),
];

/// Pay every soul gem above Grand in an EVENT reward's `stacks` as Grand, same
/// count, for a character of `character_level` in [`EVENT_GRAND_SOUL_GEM_LEVELS`].
/// A no-op at every other level.
pub fn cap_event_soul_gems(stacks: &mut HashMap<Uuid, u64>, character_level: i64) {
    if !EVENT_GRAND_SOUL_GEM_LEVELS.contains(&character_level) {
        return;
    }
    let capped: u64 = SOUL_GEMS_ABOVE_GRAND
        .iter()
        .filter_map(|gem| stacks.remove(gem))
        .sum();
    if capped > 0 {
        *stacks.entry(GRAND_SOUL_GEM).or_insert(0) += capped;
    }
}

/// [`cap_event_soul_gems`] over an event dungeon's corpse and container loot.
/// Chests are rolled when opened, from the chest tables every source shares, and
/// are left alone.
pub fn cap_event_dungeon_soul_gems(data: &mut DungeonGeneratedData, character_level: i64) {
    if !EVENT_GRAND_SOUL_GEM_LEVELS.contains(&character_level) {
        return;
    }
    let mut cap = |loot: &mut LootTableResult| {
        cap_event_soul_gems(&mut loot.stackable_items, character_level)
    };
    for enemy in data.enemy_generated_data.values_mut().flatten().flatten() {
        cap(&mut enemy.spawn_group_loot);
        enemy.loot_table_loot.values_mut().for_each(&mut cap);
    }
    for item in data.item_generated_data.values_mut().flatten() {
        item.loot_table_loot.values_mut().for_each(&mut cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_49_is_the_identity_and_from_49_it_is_100() {
        for level in -1..49 {
            assert_eq!(effective_loot_level(level), level);
        }
        for level in 49..=100 {
            assert_eq!(effective_loot_level(level), 100, "level {level}");
        }
        assert_eq!(effective_loot_level_u64(48), 48);
        assert_eq!(effective_loot_level_u64(49), 100);
        assert_eq!(effective_loot_level_u16(60), 100);
        assert_eq!(effective_loot_level_u16(1), 1);
    }

    #[test]
    fn no_dungeon_moves_below_49() {
        let scaling = QuestLevelScaling::default();
        for level in 1..49 {
            assert_eq!(
                dungeon_loot_shift(&scaling, level, scaling.enemy_level(level)),
                0
            );
        }
    }

    /// HauDrauf #398: Glorious and Transcendent are paid as Grand at 49-60 only.
    #[test]
    fn event_soul_gems_above_grand_are_grand_at_49_to_60_only() {
        let [glorious, transcendent] = SOUL_GEMS_ABOVE_GRAND;
        let exceptional: Uuid = "3932e499-441e-4c6d-b671-9a03131ebe6f".parse().unwrap();
        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();
        let stacks = HashMap::from([
            (glorious, 5),
            (transcendent, 2),
            (GRAND_SOUL_GEM, 1),
            (exceptional, 3),
            (lumber, 9),
        ]);
        for level in [49, 55, 60] {
            let mut s = stacks.clone();
            cap_event_soul_gems(&mut s, level);
            assert_eq!(
                s,
                HashMap::from([(GRAND_SOUL_GEM, 8), (exceptional, 3), (lumber, 9)]),
                "level {level}"
            );
        }
        for level in [1, 48, 61, 100] {
            let mut s = stacks.clone();
            cap_event_soul_gems(&mut s, level);
            assert_eq!(s, stacks, "level {level}");
        }
        assert_eq!(
            GRAND_SOUL_GEM.to_string(),
            "68d7941e-8c8d-47bf-9f66-becb058f1817"
        );
        assert_eq!(glorious.to_string(), "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb");
        assert_eq!(
            transcendent.to_string(),
            crate::features::abyss_rewards::TRANSCENDENT_SOUL_GEM
        );
    }
}
