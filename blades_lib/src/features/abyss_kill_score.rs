//! The Abyss kill-score multiplier for a floor (#312).
//!
//! The client scores a kill as `GetKillScore(levelDelta) * enemy.KillScoreMultiplier`
//! (`AbyssController.NotifyEnemyKill`, RVA 0x1C811D4) and fills its reward gauge from
//! that score alone. The server has to land on the same rungs: the client only moves its
//! gauge on when an `enemy_killed` response carries a `reward`, and it takes the new
//! "previous rung" from the rung it was showing. A server that scores AHEAD of the client
//! pays rungs early, the client's previous rung jumps past its own score, and from then
//! on the gauge never fills and the reward animation never plays — the report.
//!
//! The multiplier is 0.33 on critters, 1.0 on most enemies, 2.0 on dragons and the
//! higher liches. The kill names its enemy's spawn group (`enemy_killed.spawnGroupId`),
//! and the APK says which family each group spawns, so [`spawn_group_multiplier`] is the
//! enemy's own multiplier — see `script/build_abyss_spawn_group_multipliers.py`. Replaying
//! the complete captured retail run through it pays all nine rungs on the kill retail paid
//! them; the per-floor [`floor_multiplier`] (the LOWEST multiplier of a floor's candidate
//! variants, `script/build_abyss_kill_multipliers.py`) pays two of nine on time and runs
//! behind the client on every mixed floor. It stays as the fallback for a group the
//! spawn-group table does not hold.

use std::collections::HashMap;

use serde::Deserialize;
use uuid::Uuid;

static RAW: &str = include_str!("../abyss_kill_multipliers.json");
static RAW_SPAWN_GROUPS: &str = include_str!("../abyss_spawn_group_multipliers.json");

#[derive(Deserialize)]
struct Table {
    dungeons: HashMap<Uuid, Dungeon>,
}

#[derive(Deserialize)]
struct Dungeon {
    /// `[fromEnemyLevel, multiplier]`, ascending; each step holds until the next.
    steps: Vec<(u32, f64)>,
}

fn table() -> &'static Table {
    static TABLE: std::sync::OnceLock<Table> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(RAW).unwrap_or_else(|_| Table {
            dungeons: HashMap::new(),
        })
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpawnGroupTable {
    spawn_groups: HashMap<Uuid, Dungeon>,
}

fn spawn_group_table() -> &'static SpawnGroupTable {
    static TABLE: std::sync::OnceLock<SpawnGroupTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(RAW_SPAWN_GROUPS).unwrap_or_else(|_| SpawnGroupTable {
            spawn_groups: HashMap::new(),
        })
    })
}

fn step_at(steps: &[(u32, f64)], enemy_level: u32) -> Option<f64> {
    steps
        .iter()
        .take_while(|(from, _)| *from <= enemy_level)
        .last()
        .or_else(|| steps.first())
        .map(|(_, multiplier)| *multiplier)
}

/// The kill-score multiplier for an enemy of `enemy_level` on a floor of dungeon
/// `dungeon_settings_id`. `None` for a dungeon the table does not hold.
pub fn floor_multiplier(dungeon_settings_id: Uuid, enemy_level: u32) -> Option<f64> {
    step_at(&table().dungeons.get(&dungeon_settings_id)?.steps, enemy_level)
}

/// The kill-score multiplier of the enemy that spawn group `spawn_group_id` spawns at
/// `enemy_level` — the very number the client multiplies that kill by. `None` for a
/// group the table does not hold (only Abyss dungeons' groups are in it).
pub fn spawn_group_multiplier(spawn_group_id: Uuid, enemy_level: u32) -> Option<f64> {
    step_at(&spawn_group_table().spawn_groups.get(&spawn_group_id)?.steps, enemy_level)
}

/// The multiplier for one kill: the enemy's own, else the floor's, else `None`.
pub fn kill_multiplier(
    spawn_group_id: Option<Uuid>,
    dungeon_settings_id: Uuid,
    enemy_level: u32,
) -> Option<f64> {
    spawn_group_id
        .and_then(|group| spawn_group_multiplier(group, enemy_level))
        .or_else(|| floor_multiplier(dungeon_settings_id, enemy_level))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    #[test]
    fn the_table_loads_every_slice_dungeon() {
        assert_eq!(
            table().dungeons.len(),
            164,
            "168 pool dungeons less the 4 entrances"
        );
    }

    #[test]
    fn a_critter_floor_scores_a_third() {
        // Skeever dungeon on floor 4 of a captured retail run (level-4 enemies).
        assert_eq!(
            floor_multiplier(id("fe22c3c8-e4c9-491c-bd85-7c0ba9dc6b31"), 4),
            Some(0.33)
        );
    }

    #[test]
    fn an_ordinary_floor_scores_one() {
        // Goblin dungeon, floor 1 of the same run.
        assert_eq!(
            floor_multiplier(id("663053f0-3a46-4012-b004-6cb2e907f33c"), 1),
            Some(1.0)
        );
    }

    #[test]
    fn a_dragon_floor_scores_double() {
        let dragon = table()
            .dungeons
            .iter()
            .find(|(_, d)| d.steps == vec![(1, 2.0)])
            .map(|(id, _)| *id)
            .expect("a dragon dungeon");
        assert_eq!(floor_multiplier(dragon, 60), Some(2.0));
    }

    #[test]
    fn the_spawn_group_table_holds_every_abyss_group() {
        assert_eq!(spawn_group_table().spawn_groups.len(), 244);
    }

    /// A floor that mixes a critter group with an ordinary one: the per-floor table can
    /// only give the whole floor the critter's 0.33, the spawn group gives each kill its
    /// own enemy's multiplier. Floor 13 of the fixed ladder (and of the captured retail
    /// run), dungeon 0ec5e913 at level 16: a critter-spider group and a full-score one.
    #[test]
    fn a_mixed_floor_scores_each_kill_by_its_own_enemy() {
        let floor = id("0ec5e913-6074-4490-abaa-f1802827b007");
        let critter = id("998b8cd8-75e2-4826-af97-253bd1ad378a");
        let ordinary = id("17fca83f-dfe0-484c-a1e8-6a96c104f621");
        assert_eq!(floor_multiplier(floor, 16), Some(0.33), "the floor table: lowest");
        assert_eq!(kill_multiplier(Some(critter), floor, 16), Some(0.33));
        assert_eq!(kill_multiplier(Some(ordinary), floor, 16), Some(1.0));
    }

    #[test]
    fn an_unknown_spawn_group_falls_back_to_the_floor() {
        let goblin_floor = id("663053f0-3a46-4012-b004-6cb2e907f33c");
        assert_eq!(kill_multiplier(Some(Uuid::nil()), goblin_floor, 1), Some(1.0));
        assert_eq!(kill_multiplier(None, goblin_floor, 1), Some(1.0));
        assert_eq!(kill_multiplier(None, Uuid::nil(), 1), None);
    }

    #[test]
    fn an_unknown_dungeon_has_no_multiplier() {
        assert_eq!(floor_multiplier(Uuid::nil(), 10), None);
    }
}
