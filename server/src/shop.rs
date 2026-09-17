//! Town vendor shops — `POST /shops/{id}` (open), `/shops/{id}/auth/refreshloot`,
//! `/shops/{id}/purchase` (buy), `/shops/{id}/sell`.
//!
//! # The money system (tracker #30)
//!
//! *"Merchants don't have money — we should reverse engineer the money system.
//! 8h reset cycle on items on sale and amount they have to buy."*
//!
//! They had none: the generated-stock path emitted `wallet: vec![]`, so every
//! crafting vendor advertised an empty purse and selling paid nothing. Nothing was
//! persisted either — the catalog was recomputed per request from a wall-clock
//! window index, so stock could rotate mid-visit and neither the player's purchases
//! nor the merchant's spending survived the response.
//!
//! Retail's actual model, measured from 1,720 shop opens / 1,467 sells / 1,517
//! purchases and documented in [`blades_lib::features::merchant`]:
//!
//! * `catalog.wallet` is a **static gold budget** rolled once per window;
//! * `shop.revenue` is a **signed ledger** — negative when the merchant buys from
//!   the player, positive when the player buys, so buying replenishes it;
//! * spendable gold is `wallet + revenue` floored at 0, and a drained merchant
//!   **still takes the item and pays 0** rather than refusing the sale;
//! * the window is **10 hours from first visit** (not 8, and not wall-clock
//!   aligned), and the server rerolls on read rather than serving an expired one;
//! * `catalog.bundles` is the window's stock and `shop.sales` is what has been
//!   bought out of it.
//!
//! That state now lives in `server_state.shops[shopId]` as a
//! [`MerchantWindow`], so a vendor is a persistent, coherent trading partner
//! across a whole window.
//!
//! # Stock generation
//!
//! Unchanged in shape, two tiers best-first:
//! 1. **Authored per-level generation** ([`crate::shop_gen`]) — the `shop_id` is the
//!    character's building INSTANCE id, so we resolve its `typeId` + `level` from
//!    the stored town and roll a level-appropriate catalog from `shop_stock.json`.
//! 2. **Capture-derived template** fallback — if the shop isn't one of the 4
//!    crafting vendors, or the town/level can't be resolved, or the config lacks
//!    that building/level, we serve a captured template. A vendor is thus NEVER
//!    empty/timing-out, and a DB failure still yields a renderable storefront.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{GOLD, apply_reward};
use blades_lib::features::merchant::{
    self, Buyback, MerchantWindow, SellPrices,
};
use blades_lib::static_data::{ShopBundleRef, ShopWalletEntry};
use blades_lib::user_data::{
    CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
    InventoryChangeTracker,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryShop, session::SessionLookedUpMaybe,
    shop_gen, util::check_permission_for_character_and_get_it,
};

