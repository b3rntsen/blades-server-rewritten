use std::collections::HashMap;

use thiserror::Error;
use uuid::Uuid;

use crate::{
    game_data::GameData,
    static_data::QuestLevelScaling,
    user_data::{
        DungeonGeneratedData,
        ObjectiveStatus, Quest, QuestStatus, QuestType,
    },
};

#[derive(Error, Debug, Clone)]
pub enum GenerateQuestDataError {
    #[error("quest {0} does not exist")]
    QuestNotFound(Uuid),
    #[error("dungeon {0} does not exist")]
    DungeonNotFound(Uuid),
}

/// Generate a quest's body + dungeon data, scaled to the player's level.
///
/// `player_level` + `scaling` (from `quests_daily.json.levelScaling`) drive the enemy /
/// difficulty level and per-enemy XP — replacing the old hard-coded `level 1 / 1000 XP`
/// stub that spawned level-1 enemies for everyone. With an empty `scaling` the enemy
/// level degrades to the player's own level (still never the flat 1).
///
/// A NIL-dungeon quest (`dungeon_uuid == 00000000-...`, the 6 dialogue-only "daily-job"
/// quests) short-circuits to a body with NO dungeon data instead of erroring — the old
/// `.ok_or(DungeonNotFound)` crashed those on accept.
pub fn generate_quest_data(
    game_data: &GameData,
    quest_id: Uuid,
    player_level: i64,
    scaling: &QuestLevelScaling,
) -> Result<(Quest, Option<DungeonGeneratedData>), GenerateQuestDataError> {
    let quest_data = game_data
        .quests
        .get(&quest_id)
        .ok_or(GenerateQuestDataError::QuestNotFound(quest_id))?;

    // A quest without a `dungeon_info` block carries no objectives/dungeon; treat it as a
    // dialogue quest (no dungeon data) rather than panicking on `.unwrap()`.
    let Some(dungeon_info) = quest_data.dungeon_info.as_ref() else {
        return Ok((dialogue_quest(quest_id, 0, HashMap::new(), player_level, scaling), None));
    };

    let difficulty_level = scaling.enemy_level(player_level);
    let objective_statuses: HashMap<Uuid, ObjectiveStatus> = dungeon_info
        .objectives
        .iter()
        .map(|(id, _o)| {
            (
                *id,
                ObjectiveStatus {
                    completed: false,
                    progress: 0.0,
                    status: QuestStatus::Active,
                },
            )
        })
        .collect();

    let quest = Quest {
        completed: false,
        difficulty_level,
        gld_quest_id: quest_id,
        seed: 1234.into(),
        r#type: QuestType::Normal,
        version: dungeon_info.version,
        objective_statuses: objective_statuses.clone(),
        // An ordinary quest carries none of the event fields; the event path in
        // `server::quest::event_quests` fills them in after calling this.
        game_event_quest_data: None,
        rewards: None,
        final_reward: None,
        // Not a town job: jobs are built by `server::quest::jobs_gen`, never here.
        job_reward: None,
    };

    // Nil-dungeon (dialogue-only) quests have no dungeon to generate — short-circuit to a
    // no-dungeon completion so /accept doesn't error (they were the DungeonNotFound crash).
    if dungeon_info.dungeon_uuid.is_nil() {
        return Ok((quest, None));
    }


    let enemy_level = scaling.enemy_level(player_level);
    let given_xp = scaling.given_xp(enemy_level);

    // Shared with the Abyss — see `util::dungeon`. The Abyss used to have its
    // own hard-coded copy of this shape, which served floor 1's spawn groups on
    // every floor and hung every deeper run.
    let generated_dungeon_data =
        crate::util::dungeon::generate_for_dungeon(game_data, &dungeon_info.dungeon_uuid, enemy_level, given_xp)
            .ok_or(GenerateQuestDataError::DungeonNotFound(dungeon_info.dungeon_uuid))?;

    Ok((quest, Some(generated_dungeon_data)))
}

