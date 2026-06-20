//! Item repair — `POST /…/characters/{id}/repairs`.
//!
//! In Elder Scrolls: Blades gear loses durability and eventually breaks; the
//! player repairs it at the blacksmith. Captured request/response (prod):
//!
//! ```jsonc
//! // request
//! { "repairInfos": [ { "recipeId": "<uuid>", "itemId": "<uuid>" }, … ],
//!   "buildingId": "<smithy-uuid>", "gemsPayment": false }
//! // response
//! { "inventory": <CompleteInventoryUpdate>, "wallet": [ { currencyId, balance } ] }
//! ```
//!
//! We restore each listed item's `durability` to its full value (the max for its
//! `(itemTemplateId, temperingLevel)`, from the captures-derived
//! `item_max_durability` lookup, since `GameData` carries no durability) and
//! return the inventory diff plus the current wallet. `recipeId` / `buildingId` /
//! `gemsPayment` are accepted but unused; gold is not charged (the player has
//! ample, and there is no recipe→cost table in `game_data` yet).

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::user_data::{
    CompleteInventory, CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker, Item,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryCharacterWalletInventory,
    session::SessionLookedUpMaybe,
};

/// Out-of-band service id for repair error envelopes (not a real Blades service
/// id; the only failure path that fires in practice is "blacksmith busy", which
/// the emulator never hits today — see [`blacksmith_has_free_slot`]).
const REPAIR_SERVICE_ID: u64 = 9002;