/// Catalog validity window used when the shop isn't a config-driven crafting
/// vendor. Retail's measured window is 10 hours for every vendor
/// ([`merchant::REFRESH_MS`]); config-driven shops read their level's
/// `refreshSeconds`, which the same measurement set to 36,000.
const CATALOG_WINDOW_MS: i64 = merchant::REFRESH_MS;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
struct ShopStateWire {
    id: Uuid,
    catalog_id: Uuid,
    sales: Vec<SaleEntry>,
    revenue: Vec<RevenueEntry>,
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

/// Turn a persisted window into the open/refresh wire shape.
fn window_to_wire(shop_id: Uuid, window: &MerchantWindow) -> OpenShopResponse {
    let mut sales: Vec<SaleEntry> = window
        .sales
        .iter()
        .filter(|(_, q)| **q > 0)
        .map(|(id, quantity)| SaleEntry {
            id: *id,
            quantity: *quantity,
        })
        .collect();
    sales.sort_by_key(|s| s.id);

    OpenShopResponse {
        shop: ShopStateWire {
            id: shop_id,
            // The client binds shop↔catalog by id: `shop.catalogId` MUST equal
            // `catalog.id` or it cannot resolve the catalog and renders an EMPTY
            // list.
            catalog_id: window.catalog_id,
            sales,
            revenue: window
                .revenue_wire()
                .into_iter()
                .map(|(currency_id, balance)| RevenueEntry {
                    currency_id,
                    balance,
                })
                .collect(),
        },
        catalog: CatalogWire {
            id: window.catalog_id,
            template_id: window.template_id,
            bundles: window
                .bundles
                .iter()
                .map(|(id, quantity)| ShopBundleRef {
                    id: *id,
                    quantity: *quantity,
                })
                .collect(),
            // The merchant's own gold — what tracker #30 said was missing. It is a
            // STATIC budget for the window; `shop.revenue` above carries the
            // drawdown, exactly as retail did.
            wallet: vec![ShopWalletEntry {
                currency_id: GOLD,
                balance: window.wallet_gold as i64,
            }],
            start: window.start_ms,
            expiration: window.expiration_ms,
            // Retail never served an expired catalog (false in all 1,720 opens) —
            // it rerolled on read, which is what `window_for` does.
            expired: false,
        },
    }
}

/// Roll a fresh window for a shop, or reuse the live one.
///
/// `building` = the resolved `(typeId, level)` when `shop_id` is one of the
/// character's crafting-vendor buildings; `None` when it couldn't be resolved.
/// Tier 1 rolls generated, level-appropriate stock plus the level's measured gold
/// band. Tier 2 falls back to the capture-derived template so a vendor is never
/// empty — including its captured `wallet`, which is real retail merchant gold.
///
/// `force_reroll` is set by `/auth/refreshloot`, the client's explicit restock.
fn window_for(
    app_state: &ServerGlobal,
    shop_id: Uuid,
    building: Option<(Uuid, u64)>,
    existing: Option<&MerchantWindow>,
    now: i64,
    force_reroll: bool,
) -> MerchantWindow {
    if !force_reroll {
        if let Some(live) = existing.filter(|w| w.is_live(now)) {
            let mut w = live.clone();
            w.expire_buybacks(now);
            return w;
        }
    }

    let start = now;
    let mut window = MerchantWindow {
        catalog_id: Uuid::new_v4(),
        start_ms: start,
        expiration_ms: merchant::expiration_for(start),
        // Buybacks outlive a restock: they are keyed to the sale, not the catalog.
        buybacks: existing
            .map(|w| {
                w.buybacks
                    .iter()
                    .filter(|b| b.expiration > now)
                    .cloned()
                    .collect::<Vec<Buyback>>()
            })
            .unwrap_or_default(),
        ..Default::default()
    };

    // Tier 1 — authored per-level generation (crafting vendors we can resolve).
    if let Some((type_id, level)) = building {
        let refresh_s = app_state
            .shop_stock
            .refresh_seconds(&type_id, level)
            .unwrap_or(CATALOG_WINDOW_MS / 1000);
        window.expiration_ms = ((start + refresh_s * 1000) / 1000) * 1000;
        let win_index = shop_gen::window_index(start, refresh_s);
        let bundles =
            shop_gen::generate_catalog(&app_state.shop_stock, &type_id, level, &shop_id, win_index);
        if !bundles.is_empty() {
            window.template_id = type_id;
            window.bundles = bundles.into_iter().map(|b| (b.id, b.quantity)).collect();
            window.wallet_gold = app_state
                .shop_stock
                .merchant_gold(&type_id, level)
                .map(|band| band.roll(&shop_id, start))
                .unwrap_or(0);
            if window.wallet_gold == 0 {
                log::warn!(
                    "[shop] no merchantGold band for building {type_id} level {level}; \
                     the vendor will pay 0 for the player's items"
                );
            }
            return window;
        }
    }

    // Tier 2 — capture-derived template fallback (never empty).
    window.template_id = app_state
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
    window.bundles = cat.bundles.into_iter().map(|b| (b.id, b.quantity)).collect();
    // The captured templates carry a real retail merchant wallet (30 templates,
    // 545..35,885 gold) — use it rather than leaving the vendor penniless.
    window.wallet_gold = cat
        .wallet
        .iter()
        .find(|w| w.currency_id == GOLD)
        .map(|w| w.balance.max(0) as u64)
        .unwrap_or(0);
    window
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

/// Shared open/refresh path: load the character, resolve the building, reuse or
/// roll the window, persist it, and serialize.
///
/// A DB failure must never leave the storefront hanging (that was the original
/// smith bug), so on any error we fall back to an unpersisted window built from the
/// capture templates.
async fn open_or_refresh(
    app_state: &web::Data<Arc<ServerGlobal>>,
    user_id: Uuid,
    character_id: Uuid,
    shop_id: Uuid,
    force_reroll: bool,
) -> Json<OpenShopResponse> {
    let now = now_ms();
    let globals = app_state.get_ref().clone();

    let persisted: Option<Json<OpenShopResponse>> = async {
        let mut conn = app_state.db_pool.get().await.ok()?;
        conn.transaction(move |mut conn| {
            async move {
                let mut entry = load_owned(&mut conn, character_id, user_id).await?;
                let building = entry
                    .town
                    .as_ref()
                    .and_then(|t| find_building_type_level(&t.0, shop_id));
                let window = window_for(
                    &globals,
                    shop_id,
                    building,
                    entry.server_state.0.shops.get(&shop_id),
                    now,
                    force_reroll,
                );
                let wire = window_to_wire(shop_id, &window);
                entry.server_state.0.shops.insert(shop_id, window);
                prune_stale_shops(&mut entry.server_state.0.shops, now);
                write_back(&mut conn, entry).await?;
                Ok::<_, BladeApiError>(wire)
            }
            .scope_boxed()
        })
        .await
        .ok()
    }
    .await
    .map(Json);

    persisted.unwrap_or_else(|| {
        log::warn!(
            "[shop] could not persist the merchant window for shop {shop_id} \
             (character {character_id}); serving an unpersisted catalog"
        );
        let window = window_for(app_state, shop_id, None, None, now, force_reroll);
        Json(window_to_wire(shop_id, &window))
    })
}

/// Forget windows that expired long enough ago that no buyback can still be live,
/// so `server_state.shops` cannot grow without bound as a player wanders a town.
fn prune_stale_shops(shops: &mut HashMap<Uuid, MerchantWindow>, now: i64) {
    for w in shops.values_mut() {
        // Drop dead buyback slots FIRST, or a window whose only remaining slots
        // have expired would be kept forever by the check below.
        w.expire_buybacks(now);
    }
    shops.retain(|_, w| w.expiration_ms > now || !w.buybacks.is_empty());
}

/// `POST /shops/{id}` — open a vendor (returns its current catalog).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}")]
pub async fn open_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<OpenShopResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, shop_id) = path.into_inner();
    Ok(open_or_refresh(
        &app_state,
        session.session.user_id,
        character_id,
        shop_id,
        false,
    )
    .await)
}

