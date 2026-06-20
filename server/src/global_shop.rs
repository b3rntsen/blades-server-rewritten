//! Global store — `GET /catalogoverrides/globalshop`, `GET /catalogoverrides/iap`,
//! `GET /…/globalshops/current`, `POST /…/globalshops/current/purchase`.
//!
//! The Sigil/Gem sink. The override catalogue and IAP catalogue are served verbatim
//! from capture-derived JSON; a purchase debits the client-supplied (and
//! sanity-checked) price for real, grants the capture-derived product reward, and
//! bumps the per-character purchase count. IAP (real money) is a priced placeholder
//! only — there is no fulfillment route. See [`blades_lib::features::global_shop`].

use std::sync::Arc;

use actix_web::{
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{Price, RewardGrant, apply_reward};
use blades_lib::features::global_shop::{self, PurchaseEntry, PurchaseError};
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    models::{CharacterDbEntryEconomy, CharacterDbEntryServerState},
    session::SessionLookedUpMaybe,
    util::get_only_single_character_and_check_permission,
};

/// Out-of-band service id for global-shop error envelopes (not a real Blades id).
const SHOP_SERVICE_ID: u64 = 9004;

fn map_purchase_err(e: PurchaseError) -> BladeApiError {
    match e {
        PurchaseError::NoSuchProduct => {
            BladeApiError::new(StatusCode::NOT_FOUND, SHOP_SERVICE_ID, 1)
        }
        PurchaseError::InvalidPrice => {
            BladeApiError::new(StatusCode::BAD_REQUEST, SHOP_SERVICE_ID, 2)
        }
    }
}

/// `GET /catalogoverrides/globalshop` — the override catalogue, served verbatim.
#[get("/blades.bgs.services/api/game/v1/public/catalogoverrides/globalshop")]
pub async fn get_override(app_state: web::Data<Arc<ServerGlobal>>) -> Json<Value> {
    Json(app_state.static_data.global_shop_overrides.clone())
}

/// `GET /catalogoverrides/iap` — real-money SKU catalogue, served verbatim (priced
/// placeholders, all inactive; no purchase flow exists).
#[get("/blades.bgs.services/api/game/v1/public/catalogoverrides/iap")]
pub async fn get_iap(app_state: web::Data<Arc<ServerGlobal>>) -> Json<Value> {
    Json(app_state.static_data.iap.clone())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GlobalShopState {
    global_shop_purchases: Vec<PurchaseEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GlobalShopForCharacterResponse {
    global_shop: GlobalShopState,
}

/// `GET /…/globalshops/current` — this character's per-product purchase counts.
#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/globalshops/current")]
pub async fn get_global_shop_for_character(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GlobalShopForCharacterResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    let rows = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(id.eq(character_id))
            .select(CharacterDbEntryServerState::as_select())
            .load(&mut conn)
            .await
            .unwrap()
    };
    let entry = get_only_single_character_and_check_permission(rows, &session.session)?;
    Ok(Json(GlobalShopForCharacterResponse {
        global_shop: GlobalShopState {
            global_shop_purchases: global_shop::purchases_list(
                &entry.server_state.0.global_shop_purchases,
            ),
        },
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PurchaseRequest {
    global_shop_product_id: Uuid,
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
    #[serde(default)]
    expected_prices: Vec<Price>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PurchaseResponse {
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
    global_shop: GlobalShopState,
    reward: RewardGrant,
}

/// `POST /…/globalshops/current/purchase` — buy a global-shop product: validate the
/// client price, debit it, grant the product, bump the purchase count.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/globalshops/current/purchase")]
pub async fn purchase_global_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<PurchaseRequest>,
) -> Result<Json<PurchaseResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();

    // What this product grants (capture-derived). Unknown product → can't fulfill.
    let reward = app_state
        .static_data
        .global_shop_grants
        .get(&body.global_shop_product_id)
        .cloned()
        .ok_or_else(|| map_purchase_err(PurchaseError::NoSuchProduct))?;
    global_shop::sanitize_prices(&body.expected_prices).map_err(map_purchase_err)?;

    let product_id = body.global_shop_product_id;
    let prices = body.expected_prices;
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

            // Charge the (validated) price; fail on insufficient funds.
            entry
                .wallet
                .0
                .try_pay(&prices)
                .map_err(BladeApiError::from_economy)?;

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
            *entry
                .server_state
                .0
                .global_shop_purchases
                .entry(product_id)
                .or_insert(0) += 1;

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let global_shop_purchases =
                global_shop::purchases_list(&entry.server_state.0.global_shop_purchases);

            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(PurchaseResponse {
                inventory,
                wallet,
                global_shop: GlobalShopState {
                    global_shop_purchases,
                },
                reward,
            }))
        }
        .scope_boxed()
    })
    .await
}
