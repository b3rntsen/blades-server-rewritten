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

use crate::static_data::QuestLevelScaling;

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
pub fn dungeon_loot_shift(scaling: &QuestLevelScaling, character_level: i64, dungeon_level: i64) -> i64 {
    let paid_as = effective_loot_level(character_level);
    if paid_as == character_level {
        return 0;
    }
    (scaling.enemy_level(paid_as) - dungeon_level).max(0)
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
            assert_eq!(dungeon_loot_shift(&scaling, level, scaling.enemy_level(level)), 0);
        }
    }
}
