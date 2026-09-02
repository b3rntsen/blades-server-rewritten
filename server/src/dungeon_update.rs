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
    EnemyIndex, EnemyStatus, InventoryChangeTracker, CompleteWallet, DungeonGeneratedData,
    DungeonState,
};
use blades_lib::economy::{RewardGrant, apply_reward};
use diesel;
use diesel::{
     prelude::*,
    ExpressionMethods, QueryDsl, SelectableHelper,
};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use std::{collections::HashMap, sync::Arc};

use crate::{BladeApiError, ServerGlobal, session::{Session, SessionLookedUpMaybe}};

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

#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = crate::schema::event_dungeons)]
struct EventDungeonStateAndGeneratedData {
    pub id: Uuid,
    pub dungeon_state: Option<serde_json::Value>,
    pub generated_data: serde_json::Value,
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
    let session_lookup = session.get_session_or_error()?;
    let validated_session = &session_lookup.session;
    let (character_id, quest_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    // Determine whether this is an event dungeon the SAME way enter_quest_dungeon
    // does: `quest_id` here is this character's per-quest DB row id, not the event
    // template id `event_quests.templates` is keyed by. Checking `quest_id` directly
    // against the template map (as this used to) is always false for event quests,
    // silently routing every update to handle_quest_dungeon_update — which looks in
    // the wrong table (`quests` instead of `event_dungeons`) and 400s with a
    // "missing dungeon_state/generated_data" error, because the real state was
    // stored under `gldQuestId` by enter, not under `quest_id`.
    let gld_quest_id: Option<Uuid> = {
        use crate::schema::quests;
        let row_info: Option<JsonDbWrapper<serde_json::Value>> = quests::table
            .filter(quests::id.eq(quest_id))
            .filter(quests::character_id.eq(character_id))
            .select(quests::info)
            .first(&mut *conn)
            .await
            .optional()?;

        row_info.and_then(|info| {
            info.0["gldQuestId"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
        })
    };

    let is_event = gld_quest_id
        .map(|gid| app_state.event_quests.templates.contains_key(&gid))
        .unwrap_or(false);

    if is_event {
        // Safe to unwrap: is_event is only true when gld_quest_id is Some.
        return handle_event_dungeon_update(
            &mut conn,
            &app_state,
            character_id,
            gld_quest_id.unwrap(),
            body.0,
            validated_session,
        ).await;
    }

    handle_quest_dungeon_update(
        &mut conn,
        character_id,
        quest_id,
        body.0,
        validated_session,
    ).await
}

async fn handle_quest_dungeon_update(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    quest_id: Uuid,
    body: DungeonUpdateRequest,
    session: &Session,
) -> Result<Json<DungeonUpdateResponse>, BladeApiError> {
    conn.transaction(|mut conn| {
        async move {
            let (quest_data, mut character_data) = {
                use crate::schema::characters;
                use crate::schema::quests;

                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(characters::id.eq(character_id))
                    .inner_join(characters::table)
                    .filter(characters::user_id.eq(session.user_id))
                    .select((
                        QuestDbEntryDungeonStateAndGeneratedData::as_select(),
                        CharacterDbEntryCharacterWalletInventory::as_select(),
                    ))
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };
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

            let mut wallet = std::mem::take(&mut character_data.wallet.0);

            let result = { 
                process_dungeon_actions(
                    &body.actions,
                    &generated_data,
                    &mut dungeon_state,
                    &mut character_data,
                    &mut wallet,
                    &mut inventory_modification_tracker,
                );

                character_data.wallet.0 = wallet;

                DungeonUpdateResponse {
                    dungeon_status: dungeon_state.dungeon_status.clone(),
                    character: CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: character_data.character.0.clone(),
                    },
                    inventory: character_data.inventory.0.generate_client_update(&inventory_modification_tracker),
                    wallet: character_data.wallet.0.clone(),
                }
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

async fn handle_event_dungeon_update(
    conn: &mut AsyncPgConnection,
    app_state: &ServerGlobal,
    character_id: Uuid,
    dungeon_id: Uuid, // This is actually the event dungeon ID
    body: DungeonUpdateRequest,
    session: &Session,
) -> Result<Json<DungeonUpdateResponse>, BladeApiError> {
    conn.transaction(|mut conn| {
        async move {
            let event_template = app_state.event_quests.templates.get(&dungeon_id)
                .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?;

            // `event_id` is NOT the same value as `dungeon_id` (gldQuestId) — it's
            // the template's own event_ids[0], a separate UUID. handle_event_dungeon_entry
            // stores the row with event_id = event_ids[0], so the lookup here must
            // derive it the exact same way, or it filters on the wrong value and
            // never matches the row entry created (→ spurious 404).
            let event_id = *event_template.event_ids.get(0)
                .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?;

            // No game_data.events lookup here — handle_event_dungeon_entry deliberately
            // skips it too ("we don't need game_data.events - use the quest_id as the
            // dungeon UUID"), and checking it against dungeon_id/event_id was liable to
            // 404 on a key that was never expected to exist there for event quests.

            // Get the event dungeon state
            let (event_dungeon_data, mut character_data) = {
                use crate::schema::characters;
                use crate::schema::event_dungeons;

                event_dungeons::table
                    .filter(event_dungeons::dungeon_id.eq(dungeon_id))
                    .filter(event_dungeons::character_id.eq(character_id))
                    .filter(event_dungeons::event_id.eq(event_id))
                    .inner_join(characters::table)
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(session.user_id))
                    .select((
                        EventDungeonStateAndGeneratedData::as_select(),
                        CharacterDbEntryCharacterWalletInventory::as_select(),
                    ))
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            let generated_data: DungeonGeneratedData = serde_json::from_value(event_dungeon_data.generated_data)
                .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;

            // `dungeon_state` is cleared to NULL by `handle_event_dungeon_exit`. A kill/loot
            // update can legitimately arrive AFTER that clear (late network delivery, client
            // retry racing the exit, etc). Event dungeons are single-entry (`max_entries: 1`),
            // so there's no re-entry to repopulate this — erroring here just strands the
            // client on an update it has no way to recover from. Treat it as a no-op and hand
            // back current character/wallet/inventory unchanged, same idempotency rationale
            // as `exit_quest_dungeon`.
            let Some(dungeon_state_value) = event_dungeon_data.dungeon_state else {
                log::info!(
                    "event dungeon_update: dungeon_state already cleared for dungeon {} / character {} — treating as no-op",
                    dungeon_id, character_id
                );
                return Ok(Json(DungeonUpdateResponse {
                    dungeon_status: DungeonStatus {
                        dungeon_settings_ids: vec![dungeon_id],
                        revive_count: 0,
                        algorithm_version: 1,
                        current_state: body.current_state.clone(),
                        enemy_status: HashMap::default(),
                        seed: 0,
                        level: 1,
                        version: 1,
                        collected_chests: Default::default(),
                    },
                    character: CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: character_data.character.0.clone(),
                    },
                    inventory: character_data.inventory.0.generate_client_update(&InventoryChangeTracker::default()),
                    wallet: character_data.wallet.0.clone(),
                }));
            };

            let mut dungeon_state: DungeonState = serde_json::from_value(dungeon_state_value)
                .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;

            let mut inventory_modification_tracker = InventoryChangeTracker::default();

            dungeon_state.dungeon_status.current_state = body.current_state.clone();

            let mut wallet = std::mem::take(&mut character_data.wallet.0);

            process_dungeon_actions(
                &body.actions,
                &generated_data,
                &mut dungeon_state,
                &mut character_data,
                &mut wallet,
                &mut inventory_modification_tracker,
            );

            character_data.wallet.0 = wallet;

            let result = DungeonUpdateResponse {
                dungeon_status: dungeon_state.dungeon_status.clone(),
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: character_data.character.0.clone(),
                },
                inventory: character_data.inventory.0.generate_client_update(&inventory_modification_tracker),
                wallet: character_data.wallet.0.clone(),
            };

            // Update event dungeon state
            {
                use crate::schema::event_dungeons;

                let dungeon_state_json = serde_json::to_value(&dungeon_state)
                        .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

                diesel::update(event_dungeons::table)
                    .filter(event_dungeons::id.eq(event_dungeon_data.id))
                    .set((
                        event_dungeons::dungeon_state.eq(Some(dungeon_state_json)),
                    ))
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

fn process_dungeon_actions(
    actions: &[DungeonUpdateAction],
    generated_data: &DungeonGeneratedData,
    dungeon_state: &mut DungeonState,
    character_data: &mut CharacterDbEntryCharacterWalletInventory,
    wallet: &mut CompleteWallet,
    inventory_modification_tracker: &mut InventoryChangeTracker,
) {
    for action in actions {
        match action {
            DungeonUpdateAction::EnemyKilled(enemy_killed) => {
                let enemy_index = EnemyIndex::new(
                    enemy_killed.spawn_group_id,
                    enemy_killed.spawner_index,
                    enemy_killed.enemy_index,
                );
                
                let Some(enemy_generated_data) = generated_data.get_enemy(&enemy_index) else {
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

            DungeonUpdateAction::EnemyLootCollected(enemy_loot) => {
                let enemy_index = EnemyIndex::new(
                    enemy_loot.spawn_group_id,
                    enemy_loot.spawner_index,
                    enemy_loot.enemy_index,
                );

                if let Some(enemy_status) = dungeon_state.dungeon_status.enemy_status.get(&enemy_index) {
                    let mut reward = RewardGrant::default();

                    for (item_id, quantity) in &enemy_status.loot.stackable_items {
                        reward.stackable_items.insert(*item_id, *quantity);
                    }

                    for (currency_id, amount) in &enemy_status.loot.currencies {
                        reward.currencies.insert(*currency_id, *amount);
                    }

                    apply_reward(
                        &reward,
                        wallet,
                        &mut character_data.inventory.0,
                        &mut character_data.character.0,
                        inventory_modification_tracker,
                    );
                }
            }

            DungeonUpdateAction::ItemLootCollected(item_loot) => {
                let mut reward = RewardGrant::default();

                for (item_id, quantity) in &item_loot.loot.stackable_items {
                    reward.stackable_items.insert(*item_id, (*quantity).into());
                }

                for (currency_id, amount) in &item_loot.loot.currencies {
                    reward.currencies.insert(*currency_id, (*amount).into());
                }

                apply_reward(
                    &reward,
                    wallet,
                    &mut character_data.inventory.0,
                    &mut character_data.character.0,
                    inventory_modification_tracker,
                );
            }

            DungeonUpdateAction::ChestCollected(chest) => {
                if let Some(chest_data) = generated_data.get_chest(&chest.spawn_group_id, chest.spawn_group_index) {
                    let tier = if chest_data.tier > 0 { 
                        chest_data.tier as u32 
                    } else { 
                        chest.tier
                    };

                    let chest_id = character_data.inventory.0.treasury.add_chest(
                        tier as u64, 
                        dungeon_state.dungeon_status.level
                    );

                    inventory_modification_tracker.modified_treasury.added.push(chest_id);
                    dungeon_state.dungeon_status.collected_chests.insert(chest.spawn_group_id);
                }
            }

            DungeonUpdateAction::CombatCompleted(_) => {}
            DungeonUpdateAction::Unknown => {
                log::warn!("dungeon_update: ignoring unknown action type");
            }
        }
    }
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