/// `POST /shops/{id}/auth/refreshloot` — the client's explicit restock: re-roll the
/// catalog and the merchant's budget, starting a fresh window.
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
    Ok(open_or_refresh(
        &app_state,
        session.session.user_id,
        character_id,
        shop_id,
        true,
    )
    .await)
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
struct ShopTxnState {
    id: Uuid,
    catalog_id: Uuid,
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

/// The `shop` block for a buy/sell response: cumulative sales + revenue for the
/// window, matching the captured shape (capture 5027 showed `sales: [{id, 18}]`
/// and `revenue: [{gold, +4500}]` after buying 18 units).
fn txn_state(shop_id: Uuid, window: &MerchantWindow) -> ShopTxnState {
    let mut sales: Vec<SaleEntry> = window
        .sales
        .iter()
        .filter(|(_, q)| **q > 0)
        .map(|(id, quantity)| SaleEntry {
            id: *id,
            quantity: *quantity,
        })
        .collect();
    sales.sort_by_key(|s| s.id);
    ShopTxnState {
        id: shop_id,
        catalog_id: window.catalog_id,
        sales,
        revenue: window
            .revenue_wire()
            .into_iter()
            .map(|(currency_id, balance)| RevenueEntry {
                currency_id,
                balance,
            })
            .collect(),
    }
}

/// `POST /shops/{id}/purchase` — buy bundles out of the window's stock.
///
/// Stock is finite: a request for more than remains is clamped, and buying draws
/// down `remaining_stock` while pushing `revenue` positive — which is what lets the
/// merchant afford to buy from the player again.
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
    let now = now_ms();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let building = entry
                .town
                .as_ref()
                .and_then(|t| find_building_type_level(&t.0, shop_id));
            let mut window = window_for(
                &globals,
                shop_id,
                building,
                entry.server_state.0.shops.get(&shop_id),
                now,
                false,
            );

