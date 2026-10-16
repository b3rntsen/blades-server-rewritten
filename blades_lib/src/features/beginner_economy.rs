//! The beginner economy (tracker #388/#389/#390).
//!
//! OWNER-APPROVED DEVIATION FROM RETAIL, #388, 2026-10-16. HauDrauf proposed a
//! package to help new and returning players rebuild their characters, and the
//! owner approved it as a deliberate departure from retail's numbers. Every number
//! the package sets is a named constant here, so each one can be tuned or reverted
//! on its own.

use uuid::Uuid;

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
}
