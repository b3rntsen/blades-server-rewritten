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

use std::collections::HashMap;

use uuid::Uuid;

use crate::static_data::QuestLevelScaling;

/// The gem materials (`Items.Name.*`, type 8): the jewellery and enchanting gems.
/// Soul gems are not among them.
pub const GEM_MATERIALS: [Uuid; 12] = [
    Uuid::from_u128(0x89ece62c_9ff6_470a_a152_d45ef4e0c222), // Pearl
    Uuid::from_u128(0x3ca59113_a093_4cc8_8389_0f578ad94851), // Topaz
    Uuid::from_u128(0x9df9233f_e7b9_47e8_bc75_f37707917759), // Garnet
    Uuid::from_u128(0x3ec6cf6f_d90e_4b76_bb7f_82da251ab5e5), // Amethyst
    Uuid::from_u128(0xcf4b1b42_a736_4aa1_99b8_baca9f9c2276), // Ruby
    Uuid::from_u128(0x014606cd_8898_4c1d_8029_63220a179c47), // Sapphire
    Uuid::from_u128(0x4d1231be_d3fa_4282_81b2_0f7bed4aabff), // Emerald
    Uuid::from_u128(0x16e102fb_b1c0_42de_8106_0aa27e77f7f0), // Diamond
    Uuid::from_u128(0xab97efe1_bae9_4d16_8fd9_e05bad9ecb95), // Atronite
    Uuid::from_u128(0x70f2c013_813e_4f63_8b29_7a54a13b5e58), // Faerite
    Uuid::from_u128(0xc1abc963_c03c_4cd9_84d7_433ed8ab3877), // Arentine
    Uuid::from_u128(0x91c40a5d_36b8_4ed9_9c07_209b52ebc732), // Firebreath Opal
];

pub fn is_gem_material(id: &Uuid) -> bool {
    GEM_MATERIALS.contains(id)
}

/// Whether a character of `level` is paid level 100's loot PLUS the gem materials
/// its own level would have paid: 49-99 (#398).
///
/// Retail's level-100 tables pay almost no gems (a level-100 Gold chest none, where
/// one at 49 paid ~1.1 Emerald or Diamond). Paying 49-99 exactly as 100 would take
/// those gems away, and HauDrauf's standing rule is that no reward is narrowed
/// without asking him. So for these levels the gems of the character's own roll are
/// kept on top of the level-100 roll (better of the two, per gem).
pub fn keeps_own_gems(level: i64) -> bool {
    (TOP_LOOT_FROM_LEVEL..TOP_LOOT_LEVEL).contains(&level)
}

/// Add to `paid` every gem material `own` pays more of: per gem, the larger count.
/// Nothing else in `paid` changes.
pub fn keep_own_gems(paid: &mut HashMap<Uuid, u64>, own: &HashMap<Uuid, u64>) {
    for (id, n) in own {
        if is_gem_material(id) {
            let p = paid.entry(*id).or_insert(0);
            *p = (*p).max(*n);
        }
    }
}

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