            let mut tracker = InventoryChangeTracker::default();
            let mut bought_anything = false;

            for b in &bundles {
                let want = b.quantity.max(1);
                let Some(def) = globals.static_data.shop_bundles.get(&b.id) else {
                    // No price/contents for this bundle — skip rather than hand out
                    // something unpriced. With the APK bundle table this covers all
                    // 94 town-vendor bundles, so it should not fire.
                    log::warn!("[shop] bundle {} has no price/grant definition", b.id);
                    continue;
                };
                // Finite stock: never sell more than the window still holds.
                let qty = want.min(window.remaining_stock(b.id));
                if qty == 0 {
                    continue;
                }
                let currency = def.currency_id.unwrap_or(GOLD);
                let cost = def.price.saturating_mul(qty);
                entry
                    .wallet
                    .0
                    .debit(currency, cost)
                    .map_err(BladeApiError::from_economy)?;

                let mut reward = def.grant.clone();
                for v in reward.stackable_items.values_mut() {
                    *v = v.saturating_mul(qty);
                }
                // Instanced gear needs a fresh instance id per purchase (the static
                // definition carries a placeholder) — same as chests.rs.
                let unit_items = reward.items.clone();
                reward.items.clear();
                for _ in 0..qty {
                    for item in &unit_items {
                        let mut fresh = item.clone();
                        fresh.id = Uuid::new_v4();
                        reward.items.push(fresh);
                    }
                }
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );

                *window.sales.entry(b.id).or_insert(0) += qty;
                // Retail: buying pushes `revenue` POSITIVE, replenishing what the
                // merchant can spend buying from the player.
                if currency == GOLD {
                    window.revenue_gold += cost as i64;
                }
                bought_anything = true;
            }

            if bought_anything {
                entry.inventory.0.backpack_version += 1;
            }

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let shop = txn_state(shop_id, &window);
            entry.server_state.0.shops.insert(shop_id, window);
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(BuyResponse {
                shop,
                inventory,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}

// ─────────────────────────────────────────────────────────────────────────────
// Visiting someone else's town — `…/social/users/{u}/characters/{c}/shops/{s}`
// ─────────────────────────────────────────────────────────────────────────────
//
// A player can walk into a friend's town and trade with THEIR merchants. Retail
// served it on 1,673 captured requests from 9 different players — 544 opens and
// 1,129 purchases — and we had no route at all, so the shop never opened.
//
// The shapes are the ordinary vendor's, wrapped in a `social` envelope:
//
// ```text
// POST …/shops/{s}          null      → {"social":{"shop":…,"catalog":…}}
// POST …/shops/{s}/purchase {bundles} → {"character","inventory","wallet",
//                                        "social":{"shop":…}}
// ```
//
// The stock, the window and the sales ledger belong to the OWNER — it is their
// merchant — while the gold and the goods move on the VISITOR. So both character
// rows are touched, in one transaction, and they are loaded in a fixed order
// (owner, then visitor) so two players buying from each other at the same moment
// cannot deadlock on each other's row locks.

/// The `social` envelope retail wraps a visited shop in.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialShopEnvelope<T> {
    social: T,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialOpenInner {
    shop: ShopStateWire,
    catalog: CatalogWire,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialBuyInner {
    shop: ShopTxnState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialBuyResponse {
    character: CompleteCharacterWithIdWithoutData,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
    social: SocialBuyInner,
}

/// Load another player's character row for writing. Unlike [`load_owned`] the
/// caller is NOT the owner, so there is no `user_id` ownership filter — the pair
/// in the URL is the filter, which is also what stops a visitor naming a
/// character that is not the user they claim to be visiting.
async fn load_other(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
    user_id: Uuid,
) -> Result<CharacterDbEntryShop, BladeApiError> {
    use crate::schema::characters;
    characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryShop::as_select())
        .for_no_key_update()
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))
}

