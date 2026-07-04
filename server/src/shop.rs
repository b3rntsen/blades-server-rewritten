//! Town vendor shops — `POST /shops/{id}` (open), `/shops/{id}/auth/refreshloot`,
//! `/shops/{id}/purchase` (buy), `/shops/{id}/sell`.
//!
//! Opening a shop was previously unhandled → the smith/store screen 404'd and the
//! client hung (empty lists + timeout). Open now returns a valid catalog: the client
//! renders the bundle items/prices from its own asset data, so the server just lists
//! the in-stock bundle ids + a FUTURE `expiration` (a past one makes the client refetch
//! → the hang). Buy/sell mutate gold + inventory via the economy core.
//!
//! Stock is generated in two tiers, best-first:
//! 1. **Authored per-level generation** ([`crate::shop_gen`]) — the `shop_id` is the
//!    character's building INSTANCE id, so we resolve its `typeId` + current `level`
//!    from the stored town and roll a level-appropriate catalog from `shop_stock.json`
//!    (deterministic per shop + refresh window). This is what makes a Forge/Alchemist/
//!    Enchanter/Workshop actually stock level-appropriate items.
//! 2. **Capture-derived template** fallback — if the shop isn't one of the 4 crafting
//!    vendors, or the town/level can't be resolved, or the config lacks that
//!    building/level, we serve a captured template (a shopId's mapped template, else a
//!    default). A vendor is thus NEVER empty/timing-out.

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
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy, shop_gen,
    session::SessionLookedUpMaybe,
};

/// Default catalog validity window (the client refetches once `expiration` passes).
/// Used when the shop isn't a config-driven crafting vendor; config-driven shops use
/// their level's `refreshSeconds`.
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

/// Build the open/refresh catalog for a shop.
///
/// `building` = the resolved `(typeId, level)` when `shop_id` is one of the character's
/// crafting-vendor buildings; `None` when we couldn't resolve it (no town / not a
/// crafting vendor). Tier 1: if `building` is set and the authored config produces a
/// non-empty catalog, serve the GENERATED, level-appropriate stock. Tier 2 (fallback):
/// serve the capture-derived template so the vendor is never empty.
fn build_open(
    app_state: &ServerGlobal,
    shop_id: Uuid,
    building: Option<(Uuid, u64)>,
) -> OpenShopResponse {
    let start = now_ms();

    // Tier 1 — authored per-level generation (crafting vendors we can resolve).
    if let Some((type_id, level)) = building {
        let refresh_s = app_state
            .shop_stock
            .refresh_seconds(&type_id, level)
            .unwrap_or(CATALOG_WINDOW_MS / 1000);
        let window = shop_gen::window_index(start, refresh_s);
        let bundles =
            shop_gen::generate_catalog(&app_state.shop_stock, &type_id, level, &shop_id, window);
        if !bundles.is_empty() {
            // The client binds shop↔catalog by id: `shop.catalogId` MUST equal
            // `catalog.id` (both are the same value per open in captures) or the client
            // can't resolve the catalog and renders an EMPTY list.
            let catalog_id = Uuid::new_v4();
            return OpenShopResponse {
                shop: ShopStateWire {
                    id: shop_id,
                    catalog_id,
                    sales: vec![],
                    revenue: vec![],
                },
                catalog: CatalogWire {
                    id: catalog_id,
                    // No capture template drives generated stock; use the building
                    // typeId as the (informational) template id.
                    template_id: type_id,
                    bundles,
                    // The vendor's own wallet is cosmetic (buyback funds); leave empty
                    // — the client tolerates it and prices come from its asset data.
                    wallet: vec![],
                    start,
                    expiration: start + refresh_s * 1000,
                    expired: false,
                },
            };
        }
    }

    // Tier 2 — capture-derived template fallback (never empty).
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

/// Resolve `shop_id` (a building INSTANCE id) to its `(typeId, level)` by scanning the
/// character's stored town. Returns `None` on any miss (no town / DB error, or the shop
/// id isn't a known building) so the caller falls back to the capture templates.
/// Read-only, ownership-checked; never mutates.
async fn resolve_building(
    app_state: &ServerGlobal,
    user_id: Uuid,
    character_id: Uuid,
    shop_id: Uuid,
) -> Option<(Uuid, u64)> {
    use crate::schema::characters;
    let mut conn = app_state.db_pool.get().await.ok()?;

    let entry: crate::models::CharacterDbEntryTown = characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(user_id))
        .select(crate::models::CharacterDbEntryTown::as_select())
        .load(&mut conn)
        .await
        .ok()?
        .into_iter()
        .next()?;

    let town = entry.town?.0;
    find_building_type_level(&town, shop_id)
}

