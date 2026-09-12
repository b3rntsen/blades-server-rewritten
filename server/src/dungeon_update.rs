use crate::{
    dungeon::event_dungeon_data,
    json_db::JsonDbWrapper,
    models::{CharacterDbEntryCharacterWalletInventory, QuestDbEntryDungeonStateAndGeneratedData},
};
use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, RewardItem};
use blades_lib::user_data::{
    B64EncodedData, CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, DungeonStatus,
    EnemyIndex, EnemyStatus, InventoryChangeTracker, CompleteWallet, DungeonGeneratedData,
    DungeonState, LootTableResult,
};
use blades_lib::economy::apply_reward;
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
/// the `EnemyKilled` actions in the SAME batch; the useful payload here is the
/// post-fight durability of equipped gear. Fields vary by client version; serde ignores
/// any we don't name, so an evolving payload never 400s.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CombatCompletedUpdate {
    #[serde(default)]
    items: Vec<CombatDurabilityUpdate>,
    #[serde(default)]
    #[allow(dead_code)]
    time: Option<u64>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CombatDurabilityUpdate {
    id: Uuid,
    durability: f64,
}

/// A `*_loot_collected` action: the player picked something up inside the dungeon.
///
/// The client reports the generated item's identity and repeats its contents inline,
/// e.g.
///
/// ```json
/// {"type":"item_loot_collected","spawnGroupId":"e7edb276-…","spawnGroupIndex":0,
///  "loot":{"stackableItems":{"e7193116-…":1}},"time":1777808410209}
/// ```
///
/// The server generated the same result into `itemGeneratedData` when the dungeon was
/// created. That stored result is authoritative when present; `loot` remains a fallback
/// for imported/legacy dungeon data that has no matching generated item.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LootCollectedUpdate {
    #[serde(default)]
    spawn_group_id: Option<Uuid>,
    #[serde(default)]
    spawn_group_index: Option<usize>,
    #[serde(default)]
    loot: RewardGrant,
}

fn item_loot_grant(
    item_loot: &LootCollectedUpdate,
    generated_data: &DungeonGeneratedData,
) -> RewardGrant {
    let Some((spawn_group_id, spawn_group_index)) = item_loot
        .spawn_group_id
        .as_ref()
        .zip(item_loot.spawn_group_index)
    else {
        return item_loot.loot.clone();
    };
    let Some(item) = generated_data.get_item(spawn_group_id, spawn_group_index) else {
        // Old imported dungeon rows can lack itemGeneratedData. Preserve their
        // pre-existing client fallback rather than making an in-progress run lose loot.
        return item_loot.loot.clone();
    };

    let mut loot = LootTableResult::default();
    for result in item.loot_table_loot.values() {
        loot.merge(result.clone());
    }

    RewardGrant {
        currencies: loot.currencies,
        stackable_items: loot.stackable_items,
        items: loot
            .item
            .0
            .into_iter()
            .map(|(id, item)| RewardItem { id, item })
            .collect(),
        ..RewardGrant::default()
    }
}

fn collected_chest_key(spawn_group_id: Uuid, spawn_group_index: usize) -> String {
    if spawn_group_index == 0 {
        // Backward compatibility: existing dungeon states stored a bare UUID
        // for the only chest the old generator ever emitted.
        spawn_group_id.to_string()
    } else {
        format!("{spawn_group_id}-{spawn_group_index}")
    }
}