/// `POST /characters/{visitor}/social/users/{u}/characters/{c}/shops/{s}` — open a
/// merchant in someone else's town.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{visitor_character_id}/social/users/{owner_user_id}/characters/{owner_character_id}/shops/{shop_id}"
)]
pub async fn open_social_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid, Uuid, Uuid)>,
    _body: Json<Option<Value>>,
) -> Result<Json<SocialShopEnvelope<SocialOpenInner>>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (visitor_character_id, owner_user_id, owner_character_id, shop_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let now = now_ms();
    let mut conn = app_state.db_pool.get().await?;

    // The visitor must be who they say they are before we touch anyone's row.
    check_permission_for_character_and_get_it(&mut conn, &session.session, visitor_character_id)
        .await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut owner = load_other(&mut conn, owner_character_id, owner_user_id).await?;
            let building = owner
                .town
                .as_ref()
                .and_then(|t| find_building_type_level(&t.0, shop_id));
            let window = window_for(
                &globals,
                shop_id,
                building,
                owner.server_state.0.shops.get(&shop_id),
                now,
                false,
            );
            let wire = window_to_wire(shop_id, &window);
            owner.server_state.0.shops.insert(shop_id, window);
            prune_stale_shops(&mut owner.server_state.0.shops, now);
            write_back(&mut conn, owner).await?;

            Ok::<_, BladeApiError>(Json(SocialShopEnvelope {
                social: SocialOpenInner {
                    shop: wire.shop,
                    catalog: wire.catalog,
                },
            }))
        }
        .scope_boxed()
    })
    .await
}

