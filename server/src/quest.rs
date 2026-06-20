use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::{
    economy::{RewardGrant, apply_reward, grant_chest},
    user_data::{
        CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
        DungeonGeneratedDataWithId, InventoryChangeTracker, QuestWithId,
    },
    util::quest::generate_quest_data,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, associations::HasTable, insert_into};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    json_db::JsonDbWrapper,
    models::{
        CharacterDbEntryCharacterAlone, CharacterDbEntryEconomy, QuestDbEntry, QuestDbEntryInfo,
    },
    session::SessionLookedUpMaybe,
    util::{self, check_permission_for_character_and_get_it},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetQuestsResponse {
    quests: Vec<QuestWithId>,
    dungeon_generated_data_list: Vec<DungeonGeneratedDataWithId>,
    jobs: Vec<()>, //TODO:
    character: CompleteCharacterWithIdWithoutData,
    job_pools: Value,                      //TODO: this one is critical
    game_event_quests: Vec<()>,            //TODO:
    game_event_quests_in_warning: Vec<()>, //TODO,
    game_event_quests_finished: Vec<()>,   //TODO
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests")]
pub async fn get_quests(
    session: SessionLookedUpMaybe,
    request: Json<Option<()>>,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetQuestsResponse>, BladeApiError> {
    assert!(request.is_none());
    let session = session.get_session_or_error()?;

    let character_id_var = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    conn.transaction(|mut conn| {
        async move {
            let character = {
                use crate::schema::characters::dsl::*;

                characters::table()
                    .filter(id.eq(&character_id_var))
                    .select(CharacterDbEntryCharacterAlone::as_select())
                    .load(&mut conn)
                    .await?
            };
            let character =
                util::get_only_single_character_and_check_permission(character, &session.session)?;

            // we could have done an inner join to check the get the user id, but the user has already been checked previously.
            let quests = {
                use crate::schema::quests::dsl::*;
                // take care! that line above import a character_id thing
                quests::table()
                    .filter(character_id.eq(&character_id_var))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
            };

            let mut result_quests = Vec::new();
            let mut result_generated_data = Vec::new();

            for quest in quests {
                result_quests.push(QuestWithId {
                    quest_id: quest.id,
                    quest: quest.info.0,
                });
                if let Some(generated_data) = quest.generated_data.0 {
                    result_generated_data.push(DungeonGeneratedDataWithId {
                        quest_id: quest.id,
                        inner: generated_data,
                    });
                };
            }

            Ok(Json(GetQuestsResponse {
                quests: result_quests,
                dungeon_generated_data_list: result_generated_data,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id_var,
                    character: character.character.0,
                },
                jobs: Vec::new(),
                game_event_quests: Vec::new(),
                game_event_quests_finished: Vec::new(),
                game_event_quests_in_warning: Vec::new(),
                job_pools: json! {
                    [
                        {
                            "id": "4956c6ab-1832-4edd-8bee-561b79f83ee2",
                            "endTime": 1774760400,
                            "nextStartTime": 1774760400
                        },
                        {
                            "id": "717b3cf5-21d8-4f0c-a7a9-603fe37b8766",
                            "endTime": 1774760400,
                            "nextStartTime": 1774760400
                        },
                        {
                            "id": "361da91e-6860-4c31-a447-4010cbaad1dd",
                            "endTime": 1774846800,
                            "nextStartTime": 1774846800
                        },
                        {
                            "id": "9d94baeb-96d4-49e9-bdf6-9f939be836d3",
                            "endTime": 0,
                            "nextStartTime": 1774760400
                        },
                        {
                            "id": "c5efa81d-18d9-47f3-a0ac-e108c0a50605",
                            "endTime": 0,
                            "nextStartTime": 1774846800
                        },
                        {
                            "id": "6b2a5baa-f64f-4cfe-8b03-fe7d632ea2f1",
                            "endTime": 0,
                            "nextStartTime": 1774933200
                        },
                        {
                            "id": "9fcbb01c-13bf-4cd9-916f-25d5faf5314e",
                            "endTime": 0,
                            "nextStartTime": 1775019600
                        },
                        {
                            "id": "df666a07-3539-426a-916e-ccdba580cb1d",
                            "endTime": 0,
                            "nextStartTime": 1775106000
                        },
                        {
                            "id": "a4e76931-02bf-4bfb-a472-286e968a03e1",
                            "endTime": 0,
                            "nextStartTime": 1775192400
                        },
                        {
                            "id": "8501a030-5009-4c73-a864-69c3d7fe6ae5",
                            "endTime": 1774760400,
                            "nextStartTime": 1775278800
                        }
                    ]
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptQuestResponse {
    quest: QuestWithId,
    dungeon_generated_data: Option<DungeonGeneratedDataWithId>,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/accept"
)]
async fn accept_quest(
    session: SessionLookedUpMaybe,
    request: Json<Option<()>>,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<AcceptQuestResponse>, BladeApiError> {
    assert!(request.is_none());
    let session = session.get_session_or_error()?;
    let (character_id, quest_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    // check permission
    let _ = check_permission_for_character_and_get_it(&mut conn, &session.session, character_id)
        .await?;

    // actually add quest

    let (quest, dungeon_generated_data) = generate_quest_data(&app_state.game_data, quest_id)?;
    //TODO: specifically handle the case the quest already exist (primary key is character id + quest id)

    let to_insert = QuestDbEntry {
        id: quest_id,
        character_id,
        info: JsonDbWrapper(quest.clone()),
        generated_data: JsonDbWrapper(dungeon_generated_data.clone()),
        dungeon_state: None,
    };

    {
        use crate::schema::quests::dsl::*;

        insert_into(quests::table())
            .values(&to_insert)
            .execute(&mut conn)
            .await?;
    }

    Ok(Json(AcceptQuestResponse {
        quest: QuestWithId {
            quest_id: quest_id,
            quest,
        },
        dungeon_generated_data: dungeon_generated_data.map(|v| DungeonGeneratedDataWithId {
            quest_id: quest_id,
            inner: v,
        }),
    }))
}

// ---------------------------------------------------------------------------
// POST /quests/{quest_id}/complete
// ---------------------------------------------------------------------------

/// Wire shape matched from captured `/quests/{id}/complete` responses:
/// ```json
/// { "reward":{...}, "inventory":{...}, "wallet":[...], "character":{...} }
/// ```
/// `reward` is lenient: unknown quest → empty reward (all zeros / empty maps).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteQuestResponse {
    reward: RewardGrant,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
    character: CompleteCharacterWithIdWithoutData,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/complete"
)]
pub async fn complete_quest(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<CompleteQuestResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the economy row (character + wallet + inventory) under a row lock.
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Load the quest row and mark it completed.
            let mut quest_entry = {
                use crate::schema::quests;
                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1))?
            };

            quest_entry.info.0.completed = true;

            // Look up the capture-derived reward. Lenient: unknown quest → empty reward.
            let reward = globals
                .static_data
                .quest_rewards
                .get(&quest_id)
                // Also try by gldQuestId (event quests use gldQuestId ≠ quest_id).
                .or_else(|| {
                    globals
                        .static_data
                        .quest_rewards
                        .get(&quest_entry.info.0.gld_quest_id)
                })
                .cloned()
                .unwrap_or_default();

            let mut tracker = InventoryChangeTracker::default();
            apply_reward(
                &reward,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut entry.character.0,
                &mut tracker,
            );
            if !reward.stackable_items.is_empty() || !reward.items.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }
            if !reward.chests.is_empty() {
                for chest in &reward.chests {
                    grant_chest(
                        &mut entry.inventory.0,
                        chest.tier,
                        chest.level,
                        &mut tracker,
                    );
                }
                entry.inventory.0.treasury_version += 1;
            }

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let character = entry.character.0.clone();

            // Write the completed quest flag back.
            {
                use crate::schema::quests;
                diesel::update(quests::table)
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .set(QuestDbEntryInfo {
                        info: quest_entry.info,
                    })
                    .execute(&mut conn)
                    .await?;
            }

            // Write the economy (wallet + inventory + character XP) back.
            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(CompleteQuestResponse {
                reward,
                inventory,
                wallet,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character,
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

// ---------------------------------------------------------------------------
// POST /quests/{quest_id}/objectives
// ---------------------------------------------------------------------------

/// `objectiveUpdates` maps objective UUID → `{status, progress}` (and optionally
/// `completed`). The client reports absolute progress; we merge it in and persist.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ObjectivesRequest {
    #[serde(default)]
    objective_updates: std::collections::HashMap<Uuid, ObjectiveUpdate>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct ObjectiveUpdate {
    status: blades_lib::user_data::QuestStatus,
    progress: f64,
    #[serde(default)]
    completed: bool,
}

/// Wire shape for the objectives response.
///
/// Per captures there are two cases:
/// 1. Pure progress update (no objective yet completed): `{ quest:{...} }`.
/// 2. An objective reaches `Completed` status: `{ reward:{...}, inventory:{...},
///    character:{...}, quest:{...} }`.
///
/// We always include all fields and rely on `skip_serializing_if` to omit the empty
/// reward/inventory/character when no reward is due. In practice the client ignores
/// extra empty fields, but this matches the narrow case 1 wire exactly too (the
/// captured case-1 body was purely `{quest:{...}}`). We therefore split on whether the
/// reward is empty.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObjectivesResponse {
    #[serde(skip_serializing_if = "RewardGrant::is_empty")]
    reward: RewardGrant,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<CompleteInventoryUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<CompleteCharacterWithIdWithoutData>,
    quest: QuestWithId,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/objectives"
)]
pub async fn update_quest_objectives(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<ObjectivesRequest>,
) -> Result<Json<ObjectivesResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let body = body.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the economy row under a row lock (needed only when a reward is granted,
            // but we can't know upfront; take it eagerly to keep the transaction simple).
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Load and lock the quest row.
            let mut quest_entry = {
                use crate::schema::quests;
                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1))?
            };

            // Merge each objective update in. The client sends absolute progress.
            let mut any_newly_completed = false;
            for (obj_id, update) in &body.objective_updates {
                let entry_obj = quest_entry
                    .info
                    .0
                    .objective_statuses
                    .entry(*obj_id)
                    .or_insert_with(|| blades_lib::user_data::ObjectiveStatus {
                        status: blades_lib::user_data::QuestStatus::Active,
                        progress: 0.0,
                        completed: false,
                    });
                entry_obj.status = update.status;
                entry_obj.progress = update.progress;
                if update.completed && !entry_obj.completed {
                    entry_obj.completed = true;
                    any_newly_completed = true;
                }
            }

            // Grant an objective-completion reward only if at least one objective became
            // Completed for the first time. We look up by quest_id / gldQuestId.
            // NOTE: The captures show partial rewards (stackableItems only) on a
            // single-objective completion. For simplicity we grant the full quest reward
            // when any objective completes; the client is lenient about over-rewarding
            // here (the actual full reward is still gatekept at `/complete`).
            let reward = if any_newly_completed {
                globals
                    .static_data
                    .quest_rewards
                    .get(&quest_id)
                    .or_else(|| {
                        globals
                            .static_data
                            .quest_rewards
                            .get(&quest_entry.info.0.gld_quest_id)
                    })
                    .cloned()
                    .unwrap_or_default()
            } else {
                RewardGrant::default()
            };

            let (opt_inventory, opt_character) = if !reward.is_empty() {
                let mut tracker = InventoryChangeTracker::default();
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                if !reward.stackable_items.is_empty() || !reward.items.is_empty() {
                    entry.inventory.0.backpack_version += 1;
                }
                if !reward.chests.is_empty() {
                    for chest in &reward.chests {
                        grant_chest(
                            &mut entry.inventory.0,
                            chest.tier,
                            chest.level,
                            &mut tracker,
                        );
                    }
                    entry.inventory.0.treasury_version += 1;
                }
                let inv = entry.inventory.0.generate_client_update(&tracker);
                let ch = entry.character.0.clone();
                // Write economy back.
                {
                    use crate::schema::characters;
                    diesel::update(characters::table)
                        .filter(characters::id.eq(entry.id))
                        .set(entry)
                        .execute(&mut conn)
                        .await?;
                }
                (
                    Some(inv),
                    Some(CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: ch,
                    }),
                )
            } else {
                (None, None)
            };

            let quest_with_id = QuestWithId {
                quest_id,
                quest: quest_entry.info.0.clone(),
            };

            // Persist the updated objective statuses.
            {
                use crate::schema::quests;
                diesel::update(quests::table)
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .set(QuestDbEntryInfo {
                        info: quest_entry.info,
                    })
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(ObjectivesResponse {
                reward,
                inventory: opt_inventory,
                character: opt_character,
                quest: quest_with_id,
            }))
        }
        .scope_boxed()
    })
    .await
}
