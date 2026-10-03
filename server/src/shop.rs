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
//!    character's building INSTANCE id, so we resolve its `typeId` + town-building
//!    level from the stored town. Retail selects the stock band from that building
//!    level; the character level does not select the catalog.
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
use blades_lib::features::merchant::{self, Buyback, MerchantWindow, SellPrices};
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
    shop_gen,
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
/// `building` = the resolved `(typeId, building_level)` when `shop_id`
/// is one of the character's crafting-vendor buildings; `None` when it couldn't be
/// resolved. Tier 1 rolls generated stock from that building level plus the same
/// level's measured gold band. Tier 2 falls back to the capture-derived template
/// so a vendor is never empty — including its captured `wallet`, which is real
/// retail merchant gold.
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
    if let Some((type_id, building_level)) = building {
        let refresh_s = app_state
            .shop_stock
            .refresh_seconds(&type_id, building_level)
            .unwrap_or(CATALOG_WINDOW_MS / 1000);
        window.expiration_ms = ((start + refresh_s * 1000) / 1000) * 1000;
        let win_index = shop_gen::window_index(start, refresh_s);
        let bundles = shop_gen::generate_catalog(
            &app_state.shop_stock,
            &type_id,
            building_level,
            &shop_id,
            win_index,
        );
        if !bundles.is_empty() {
            window.template_id = type_id;
            window.bundles = bundles.into_iter().map(|b| (b.id, b.quantity)).collect();
            window.wallet_gold = app_state
                .shop_stock
                .merchant_gold(&type_id, building_level)
                .map(|band| band.roll(&shop_id, start))
                .unwrap_or(0);
            if window.wallet_gold == 0 {
                log::warn!(
                    "[shop] no merchantGold band for building {type_id} level {building_level}; \
                     the vendor will pay 0 for the player's items"
                );
            }
            roll_generated_jewelry_for_window(app_state, shop_id, &mut window);
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
    window.bundles = cat
        .bundles
        .into_iter()
        .map(|b| (b.id, b.quantity))
        .collect();
    // The captured templates carry a real retail merchant wallet (30 templates,
    // 545..35,885 gold) — use it rather than leaving the vendor penniless.
    window.wallet_gold = cat
        .wallet
        .iter()
        .find(|w| w.currency_id == GOLD)
        .map(|w| w.balance.max(0) as u64)
        .unwrap_or(0);
    roll_generated_jewelry_for_window(app_state, shop_id, &mut window);
    window
}

fn roll_generated_jewelry_for_window(
    app_state: &ServerGlobal,
    shop_id: Uuid,
    window: &mut MerchantWindow,
) {
    window.generated_grants.clear();
    for (idx, (bundle_id, _)) in window.bundles.iter().enumerate() {
        let Some(def) = app_state.static_data.shop_bundles.get(bundle_id) else {
            continue;
        };
        if !def.grant.items.iter().any(|i| {
            crate::jewelry_roll::is_jewelry_template(
                i.item.item_template_id,
                &app_state.game_data.items_template,
            )
        }) {
            continue;
        }
        let mut grant = def.grant.clone();
        let mut changed = false;
        let mut rng = crate::jewelry_roll::seeded(
            &[shop_id.as_bytes(), bundle_id.as_bytes()],
            window.start_ms as u64 ^ idx as u64,
        );
        for item in &mut grant.items {
            changed |= crate::jewelry_roll::roll_generated_jewelry(
                &mut item.item,
                &app_state.game_data.items_template,
                &mut rng,
            );
        }
        if changed {
            window.generated_grants.insert(*bundle_id, grant);
        }
    }
}

fn stock_building_for(entry: &CharacterDbEntryShop, shop_id: Uuid) -> Option<(Uuid, u64)> {
    entry
        .town
        .as_ref()
        .and_then(|t| find_building_type_level(&t.0, shop_id))
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
                let building = stock_building_for(&entry, shop_id);
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
fn prune_stale_shops<K: Eq + std::hash::Hash>(shops: &mut HashMap<K, MerchantWindow>, now: i64) {
    for w in shops.values_mut() {
        // Drop dead buyback slots FIRST, or a window whose only remaining slots
        // have expired would be kept forever by the check below.
        w.expire_buybacks(now);
    }
    shops.retain(|_, w| w.expiration_ms > now || !w.buybacks.is_empty());
}

/// The four town buildings that run a merchant: Forge, Enchanter, Workshop and
/// Alchemist — the building types `shop_stock.json` generates stock for.
pub(crate) const VENDOR_BUILDING_TYPES: [Uuid; 4] = [
    uuid::uuid!("26fdb92f-a4df-4928-a97b-dee8699af605"), // Forge
    uuid::uuid!("82108d94-ebf7-434f-8623-ca66d7504f27"), // Enchanter
    uuid::uuid!("b6c023e6-3b81-497f-9c2c-f532ecff3bb2"), // Workshop
    uuid::uuid!("e1dd10fc-8b14-4288-9b23-99b0d58388de"), // Alchemist
];

pub(crate) fn is_vendor_building(type_id: Uuid) -> bool {
    VENDOR_BUILDING_TYPES.contains(&type_id)
}

/// The `shop` object retail's `/towns/current/buildings/{id}/complete` returns for
/// a vendor building: the shop's id with its sales and revenue emptied, and no
/// `catalogId`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildCompleteShop {
    id: Uuid,
    sales: Vec<SaleEntry>,
    revenue: Vec<RevenueEntry>,
}

/// Finishing a vendor building's construction or upgrade restocks its merchant
/// (report #287).
///
/// MEASURED, every retail vendor completion in the corpus: 208 of 208 `/complete`
/// responses for the four vendor types (9 players, `speedUp` true and false) carry
/// `shop: {id, sales: [], revenue: []}`; 0 of 96 completions of any other building
/// do. And the next `/shops/{id}` after a completion always opened a NEW catalog —
/// new id, `start` = that open, the new level's template and gold — including the
/// 15 cases where the old 10-hour window still had up to 600 minutes to run (Forge
/// `f7dc114d`, 2026-05-13: 60 minutes left, template `6cda3555` → `57eeb277`,
/// wallet 685 → 1,174).
///
/// We kept the old window until its 10 hours ran out, so a player who upgraded a
/// shop came back to the previous level's stock and to a merchant whose gold they
/// had already spent down to 0.
///
/// The window is closed here rather than rolled, so the next open rolls it from the
/// building's new level exactly like any other new window. Live buyback slots
/// survive, as they do across every other restock.
pub(crate) fn restock_on_build_complete(
    shops: &mut HashMap<Uuid, MerchantWindow>,
    shop_id: Uuid,
    now: i64,
) -> BuildCompleteShop {
    if let Some(mut old) = shops.remove(&shop_id) {
        old.expire_buybacks(now);
        if !old.buybacks.is_empty() {
            shops.insert(
                shop_id,
                MerchantWindow {
                    buybacks: old.buybacks,
                    ..Default::default()
                },
            );
        }
    }
    BuildCompleteShop {
        id: shop_id,
        sales: Vec::new(),
        revenue: Vec::new(),
    }
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

/// What `qty` units of a merchant bundle put in the buyer's backpack.
///
/// Stackables multiply. Instanced gear gets a fresh instance id per unit (the
/// static definition carries a placeholder — same as chests.rs). Generated
/// jewelry rolls are already stored on the merchant window; legacy/fallback bare
/// jewelry still gets the old grade-only roll here.
///
/// A town merchant never sells enchanted gear (#321): none of retail's town-shop
/// purchases carried ENCHANTING (4/4 rings and necklaces, 2/2 weapons and
/// armour). Clearing it here also covers windows persisted before the fix,
/// whose `generatedGrants` still hold the old Sigil-count enchantments.
fn mint_bundle<R: rand::Rng + ?Sized>(
    grant: &blades_lib::economy::RewardGrant,
    qty: u64,
    items: &HashMap<Uuid, blades_lib::game_data::GameDataItem>,
    rng: &mut R,
) -> blades_lib::economy::RewardGrant {
    let mut reward = grant.clone();
    for v in reward.stackable_items.values_mut() {
        *v = v.saturating_mul(qty);
    }
    reward.items.clear();
    for _ in 0..qty {
        for item in &grant.items {
            let mut fresh = item.clone();
            fresh.id = Uuid::new_v4();
            fresh.item.properties.enchanting.clear();
            crate::jewelry_grade::grade_if_bare(&mut fresh.item, items, rng);
            reward.items.push(fresh);
        }
    }
    reward
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

/// Book `want` units of `bundle` against `window`: clamp to what the window still
/// holds, count the sale and, for gold, push `revenue` positive. Returns the units
/// booked — 0 when the window has none left.
fn book_sale(
    window: &mut MerchantWindow,
    bundle: Uuid,
    want: u64,
    unit_price: u64,
    currency: Uuid,
) -> u64 {
    // Finite stock: never sell more than the window still holds.
    let qty = want.max(1).min(window.remaining_stock(bundle));
    if qty == 0 {
        return 0;
    }
    *window.sales.entry(bundle).or_insert(0) += qty;
    // Retail: buying pushes `revenue` POSITIVE, replenishing what the merchant
    // can spend buying from the player.
    if currency == GOLD {
        window.revenue_gold += unit_price.saturating_mul(qty) as i64;
    }
    qty
}

/// Buy `bundles` out of `window` for `buyer`: the gold leaves the buyer, the goods
/// land in the buyer's backpack, and the sale is booked on `window`. Shared by a
/// player's own merchants and by a merchant they visit in someone else's town —
/// the only difference between the two is whose window is passed in.
fn buy_bundles(
    globals: &ServerGlobal,
    window: &mut MerchantWindow,
    bundles: &[BuyBundle],
    buyer: &mut CharacterDbEntryShop,
    tracker: &mut InventoryChangeTracker,
) -> Result<bool, BladeApiError> {
    let mut bought_anything = false;
    for b in bundles {
        let Some(def) = globals.static_data.shop_bundles.get(&b.id) else {
            // No price/contents for this bundle — skip rather than hand out
            // something unpriced. With the APK bundle table this covers all 94
            // town-vendor bundles, so it should not fire.
            log::warn!("[shop] bundle {} has no price/grant definition", b.id);
            continue;
        };
        let currency = def.currency_id.unwrap_or(GOLD);
        let grant = window
            .generated_grants
            .get(&b.id)
            .unwrap_or(&def.grant)
            .clone();
        let qty = book_sale(window, b.id, b.quantity, def.price, currency);
        if qty == 0 {
            continue;
        }
        // An error here aborts the whole transaction, so the booking above is
        // never persisted for a purchase the buyer could not pay for.
        buyer
            .wallet
            .0
            .debit(currency, def.price.saturating_mul(qty))
            .map_err(BladeApiError::from_economy)?;
        let reward = mint_bundle(
            &grant,
            qty,
            &globals.game_data.items_template,
            &mut rand::rng(),
        );
        apply_reward(
            &reward,
            &mut buyer.wallet.0,
            &mut buyer.inventory.0,
            &mut buyer.character.0,
            tracker,
        );
        bought_anything = true;
    }
    if bought_anything {
        buyer.inventory.0.backpack_version += 1;
    }
    Ok(bought_anything)
}

/// `POST /shops/{id}/purchase` — buy bundles out of the window's stock.
///
/// Stock is finite: a request for more than remains is clamped, and buying draws
/// down `remaining_stock` while pushing `revenue` positive — which is what lets the
/// merchant afford to buy from the player again.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/shops/{shop_id}/purchase"
)]
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
            let building = stock_building_for(&entry, shop_id);
            let mut window = window_for(
                &globals,
                shop_id,
                building,
                entry.server_state.0.shops.get(&shop_id),
                now,
                false,
            );

            let mut tracker = InventoryChangeTracker::default();
            buy_bundles(&globals, &mut window, &bundles, &mut entry, &mut tracker)?;

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
// The merchant's catalog comes from the OWNER's building — its type and level
// pick the stock band, exactly as for the owner — but the window a visitor trades
// against is the VISITOR's own: its stock, sales and revenue live on the visitor's
// row, keyed by owner and shop (report #316). Retail, 1,671 answered requests from
// 9 visitors across 67 owners and 223 shops:
//
// * 0 of 498 first opens of a catalog showed any sale or revenue — not even when
//   the catalog was hours old, which an owner who sells to their own merchant
//   would have left negative;
// * 466 of 466 first purchases came back with `sales` equal to exactly what that
//   visitor had just bought, and 45 of 45 re-opens showed only that visitor's own
//   purchases;
// * 482 of the 498 catalogs started within 5 minutes of that visitor's open
//   (median under a second) — the visit rolled it, not the owner.
//
// We kept the window on the owner's row, so the first guildmate to shop there
// bought it empty for everyone, owner included.
//
// The goods and gold move on the visitor, and the owner's own merchant is never
// written. The owner's row is still loaded first (owner, then visitor) so a buy
// in each other's towns at the same instant cannot deadlock on row locks.

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

/// Load another player's character row. Unlike [`load_owned`] the caller is NOT
/// the owner, so there is no `user_id` ownership filter — the pair in the URL is
/// the filter, which is also what stops a visitor naming a character that is not
/// the user they claim to be visiting.
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

/// Where a visitor's window on `owner_character_id`'s merchant `shop_id` is kept in
/// the visitor's `server_state.visited_shops`. The owner is part of the key because
/// a building instance id is only guaranteed unique within one town.
pub(crate) fn visited_shop_key(owner_character_id: Uuid, shop_id: Uuid) -> String {
    format!("{owner_character_id}:{shop_id}")
}

/// The window `visitor` trades against at `owner`'s merchant `shop_id`.
///
/// `roll` is [`window_for`] in production: given the owner's building (`typeId`,
/// level) and the visitor's previous window, it returns that window while it is
/// live and rolls a fresh one from the owner's building once it is not. The result
/// is stored on the VISITOR, and the visitor's expired windows are pruned in the
/// same step, so wandering many towns cannot grow the map without bound.
fn visited_window_mut<'a>(
    owner: &CharacterDbEntryShop,
    visitor: &'a mut CharacterDbEntryShop,
    shop_id: Uuid,
    now: i64,
    roll: impl FnOnce(Option<(Uuid, u64)>, Option<&MerchantWindow>) -> MerchantWindow,
) -> &'a mut MerchantWindow {
    let key = visited_shop_key(owner.id, shop_id);
    let visited = &mut visitor.server_state.0.visited_shops;
    let window = roll(stock_building_for(owner, shop_id), visited.get(&key));
    prune_stale_shops(visited, now);
    visited.insert(key.clone(), window);
    visited.get_mut(&key).expect("just inserted")
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
    let user_id = session.session.user_id;
    let (visitor_character_id, owner_user_id, owner_character_id, shop_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let now = now_ms();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            // A visitor's window lives on the visitor; "visiting" your own town
            // would lock the same row twice. Retail never sent it (0 of 1,673).
            if owner_character_id == visitor_character_id {
                return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20000, 5));
            }
            let owner = load_other(&mut conn, owner_character_id, owner_user_id).await?;
            let mut visitor = load_owned(&mut conn, visitor_character_id, user_id).await?;
            let window = visited_window_mut(&owner, &mut visitor, shop_id, now, |b, prev| {
                window_for(&globals, shop_id, b, prev, now, false)
            });
            let wire = window_to_wire(shop_id, window);
            write_back(&mut conn, visitor).await?;

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
/// else's merchant, out of the visitor's own window on it.
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
            if owner_character_id == visitor_character_id {
                return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 20000, 5));
            }
            let owner = load_other(&mut conn, owner_character_id, owner_user_id).await?;
            let mut visitor = load_owned(&mut conn, visitor_character_id, user_id).await?;

            let mut window = visited_window_mut(&owner, &mut visitor, shop_id, now, |b, prev| {
                window_for(&globals, shop_id, b, prev, now, false)
            })
            .clone();
            let mut tracker = InventoryChangeTracker::default();
            buy_bundles(&globals, &mut window, &bundles, &mut visitor, &mut tracker)?;

            let inventory = visitor.inventory.0.generate_client_update(&tracker);
            let wallet = visitor.wallet.0.clone();
            let character = CompleteCharacterWithIdWithoutData {
                id: visitor_character_id,
                character: visitor.character.0.clone(),
            };
            let shop = txn_state(shop_id, &window);
            visitor
                .server_state
                .0
                .visited_shops
                .insert(visited_shop_key(owner_character_id, shop_id), window);
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
            let building = stock_building_for(&entry, shop_id);
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
            let building = stock_building_for(&entry, shop_id);
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
    use crate::json_db::JsonDbWrapper;
    use blades_lib::{
        server_state::ServerState,
        user_data::{Backpack, CompleteCharacter, CompleteInventory, Loadout, Treasury},
    };
    use serde_json::json;

    const FORGE: &str = "26fdb92f-a4df-4928-a97b-dee8699af605";

    /// A town shaped like the real JSONB: districts[].segments{}.buildings{}. The shop
    /// id passed to `/shops/{id}` is a building INSTANCE id and must resolve to its
    /// `(typeId, level)`.
    fn town_fixture(building_id: Uuid) -> Value {
        town_fixture_with_level(building_id, 4)
    }

    fn town_fixture_with_level(building_id: Uuid, building_level: u64) -> Value {
        json!({
            "levelInfo": { "level": 6 },
            "districts": [{
                "segments": {
                    "seg-1": {
                        "buildings": {
                            building_id.to_string(): {
                                "id": building_id.to_string(),
                                "typeId": FORGE,
                                "level": building_level,
                                "state": "NORMAL"
                            }
                        }
                    }
                }
            }]
        })
    }

    fn shop_entry(
        building_id: Uuid,
        building_level: u64,
        player_level: u16,
    ) -> CharacterDbEntryShop {
        let mut character = CompleteCharacter::default();
        character.level = player_level;
        CharacterDbEntryShop {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character: JsonDbWrapper(character),
            wallet: JsonDbWrapper(CompleteWallet::default()),
            inventory: JsonDbWrapper(CompleteInventory {
                backpack: Backpack::default(),
                loadout: Loadout::default(),
                treasury: Treasury::default(),
                overflow_treasury: Treasury::default(),
                backpack_version: 0,
                treasury_version: 0,
            }),
            town: Some(JsonDbWrapper(town_fixture_with_level(
                building_id,
                building_level,
            ))),
            server_state: JsonDbWrapper(ServerState::default()),
        }
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

    #[test]
    fn stock_band_comes_from_building_level_not_high_player_level() {
        let bid = Uuid::new_v4();
        let entry = shop_entry(bid, 1, 86);
        let (_type_id, stock_level) = stock_building_for(&entry, bid).unwrap();

        assert_eq!(
            stock_level, 1,
            "negative control for #397: a level-86 player in a level-1 forge must not roll level-9 stock"
        );
    }

    #[test]
    fn same_player_level_can_open_different_building_stock_levels() {
        let low = Uuid::new_v4();
        let high = Uuid::new_v4();
        let low_entry = shop_entry(low, 4, 20);
        let high_entry = shop_entry(high, 6, 20);

        assert_eq!(stock_building_for(&low_entry, low).unwrap().1, 4);
        assert_eq!(
            stock_building_for(&high_entry, high).unwrap().1,
            6,
            "control: changing only the building level changes the generated-stock band"
        );
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
        shops.insert(
            live,
            MerchantWindow {
                expiration_ms: now + 1000,
                ..Default::default()
            },
        );
        shops.insert(
            stale,
            MerchantWindow {
                expiration_ms: now - merchant::BUYBACK_MS - 1,
                ..Default::default()
            },
        );
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
        assert!(
            !shops.contains_key(&stale),
            "expired, buyback-free windows are dropped"
        );
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

    // ── report #287: finishing a vendor's build restocks it ─────────────────

    fn buyback(shop_id: Uuid, id: u128, expiration: i64) -> Buyback {
        Buyback {
            id: Uuid::from_u128(id),
            shop_id,
            item: None,
            stackable_item: None,
            expiration,
            price: 5,
        }
    }

    /// A drained window with an hour to run is closed by the restock, so the next
    /// open rolls a new one; buybacks that are still live carry over, dead ones
    /// do not, and the left-over slot is neither served nor pruned.
    #[test]
    fn restock_on_build_complete_closes_the_window_and_keeps_live_buybacks() {
        let now = 50_000_000i64;
        let shop = Uuid::from_u128(7);
        let mut shops = HashMap::new();
        let mut drained = MerchantWindow {
            catalog_id: Uuid::new_v4(),
            start_ms: now - 1000,
            expiration_ms: now + 3_600_000,
            bundles: vec![(Uuid::from_u128(0xb1), 4)],
            wallet_gold: 1174,
            revenue_gold: -1174,
            buybacks: vec![
                buyback(shop, 1, now + 60_000),
                buyback(shop, 2, now - 1),
            ],
            ..Default::default()
        };
        drained.sales.insert(Uuid::from_u128(0xb1), 4);
        shops.insert(shop, drained);
        assert!(shops[&shop].is_live(now) && shops[&shop].remaining_budget() == 0);

        restock_on_build_complete(&mut shops, shop, now);

        let left = &shops[&shop];
        assert!(!left.is_live(now), "the next open must roll a new window");
        assert!(left.sales.is_empty() && left.revenue_gold == 0 && left.bundles.is_empty());
        assert_eq!(left.buybacks.len(), 1, "only the live buyback survives");
        assert_eq!(left.buybacks[0].id, Uuid::from_u128(1));
        prune_stale_shops(&mut shops, now);
        assert!(shops.contains_key(&shop), "a live buyback keeps its slot");
    }

    #[test]
    fn restock_on_build_complete_without_buybacks_forgets_the_window() {
        let now = 50_000_000i64;
        let shop = Uuid::from_u128(8);
        let other = Uuid::from_u128(9);
        let live = MerchantWindow {
            catalog_id: Uuid::new_v4(),
            expiration_ms: now + 3_600_000,
            ..Default::default()
        };
        let mut shops = HashMap::from([(shop, live.clone()), (other, live)]);
        restock_on_build_complete(&mut shops, shop, now);
        assert!(!shops.contains_key(&shop));
        assert!(shops[&other].is_live(now), "only the finished shop restocks");
        // A shop never opened before has nothing to close and still answers.
        restock_on_build_complete(&mut shops, Uuid::from_u128(10), now);
    }

    #[test]
    fn the_vendor_buildings_are_the_four_with_generated_stock() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/shop_stock.json");
        let f = std::fs::File::open(&p).expect("shop_stock.json present");
        let cfg: shop_gen::ShopStockConfig =
            serde_json::from_reader(std::io::BufReader::new(f)).expect("shop_stock.json parses");
        let mut generated: Vec<Uuid> = cfg.generation.keys().copied().collect();
        let mut vendors = VENDOR_BUILDING_TYPES.to_vec();
        generated.sort();
        vendors.sort();
        assert_eq!(generated, vendors);
        let house = Uuid::parse_str("597f678f-b49e-4559-96a8-266aafeca6ad").unwrap();
        assert!(!is_vendor_building(house));
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
        for key in [
            "id",
            "templateId",
            "bundles",
            "wallet",
            "start",
            "expiration",
            "expired",
        ] {
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

    // ── #316: a merchant in someone else's town is stocked per visitor ────────

    const RING: Uuid = Uuid::from_u128(0x3161);
    const POTION: Uuid = Uuid::from_u128(0x3162);

    /// The catalog the owner's building would roll: one ring, five potions.
    fn stocked_window(now: i64) -> MerchantWindow {
        MerchantWindow {
            catalog_id: Uuid::new_v4(),
            start_ms: now,
            expiration_ms: merchant::expiration_for(now),
            bundles: vec![(RING, 1), (POTION, 5)],
            wallet_gold: 1_000,
            ..Default::default()
        }
    }

    /// `window_for`'s contract without a `ServerGlobal`: reuse a live window, else
    /// roll the owner's catalog.
    fn roll(
        now: i64,
    ) -> impl FnOnce(Option<(Uuid, u64)>, Option<&MerchantWindow>) -> MerchantWindow {
        move |_, prev| {
            prev.filter(|w| w.is_live(now))
                .cloned()
                .unwrap_or_else(|| stocked_window(now))
        }
    }

    /// What `visitor` sees opening `owner`'s merchant — the open route's state step.
    fn open_as(
        owner: &CharacterDbEntryShop,
        visitor: &mut CharacterDbEntryShop,
        shop: Uuid,
        now: i64,
    ) -> MerchantWindow {
        visited_window_mut(owner, visitor, shop, now, roll(now)).clone()
    }

    /// `visitor` buying `qty` of `bundle` there — the purchase route's state step.
    fn buy_as(
        owner: &CharacterDbEntryShop,
        visitor: &mut CharacterDbEntryShop,
        shop: Uuid,
        bundle: Uuid,
        qty: u64,
        now: i64,
    ) -> u64 {
        let window = visited_window_mut(owner, visitor, shop, now, roll(now));
        book_sale(window, bundle, qty, 100, GOLD)
    }

    /// THE BUG (#316): one guildmate shopping at a merchant left the next one a
    /// half-empty shop. Retail gave every visitor the full assortment.
    #[test]
    fn two_visitors_each_see_the_owners_full_assortment() {
        let shop = Uuid::from_u128(0x316);
        let now = 1_780_000_000_000;
        let owner = shop_entry(shop, 4, 20);
        let mut a = shop_entry(Uuid::new_v4(), 4, 20);
        let mut b = shop_entry(Uuid::new_v4(), 4, 20);

        assert_eq!(buy_as(&owner, &mut a, shop, RING, 1, now), 1);
        assert_eq!(buy_as(&owner, &mut a, shop, POTION, 3, now), 3);

        let seen_by_b = open_as(&owner, &mut b, shop, now + 60_000);
        assert_eq!(
            seen_by_b.remaining_stock(RING),
            1,
            "B sees the ring A bought"
        );
        assert_eq!(seen_by_b.remaining_stock(POTION), 5);
        assert!(
            seen_by_b.sales.values().all(|q| *q == 0),
            "{:?}",
            seen_by_b.sales
        );
        assert_eq!(
            seen_by_b.revenue_gold, 0,
            "A's spending is not on B's ledger"
        );

        let seen_by_a = open_as(&owner, &mut a, shop, now + 60_000);
        assert_eq!(
            seen_by_a.remaining_stock(RING),
            0,
            "A's own purchase sticks"
        );
        assert_eq!(seen_by_a.remaining_stock(POTION), 2);
        assert_eq!(seen_by_a.revenue_gold, 400);
    }

    #[test]
    fn one_visitors_purchase_does_not_reduce_another_visitors_stock() {
        let shop = Uuid::from_u128(0x316);
        let now = 1_780_000_000_000;
        let owner = shop_entry(shop, 4, 20);
        let mut a = shop_entry(Uuid::new_v4(), 4, 20);
        let mut b = shop_entry(Uuid::new_v4(), 4, 20);

        // Both open first, then A buys the only ring.
        open_as(&owner, &mut a, shop, now);
        open_as(&owner, &mut b, shop, now);
        assert_eq!(buy_as(&owner, &mut a, shop, RING, 1, now), 1);
        assert_eq!(
            buy_as(&owner, &mut a, shop, RING, 1, now),
            0,
            "A has none left"
        );

        assert_eq!(
            buy_as(&owner, &mut b, shop, RING, 1, now),
            1,
            "B can still buy the ring A bought"
        );
    }

    #[test]
    fn a_visitor_never_writes_the_owners_own_merchant() {
        let shop = Uuid::from_u128(0x316);
        let now = 1_780_000_000_000;
        let mut owner = shop_entry(shop, 4, 20);
        let own = stocked_window(now);
        owner.server_state.0.shops.insert(shop, own.clone());
        let mut a = shop_entry(Uuid::new_v4(), 4, 20);

        buy_as(&owner, &mut a, shop, RING, 1, now);
        buy_as(&owner, &mut a, shop, POTION, 5, now);

        let after = &owner.server_state.0.shops[&shop];
        assert_eq!(after.catalog_id, own.catalog_id);
        assert_eq!(after.remaining_stock(RING), 1);
        assert_eq!(after.remaining_stock(POTION), 5);
        assert_eq!(after.revenue_gold, 0);
        assert!(
            a.server_state.0.shops.is_empty(),
            "nor is it filed among the visitor's own merchants"
        );
    }

    #[test]
    fn visited_windows_are_per_owner_and_stale_ones_are_pruned() {
        let shop = Uuid::from_u128(0x316);
        let now = 1_780_000_000_000;
        // Two towns that happen to share a building instance id.
        let owner1 = shop_entry(shop, 4, 20);
        let owner2 = shop_entry(shop, 4, 20);
        let mut a = shop_entry(Uuid::new_v4(), 4, 20);

        buy_as(&owner1, &mut a, shop, RING, 1, now);
        assert_eq!(open_as(&owner2, &mut a, shop, now).remaining_stock(RING), 1);
        assert_eq!(a.server_state.0.visited_shops.len(), 2);

        // Eleven hours on, visiting owner2 forgets owner1's dead window.
        let later = now + 11 * 3_600_000;
        let fresh = open_as(&owner2, &mut a, shop, later);
        assert_eq!(
            fresh.start_ms, later,
            "an expired window re-rolls on the visit"
        );
        assert_eq!(a.server_state.0.visited_shops.len(), 1);
        assert!(
            a.server_state
                .0
                .visited_shops
                .contains_key(&visited_shop_key(owner2.id, shop))
        );
    }

    // ── #240/#jewelry: merchant jewellery rolls ───────────────────────────────

    fn deploy_items() -> &'static HashMap<Uuid, blades_lib::game_data::GameDataItem> {
        static T: std::sync::OnceLock<HashMap<Uuid, blades_lib::game_data::GameDataItem>> =
            std::sync::OnceLock::new();
        T.get_or_init(|| {
            let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
            let gd: blades_lib::game_data::GameData =
                serde_json::from_str(&std::fs::read_to_string(p).expect("parsed.json"))
                    .expect("game data");
            gd.items_template
        })
    }

    fn deploy_bundle(id: &str) -> blades_lib::static_data::ShopBundle {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/shop_bundles.json"
        );
        let all: serde_json::Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(p).expect("shop_bundles.json"))
                .expect("parses");
        serde_json::from_value(all[id].clone()).expect("bundle parses")
    }

    /// The two bundles the reporter bought, which retail sold graded.
    const GOLD_EMERALD_RING: &str = "01ada486-b642-43b2-8776-e444dbe9343e";
    const GOLD_EMERALD_NECKLACE: &str = "ee486683-c721-4bd6-8619-78cc00e34acc";

    fn slot_pool(slot: &str) -> std::collections::HashSet<Uuid> {
        crate::arena::combat::gamedata::GRADE_PROPERTIES
            .iter()
            .filter(|g| g.slot == slot)
            .map(|g| Uuid::parse_str(g.uuid).unwrap())
            .collect()
    }

    /// THE BUG: a merchant ring or necklace arrived with no grade and no GRADING.
    #[test]
    fn merchant_jewellery_is_minted_graded() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(240);
        for (bundle, slot) in [
            (GOLD_EMERALD_RING, "Ring"),
            (GOLD_EMERALD_NECKLACE, "Necklace"),
        ] {
            let def = deploy_bundle(bundle);
            let own = slot_pool(slot);
            // CONTROL: the static definition really is bare, so a graded result
            // can only have come from the minting path.
            assert!(
                def.grant.items[0].item.grade.is_none(),
                "{bundle}: fixture must be bare"
            );
            let reward = mint_bundle(&def.grant, 3, deploy_items(), &mut rng);
            assert_eq!(reward.items.len(), 3, "one instance per unit");
            for ri in &reward.items {
                let it = &ri.item;
                let grade = it.grade.expect("merchant jewellery must carry a grade");
                assert!(!it.properties.grading.is_empty(), "{bundle}: no GRADING");
                assert_eq!(
                    grade,
                    it.properties.grading.iter().map(|p| p.tier).sum::<u64>()
                );
                assert!(
                    it.properties.grading.iter().all(|p| own.contains(&p.id)),
                    "{bundle}: wrong slot"
                );
                assert!(
                    it.properties.enchanting.is_empty(),
                    "retail merchant jewellery had no ENCHANTING"
                );
                assert_eq!(it.arcane_tier, None);
                // The wire shape retail sent: grade, never temperingLevel/durability.
                let wire = serde_json::to_value(ri).unwrap();
                assert!(wire.get("grade").is_some(), "{wire}");
                assert!(
                    wire.get("durability").is_none() && wire.get("temperingLevel").is_none(),
                    "{wire}"
                );
            }
            let ids: std::collections::HashSet<_> = reward.items.iter().map(|i| i.id).collect();
            assert_eq!(ids.len(), 3, "fresh instance ids");
        }
    }

    /// Retail rolled per purchase: the same bundle came out at grade 1 and grade 3.
    #[test]
    fn buying_the_same_ring_twice_can_give_two_grades() {
        use rand::SeedableRng;
        let def = deploy_bundle(GOLD_EMERALD_RING);
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        let grades: std::collections::HashSet<u64> = (0..200)
            .map(|_| {
                mint_bundle(&def.grant, 1, deploy_items(), &mut rng).items[0]
                    .item
                    .grade
                    .unwrap()
            })
            .collect();
        assert_eq!(grades, (1..=6).collect(), "every grade must be reachable");
    }

    /// Generated Enchanter jewelry is rolled at catalog/window creation. Buying it
    /// must mint exactly that stored item, not roll a different one at purchase.
    #[test]
    fn generated_jewelry_purchase_keeps_the_window_roll() {
        let def = deploy_bundle(GOLD_EMERALD_RING);
        let mut generated = def.grant.clone();
        let mut roll_rng = crate::jewelry_roll::seeded(
            &[
                Uuid::from_u128(0x5150).as_bytes(),
                Uuid::parse_str(GOLD_EMERALD_RING).unwrap().as_bytes(),
            ],
            7,
        );
        crate::jewelry_roll::roll_generated_jewelry(
            &mut generated.items[0].item,
            deploy_items(),
            &mut roll_rng,
        );
        let shown = generated.items[0].item.clone();
        assert!(
            shown.grade.is_some() && !shown.properties.grading.is_empty(),
            "generated stock must carry its rolled grade"
        );

        let mut purchase_rng = crate::jewelry_roll::seeded(&[b"purchase"], 99);
        let bought = mint_bundle(&generated, 1, deploy_items(), &mut purchase_rng);
        let item = &bought.items[0].item;
        assert_eq!(item.grade, shown.grade);
        assert_eq!(item.properties, shown.properties);
        assert_eq!(item.item_template_id, shown.item_template_id);
    }

    /// Every ring/necklace bundle a town merchant can carry.
    fn deploy_jewelry_bundles() -> Vec<(String, blades_lib::static_data::ShopBundle)> {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/shop_bundles.json"
        );
        let all: serde_json::Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        all.into_iter()
            .filter_map(|(id, raw)| {
                let def: blades_lib::static_data::ShopBundle = serde_json::from_value(raw).ok()?;
                def.grant
                    .items
                    .iter()
                    .any(|i| {
                        crate::jewelry_roll::is_jewelry_template(
                            i.item.item_template_id,
                            deploy_items(),
                        )
                    })
                    .then_some((id, def))
            })
            .collect()
    }

    /// The retail shape of a town-merchant ring or necklace, measured on the 4
    /// jewellery items among the 1,516 retail `POST /shops/{id}/purchase`
    /// responses (prod capture archive, 2026-05-09..06-30): `grade` 1..=3 with
    /// that many GRADING properties (the random skills), and no ENCHANTING at
    /// all — a blank for the player to enchant.
    fn assert_retail_merchant_jewelry(where_: &str, it: &blades_lib::user_data::Item) {
        let grade = it.grade.unwrap_or_else(|| panic!("{where_}: no grade"));
        assert!(
            !it.properties.grading.is_empty() && it.properties.grading.len() <= 3,
            "{where_}: {} GRADING",
            it.properties.grading.len()
        );
        assert_eq!(
            grade,
            it.properties.grading.iter().map(|p| p.tier).sum::<u64>(),
            "{where_}: grade must equal the GRADING tiers"
        );
        assert!(
            it.properties.enchanting.is_empty(),
            "{where_}: retail merchant jewellery was a blank, got {} ENCHANTING",
            it.properties.enchanting.len()
        );
        assert_eq!(it.arcane_tier, None, "{where_}: arcaneTier on a blank");
    }

    /// Roll a bundle the way `roll_generated_jewelry_for_window` does.
    fn roll_like_the_window(
        grant: &mut blades_lib::economy::RewardGrant,
        rng: &mut rand::rngs::StdRng,
    ) {
        for item in &mut grant.items {
            crate::jewelry_roll::roll_generated_jewelry(&mut item.item, deploy_items(), rng);
        }
    }

    /// THE BUG (#321): Enchanter stock was rolled with the Sigil store's measured
    /// ENCHANTING counts (mostly 3), so a ring bought in town arrived with three
    /// secondary enchantments and no primary. Retail sold every one blank.
    #[test]
    fn generated_merchant_jewelry_is_a_blank_with_random_skills() {
        let bundles = deploy_jewelry_bundles();
        assert!(
            bundles.len() >= 16,
            "the sweep must cover the merchant jewellery: {}",
            bundles.len()
        );
        let mut grades = std::collections::HashSet::new();
        for (id, def) in &bundles {
            for nonce in 0..12 {
                let mut grant = def.grant.clone();
                let mut rng = crate::jewelry_roll::seeded(&[id.as_bytes()], nonce);
                roll_like_the_window(&mut grant, &mut rng);
                for ri in &grant.items {
                    assert_retail_merchant_jewelry(id, &ri.item);
                    grades.insert(ri.item.grade.unwrap());
                }
            }
        }
        assert!(
            grades.len() > 1,
            "skills must be random, got grades {grades:?}"
        );
    }

    /// A window rolled before this fix is persisted for up to ten hours with
    /// enchanted jewellery in `generatedGrants`. Buying from it must still hand
    /// over the blank — the GRADING roll it showed stays, the ENCHANTING goes.
    /// The fixture is the shape SpaceMunk's alt received (grade 5, 3 + 3).
    #[test]
    fn a_window_rolled_before_the_fix_still_sells_a_blank() {
        use rand::SeedableRng;
        let prop = |s: &str, tier| blades_lib::user_data::ItemSingleProperty {
            id: Uuid::parse_str(s).unwrap(),
            tier,
        };
        let mut grant = deploy_bundle(GOLD_EMERALD_RING).grant;
        let item = &mut grant.items[0].item;
        item.grade = Some(5);
        item.properties.grading = vec![
            prop("6bc19568-2f76-4c0e-8482-dee16629dc5b", 2),
            prop("41e72fea-cc55-41c6-b89c-e9ccc88a17f3", 2),
            prop("ede8bce4-de2c-4ca5-bc44-c214f15189ba", 1),
        ];
        item.properties.enchanting = vec![
            prop("848e02b4-32ae-4e1b-809c-83bca039542a", 1),
            prop("262ece9b-bd65-4876-b698-5926ca4422be", 1),
            prop("98757a01-33b8-40ea-bb45-6acd89811ae3", 1),
        ];
        let shown = grant.items[0].item.clone();
        let mut rng = rand::rngs::StdRng::seed_from_u64(321);
        let bought = mint_bundle(&grant, 2, deploy_items(), &mut rng);
        assert_eq!(bought.items.len(), 2);
        for ri in &bought.items {
            assert_retail_merchant_jewelry("stale window", &ri.item);
            assert_eq!(ri.item.grade, shown.grade, "the shown grade is kept");
            assert_eq!(ri.item.properties.grading, shown.properties.grading);
        }
    }

    /// NEGATIVE CONTROL: gear that wears is never graded, and keeps its durability.
    #[test]
    fn merchant_weapons_and_stackables_are_untouched() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let items = deploy_items();
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/shop_bundles.json"
        );
        let all: serde_json::Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let (mut gear, mut stacks) = (0, 0);
        for (id, raw) in &all {
            let Ok(def) =
                serde_json::from_value::<blades_lib::static_data::ShopBundle>(raw.clone())
            else {
                continue;
            };
            let reward = mint_bundle(&def.grant, 2, items, &mut rng);
            assert_eq!(
                reward.stackable_items.len(),
                def.grant.stackable_items.len()
            );
            for (t, n) in &def.grant.stackable_items {
                assert_eq!(
                    reward.stackable_items[t],
                    n * 2,
                    "{id}: stackables multiply"
                );
                stacks += 1;
            }
            for (ri, src) in reward.items.iter().zip(def.grant.items.iter().cycle()) {
                let ty = items.get(&ri.item.item_template_id).map(|t| t.r#type);
                if matches!(ty, Some(10) | Some(11)) {
                    continue;
                }
                gear += 1;
                assert_eq!(ri.item.grade, None, "{id}: non-jewellery graded");
                assert!(
                    ri.item.properties.grading.is_empty(),
                    "{id}: GRADING on gear"
                );
                assert_eq!(
                    ri.item.durability, src.item.durability,
                    "{id}: durability changed"
                );
            }
        }
        assert!(
            gear > 100 && stacks > 100,
            "the sweep must cover real data: {gear} gear, {stacks} stacks"
        );
    }

    /// NEGATIVE CONTROL: an item that already carries a grade (authored or
    /// captured) is what retail sent, and must pass through verbatim.
    #[test]
    fn an_already_graded_item_is_not_rerolled() {
        use rand::SeedableRng;
        let mut def = deploy_bundle(GOLD_EMERALD_RING);
        let authored = blades_lib::user_data::ItemSingleProperty {
            id: Uuid::parse_str("b442ea19-02cb-4825-aa13-2f2f14ccd338").unwrap(),
            tier: 1,
        };
        def.grant.items[0].item.grade = Some(1);
        def.grant.items[0].item.properties.grading = vec![authored.clone()];
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        for _ in 0..50 {
            let r = mint_bundle(&def.grant, 1, deploy_items(), &mut rng);
            assert_eq!(r.items[0].item.grade, Some(1));
            assert_eq!(r.items[0].item.properties.grading, vec![authored.clone()]);
        }
    }
}