/// `POST …/social/users/{u}/characters/{c}/shops/{s}/purchase` — buy from someone
/// else's merchant.
///
/// The gold leaves the visitor and the goods arrive in the visitor's backpack; the
/// stock drawdown and the revenue land on the owner's merchant, exactly as they
/// would if the owner had bought it themselves. That is what makes a visited shop
/// run out — retail's stock is per-merchant, not per-visitor.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{visitor_character_id}/social/users/{owner_user_id}/characters/{owner_character_id}/shops/{shop_id}/purchase"
)]
pub async fn buy_from_social_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid, Uuid, Uuid)>,
    body: Json<BuyRequest>,
) -> Result<Json<SocialBuyResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (visitor_character_id, owner_user_id, owner_character_id, shop_id) = path.into_inner();
    let bundles = body.into_inner().bundles;
    let globals = app_state.get_ref().clone();
    let now = now_ms();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            // Owner first, then visitor — a fixed lock order, so two players buying
            // from each other at once cannot deadlock. Buying from your own shop
            // through this route would lock the same row twice, which is why that
            // case is sent to the ordinary vendor path instead.
            if owner_character_id == visitor_character_id {
                return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20000, 5));
            }
            let mut owner = load_other(&mut conn, owner_character_id, owner_user_id).await?;
            let mut visitor = load_owned(&mut conn, visitor_character_id, user_id).await?;

            let building = owner
                .town
                .as_ref()
                .and_then(|t| find_building_type_level(&t.0, shop_id));
            let mut window = window_for(
                &globals,
                shop_id,
                building,
                owner.server_state.0.shops.get(&shop_id),
                now,
                false,
            );

            let mut tracker = InventoryChangeTracker::default();
            let mut bought_anything = false;
            for b in &bundles {
                let want = b.quantity.max(1);
                let Some(def) = globals.static_data.shop_bundles.get(&b.id) else {
                    log::warn!("[shop] bundle {} has no price/grant definition", b.id);
                    continue;
                };
                let qty = want.min(window.remaining_stock(b.id));
                if qty == 0 {
                    continue;
                }
                let currency = def.currency_id.unwrap_or(GOLD);
                let cost = def.price.saturating_mul(qty);
                visitor
                    .wallet
                    .0
                    .debit(currency, cost)
                    .map_err(BladeApiError::from_economy)?;

                let mut reward = def.grant.clone();
                for v in reward.stackable_items.values_mut() {
                    *v = v.saturating_mul(qty);
                }
                let unit_items = reward.items.clone();
                reward.items.clear();
                for _ in 0..qty {
                    for item in &unit_items {
                        let mut fresh = item.clone();
                        fresh.id = Uuid::new_v4();
                        reward.items.push(fresh);
                    }
                }
                apply_reward(
                    &reward,
                    &mut visitor.wallet.0,
                    &mut visitor.inventory.0,
                    &mut visitor.character.0,
                    &mut tracker,
                );

                *window.sales.entry(b.id).or_insert(0) += qty;
                if currency == GOLD {
                    window.revenue_gold += cost as i64;
                }
                bought_anything = true;
            }

            if bought_anything {
                visitor.inventory.0.backpack_version += 1;
            }

            let inventory = visitor.inventory.0.generate_client_update(&tracker);
            let wallet = visitor.wallet.0.clone();
            let character = CompleteCharacterWithIdWithoutData {
                id: visitor_character_id,
                character: visitor.character.0.clone(),
            };
            let shop = txn_state(shop_id, &window);
            owner.server_state.0.shops.insert(shop_id, window);
            write_back(&mut conn, owner).await?;
            write_back(&mut conn, visitor).await?;

            Ok::<_, BladeApiError>(Json(SocialBuyResponse {
                character,
                inventory,
                wallet,
                social: SocialBuyInner { shop },
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
    stackable_items: HashMap<Uuid, u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SellResponse {
    shop: ShopTxnState,
    inventory: CompleteInventoryUpdate,
    /// Retail echoed `wallet` **iff the payout was non-zero** — 727 of 1,466
    /// sells, and 1,466 − 739 zero-price buybacks = 727 exactly. Mirrored.
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
    buybacks: Vec<Buyback>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BuybackResponse {
    shop: ShopTxnState,
    inventory: CompleteInventoryUpdate,
    /// Echoed only when gold actually moved, mirroring `SellResponse`.
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
    /// The slots still open after this one was consumed.
    buybacks: Vec<merchant::Buyback>,
}

/// `POST /shops/{id}/buybacks/{id}` — take back something you just sold.
///
/// THIS ROUTE DID NOT EXIST. `sell_to_shop` creates buyback slots and the client
/// renders them, but the endpoint it posts to was never written, so every attempt
/// answered **404**. Selling by accident was irreversible.
///
/// That is report #163: the player sold six items, tried to buy them back, got
/// nothing, and the five-minute window closed while the slots sat there unusable.
/// The edge log has his three 404s on this exact path.
///
/// The transaction is the inverse of the sale: the player pays back precisely what
/// the merchant paid them (`price`, often 0 once the merchant's budget is spent),
/// the merchant's revenue is restored, and the item returns to the backpack.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}/buybacks/{buyback_id}"
)]
pub async fn buy_back_from_shop(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid, Uuid)>,
) -> Result<Json<BuybackResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, shop_id, buyback_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let now = now_ms();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let building = entry
                .town
                .as_ref()
                .and_then(|t| find_building_type_level(&t.0, shop_id));
            // `false`: never reroll the catalogue on a buyback. Rerolling would
            // discard the very slot being claimed.
            let mut window = window_for(
                &globals,
                shop_id,
                building,
                entry.server_state.0.shops.get(&shop_id),
                now,
                false,
            );

            let mut tracker = InventoryChangeTracker::default();
            let outcome = merchant::apply_buyback(
                &mut window,
                buyback_id,
                &mut entry.inventory.0,
                &mut entry.wallet.0,
                &mut tracker,
                now,
            )
            .map_err(|e| match e {
                merchant::BuybackError::NoSuchSlot => {
                    BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2)
                }
                merchant::BuybackError::Expired => {
                    BladeApiError::new(StatusCode::BAD_REQUEST, 20000, 3)
                }
                merchant::BuybackError::InsufficientGold => {
                    BladeApiError::new(StatusCode::BAD_REQUEST, 20000, 4)
                }
            })?;

            entry.inventory.0.backpack_version += 1;
            log::info!(
                "[shop] character {character_id} bought back {} from shop {shop_id} for {} gold",
                outcome.slot.id,
                outcome.gold_spent,
            );

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = (outcome.gold_spent > 0).then(|| entry.wallet.0.clone());
            let shop = txn_state(shop_id, &window);
            let buybacks = window.buybacks.clone();
            entry.server_state.0.shops.insert(shop_id, window);
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(BuybackResponse {
                shop,
                inventory,
                wallet,
                buybacks,
            }))
        }
        .scope_boxed()
    })
    .await
}

