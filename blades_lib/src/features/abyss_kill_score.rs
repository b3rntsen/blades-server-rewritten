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
//! higher liches. Nothing on the wire names the variant a kill was, so it is resolved per
//! floor dungeon — see `script/build_abyss_kill_multipliers.py`, which also explains why
//! the table holds the LOWEST multiplier a floor's candidate variants have.

use std::collections::HashMap;

use serde::Deserialize;
use uuid::Uuid;

static RAW: &str = include_str!("../abyss_kill_multipliers.json");

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

/// The kill-score multiplier for an enemy of `enemy_level` on a floor of dungeon
/// `dungeon_settings_id`. `None` for a dungeon the table does not hold.
pub fn floor_multiplier(dungeon_settings_id: Uuid, enemy_level: u32) -> Option<f64> {
    let steps = &table().dungeons.get(&dungeon_settings_id)?.steps;
    steps
        .iter()
        .take_while(|(from, _)| *from <= enemy_level)
        .last()
        .or_else(|| steps.first())
        .map(|(_, multiplier)| *multiplier)
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
    fn an_unknown_dungeon_has_no_multiplier() {
        assert_eq!(floor_multiplier(Uuid::nil(), 10), None);
    }
}