/// A dialogue / no-dungeon quest body (no dungeon data). Used for a quest whose
/// `dungeon_info` is absent — objectives default to whatever is passed (empty for a bare
/// dialogue quest).
fn dialogue_quest(
    quest_id: Uuid,
    version: u64,
    objective_statuses: HashMap<Uuid, ObjectiveStatus>,
    player_level: i64,
    scaling: &QuestLevelScaling,
) -> Quest {
    Quest {
        completed: false,
        difficulty_level: scaling.enemy_level(player_level),
        gld_quest_id: quest_id,
        seed: 1234.into(),
        r#type: QuestType::Normal,
        version,
        objective_statuses,
        game_event_quest_data: None,
        rewards: None,
        final_reward: None,
        job_reward: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::{EnemyLevelScaling, QuestLevelScaling};

    /// A scaling table like `quests_daily.json`: default skull (2) → offset 0.
    fn scaling() -> QuestLevelScaling {
        QuestLevelScaling {
            enemy_level_from_player_level: EnemyLevelScaling {
                offset_by_skull: [("2".to_string(), 0i64)].into_iter().collect(),
                default_skull: 2,
            },
            // No measured curve: this fixture exercises the skull-offset fallback.
            measured: Default::default(),
        }
    }

    #[test]
    fn enemy_level_scales_with_player_not_flat_one() {
        let s = scaling();
        assert_eq!(s.enemy_level(50), 50, "level-50 player → level-50 enemies");
        assert_eq!(s.enemy_level(1), 1, "clamped floor");
        assert_eq!(s.enemy_level(200), 100, "clamped ceiling at 100");
        // XP scales with enemy level, not a flat 1000.
        assert_eq!(s.given_xp(50), 5000);
        assert_ne!(s.given_xp(50), 1000);
    }

    #[test]
    fn empty_scaling_degrades_to_player_level_never_flat_one() {
        let s = QuestLevelScaling::default();
        assert_eq!(s.enemy_level(37), 37, "no table → the player's own level");
        assert_eq!(s.enemy_level(0), 1, "clamped to at least 1");
    }
}

/// How retail scaled quest enemies — replayed against 66,994 captured spawns.
///
/// These are the two numbers that decide how fast a character levels: what an
/// enemy is worth, and how hard it is. Both were formulas here and neither
/// matched retail, so they are asserted against the corpus rather than against
/// each other.
#[cfg(test)]
mod how_retail_scaled_enemies {
    use crate::static_data::QuestLevelScaling;
    use serde_json::Value;

    /// The shipped table — the one the server loads at boot.
    fn shipped() -> QuestLevelScaling {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/quests_daily.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let json: Value = serde_json::from_str(&raw).expect("valid quests_daily.json");
        let scaling: QuestLevelScaling =
            serde_json::from_value(json["levelScaling"].clone()).expect("levelScaling parses");
        assert!(
            !scaling.measured.given_xp_by_enemy_level.is_empty()
                && !scaling.measured.enemy_level_by_player_level.is_empty(),
            "the measured curves did not survive deserialization — both tables carry a \
             prose `note` next to their numeric rows and a stricter reader drops the lot"
        );
        scaling
    }

    /// `[[playerLevel, enemyLevel, count], ...]` plus `[[enemyLevel, xp, n], ...]`.
    fn corpus() -> Value {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/retail-journey/spawn_levels.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        serde_json::from_str(&raw).expect("valid spawn_levels.json")
    }

    fn pairs() -> Vec<(i64, i64, u64)> {
        corpus()["playerEnemyPairs"]
            .as_array()
            .expect("playerEnemyPairs")
            .iter()
            .map(|r| {
                (
                    r[0].as_i64().unwrap(),
                    r[1].as_i64().unwrap(),
                    r[2].as_u64().unwrap(),
                )
            })
            .collect()
    }

    /// Share of captured spawns a candidate rule reproduces exactly, and within
    /// two levels.
    fn score(rule: impl Fn(i64) -> i64) -> (f64, f64) {
        let (mut total, mut exact, mut near) = (0u64, 0u64, 0u64);
        for (player, enemy, n) in pairs() {
            total += n;
            let want = rule(player);
            if want == enemy {
                exact += n;
            }
            if (want - enemy).abs() <= 2 {
                near += n;
            }
        }
        assert!(total > 60_000, "only {total} spawns in the fixture");
        (exact as f64 / total as f64, near as f64 / total as f64)
    }

    /// THE measurement, with its controls.
    ///
    /// The curve is a median over every spawn group, so it is closer rather than
    /// right — which is exactly why it is scored against alternatives instead of
    /// asserted to be exact. What matters is that it beats what we shipped, and
    /// by how much.
    #[test]
    fn the_measured_curve_beats_serving_enemies_at_the_players_level() {
        let scaling = shipped();
        let (exact, near) = score(|p| scaling.enemy_level(p));
        // What the server did before: enemyLevel = playerLevel.
        let (flat_exact, flat_near) = score(|p| p.clamp(1, 100));
        // A second control, so "anything below the player" cannot be the reason.
        let (minus_ten_exact, _) = score(|p| (p - 10).clamp(1, 100));

        assert!(
            exact > flat_exact * 2.0,
            "the curve reproduces {:.1}% of captured spawns exactly against {:.1}% \
             for enemy=player — it must beat it by a wide margin or it is not \
             worth the table",
            exact * 100.0,
            flat_exact * 100.0
        );
        assert!(
            near > flat_near * 2.0,
            "within two levels: {:.1}% vs {:.1}%",
            near * 100.0,
            flat_near * 100.0
        );
        assert!(
            exact > minus_ten_exact * 2.0,
            "the curve ({:.1}%) must beat a flat player-10 ({:.1}%); otherwise all \
             it has found is 'lower'",
            exact * 100.0,
            minus_ten_exact * 100.0
        );
    }

    /// The shape the fix exists for: retail's enemies track the player early and
    /// fall behind later. Serving `playerLevel` all the way up is what made our
    /// quests harder than retail's at high level.
    #[test]
    fn enemies_track_the_player_early_and_fall_behind_later() {
        let scaling = shipped();
        for p in 1..=20 {
            let gap = scaling.enemy_level(p) - p;
            assert!(
                gap.abs() <= 3,
                "at player level {p} retail's enemies were within 3 levels; ours are {gap:+}"
            );
        }
        assert!(
            scaling.enemy_level(100) <= 85,
            "at player level 100 retail's median enemy was 78; ours is {}",
            scaling.enemy_level(100)
        );
        // Monotone: a level-up must never make the world easier in absolute terms.
        for p in 2..=100 {
            assert!(
                scaling.enemy_level(p) >= scaling.enemy_level(p - 1),
                "enemy level went DOWN from player {} to {p}",
                p - 1
            );
        }
    }

    /// Every enemy level the corpus covers is worth what retail paid for it.
    ///
    /// The canonical value is the maximum observed: the same enemy level carries
    /// two XP populations (14 and 41 both appear at level 12) and the lower one is
    /// the partial/zero-XP case.
    #[test]
    fn an_enemy_is_worth_what_retail_paid_for_it() {
        let scaling = shipped();
        let corpus = corpus();
        let mut checked = 0;
        for row in corpus["givenXpObservations"].as_array().unwrap() {
            let (level, canonical, n) = (
                row[0].as_i64().unwrap(),
                row[1].as_u64().unwrap(),
                row[2].as_u64().unwrap(),
            );
            // The extractor drops thin and zero-XP rows from the shipped table on
            // purpose; skip them here for the same reason.
            if n < 5 || canonical == 0 {
                continue;
            }
            assert_eq!(
                scaling.given_xp(level),
                canonical,
                "an enemy of level {level} was worth {canonical} XP at retail ({n} observations)"
            );
            checked += 1;
        }
        assert!(checked >= 80, "only {checked} enemy levels checked");
    }

    /// THE CONTROL on the XP fix: the old formula was not slightly off, it was an
    /// order of magnitude out, and that is most of why levelling here was fast.
    #[test]
    fn the_old_hundred_times_level_formula_was_an_order_of_magnitude_out() {
        let scaling = shipped();
        assert_eq!(scaling.given_xp(50), 220, "retail's level-50 enemy");
        assert_eq!(100 * 50, 5000, "what we paid for it");
        assert!(
            scaling.given_xp(50) * 20 < 5000,
            "the formula was more than 20x retail at level 50"
        );
        // Both ends stay sane: the table's floor at 1 and its flat top past 90.
        assert_eq!(scaling.given_xp(1), 11);
        assert_eq!(scaling.given_xp(0), 11, "clamped, not zero");
        assert_eq!(
            scaling.given_xp(200),
            scaling.given_xp(90),
            "past the table's top it holds, rather than resuming a formula that \
             would pay 9,100 at level 91 against 284 at 90"
        );
    }

    /// A server booted with no static data must still spawn something playable.
    #[test]
    fn an_empty_table_degrades_instead_of_breaking() {
        let empty = QuestLevelScaling::default();
        assert_eq!(empty.enemy_level(37), 37, "the player's own level");
        assert_eq!(empty.enemy_level(0), 1, "clamped");
        assert_eq!(empty.given_xp(50), 5000, "the old formula, only as a last resort");
    }
}
