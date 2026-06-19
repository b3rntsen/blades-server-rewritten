//! Chests — `POST /chests/{id}/collect`.
//!
//! Open a treasury chest for loot. We draw a representative loot bundle from a
//! capture-derived pool (deterministic per chest id — per-tier loot tables aren't
//! captured), re-mint the instanced item ids (capture ids would collide across
//! players), grant it, and remove the chest. See [`blades_lib::features::chests`].

use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, apply_reward, remove_chest};
use blades_lib::features::chests;
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
};

const CHEST_SERVICE_ID: u64 = 9007;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectResponse {
    reward: RewardGrant,
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/chests/{chest_id}/collect")]
pub async fn collect_chest(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, String)>,
) -> Result<Json<CollectResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, chest_id) = path.into_inner();
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

            // The chest must exist in the treasury.
            if entry.inventory.0.treasury.get_chest(&chest_id).is_none() {
                return Err(BladeApiError::new(StatusCode::NOT_FOUND, CHEST_SERVICE_ID, 1));
            }

            // Representative loot for this chest; re-mint instanced item ids.
            let mut reward = chests::pick_loot(&globals.static_data.chest_loots, &chest_id)
                .cloned()
                .unwrap_or_default();
            for item in &mut reward.items {
                item.id = Uuid::new_v4();
            }

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
            remove_chest(&mut entry.inventory.0, &chest_id, &mut tracker);
            entry.inventory.0.treasury_version += 1;

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();

            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(CollectResponse {
                reward,
                wallet,
                inventory,
            }))
        }
        .scope_boxed()
    })
    .await
}