/// An `enemy_loot_collected` action — the player looted a corpse.
///
/// Only the enemy's IDENTITY is read. The contents were rolled server-side the moment
/// the enemy died (`EnemyStatus.loot`, from `merged_loot_table()`), so the client is
/// told what it got rather than asked. Reading `loot` from the request here would let a
/// caller name its own payout off any corpse it had killed.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnemyLootCollectedUpdate {
    pub spawn_group_id: Uuid,
    pub spawner_index: usize,
    pub enemy_index: usize,
    /// Only read when the stored loot is empty — see the handler.
    #[serde(default)]
    pub loot: RewardGrant,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ChestCollectedUpdate {
    pub spawn_group_id: Uuid,
    pub spawn_group_index: usize,
    /// Parsed for wire compatibility, never trusted over generated chest data.
    #[serde(rename = "tier")]
    pub _tier: u32,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DungeonUpdateAction {
    EnemyKilled(EnemyKilledUpdate),
    ChestCollected(ChestCollectedUpdate),
    /// Accepted so a mixed `enemy_killed` + `combat_completed` batch deserializes —
    /// previously an unknown variant made serde reject the whole POST (→400), which is
    /// PaganBlueNose's "network error … with a quest".
    CombatCompleted(CombatCompletedUpdate),
    /// Loot off a corpse. The stored `EnemyStatus.loot` wins whenever it has contents;
    /// the request's `loot` is the fallback for as long as we generate none.
    EnemyLootCollected(EnemyLootCollectedUpdate),
    /// Loot off the dungeon floor — loose items and harvested plants. This is the one
    /// tracker #95 is about.
    ItemLootCollected(LootCollectedUpdate),
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
    /// Present only when the batch actually moved currency. Retail is strict
    /// about this: of 29,569 captured update responses, `wallet` appears in
    /// 6,534 of the 6,537 whose request carried `currencies` and in 0 of the
    /// 23,032 that did not. Serialising it always would send the client a key
    /// retail never sent it on that request.
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
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

            let mut inventory_modification_tracker = InventoryChangeTracker::default();

            dungeon_state.dungeon_status.current_state = body.current_state.clone();

            let mut wallet = std::mem::take(&mut character_data.wallet.0);

            let result = {
                let currency_moved = process_dungeon_actions(
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
                    wallet: currency_moved.then(|| character_data.wallet.0.clone()),
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
                    // BOTH halves of the primary key. `quests.id` alone is NOT unique:
                    // an ordinary story quest is stored under the template id, so every
                    // character on that quest has a row with the same `id`, and an
                    // update filtered on `id` writes one player's dungeon state into
                    // all of them. The SELECT above is already scoped to this
                    // character; the write has to be too.
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
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

            let (event_dungeon_settings_id, regenerated_data) =
                event_dungeon_data(&app_state.game_data, dungeon_id)?;

            // No game_data.events lookup here: the event template identifies the event,
            // while parsed quest data identifies the dungeon and its spawn groups.

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

            let stored_generated_data: DungeonGeneratedData = serde_json::from_value(event_dungeon_data.generated_data)
                .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;
            let generated_data = if stored_generated_data.enemy_generated_data.is_empty()
                && stored_generated_data.item_generated_data.is_empty()
                && stored_generated_data.chest_generated_data.is_empty()
            {
                // Attempts started before report #135's fix were generated using the
                // event quest UUID, which is not a dungeon UUID, so all three maps
                // were empty. Repair them in-place on their next update.
                regenerated_data
            } else {
                stored_generated_data
            };

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
                        dungeon_settings_ids: vec![event_dungeon_settings_id],
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
                    // Nothing has been applied on this path, so there is nothing to
                    // say about the wallet.
                    wallet: None,
                }));
            };

            let mut dungeon_state: DungeonState = serde_json::from_value(dungeon_state_value)
                .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, 20001, 2))?;

            let mut inventory_modification_tracker = InventoryChangeTracker::default();

            dungeon_state.dungeon_status.current_state = body.current_state.clone();
            dungeon_state.dungeon_status.dungeon_settings_ids =
                vec![event_dungeon_settings_id];

            let mut wallet = std::mem::take(&mut character_data.wallet.0);

            let currency_moved = process_dungeon_actions(
                &body.actions,
                &generated_data,
                &mut dungeon_state,
                &mut character_data,
                &mut wallet,
                &mut inventory_modification_tracker,
            );

            // PUT THE WALLET BACK. `std::mem::take` above emptied
            // `character_data.wallet.0` so it could be handed over as
            // `&mut wallet`, and `CharacterDbEntryCharacterWalletInventory`
            // derives `AsChangeset` INCLUDING the wallet column — so the
            // `diesel::update(...).set(character_data)` at the end of this
            // handler persists whatever is left in it. Without this line every
            // event-dungeon update wrote an EMPTY wallet: one enemy killed in
            // an event dungeon and the player's gold, gems and everything else
            // were gone, and the response reported the empty wallet too.
            //
            // The quest path has always had this (the identical line after its
            // own process_dungeon_actions). This one lost it in b9d76e2
            // ("collect chests, rebased onto main"), and nothing caught it
            // because no test drives the event path through to the DB write.
            character_data.wallet.0 = wallet;

            let result = DungeonUpdateResponse {
                dungeon_status: dungeon_state.dungeon_status.clone(),
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: character_data.character.0.clone(),
                },
                inventory: character_data.inventory.0.generate_client_update(&inventory_modification_tracker),
                wallet: currency_moved.then(|| character_data.wallet.0.clone()),
            };

            // Update event dungeon state
            {
                use crate::schema::event_dungeons;

                let dungeon_state_json = serde_json::to_value(&dungeon_state)
                        .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;
                let generated_data_json = serde_json::to_value(&generated_data)
                    .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;

                diesel::update(event_dungeons::table)
                    .filter(event_dungeons::id.eq(event_dungeon_data.id))
                    .set((
                        event_dungeons::dungeon_state.eq(Some(dungeon_state_json)),
                        event_dungeons::generated_data.eq(generated_data_json),
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
) -> bool {
    let mut currency_moved = false;
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

            // Corpse loot. Contents are rolled server-side when the enemy dies, so the
            // stored `EnemyStatus` is authoritative WHEN IT HAS ANY -- see below.
            DungeonUpdateAction::EnemyLootCollected(enemy_loot) => {
                let enemy_index = EnemyIndex::new(
                    enemy_loot.spawn_group_id,
                    enemy_loot.spawner_index,
                    enemy_loot.enemy_index,
                );

                let Some(status) = dungeon_state.dungeon_status.enemy_status.get_mut(&enemy_index)
                else {
                    // Looting a corpse we have no record of killing credits nothing --
                    // that is the whole point of not trusting the body.
                    log::warn!(
                        "dungeon_update: enemy_loot_collected for unknown enemy {:?} -- crediting nothing",
                        enemy_index
                    );
                    continue;
                };

                // Looting the same corpse twice must not pay twice; taking the stored
                // loot empties it.
                let loot = std::mem::take(&mut status.loot);
                let stored_is_empty = loot.currencies.is_empty() && loot.stackable_items.is_empty();

                // We do not generate enemy loot yet: generate_for_dungeon sets
                // spawn_group_loot and loot_table_loot to HashMap::default() and nothing
                // fills them, so merged_loot_table() is always empty. Reading only the
                // stored value therefore reduces every corpse to nothing (#138, fixed in
                // #145). Stored wins when it HAS contents -- the moment we generate real
                // loot the client stops being trusted, with no further change here -- and
                // until then the request is the only source there is.
                let grant = if stored_is_empty {
                    enemy_loot.loot.clone()
                } else {
                    RewardGrant {
                        currencies: loot.currencies,
                        stackable_items: loot.stackable_items,
                        ..Default::default()
                    }
                };

                currency_moved |= !grant.currencies.is_empty();
                apply_reward(
                    &grant,
                    wallet,
                    &mut character_data.inventory.0,
                    &mut character_data.character.0,
                    inventory_modification_tracker,
                );
            }

            DungeonUpdateAction::ItemLootCollected(item_loot) => {
                // `generate_for_dungeon` already rolled capture-derived quantities
                // into itemGeneratedData. Prefer that stored result to the client's
                // repeated `loot` body, which can report a single lumber even when the
                // generated pile contains several (#104). Imported/legacy dungeon rows
                // without matching itemGeneratedData retain the request fallback.
                let reward = item_loot_grant(item_loot, generated_data);
                currency_moved |= !reward.currencies.is_empty();
                apply_reward(
                    &reward,
                    wallet,
                    &mut character_data.inventory.0,
                    &mut character_data.character.0,
                    inventory_modification_tracker,
                );
            }

            // The request repeats `tier`, but it is not authoritative. The chest
            // generated for this spawn group was already assigned its APK-derived
            // tier and quantity when the dungeon was created; only that stored
            // result is used here.
            DungeonUpdateAction::ChestCollected(chest) => {
                let Some(chest_data) =
                    generated_data.get_chest(&chest.spawn_group_id, chest.spawn_group_index)
                else {
                    log::warn!(
                        "dungeon_update: chest_collected for unknown chest {:?}/{} -- ignoring",
                        chest.spawn_group_id,
                        chest.spawn_group_index
                    );
                    continue;
                };

                // Collecting the same chest twice must not mint two chests. The
                // collected set is persisted with the dungeon state, so this holds
                // across requests as well as within one batch.
                //
                if !dungeon_state
                    .dungeon_status
                    .collected_chests
                    .insert(collected_chest_key(
                        chest.spawn_group_id,
                        chest.spawn_group_index,
                    ))
                {
                    continue;
                }

                let tier = chest_data.tier;
                let chest_id = character_data
                    .inventory
                    .0
                    .treasury
                    .add_chest(tier, dungeon_state.dungeon_status.level);
                inventory_modification_tracker
                    .modified_treasury
                    .added
                    .push(chest_id);
            }

            DungeonUpdateAction::CombatCompleted(combat) => {
                apply_combat_durability(
                    combat,
                    &mut character_data.inventory.0,
                    inventory_modification_tracker,
                );
            }
            DungeonUpdateAction::Unknown => {
                log::warn!("dungeon_update: ignoring unknown action type");
            }
        }
    }

    // The client applies a backpack diff only when `backpackVersion` moves. Credit the
    // loot without bumping it and the item reaches the database and is never shown --
    // which is exactly what "floor pickups still don't work" looked like after #136/#138
    // credited them correctly (#142). Every other grant path bumps here; this one did not.
    //
    // Bumped ONCE per request rather than per action, because a batch can carry several
    // pickups and bumping per action moves the version by more than one -- the same
    // double-bump that had to be undone in the town prop handler. Living in the shared
    // function means the event-dungeon path gets it too.
    if !inventory_modification_tracker
        .modified_backpack
        .stackable_items
        .is_empty()
        || !inventory_modification_tracker
            .modified_backpack
            .items
            .is_empty()
    {
        character_data.inventory.0.backpack_version += 1;
    }

    // The same rule for the treasury, and the same bug: #155 added the chest to
    // `treasury` and reported it in `modified_treasury`, but the client applies a
    // treasury diff only when `treasuryVersion` moves. Without this the chest
    // reaches the database and the client never learns it owns one -- then hangs
    // when it opens the chest it believes it just picked up (#104). Exactly the
    // failure #142 fixed for the backpack.
    if !inventory_modification_tracker.modified_treasury.added.is_empty() {
        character_data.inventory.0.treasury_version += 1;
    }

    currency_moved
}

