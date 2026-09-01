use std::collections::HashMap;

use thiserror::Error;
use uuid::Uuid;

use crate::util::dungeon::generate_for_dungeon;

use crate::{
    game_data::GameData,
    static_data::{QuestLevelScaling, StaticData},
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
    static_data: &StaticData,
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
    let generated_dungeon_data = generate_for_dungeon(
        game_data,
        static_data,
        &dungeon_info.dungeon_uuid,
        enemy_level,
        given_xp
    ).ok_or(GenerateQuestDataError::DungeonNotFound(dungeon_info.dungeon_uuid))?;

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
