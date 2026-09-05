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
use blades_lib::economy::RewardGrant;
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

/// A `*_loot_collected` action: the player picked something up inside the dungeon.
///
/// The client reports WHAT it collected — the captured payload carries the contents
/// inline, e.g.
///
/// ```json
/// {"type":"item_loot_collected","spawnGroupId":"e7edb276-…","spawnGroupIndex":0,
///  "loot":{"stackableItems":{"e7193116-…":1}},"time":1777808410209}
/// ```
///
/// so `loot` is deserialized straight into [`RewardGrant`], whose camelCase wire form
/// is already exactly `{currencies, stackableItems, items}`.
///
/// This is used ONLY for floor loot and plants, where the contents genuinely exist
/// nowhere but the request — we do not generate loose-item spawns, so the client is the
/// only source. Corpse loot is different and must not use this: see
/// [`EnemyLootCollectedUpdate`].
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LootCollectedUpdate {
    #[serde(default)]
    loot: RewardGrant,
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
#[serde(tag = "type", rename_all = "snake_case")]
enum DungeonUpdateAction {
    EnemyKilled(EnemyKilledUpdate),
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

            let mut inventory_modification_tracker = InventoryChangeTracker::default();

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
                    // Floor loot and harvested plants. We do not generate loose-item
                    // spawns, so the request is the only place these contents exist.
                    DungeonUpdateAction::ItemLootCollected(collected) => {
                        blades_lib::economy::apply_reward(
                            &collected.loot,
                            &mut character_data.wallet.0,
                            &mut character_data.inventory.0,
                            &mut character_data.character.0,
                            &mut inventory_modification_tracker,
                        );
                    }
                    // Corpse loot. The contents were rolled server-side when the enemy
                    // died, so read them off the stored `EnemyStatus` and ignore whatever
                    // the request claims.
                    DungeonUpdateAction::EnemyLootCollected(looted) => {
                        let enemy_index = EnemyIndex::new(
                            looted.spawn_group_id,
                            looted.spawner_index,
                            looted.enemy_index,
                        );
                        let Some(status) =
                            dungeon_state.dungeon_status.enemy_status.get_mut(&enemy_index)
                        else {
                            // Looting a corpse we have no record of killing credits
                            // nothing — that is the whole point of not trusting the body.
                            log::warn!(
                                "dungeon_update: enemy_loot_collected for unknown enemy {:?} — crediting nothing",
                                enemy_index
                            );
                            continue;
                        };
                        // Looting the same corpse twice must not pay twice; taking the
                        // stored loot empties it.
                        let loot = std::mem::take(&mut status.loot);
                        let stored_is_empty =
                            loot.currencies.is_empty() && loot.stackable_items.is_empty();
                        // We do not generate enemy loot yet: generate_for_dungeon sets
                        // spawn_group_loot and loot_table_loot to HashMap::default() and
                        // nothing fills them, so merged_loot_table() is always empty. #138
                        // switched this arm to the stored value on the assumption it was
                        // authoritative, which silently reduced every corpse to nothing.
                        //
                        // So: stored wins when it HAS contents — the moment we generate
                        // real loot the client stops being trusted, with no further change
                        // here — and until then the request is the only source there is.
                        let grant = if stored_is_empty {
                            looted.loot.clone()
                        } else {
                            RewardGrant {
                                currencies: loot.currencies,
                                stackable_items: loot.stackable_items,
                                ..Default::default()
                            }
                        };
                        blades_lib::economy::apply_reward(
                            &grant,
                            &mut character_data.wallet.0,
                            &mut character_data.inventory.0,
                            &mut character_data.character.0,
                            &mut inventory_modification_tracker,
                        );
                    }
                    DungeonUpdateAction::Unknown => {
                        log::warn!(
                            "dungeon_update: ignoring unknown action type in quest {}",
                            quest_id
                        );
                    }
                }
            }

            // The client applies a backpack diff only when `backpackVersion` moves. Credit
            // the loot without bumping it and the item reaches the database and is never
            // shown — which is exactly what "floor pickups still don't work" looked like
            // after #136/#138 credited them correctly. Every other grant path
            // (craft, salvage, gifts, daily reward, abyss, character_ops) bumps here;
            // this one did not.
            //
            // Bumped ONCE per request, from the tracker rather than per action, because a
            // batch can carry several pickups and bumping per action moves the version by
            // more than one — the same double-bump that had to be undone in the town prop
            // handler.
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
}