/// The blacksmith has two work slots; repair is blocked only when both are busy
/// crafting/tempering.
const BLACKSMITH_SLOTS: usize = 2;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RepairInfo {
    /// The repair recipe (which materials/cost). Accepted from the client but
    /// unused: we restore durability directly by `item_id`.
    #[serde(default)]
    #[allow(dead_code)]
    recipe_id: Option<Uuid>,
    item_id: Uuid,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RepairRequest {
    repair_infos: Vec<RepairInfo>,
    #[serde(default)]
    #[allow(dead_code)]
    building_id: Option<Uuid>,
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairResponse {
    inventory: CompleteInventoryUpdate,
    /// `CompleteWallet` (de)serializes as a bare ARRAY of `{currencyId, balance}`.
    wallet: CompleteWallet,
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/repairs")]
pub async fn repair_items(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<RepairRequest>,
) -> Result<Json<RepairResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    // `globals` is moved into the transaction closure (for the durability lookup),
    // so take `conn` from the `app_state` Data handle — not from `globals` — to
    // avoid borrowing the value we move.
    let globals: Arc<ServerGlobal> = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the requesting user's character (ownership enforced by the
            // user_id filter), locking the row for the read-modify-write.
            let mut character_data = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryCharacterWalletInventory::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Real-game rule: repair needs a free blacksmith slot (the smith can't
            // repair while crafting/tempering). The emulator persists no craft jobs,
            // so the smith is always free; gate kept here so it activates once
            // crafting is modeled.
            if !blacksmith_has_free_slot() {
                return Err(BladeApiError::new(StatusCode::CONFLICT, REPAIR_SERVICE_ID, 1));
            }

            let mut tracker = InventoryChangeTracker::default();
            apply_repairs(
                &mut character_data.inventory.0,
                &body.repair_infos,
                &globals.item_max_durability,
                &mut tracker,
            );
            // Captures show backpackVersion increments on a repair.
            character_data.inventory.0.backpack_version += 1;

            // Build the response before writing back (mirrors dungeon_update).
            let update = character_data
                .inventory
                .0
                .generate_client_update(&tracker);
            let wallet = character_data.wallet.0.clone();

            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(character_data.id))
                    .set(character_data)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(RepairResponse {
                inventory: update,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}

/// Number of blacksmith jobs currently occupying a work slot. The emulator does
/// not persist craft/temper jobs yet (`craft::get_crafts` is empty), so this is
/// always 0. When crafting lands, count active jobs (`completedAt` > now) at the
/// blacksmith building here.
fn active_blacksmith_jobs() -> usize {
    0
}

fn blacksmith_has_free_slot() -> bool {
    active_blacksmith_jobs() < BLACKSMITH_SLOTS
}

/// Restore each requested item to full durability, marking it in the change
/// tracker so the response carries the diff. Items may live in the equipped
/// loadout (keyed by slot) or the backpack (keyed by item id); unknown ids are
/// skipped (the client may send stale ids).
fn apply_repairs(
    inventory: &mut CompleteInventory,
    repair_infos: &[RepairInfo],
    durability: &HashMap<String, HashMap<String, f64>>,
    tracker: &mut InventoryChangeTracker,
) {
    for info in repair_infos {
        if let Some(equipped) = inventory
            .loadout
            .equipped_items
            .0
            .values_mut()
            .find(|e| e.id == info.item_id)
        {
            restore_durability(&mut equipped.item, durability);
            tracker
                .modified_loadout
                .modified_equipped_items
                .insert(equipped.slot);
        } else if let Some(item) = inventory.backpack.items.0.get_mut(&info.item_id) {
            restore_durability(item, durability);
            tracker.modified_backpack.items.insert(info.item_id);
        }
    }
}

fn restore_durability(item: &mut Item, durability: &HashMap<String, HashMap<String, f64>>) {
    match durability
        .get(&item.item_template_id.to_string())
        .and_then(|m| m.get(&item.tempering_level.to_string()))
    {
        Some(max) => item.durability = *max,
        None => log::warn!(
            "[repair] no max durability for template {} tempering {}; leaving as-is",
            item.item_template_id,
            item.tempering_level
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::user_data::{
        Backpack, CompleteInventory, Item, ItemPropertiesAll, Loadout, SingleEquippedItem, Treasury,
    };

    #[test]
    fn repairs_equipped_and_backpack_items_to_max_and_tracks_them() {
        let template = Uuid::new_v4();
        let equipped_id = Uuid::new_v4();
        let slot = Uuid::new_v4();
        let backpack_id = Uuid::new_v4();

        let mut inv = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        let mk = |durability: f64| Item {
            item_template_id: template,
            tempering_level: 10,
            durability,
            properties: ItemPropertiesAll::default(),
        };
        inv.loadout.equipped_items.0.insert(
            slot,
            SingleEquippedItem {
                id: equipped_id,
                slot,
                item: mk(1.0), // broken
            },
        );
        inv.backpack.items.0.insert(backpack_id, mk(2.0)); // broken

        let mut durability: HashMap<String, HashMap<String, f64>> = HashMap::new();
        durability
            .entry(template.to_string())
            .or_default()
            .insert("10".to_string(), 675.0);

        let mut tracker = InventoryChangeTracker::default();
        apply_repairs(
            &mut inv,
            &[
                RepairInfo {
                    recipe_id: None,
                    item_id: equipped_id,
                },
                RepairInfo {
                    recipe_id: None,
                    item_id: backpack_id,
                },
            ],
            &durability,
            &mut tracker,
        );

        assert_eq!(inv.loadout.equipped_items.0[&slot].item.durability, 675.0);
        assert_eq!(inv.backpack.items.0[&backpack_id].durability, 675.0);
        assert!(
            tracker
                .modified_loadout
                .modified_equipped_items
                .contains(&slot)
        );
        assert!(tracker.modified_backpack.items.contains(&backpack_id));

        // The response diff carries both repaired items + the bumped version.
        inv.backpack_version += 1;
        let update = inv.generate_client_update(&tracker);
        assert_eq!(update.backpack_version, 2);
        assert!(update.loadout.equipped_items.0.contains_key(&slot));
        assert!(update.backpack.items.0.contains_key(&backpack_id));
    }

    #[test]
    fn unknown_template_leaves_durability_unchanged() {
        let mut item = Item {
            item_template_id: Uuid::new_v4(),
            tempering_level: 3,
            durability: 5.0,
            properties: ItemPropertiesAll::default(),
        };
        restore_durability(&mut item, &HashMap::new());
        assert_eq!(item.durability, 5.0);
    }
}
