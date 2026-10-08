//! Salvage — `POST /salvages`.
//!
//! Break gear into crafting materials at the smithy: remove each salvaged item (from
//! the backpack or the loadout) and grant the APK's yield for it at the item's
//! tempering level. Yield logic is the pure [`blades_lib::features::salvage`] layer.

use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, apply_reward};
use blades_lib::features::salvage;
use blades_lib::user_data::{CompleteInventory, CompleteInventoryUpdate, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SalvageInfo {
    recipe_id: Uuid,
    item_id: Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SalvageRequest {
    #[serde(default)]
    salvage_infos: Vec<SalvageInfo>,
    #[serde(default)]
    #[allow(dead_code)]
    building_id: Option<Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SalvageResponse {
    reward: RewardGrant,
    inventory: CompleteInventoryUpdate,
}

/// Remove an instanced item by id from the backpack or the equipped loadout, marking
/// the change. Returns `(itemTemplateId, temperingLevel)` of what was removed — the
/// two things the salvage yield depends on.
fn remove_owned_item(
    inv: &mut CompleteInventory,
    item_id: Uuid,
    tracker: &mut InventoryChangeTracker,
) -> Option<(Uuid, u64)> {
    if let Some(item) = inv.backpack.items.0.remove(&item_id) {
        tracker.modified_backpack.items.insert(item_id);
        return Some((item.item_template_id, item.tempering_level));
    }
    let slot = inv
        .loadout
        .equipped_items
        .0
        .iter()
        .find(|(_, e)| e.id == item_id)
        .map(|(s, _)| *s);
    let removed = inv.loadout.equipped_items.0.remove(&slot?)?;
    tracker
        .modified_loadout
        .modified_equipped_items
        .insert(removed.slot);
    Some((removed.item.item_template_id, removed.item.tempering_level))
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/salvages")]
pub async fn salvage_items(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<SalvageRequest>,
) -> Result<Json<SalvageResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let infos = body.into_inner().salvage_infos;
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
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

            let mut tracker = InventoryChangeTracker::default();
            let mut salvaged = Vec::new();
            for info in &infos {
                if let Some((item_template_id, tempering_level)) =
                    remove_owned_item(&mut entry.inventory.0, info.item_id, &mut tracker)
                {
                    salvaged.push(salvage::SalvagedItem {
                        recipe_id: info.recipe_id,
                        item_template_id,
                        tempering_level,
                    });
                }
            }
            // ThreadRng is not Send: keep it out of scope across the awaits below.
            let reward = {
                let mut rng = rand::rng();
                salvage::salvage_reward(
                    &salvaged,
                    &globals.static_data.salvage_recipes,
                    &mut |lo, hi| rng.random_range(lo..=hi),
                )
            };
            apply_reward(
                &reward,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut entry.character.0,
                &mut tracker,
            );
            entry.inventory.0.backpack_version += 1;

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }
            Ok::<_, BladeApiError>(Json(SalvageResponse { reward, inventory }))
        }
        .scope_boxed()
    })
    .await
}