/// Walk `town.districts[].segments{}.buildings{}` for the building whose `id` equals
/// `shop_id`, returning its `(typeId, level)`. Pure — unit-tested below.
fn find_building_type_level(town: &Value, shop_id: Uuid) -> Option<(Uuid, u64)> {
    let target = shop_id.to_string();
    fn walk(node: &Value, target: &str) -> Option<(Uuid, u64)> {
        match node {
            Value::Object(map) => {
                if map.get("id").and_then(Value::as_str) == Some(target)
                    && map.contains_key("typeId")
                {
                    let type_id = map
                        .get("typeId")
                        .and_then(Value::as_str)
                        .and_then(|s| Uuid::parse_str(s).ok())?;
                    let level = map.get("level").and_then(Value::as_u64).unwrap_or(0);
                    return Some((type_id, level));
                }
                map.values().find_map(|v| walk(v, target))
            }
            Value::Array(arr) => arr.iter().find_map(|v| walk(v, target)),
            _ => None,
        }
    }
    walk(town, &target)
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
    let session = session.get_session_or_error()?;
    let (character_id, shop_id) = path.into_inner();
    let building =
        resolve_building(&app_state, session.session.user_id, character_id, shop_id).await;
    Ok(Json(build_open(&app_state, shop_id, building)))
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
    let session = session.get_session_or_error()?;
    let (character_id, shop_id) = path.into_inner();
    let building =
        resolve_building(&app_state, session.session.user_id, character_id, shop_id).await;
    Ok(Json(build_open(&app_state, shop_id, building)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FORGE: &str = "26fdb92f-a4df-4928-a97b-dee8699af605";

    /// A town shaped like the real JSONB: districts[].segments{}.buildings{}. The shop
    /// id passed to `/shops/{id}` is a building INSTANCE id and must resolve to its
    /// `(typeId, level)`.
    fn town_fixture(building_id: Uuid) -> Value {
        json!({
            "levelInfo": { "level": 6 },
            "districts": [{
                "segments": {
                    "seg-1": {
                        "buildings": {
                            building_id.to_string(): {
                                "id": building_id.to_string(),
                                "typeId": FORGE,
                                "level": 4,
                                "state": "NORMAL"
                            }
                        }
                    }
                }
            }]
        })
    }

    #[test]
    fn resolves_building_type_and_level_from_town() {
        let bid = Uuid::new_v4();
        let town = town_fixture(bid);
        let got = find_building_type_level(&town, bid).expect("building resolved");
        assert_eq!(got.0, Uuid::parse_str(FORGE).unwrap());
        assert_eq!(got.1, 4);
    }

    #[test]
    fn unknown_shop_id_resolves_to_none() {
        let bid = Uuid::new_v4();
        let town = town_fixture(bid);
        // A different id (not a building in this town) → None → caller falls back.
        assert!(find_building_type_level(&town, Uuid::new_v4()).is_none());
    }

    #[test]
    fn missing_level_defaults_to_zero() {
        let bid = Uuid::new_v4();
        let town = json!({
            "districts": [{ "segments": { "s": { "buildings": {
                bid.to_string(): { "id": bid.to_string(), "typeId": FORGE }
            }}}}]
        });
        let (_ty, level) = find_building_type_level(&town, bid).unwrap();
        assert_eq!(level, 0);
    }
}