/// `POST /shops/{id}/sell` — sell gear/materials to a merchant for its own gold.
///
/// Price is the item's APK `sellValue` scaled by its temper multiplier plus its
/// enchantment values, clamped to what the merchant can still afford. A drained
/// merchant still takes the item and pays 0, which is what retail did.
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
    let globals = app_state.get_ref().clone();
    let now = now_ms();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let building = entry
                .town
                .as_ref()
                .and_then(|t| find_building_type_level(&t.0, shop_id));
            let mut window = window_for(
                &globals,
                shop_id,
                building,
                entry.server_state.0.shops.get(&shop_id),
                now,
                false,
            );

            let prices: &SellPrices = &globals.sell_prices;
            let mut tracker = InventoryChangeTracker::default();
            let outcome = merchant::apply_sell(
                prices,
                &mut window,
                shop_id,
                &req.items,
                &req.stackable_items,
                &mut entry.inventory.0,
                &mut entry.wallet.0,
                &mut tracker,
                now,
            );

            if !outcome.unknown.is_empty() {
                log::info!(
                    "[shop] character {character_id} tried to sell {} thing(s) it does \
                     not hold (stale client state), ignored",
                    outcome.unknown.len()
                );
            }
            if !outcome.sold.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = if outcome.gold_paid > 0 {
                Some(entry.wallet.0.clone())
            } else {
                None
            };
            let shop = txn_state(shop_id, &window);
            let buybacks = outcome.buybacks;
            entry.server_state.0.shops.insert(shop_id, window);
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(SellResponse {
                shop,
                inventory,
                wallet,
                buybacks,
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
) -> Result<CharacterDbEntryShop, BladeApiError> {
    use crate::schema::characters;
    characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryShop::as_select())
        .for_no_key_update()
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))
}