/// Apply post-combat durability only in the damaging direction and only to an
/// equipped item already owned by this character. This is the same trust boundary as
/// abyss combat: a client cannot repair an item or invent one through this update.
fn apply_combat_durability(
    action: &CombatCompletedUpdate,
    inventory: &mut blades_lib::user_data::CompleteInventory,
    tracker: &mut InventoryChangeTracker,
) -> usize {
    let mut changed = 0;
    for update in &action.items {
        if !update.durability.is_finite() || update.durability < 0.0 {
            continue;
        }
        let Some(equipped) = inventory
            .loadout
            .equipped_items
            .0
            .values_mut()
            .find(|item| item.id == update.id)
        else {
            continue;
        };
        if update.durability >= equipped.item.durability {
            continue;
        }
        equipped.item.durability = update.durability;
        tracker
            .modified_loadout
            .modified_equipped_items
            .insert(equipped.slot);
        changed += 1;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `mem::take` of the wallet must be paired with a write-back before
    /// the handler's DB update.
    ///
    /// This is a source-level guard on purpose. The bug it exists to stop was a
    /// DELETED line: the event path took the wallet into a local, credited the
    /// local, and then let `diesel::update(...).set(character_data)` persist the
    /// emptied original — one enemy killed in an event dungeon wiped the
    /// player's gold and gems. Nothing caught it, because driving either path to
    /// its DB write needs a live Postgres, so both paths' `process_dungeon_actions`
    /// unit tests passed happily while the persisted wallet was empty.
    ///
    /// A behavioural test cannot see this: the mistake is in the CALLER, not in
    /// `process_dungeon_actions`, which behaves identically either way. So pin
    /// the pairing in the text, the same way `BIND_DEVICE_SQL`'s ownership guard
    /// is pinned in admin.rs. Rewriting the handler to return the wallet instead
    /// of mutating it in place would make this test unnecessary — that is the
    /// better fix and this guard should be deleted along with the pattern.
    #[test]
    fn every_wallet_take_is_written_back_before_the_db_update() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dungeon_update.rs"),
        )
        .expect("read own source");

        const TAKE: &str = "std::mem::take(&mut character_data.wallet.0)";
        const BACK: &str = "character_data.wallet.0 = wallet;";
        const WRITE: &str = "diesel::update(characters::table)";

        let takes: Vec<usize> = src.match_indices(TAKE).map(|(i, _)| i).collect();
        assert!(
            !takes.is_empty(),
            "the take/write-back pattern is gone — if the handler now returns the \
             wallet instead of mutating it in place, delete this test with it"
        );

        for start in takes {
            let line = src[..start].lines().count();
            let rest = &src[start + TAKE.len()..];
            let back = rest.find(BACK);
            let write = rest.find(WRITE);
            match (back, write) {
                (Some(b), Some(w)) => assert!(
                    b < w,
                    "wallet taken at line {line} but written back only AFTER the \
                     DB update — the update persists the emptied wallet"
                ),
                (Some(_), None) => {}
                (None, _) => panic!(
                    "wallet taken at line {line} and never written back to \
                     character_data — AsChangeset will persist an EMPTY wallet"
                ),
            }
        }
    }

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

    #[test]
    fn combat_completed_only_damages_owned_equipped_gear() {
        use blades_lib::user_data::{
            CompleteInventory, Item, ItemPropertiesAll, SingleEquippedItem,
        };

        let slot = Uuid::new_v4();
        let item_id = Uuid::new_v4();
        let unknown_id = Uuid::new_v4();
        let mut inventory = CompleteInventory {
            backpack: Default::default(),
            loadout: Default::default(),
            treasury: Default::default(),
            overflow_treasury: Default::default(),
            backpack_version: 0,
            treasury_version: 0,
        };
        inventory.loadout.equipped_items.0.insert(
            slot,
            SingleEquippedItem {
                id: item_id,
                slot,
                item: Item {
                    item_template_id: Uuid::new_v4(),
                    grade: None,
                    tempering_level: 0,
                    durability: 100.0,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            },
        );

        let action: CombatCompletedUpdate = serde_json::from_value(serde_json::json!({
            "items": [
                {"id": item_id, "durability": 75.0},
                {"id": item_id, "durability": 95.0},
                {"id": item_id, "durability": -1.0},
                {"id": unknown_id, "durability": 0.0}
            ]
        }))
        .expect("combat payload parses");
        let mut tracker = InventoryChangeTracker::default();

        assert_eq!(
            apply_combat_durability(&action, &mut inventory, &mut tracker),
            1
        );
        assert_eq!(
            inventory.loadout.equipped_items.0[&slot].item.durability,
            75.0
        );
        assert_eq!(
            tracker.modified_loadout.modified_equipped_items,
            std::collections::HashSet::from([slot])
        );
    }

    /// Floor loot and harvested plants must parse as their own action and carry their
    /// contents — not fall into `Unknown`, which is what silently dropped them
    /// (tracker #95: "items placed on the dungeon floor or plants can't be picked up,
    /// they don't give anything to the player").
    ///
    /// The bodies here are copied from captured retail requests.
    #[test]
    fn floor_and_corpse_loot_parse_with_their_contents() {
        let raw = r#"{
            "currentState": {"b64": "AAAA"},
            "actions": [
                {"type":"item_loot_collected","spawnGroupId":"e7edb276-a04c-413f-80ab-69ffe304874f","spawnGroupIndex":0,
                 "loot":{"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":1}},"time":1777808410209},
                {"type":"enemy_loot_collected","spawnGroupId":"4295c814-e5e7-4a8a-939a-d3238471c906","spawnerIndex":0,"enemyIndex":0,
                 "loot":{"currencies":{"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":4}},"time":1777808407519}
            ]
        }"#;
        let req: DungeonUpdateRequest =
            serde_json::from_str(raw).expect("captured loot batch must deserialize");

        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();
        let gold: Uuid = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2".parse().unwrap();

        match &req.actions[0] {
            DungeonUpdateAction::ItemLootCollected(c) => {
                assert_eq!(
                    c.spawn_group_id,
                    Some("e7edb276-a04c-413f-80ab-69ffe304874f".parse().unwrap())
                );
                assert_eq!(c.spawn_group_index, Some(0));
                assert_eq!(c.loot.stackable_items.get(&lumber), Some(&1));
            }
            other => panic!("floor loot must not be dropped, got {other:?}"),
        }
        match &req.actions[1] {
            DungeonUpdateAction::EnemyLootCollected(c) => {
                assert_eq!(c.spawner_index, 0);
                assert_eq!(c.enemy_index, 0);
                // The request's loot is parsed, because it is the fallback used while
                // the server generates no enemy loot of its own.
                assert_eq!(c.loot.currencies.get(&gold), Some(&4));
            }
            other => panic!("corpse loot must not be dropped, got {other:?}"),
        }
    }

    /// The collection request can repeat a one-unit stack even though the result that
    /// was rolled and sent in itemGeneratedData contains several. The generated result
    /// is the same capture-derived source used to build the dungeon, so it must win.
    #[test]
    fn floor_loot_uses_the_generated_quantity() {
        use blades_lib::user_data::DungeonItemResult;

        let group: Uuid = "e7edb276-a04c-413f-80ab-69ffe304874f".parse().unwrap();
        let table: Uuid = "cf26d2d8-6aa9-4616-8378-16036b85c1ff".parse().unwrap();
        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();

        let mut request_loot = RewardGrant::default();
        request_loot.stackable_items.insert(lumber, 1);
        let action = LootCollectedUpdate {
            spawn_group_id: Some(group),
            spawn_group_index: Some(0),
            loot: request_loot,
        };

        let mut generated_loot = LootTableResult::default();
        generated_loot.stackable_items.insert(lumber, 6);
        let generated = DungeonGeneratedData {
            enemy_generated_data: HashMap::new(),
            item_generated_data: HashMap::from([(
                group,
                vec![DungeonItemResult {
                    loot_table_loot: HashMap::from([(table, generated_loot)]),
                }],
            )]),
            chest_generated_data: HashMap::new(),
            algorithm_version: 1,
            version: 0,
        };

        let grant = item_loot_grant(&action, &generated);
        assert_eq!(grant.stackable_items.get(&lumber), Some(&6));
    }

    /// Existing imported runs may predate generated item data. Their inline loot is a
    /// compatibility fallback; otherwise a player resuming one loses a valid pickup.
    #[test]
    fn floor_loot_falls_back_for_legacy_generated_data() {
        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();
        let mut request_loot = RewardGrant::default();
        request_loot.stackable_items.insert(lumber, 2);
        let action = LootCollectedUpdate {
            spawn_group_id: None,
            spawn_group_index: None,
            loot: request_loot,
        };
        let generated = DungeonGeneratedData {
            enemy_generated_data: HashMap::new(),
            item_generated_data: HashMap::new(),
            chest_generated_data: HashMap::new(),
            algorithm_version: 1,
            version: 0,
        };

        let grant = item_loot_grant(&action, &generated);
        assert_eq!(grant.stackable_items.get(&lumber), Some(&2));
    }

    /// A loot action with no `loot` block at all must still parse — the client omits it
    /// for an empty pickup, and a hard `loot` field would 400 the whole batch, which is
    /// the same class of bug as the old single-variant enum.
    #[test]
    fn a_loot_action_without_contents_still_parses() {
        let raw = r#"{
            "currentState": {"b64": "AAAA"},
            "actions": [{"type":"item_loot_collected","spawnGroupId":"e7edb276-a04c-413f-80ab-69ffe304874f","time":1}]
        }"#;
        let req: DungeonUpdateRequest = serde_json::from_str(raw).expect("must deserialize");
        match &req.actions[0] {
            DungeonUpdateAction::ItemLootCollected(c) => assert!(c.loot.is_empty()),
            other => panic!("expected ItemLootCollected(_), got {other:?}"),
        }
    }

    /// Parsing the action is only half of it — the loot has to land in the player's
    /// inventory and wallet. This drives the same `apply_reward` call the handler makes,
    /// so it fails if the credit is dropped rather than only if the parse is.
    #[test]
    fn collected_loot_is_credited_to_the_player() {
        use blades_lib::user_data::{
            Backpack, CompleteCharacter, CompleteInventory, CompleteWallet, Loadout, Treasury,
        };

        let raw = r#"{
            "currentState": {"b64": "AAAA"},
            "actions": [
                {"type":"item_loot_collected","spawnGroupId":"e7edb276-a04c-413f-80ab-69ffe304874f","spawnGroupIndex":0,
                 "loot":{"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":1}},"time":1},
                {"type":"enemy_loot_collected","spawnGroupId":"4295c814-e5e7-4a8a-939a-d3238471c906","spawnerIndex":0,"enemyIndex":0,
                 "loot":{"currencies":{"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":4}},"time":2}
            ]
        }"#;
        let req: DungeonUpdateRequest = serde_json::from_str(raw).unwrap();

        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();
        let gold: Uuid = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2".parse().unwrap();

        let mut wallet = CompleteWallet::default();
        let mut inventory = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        let mut character = CompleteCharacter::default();
        let mut tracker = InventoryChangeTracker::default();

        for action in &req.actions {
            if let DungeonUpdateAction::ItemLootCollected(c) = action {
                blades_lib::economy::apply_reward(
                    &c.loot,
                    &mut wallet,
                    &mut inventory,
                    &mut character,
                    &mut tracker,
                );
            }
        }

        assert_eq!(
            inventory.backpack.stackable_items.count(lumber),
            1,
            "floor loot must reach the backpack"
        );
        // Corpse gold is credited from stored state in the handler, not from this
        // payload, so it must NOT appear here.
        assert_eq!(wallet.balance(gold), 0, "the request's corpse loot must be ignored");
        assert!(
            tracker.modified_backpack.stackable_items.contains(&lumber),
            "the pickup must be reported to the client, or the bag looks unchanged"
        );
    }

    /// Corpse loot must not silently become nothing when the server has none.
    ///
    /// `generate_for_dungeon` sets `spawn_group_loot` and `loot_table_loot` to
    /// `HashMap::default()` and nothing fills them, so `merged_loot_table()` is always
    /// empty today. #138 made this arm read only the stored value, which reduced every
    /// looted corpse to a no-op. This pins the precedence: stored wins when it has
    /// contents, the request is used when it does not.
    #[test]
    fn corpse_loot_prefers_stored_and_falls_back_to_the_request() {
        use blades_lib::user_data::LootTableResult;

        let gold: Uuid = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2".parse().unwrap();
        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();

        // the handler's precedence rule, in the same shape as the code under test
        fn pick(stored: LootTableResult, from_request: RewardGrant) -> RewardGrant {
            let empty = stored.currencies.is_empty() && stored.stackable_items.is_empty();
            if empty {
                from_request
            } else {
                RewardGrant {
                    currencies: stored.currencies,
                    stackable_items: stored.stackable_items,
                    ..Default::default()
                }
            }
        }

        let mut req = RewardGrant::default();
        req.currencies.insert(gold, 4);

        // today's reality: nothing generated -> the request is honoured
        let got = pick(LootTableResult::default(), req.clone());
        assert_eq!(got.currencies.get(&gold), Some(&4),
                   "an empty stored table must not silently pay nothing");

        // once we DO generate loot, the request stops mattering
        let mut stored = LootTableResult::default();
        stored.stackable_items.insert(lumber, 7);
        let got = pick(stored, req.clone());
        assert_eq!(got.stackable_items.get(&lumber), Some(&7));
        assert!(got.currencies.is_empty(),
                "stored loot must win outright, not merge with the request");
    }

    /// Crediting the loot is not enough — the client applies a backpack diff only when
    /// `backpackVersion` moves.
    ///
    /// #136/#138 credited pickups correctly and the reporter still saw nothing, because
    /// the version never changed and the client discarded the delta. This asserts the
    /// version rule the handler now implements: it moves when something was granted,
    /// exactly once however many pickups are in the batch, and not at all when the batch
    /// granted nothing.
    #[test]
    fn a_granted_pickup_bumps_the_backpack_version_exactly_once() {
        use blades_lib::user_data::BackpackChangeTracker;

        // The handler's rule, in the same shape as the code under test.
        fn bump(tracker: &InventoryChangeTracker, version: &mut u64) {
            if !tracker.modified_backpack.stackable_items.is_empty()
                || !tracker.modified_backpack.items.is_empty()
            {
                *version += 1;
            }
        }

        let a: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();
        let b: Uuid = "38d32048-ce01-4390-a4f0-cdb94ef3ce72".parse().unwrap();

        // nothing collected -> version must not move, or every tick invalidates the bag
        let mut v = 7;
        bump(&InventoryChangeTracker::default(), &mut v);
        assert_eq!(v, 7, "an empty batch must not bump the version");

        // one pickup -> exactly one bump
        let mut t = InventoryChangeTracker::default();
        t.modified_backpack = BackpackChangeTracker::default();
        t.modified_backpack.stackable_items.insert(a);
        let mut v = 7;
        bump(&t, &mut v);
        assert_eq!(v, 8, "a granted pickup must bump the version");

        // two pickups in ONE batch -> still exactly one bump, not two
        t.modified_backpack.stackable_items.insert(b);
        let mut v = 7;
        bump(&t, &mut v);
        assert_eq!(v, 8, "a batch bumps once however many items it carried");
    }

    /// PRECONDITION for tracker #104: `apply_reward` does credit a non-stackable
    /// pickup, so passing the grant whole is sufficient.
    ///
    /// Honest about what this proves: it exercises `apply_reward`, which handled
    /// `items` all along — so it would have passed BEFORE the fix too. It is not
    /// the regression test. It establishes that the reward path is capable of
    /// crediting gear, which is what makes "pass the grant whole" a fix rather
    /// than a no-op. The test that discriminates is the source-level one below.
    #[test]
    fn a_gear_pickup_off_the_floor_reaches_the_backpack() {
        use blades_lib::economy::{RewardItem, apply_reward};
        use blades_lib::user_data::{
            Backpack, CompleteCharacter, CompleteInventory, CompleteWallet, Item, Loadout,
            Treasury,
        };

        let lumber: Uuid = "e7193116-d761-479b-8a20-5633737977f5".parse().unwrap();
        let gold: Uuid = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2".parse().unwrap();
        let sword_instance = Uuid::from_u128(0xABCD);
        let sword_template = Uuid::from_u128(0x1234);

        // The shape the client actually posts for a mixed floor pickup: a stackable,
        // some coin, and a piece of gear.
        let loot = RewardGrant {
            stackable_items: std::collections::HashMap::from([(lumber, 3)]),
            currencies: std::collections::HashMap::from([(gold, 25)]),
            items: vec![RewardItem {
                id: sword_instance,
                item: Item {
                    item_template_id: sword_template,
                    grade: None,
                    tempering_level: 0,
                    durability: 100.0,
                    properties: Default::default(),
                    arcane_tier: None,
                },
            }],
            ..RewardGrant::default()
        };

        let mut wallet = CompleteWallet::default();
        let mut inventory = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        let mut character = CompleteCharacter::default();
        let mut tracker = InventoryChangeTracker::default();
        apply_reward(&loot, &mut wallet, &mut inventory, &mut character, &mut tracker);

        // The defect: this is the assertion that failed before.
        assert!(
            inventory.backpack.items.0.contains_key(&sword_instance),
            "a gear pickup must land in the backpack, not be dropped"
        );
        // Controls: the two kinds that ALREADY worked must still work, so the test
        // is about `items` specifically and not about the reward path being broken.
        assert_eq!(inventory.backpack.stackable_items.count(lumber), 3);
        assert_eq!(wallet.balance(gold), 25);
        // And the tracker must name the item, or the client is never told to redraw.
        assert!(
            tracker.modified_backpack.items.contains(&sword_instance),
            "the pickup must be in the backpack diff, or the client discards it"
        );
    }

    /// Guard the whole-grant path for tracker #104.
    ///
    /// The bug was not in `apply_reward`: the handler rebuilt the grant by hand
    /// from two of its fields, so `items` (a weapon or armour lying on the dungeon
    /// floor) was parsed and then dropped. Catching that behaviourally would need a
    /// database and a whole request, so this checks the property at the source: the
    /// branch must resolve one complete grant and must not pick fields out of it.
    ///
    /// Source-level deliberately. The same approach caught the unregistered
    /// `/levelup` route, for the same reason — the defect is an omission, and an
    /// omission has no runtime signature to assert on.
    #[test]
    fn the_floor_pickup_branch_does_not_hand_copy_the_grant() {
        let src = include_str!("dungeon_update.rs");
        let start = src
            .find("DungeonUpdateAction::ItemLootCollected(item_loot) => {")
            .expect("the floor-pickup branch must exist");
        let end = src[start..]
            .find("DungeonUpdateAction::ChestCollected(chest)")
            .map(|i| start + i)
            .expect("the chest branch must follow it");
        let branch = &src[start..end];

        // Controls: the slice really is the branch, not an empty or runaway cut.
        assert!(branch.contains("apply_reward"), "slice missed the branch body");
        assert!(branch.len() < 4000, "slice ran past the branch: {} bytes", branch.len());

        assert!(
            !branch.contains("reward.stackable_items.insert")
                && !branch.contains("reward.currencies.insert"),
            "the floor-pickup branch is hand-copying the grant again — that is how \
             `items` got dropped in #104. Pass the whole grant instead."
        );
        assert!(branch.contains("item_loot_grant(item_loot, generated_data)"));
        assert!(
            branch.contains("&reward"),
            "the branch must pass the resolved whole grant to apply_reward"
        );
    }

    /// Retail sends `wallet` in the dungeon-update response only when the batch
    /// actually moved currency -- it is not an always-present field.
    ///
    /// Measured over the capture archive: of 29,569 update responses with a body,
    /// `wallet` is present in 6,534 of the 6,537 whose REQUEST carried `currencies`,
    /// and in 0 of the 23,032 that did not. Controls on the same rows: the keys we
    /// know are always there (`dungeonStatus`, `character`, `inventory`) appear
    /// 29,550 times each, so "0" above is a real absence and not a mis-aimed query.
    ///
    /// Serialising it unconditionally would hand the client a key retail never sent
    /// on that request -- the same "always serialize" assumption that produced the
    /// stub-character load stall.
    #[test]
    fn wallet_is_sent_only_when_currency_moved() {
        use blades_lib::user_data::{
            Backpack, CompleteCharacter, CompleteInventory, CompleteWallet, Loadout, Treasury,
        };

        let status: DungeonStatus = serde_json::from_str(
            r#"{"dungeonSettingsIds":[],"reviveCount":0,"algorithmVersion":1,
                "currentState":{"b64":"AAAA"},"enemyStatus":{},"seed":0,
                "level":1,"version":1}"#,
        )
        .expect("minimal dungeon status must deserialize");

        let inventory = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        let character = CompleteCharacter::default();

        let render = |wallet: Option<CompleteWallet>| {
            serde_json::to_value(DungeonUpdateResponse {
                inventory: inventory.generate_client_update(&InventoryChangeTracker::default()),
                character: CompleteCharacterWithIdWithoutData {
                    id: Uuid::nil(),
                    character: character.clone(),
                },
                dungeon_status: status.clone(),
                wallet,
            })
            .expect("response must serialize")
        };

        let quiet = render(None);
        assert!(
            quiet.get("wallet").is_none(),
            "a batch that moved no currency must not carry a wallet key, got {quiet:?}"
        );
        // control: the rest of the response is still there, so the assertion above
        // is about the wallet key and not about an empty object.
        assert!(quiet.get("dungeonStatus").is_some(), "the response must still be a response");

        let paid = render(Some(CompleteWallet::default()));
        assert!(
            paid.get("wallet").is_some(),
            "a batch that moved currency must carry the wallet, got {paid:?}"
        );
    }

    /// A `chest_collected` action must parse with its contents. Before #134 the
    /// variant did not exist at all, so every chest pickup fell into `Unknown`
    /// and was silently dropped — the same shape of bug as the floor loot in #95.
    #[test]
    fn chest_collected_parses_with_its_contents() {
        let raw = r#"{
            "currentState": {"b64": "AAAA"},
            "actions": [
                {"type":"chest_collected","spawnGroupId":"e7edb276-a04c-413f-80ab-69ffe304874f",
                 "spawnGroupIndex":0,"tier":3,"time":1777808410209}
            ]
        }"#;
        let req: DungeonUpdateRequest =
            serde_json::from_str(raw).expect("a chest pickup must deserialize");
        match &req.actions[0] {
            DungeonUpdateAction::ChestCollected(c) => {
                assert_eq!(c.spawn_group_index, 0);
                assert_eq!(c._tier, 3);
            }
            other => panic!("chest pickup must not be dropped, got {other:?}"),
        }
    }

    /// A collected chest must move `treasuryVersion`, or the client never sees it.
    ///
    /// #155 added the chest and reported it in `modified_treasury`, but the client
    /// applies a treasury diff only when the version moves — so the chest reached
    /// the database and the client hung opening a chest it did not believe it had
    /// (#104). The backpack had exactly this bug in #142.
    #[test]
    fn a_collected_chest_moves_the_treasury_version() {
        use blades_lib::user_data::InventoryChangeTracker;

        // the handler's rule, in the same shape as the code under test
        fn bump(tracker: &InventoryChangeTracker, version: &mut u64) {
            if !tracker.modified_treasury.added.is_empty() {
                *version += 1;
            }
        }

        let mut t = InventoryChangeTracker::default();
        let mut v = 3;
        bump(&t, &mut v);
        assert_eq!(v, 3, "no chest collected, no version change");

        t.modified_treasury.added.push("chest-1".to_string());
        let mut v = 3;
        bump(&t, &mut v);
        assert_eq!(v, 4, "a collected chest must move the version");

        // two chests in ONE batch still move it once, not twice — the same
        // double-bump that had to be undone in the town prop handler.
        t.modified_treasury.added.push("chest-2".to_string());
        let mut v = 3;
        bump(&t, &mut v);
        assert_eq!(v, 4, "a batch bumps once however many chests it carried");
    }

    /// Collecting the same indexed chest twice must mint it once.
    ///
    /// The guard is `collected_chests`, which is persisted with the dungeon state,
    /// so it holds across requests as well as within a batch — a client that
    /// replays its last update, or taps twice, does not double its treasury.
    #[test]
    fn each_generated_chest_is_only_ever_collected_once() {
        use std::collections::HashSet;
        let chest_a: Uuid = "e7edb276-a04c-413f-80ab-69ffe304874f".parse().unwrap();
        let chest_b: Uuid = "4295c814-e5e7-4a8a-939a-d3238471c906".parse().unwrap();

        // the handler's rule, in the same shape as the code under test
        let mut collected: HashSet<String> = HashSet::new();
        let mut minted = 0;
        for (id, index) in [(chest_a, 0), (chest_a, 0), (chest_a, 1), (chest_b, 0)] {
            if collected.insert(collected_chest_key(id, index)) {
                minted += 1;
            }
        }
        assert_eq!(
            minted, 3,
            "two indexes in one group and one in another must each mint once"
        );

        // control: without the guard every action mints, which is the bug
        assert_eq!(
            [(chest_a, 0), (chest_a, 0), (chest_a, 1), (chest_b, 0)].len(),
            4,
            "the unguarded count differs from the guarded one, so the test is not vacuous"
        );
    }
}
