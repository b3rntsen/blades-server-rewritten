use std::sync::Arc;

use crate::{
    json_db::JsonDbWrapper,
    models::{CharacterDbEntryCharacterWalletInventory, QuestDbEntryDungeonStateAndGeneratedData},
};
use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::user_data::{
    B64EncodedData, CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, DungeonStatus,
    EnemyIndex, EnemyStatus, InventoryChangeTracker,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, session::SessionLookedUpMaybe};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnemyKilledUpdate {
    pub spawn_group_id: Uuid,
    pub spawner_index: usize,
    pub enemy_index: usize,
    #[allow(unused)]
    // We use the data stored in the generated data instead of trusting the client
    pub xp_reward: f64,
    pub time: u64,
}

/// A `combat_completed` action — the client posts it (alongside `enemy_killed`
/// actions) when a combat encounter/room resolves. The per-enemy XP + kills arrive as
/// the `EnemyKilled` actions in the SAME batch, so this is a state-only marker here
/// (the dungeon's `current_state` blob is persisted regardless). Fields vary by client
/// version; serde ignores any we don't name, so an evolving payload never 400s.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CombatCompletedUpdate {
    #[serde(default)]
    #[allow(dead_code)]
    time: Option<u64>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DungeonUpdateAction {
    EnemyKilled(EnemyKilledUpdate),
    /// Accepted so a mixed `enemy_killed` + `combat_completed` batch deserializes —
    /// previously an unknown variant made serde reject the whole POST (→400), which is
    /// PaganBlueNose's "network error … with a quest".
    CombatCompleted(CombatCompletedUpdate),
    /// Forward-compat: any OTHER action type the client emits is accepted and ignored
    /// rather than 400-ing the whole batch.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct DungeonUpdateRequest {
    current_state: B64EncodedData,
    actions: Vec<DungeonUpdateAction>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct DungeonUpdateResponse {
    inventory: CompleteInventoryUpdate,
    character: CompleteCharacterWithIdWithoutData,
    dungeon_status: DungeonStatus,
}

#[post(
    "blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/dungeons/current/update"
)]
pub async fn dungeon_update(
    path: web::Path<(Uuid, Uuid)>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: Json<DungeonUpdateRequest>,
) -> Result<Json<DungeonUpdateResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, quest_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(|mut conn| {
        async move {
            let (quest_data, mut character_data) = {
                use crate::schema::characters;
                use crate::schema::quests;

                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(characters::id.eq(character_id))
                    .inner_join(characters::table)
                    .filter(characters::user_id.eq(session.session.user_id))
                    .select((
                        QuestDbEntryDungeonStateAndGeneratedData::as_select(),
                        CharacterDbEntryCharacterWalletInventory::as_select(),
                    ))
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    // No matching quest/character for this user → 404 instead of a panic
                    // (dropped connection = the client's "network error").
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // The dungeon must have been entered/generated first. A missing
            // generated_data / dungeon_state is a client/state error → 400, not a panic.
            let generated_data = quest_data
                .generated_data
                .0
                .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;
            let mut dungeon_state = quest_data
                .dungeon_state
                .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?
                .0;

            let inventory_modification_tracker = InventoryChangeTracker::default();

            dungeon_state.dungeon_status.current_state = body.current_state.clone();

            for action in &body.actions {
                match action {
                    DungeonUpdateAction::EnemyKilled(enemy_killed) => {
                        let enemy_index = EnemyIndex::new(
                            enemy_killed.spawn_group_id,
                            enemy_killed.spawner_index,
                            enemy_killed.enemy_index,
                        );
                        // A stale/unknown enemy index → skip THAT action, don't kill the
                        // whole dungeon update (was a panic).
                        let Some(enemy_generated_data) = generated_data.get_enemy(&enemy_index)
                        else {
                            log::warn!(
                                "dungeon_update: enemy {:?} not in generated data (stale) — skipping",
                                enemy_index
                            );
                            continue;
                        };
                        if let Some(current_enemy_data) = dungeon_state
                            .dungeon_status
                            .enemy_status
                            .get_mut(&enemy_index)
                        {
                            // Re-reporting an already-killed enemy (client retry/dup) is a
                            // no-op, not a panic — and must not double-count XP.
                            if current_enemy_data.killed {
                                continue;
                            }
                            current_enemy_data.killed = true;
                        } else {
                            dungeon_state.dungeon_status.enemy_status.insert(
                                enemy_index,
                                EnemyStatus {
                                    spawn_group_id: enemy_killed.spawn_group_id,
                                    xp_reward: enemy_generated_data.given_xp,
                                    killed: true,
                                    time: enemy_killed.time,
                                    loot: enemy_generated_data.merged_loot_table(),
                                },
                            );
                        }

                        character_data.character.0.experience += enemy_generated_data.given_xp;
                    }
                    // Room/combat finished — the kills (XP) arrived as EnemyKilled actions
                    // in this batch and the dungeon current_state blob is persisted below;
                    // no extra reward to apply here.
                    DungeonUpdateAction::CombatCompleted(_) => {}
                    DungeonUpdateAction::Unknown => {
                        log::warn!(
                            "dungeon_update: ignoring unknown action type in quest {}",
                            quest_id
                        );
                    }
                }
            }

            // generate the response before we submit data to minimize the amount of cloning needed

            let result = DungeonUpdateResponse {
                dungeon_status: dungeon_state.dungeon_status.clone(),
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: character_data.character.0.clone(),
                },
                inventory: character_data.inventory.0.generate_client_update(&inventory_modification_tracker)
            };

            let quest_data_rebuilt = QuestDbEntryDungeonStateAndGeneratedData {
                id: quest_id,
                dungeon_state: Some(JsonDbWrapper(dungeon_state)),
                generated_data: JsonDbWrapper(Some(generated_data)),
            };

            {
                use crate::schema::quests;
                diesel::update(quests::table)
                    .filter(quests::id.eq(quest_id))
                    .set(quest_data_rebuilt)
                    .execute(&mut conn)
                    .await?;
            }

            {
                use crate::schema::characters;

                diesel::update(characters::table)
                    .filter(characters::id.eq(character_data.id))
                    .set(character_data)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(result))
        }
    }.scope_boxed()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_batch_with_combat_completed_deserializes() {
        // The real client posts a MIXED actions array (enemy_killed + combat_completed).
        // With the old single-variant enum, serde rejected `combat_completed` and the
        // WHOLE POST 400'd (PaganBlueNose's quest "network error"). It must now parse,
        // and an unknown future action type must be tolerated too.
        let raw = r#"{
            "currentState": {"b64": "AAAA"},
            "actions": [
                {"type":"enemy_killed","spawnGroupId":"11111111-0000-0000-0000-000000000001","spawnerIndex":0,"enemyIndex":0,"xpReward":11.0,"time":1234},
                {"type":"combat_completed","time":1300,"someFutureField":42},
                {"type":"room_cleared","whatever":true}
            ]
        }"#;
        let req: DungeonUpdateRequest =
            serde_json::from_str(raw).expect("mixed dungeon-update batch must deserialize");
        assert_eq!(req.actions.len(), 3);
        assert!(matches!(req.actions[0], DungeonUpdateAction::EnemyKilled(_)));
        assert!(matches!(req.actions[1], DungeonUpdateAction::CombatCompleted(_)));
        // Unknown action type tolerated (not a 400).
        assert!(matches!(req.actions[2], DungeonUpdateAction::Unknown));
    }
}
