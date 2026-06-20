//! Town vendor shops — `POST /shops/{id}` (open), `/shops/{id}/auth/refreshloot`,
//! `/shops/{id}/purchase` (buy), `/shops/{id}/sell`.
//!
//! Opening a shop was previously unhandled → the smith/store screen 404'd and the
//! client hung (empty lists + timeout). Open now returns a valid catalog: the client
//! renders the bundle items/prices from its own asset data, so the server just lists
//! the in-stock bundle ids + a FUTURE `expiration` (a past one makes the client refetch
//! → the hang). The catalog is capture-derived: a shopId's mapped template, else a
//! default template (never empty). Buy/sell mutate gold + inventory via the economy core.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{GOLD, apply_reward};
use blades_lib::static_data::{ShopBundleRef, ShopWalletEntry};
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
};

/// Catalog validity window (the client refetches once `expiration` passes).
const CATALOG_WINDOW_MS: i64 = 6 * 3600 * 1000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShopStateWire {
    id: Uuid,
    catalog_id: Uuid,
    sales: Vec<Value>,
    revenue: Vec<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogWire {
    id: Uuid,
    template_id: Uuid,
    bundles: Vec<ShopBundleRef>,
    wallet: Vec<ShopWalletEntry>,
    start: i64,
    expiration: i64,
    expired: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenShopResponse {
    shop: ShopStateWire,
    catalog: CatalogWire,
}

/// Build the open/refresh catalog for a shop (capture-derived template, fresh window).
fn build_open(app_state: &ServerGlobal, shop_id: Uuid) -> OpenShopResponse {
    let template_id = app_state
        .static_data
        .shop_data
        .template_for(&shop_id)
        .unwrap_or_else(Uuid::nil);
    let cat = app_state
        .static_data
        .shop_data
        .catalog_for(&shop_id)
        .cloned()
        .unwrap_or_default();
    let start = now_ms();
    // The client binds the shop to its catalog by id: `shop.catalogId` MUST equal
    // `catalog.id` (verified in captures — both are the same value per open). Using two
    // independent UUIDs here left the client unable to resolve the catalog → every
    // vendor (smith/enchanter/alchemist) rendered an EMPTY shop/craft/temper/repair list.
    let catalog_id = Uuid::new_v4();
    OpenShopResponse {
        shop: ShopStateWire {
            id: shop_id,
            catalog_id,
            sales: vec![],
            revenue: vec![],
        },
        catalog: CatalogWire {
            id: catalog_id,
            template_id,
            bundles: cat.bundles,
            wallet: cat.wallet,
            start,
            expiration: start + CATALOG_WINDOW_MS,
            expired: false,
        },
    }
}

/// `POST /shops/{id}` — open a vendor (returns its current catalog). Session-only (no
/// DB dependency) so it can never 404/hang the storefront.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}")]
pub async fn open_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<OpenShopResponse>, BladeApiError> {
    session.get_session_or_error()?;
    let (_character_id, shop_id) = path.into_inner();
    Ok(Json(build_open(&app_state, shop_id)))
}

/// `POST /shops/{id}/auth/refreshloot` — re-roll the catalog (same shape as open).
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}/auth/refreshloot"
)]
pub async fn refresh_loot(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<OpenShopResponse>, BladeApiError> {
    session.get_session_or_error()?;
    let (_character_id, shop_id) = path.into_inner();
    Ok(Json(build_open(&app_state, shop_id)))
}

#[derive(Deserialize)]
struct BuyBundle {
    id: Uuid,
    #[serde(default)]
    quantity: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuyRequest {
    #[serde(default)]
    bundles: Vec<BuyBundle>,
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SaleEntry {
    id: Uuid,
    quantity: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RevenueEntry {
    currency_id: Uuid,
    balance: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShopTxnState {
    id: Uuid,
    sales: Vec<SaleEntry>,
    revenue: Vec<RevenueEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BuyResponse {
    shop: ShopTxnState,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
}

/// `POST /shops/{id}/purchase` — buy bundles. Known bundles (capture-derived
/// price+grant) are charged and granted; unknown ones are skipped (we can't price them
/// — the base list lives in the client bundles).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}/purchase")]
pub async fn buy_from_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<BuyRequest>,
) -> Result<Json<BuyResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, shop_id) = path.into_inner();
    let bundles = body.into_inner().bundles;
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            let mut sales = Vec::new();
            let mut spent: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();

            for b in &bundles {
                let qty = b.quantity.max(1);
                let Some(def) = globals.static_data.shop_bundles.get(&b.id) else {
                    continue; // unknown bundle — can't price/grant faithfully, skip
                };
                let currency = def.currency_id.unwrap_or(GOLD);
                let cost = def.price.saturating_mul(qty);
                entry
                    .wallet
                    .0
                    .debit(currency, cost)
                    .map_err(BladeApiError::from_economy)?;
                *spent.entry(currency).or_insert(0) += cost as i64;
                // Grant the bundle's reward, scaled by quantity.
                let mut reward = def.grant.clone();
                for v in reward.stackable_items.values_mut() {
                    *v = v.saturating_mul(qty);
                }
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                sales.push(SaleEntry { id: b.id, quantity: qty });
            }
            entry.inventory.0.backpack_version += 1;

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(BuyResponse {
                shop: ShopTxnState {
                    id: shop_id,
                    sales,
                    revenue: spent
                        .into_iter()
                        .map(|(currency_id, balance)| RevenueEntry { currency_id, balance })
                        .collect(),
                },
                inventory,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SellRequest {
    #[serde(default)]
    items: Vec<Uuid>,
    #[serde(default)]
    stackable_items: std::collections::HashMap<Uuid, u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SellResponse {
    shop: ShopTxnState,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
}

/// Nominal sell price per instanced item / per stackable unit. Retail prices scale with
/// the item's value (not captured), so this is a flat placeholder — documented.
const SELL_PRICE_ITEM: u64 = 50;
const SELL_PRICE_STACK: u64 = 5;

/// `POST /shops/{id}/sell` — sell gear/materials for gold (nominal price; see above).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}/sell")]
pub async fn sell_to_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<SellRequest>,
) -> Result<Json<SellResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, shop_id) = path.into_inner();
    let req = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            let mut gold: u64 = 0;

            for item_id in &req.items {
                if entry.inventory.0.backpack.items.0.remove(item_id).is_some() {
                    tracker.modified_backpack.items.insert(*item_id);
                    gold += SELL_PRICE_ITEM;
                }
            }
            for (template, count) in &req.stackable_items {
                if entry.inventory.0.backpack.stackable_items.remove(*template, *count).is_ok() {
                    tracker.modified_backpack.stackable_items.insert(*template);
                    gold += SELL_PRICE_STACK.saturating_mul(*count);
                }
            }
            entry.wallet.0.credit(GOLD, gold);
            entry.inventory.0.backpack_version += 1;

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(SellResponse {
                shop: ShopTxnState {
                    id: shop_id,
                    sales: vec![],
                    revenue: vec![RevenueEntry {
                        currency_id: GOLD,
                        balance: -(gold as i64),
                    }],
                },
                inventory,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}

async fn load_owned(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
    user_id: Uuid,
) -> Result<CharacterDbEntryEconomy, BladeApiError> {
    use crate::schema::characters;
    characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryEconomy::as_select())
        .for_no_key_update()
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))
}

async fn write_back(
    conn: &mut diesel_async::AsyncPgConnection,
    entry: CharacterDbEntryEconomy,
) -> Result<(), BladeApiError> {
    use crate::schema::characters;
    diesel::update(characters::table)
        .filter(characters::id.eq(entry.id))
        .set(entry)
        .execute(conn)
        .await?;
    Ok(())
}