async fn write_back(
    conn: &mut diesel_async::AsyncPgConnection,
    entry: CharacterDbEntryShop,
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

    /// The open/refresh wire must always advertise the merchant's gold — an empty
    /// `catalog.wallet` is precisely what tracker #30 reported.
    #[test]
    fn the_open_wire_always_carries_the_merchants_gold() {
        let shop = Uuid::new_v4();
        let mut w = MerchantWindow {
            catalog_id: Uuid::new_v4(),
            template_id: Uuid::new_v4(),
            start_ms: 1_000_000,
            expiration_ms: merchant::expiration_for(1_000_000),
            bundles: vec![(Uuid::from_u128(0xb1), 4)],
            wallet_gold: 24_438,
            ..Default::default()
        };
        let wire = window_to_wire(shop, &w);
        assert_eq!(wire.catalog.wallet.len(), 1, "wallet is never empty");
        assert_eq!(wire.catalog.wallet[0].currency_id, GOLD);
        assert_eq!(wire.catalog.wallet[0].balance, 24_438);
        // shop.catalogId MUST equal catalog.id or the client renders nothing.
        assert_eq!(wire.shop.catalog_id, wire.catalog.id);
        assert!(!wire.catalog.expired);
        // A fresh catalog reports empty sales/revenue, as captured.
        assert!(wire.shop.sales.is_empty() && wire.shop.revenue.is_empty());

        // After trading, both appear.
        w.sales.insert(Uuid::from_u128(0xb1), 2);
        w.revenue_gold = -1395;
        let wire = window_to_wire(shop, &w);
        assert_eq!(wire.shop.sales.len(), 1);
        assert_eq!(wire.shop.revenue[0].balance, -1395);
        // ...and the advertised wallet is still the static budget.
        assert_eq!(wire.catalog.wallet[0].balance, 24_438);
    }

    #[test]
    fn the_catalog_window_is_ten_hours() {
        assert_eq!(CATALOG_WINDOW_MS, 36_000_000);
    }

    #[test]
    fn stale_shop_windows_are_pruned_but_live_buybacks_are_kept() {
        let now = 10_000_000i64;
        let mut shops: HashMap<Uuid, MerchantWindow> = HashMap::new();
        let live = Uuid::from_u128(1);
        let stale = Uuid::from_u128(2);
        let stale_with_buyback = Uuid::from_u128(3);
        shops.insert(live, MerchantWindow { expiration_ms: now + 1000, ..Default::default() });
        shops.insert(stale, MerchantWindow { expiration_ms: now - merchant::BUYBACK_MS - 1, ..Default::default() });
        shops.insert(
            stale_with_buyback,
            MerchantWindow {
                expiration_ms: now - merchant::BUYBACK_MS - 1,
                buybacks: vec![Buyback {
                    id: Uuid::from_u128(9),
                    shop_id: stale_with_buyback,
                    item: None,
                    stackable_item: None,
                    expiration: now + 1000,
                    price: 5,
                }],
                ..Default::default()
            },
        );
        // ...and one whose window AND buyback are both dead: it must not be kept
        // alive by a slot that has already expired, or the map grows forever as a
        // player wanders a town.
        let stale_with_dead_buyback = Uuid::from_u128(4);
        shops.insert(
            stale_with_dead_buyback,
            MerchantWindow {
                expiration_ms: now - merchant::BUYBACK_MS - 1,
                buybacks: vec![Buyback {
                    id: Uuid::from_u128(10),
                    shop_id: stale_with_dead_buyback,
                    item: None,
                    stackable_item: None,
                    expiration: now - 1,
                    price: 5,
                }],
                ..Default::default()
            },
        );

        prune_stale_shops(&mut shops, now);
        assert!(shops.contains_key(&live));
        assert!(shops.contains_key(&stale_with_buyback));
        assert!(!shops.contains_key(&stale), "expired, buyback-free windows are dropped");
        assert!(
            !shops.contains_key(&stale_with_dead_buyback),
            "an expired buyback must not keep a dead window alive"
        );
        assert_eq!(
            shops[&stale_with_buyback].buybacks.len(),
            1,
            "the live buyback survives"
        );
    }

    // ── visiting someone else's town ─────────────────────────────────────────

    /// Retail's envelope, verbatim from capture (a visit to another player's
    /// smith): the vendor's ordinary `shop` + `catalog` under a `social` key.
    ///
    /// The envelope is the whole point — the client reads a visited shop from a
    /// different place than its own, so serving the bare `{shop, catalog}` here
    /// would parse as nothing.
    #[test]
    fn a_visited_shop_is_wrapped_in_the_social_envelope() {
        let window = MerchantWindow::default();
        let wire = window_to_wire(Uuid::from_u128(1), &window);
        let body = serde_json::to_value(SocialShopEnvelope {
            social: SocialOpenInner {
                shop: wire.shop,
                catalog: wire.catalog,
            },
        })
        .unwrap();

        assert!(body.get("shop").is_none(), "not at the top level: {body}");
        assert!(body["social"]["shop"].is_object(), "{body}");
        assert!(body["social"]["catalog"].is_object(), "{body}");
        // The keys the captured catalog carried, so a rename is caught here.
        for key in ["id", "templateId", "bundles", "wallet", "start", "expiration", "expired"] {
            assert!(
                body["social"]["catalog"].get(key).is_some(),
                "catalog is missing `{key}`: {body}"
            );
        }
    }

    /// A purchase answers with the BUYER's character, inventory and wallet at the
    /// top level and the OWNER's shop under `social` — the four keys retail sent.
    #[test]
    fn buying_in_someone_elses_town_answers_with_the_buyers_side_and_the_owners_shop() {
        let body = serde_json::to_value(SocialBuyResponse {
            character: CompleteCharacterWithIdWithoutData {
                id: Uuid::from_u128(2),
                character: Default::default(),
            },
            inventory: CompleteInventoryUpdate {
                backpack: Default::default(),
                loadout: Default::default(),
                treasury: Default::default(),
                overflow_treasury: Default::default(),
                backpack_version: 1,
                treasury_version: 1,
            },
            wallet: CompleteWallet::default(),
            social: SocialBuyInner {
                shop: txn_state(Uuid::from_u128(1), &MerchantWindow::default()),
            },
        })
        .unwrap();

        let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["character", "inventory", "social", "wallet"],
            "retail sent exactly these four"
        );
        assert!(body["social"]["shop"]["sales"].is_array());
        assert!(body["social"]["shop"]["revenue"].is_array());
        assert!(
            body["social"].get("catalog").is_none(),
            "the purchase response carries the shop's ledger, not its catalog"
        );
    }
}
