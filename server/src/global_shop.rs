//! Global store — `GET /catalogoverrides/globalshop`, `GET /catalogoverrides/iap`,
//! `GET /…/globalshops/current`, `POST /…/globalshops/current/purchase`.
//!
//! The Sigil/Gem sink. The override catalogue and IAP catalogue are served verbatim
//! from capture-derived JSON; a purchase verifies the client's expected price
//! against the active server-served promotion or APK base catalogue, debits it,
//! grants the capture-derived product reward, and bumps the per-character purchase
//! count. IAP (real money) is a priced placeholder only — there is no fulfillment
//! route. See [`blades_lib::features::global_shop`].

use std::sync::Arc;

use actix_web::{
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{Price, RewardGrant, apply_reward};
use blades_lib::features::global_shop::{self, PurchaseEntry, PurchaseError};
use blades_lib::static_data::{OfferContents, OfferContentsKind};
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

/// How long retail's captured rotation runs before it repeats.
///
/// The corpus spans 2026-05-01 04:00 UTC to 2026-07-06 16:00 UTC — 66.5 days —
/// once the three ~950-day evergreen offers are set aside. Rounded UP to a whole
/// number of DAYS, which matters: retail rotated the daily block at 16:00 UTC and
/// the featured slot at 05:00 UTC, and only a whole-day shift keeps those at the
/// same clock times. A shift of 66.5 days would move every rotation to the middle
/// of the night for half the cycle.
const REPLAY_PERIOD_DAYS: i64 = 67;
const REPLAY_PERIOD: i64 = REPLAY_PERIOD_DAYS * 86_400;

/// Bring retail's schedule forward so it covers the present.
///
/// WHY (tracker #18)
///
/// The catalogue was already served — 547 offers, verbatim, exactly as retail sent
/// them. But every window in it closed by 2026-07-06, so the client filtered all of
/// them out and the shop was empty. Not "we serve nothing": we served 547 expired
/// offers, which looks identical from inside the game and is a different bug.
///
/// This shifts every window by a whole number of REPLAY_PERIODs — one constant
/// offset for the entire catalogue, so the *relative* timing retail authored is
/// preserved exactly. The daily block still turns over together, the Tuesday and
/// Thursday block still lands on Tuesday and Thursday, the Monday-anchored weekly
/// windows still start on a Monday (67 is not a multiple of 7, so that last one
/// drifts — see the caveat below).
///
/// The alternative was authoring a fresh schedule. That is Phase 2 and it needs a
/// product decision; this is the smaller thing that makes the shop work today
/// without inventing anything.
///
/// CAVEAT, stated because it is the one thing this gets wrong: 67 days is not a
/// whole number of weeks, so weekday alignment drifts by 4 days each cycle. The
/// 196 Monday-anchored weekly windows will not stay on Mondays. Fixing that means
/// choosing 63 or 70 days and accepting a gap or an overlap in the daily block
/// instead — a trade with no free side, and one for the owner rather than for me.
fn shift_to_now(overrides: &Value, now: i64) -> Value {
    let Some(map) = overrides
        .get("globalShopOverrides")
        .and_then(|v| v.as_object())
    else {
        return overrides.clone();
    };

    // Anchor on the LATEST end in the corpus: the number of whole periods needed to
    // bring that past `now` is the shift for everything.
    let latest_end = map
        .values()
        .filter_map(|v| v.get("activeEndDate").and_then(|d| d.as_i64()))
        .max()
        .unwrap_or(0);
    if latest_end == 0 || latest_end >= now {
        // Still inside the original schedule — nothing to do. Also the path taken
        // by a corpus that has been refreshed with newer captures.
        return overrides.clone();
    }
    let periods = (now - latest_end).div_euclid(REPLAY_PERIOD) + 1;
    let shift = periods * REPLAY_PERIOD;

    let mut out = serde_json::Map::new();
    for (id, entry) in map {
        let mut e = entry.clone();
        for field in ["activeStartDate", "activeEndDate"] {
            if let Some(t) = e.get(field).and_then(|d| d.as_i64()) {
                e[field] = Value::from(t + shift);
            }
        }
        // `maxPurchaseLimits` third form embeds the window start in its tracking id
        // (`<offer>::override::<override>::<activeStartDate>`), which is how retail
        // gives a recurring offer a fresh allowance each time round. Shift it too,
        // or every replayed cycle would share one allowance with the original and
        // a player who bought in cycle 1 could never buy again.
        if let Some(limits) = e.get_mut("maxPurchaseLimits").and_then(|l| l.as_array_mut()) {
            for lim in limits.iter_mut() {
                let Some(tid) = lim.get("purchaseTrackingId").and_then(|t| t.as_str()) else {
                    continue;
                };
                if let Some((head, tail)) = tid.rsplit_once("::") {
                    if let Ok(ts) = tail.parse::<i64>() {
                        lim["purchaseTrackingId"] = Value::from(format!("{head}::{}", ts + shift));
                    }
                }
            }
        }
        out.insert(id.clone(), e);
    }
    serde_json::json!({ "globalShopOverrides": out })
}

/// Lay admin-authored windows over the replayed catalogue.
///
/// WHY THIS IS A SEPARATE FILE AND NOT A FEW MORE ENTRIES IN THE OTHER ONE
///
/// [`shift_to_now`] anchors on the LATEST `activeEndDate` in the catalogue and does
/// nothing at all when that anchor is already past `now`. Write an authored window
/// for next week into `global_shop_overrides.json` and it becomes the latest end —
/// so the shift switches off, and all 547 retail offers snap back to their real
/// (July 2026, expired) windows. Authoring one offer would empty the shop.
///
/// Applied here instead, after the shift, an authored entry means exactly the dates
/// someone typed and nothing else moves. An id present in both wins here: authoring
/// a window for an offer that is already on the rotation is the normal case, and the
/// human's decision is the more recent fact.
///
/// An authored offer still needs a `global_shop_grants` entry or its purchase 404s;
/// the generator that writes this file refuses to author without one, and
/// `authored_offers_are_purchasable` guards the committed file.
fn apply_authored(catalog: Value, authored: &Value) -> Value {
    let Some(extra) = authored
        .get("globalShopOverrides")
        .and_then(|v| v.as_object())
        .filter(|m| !m.is_empty())
    else {
        return catalog;
    };
    let mut out = catalog;
    let Some(map) = out
        .get_mut("globalShopOverrides")
        .and_then(|v| v.as_object_mut())
    else {
        return out;
    };
    for (id, entry) in extra {
        map.insert(id.clone(), entry.clone());
    }
    out
}

/// Price the product from the same effective catalogue the client sees.
///
/// A live replayed/authored override wins over the APK base price. This keeps
/// Bethesda's captured promotions legitimate without turning their price into
/// a permanent discount for an always-available product. If neither source can
/// name a price, the purchase fails closed.
fn authoritative_prices(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
    now: i64,
) -> Option<Vec<Price>> {
    let current = apply_authored(
        shift_to_now(&static_data.global_shop_overrides, now),
        &static_data.global_shop_authored,
    );
    let override_entry = current
        .get("globalShopOverrides")
        .and_then(Value::as_object)
        .and_then(|m| m.get(&product_id.to_string()));
    if let Some(entry) = override_entry {
        let active = entry
            .get("isActive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let starts = entry
            .get("activeStartDate")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX);
        let ends = entry
            .get("activeEndDate")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MIN);
        if active && starts <= now && now <= ends {
            let parsed = entry
                .get("prices")
                .cloned()
                .and_then(|v| serde_json::from_value::<Vec<Price>>(v).ok())
                .filter(|p| !p.is_empty());
            if parsed.is_some() {
                return parsed;
            }
        }
    }
    static_data.global_shop_prices.get(&product_id).cloned()
}

fn validate_purchase_prices(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
    requested: &[Price],
    now: i64,
) -> Result<(), PurchaseError> {
    if static_data.global_shop_free.contains(&product_id) {
        return global_shop::sanitize_free_prices(requested);
    }
    global_shop::sanitize_prices(requested)?;
    match authoritative_prices(static_data, product_id, now) {
        Some(expected) if expected == requested => Ok(()),
        _ => Err(PurchaseError::InvalidPrice),
    }
}

/// `GET /catalogoverrides/globalshop` — the override catalogue, shifted so retail's
/// rotation covers the present, then overlaid with anything an admin authored. See
/// [`shift_to_now`] and [`apply_authored`].
#[get("/blades.bgs.services/api/game/v1/public/catalogoverrides/globalshop")]
pub async fn get_override(app_state: web::Data<Arc<ServerGlobal>>) -> Json<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Json(apply_authored(
        shift_to_now(&app_state.static_data.global_shop_overrides, now),
        &app_state.static_data.global_shop_authored,
    ))
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

/// Build a grantable reward from an offer's APK-derived contents, or `None` when
/// it cannot be granted without inventing data.
///
/// Only [`OfferContentsKind::Literal`] qualifies. The other kinds are refused on
/// purpose, and refusing is not a gap being left open — it is the alternative to
/// making item stats up:
///
/// * `NeedsRoll` (398 offers) contains gear or jewellery. Retail rolled
///   `durability`, `grade`, `arcaneTier` and `properties` at purchase time, so
///   granting one from this file would hand the player a fabricated item that
///   looks as authoritative as a real one. Rolling them properly needs the
///   rarity -> loot-table model, which is a separate piece of work.
/// * `ChestRoll` grants a chest rather than items. None of the 547 storefront
///   offers is one (all ten are IAP level offers), so wiring it here would be
///   untested code for a case that cannot arrive.
/// * `Unclassified` contains a template the extractor could not place in a
///   bucket, and `Unknown` is a `kind` from a newer data file. Guessing the
///   bucket would put gold in the backpack or a sword in the wallet.
///
/// The `bucket` field is the extractor's, derived from which wire key each
/// template actually landed in across the captured purchases — not inferred
/// here. An entry whose bucket is not one this function handles makes the whole
/// offer ungrantable rather than being dropped: a partial grant is a silent
/// short-change, and the player paid.
fn grant_from_offer_contents(contents: Option<&OfferContents>) -> Option<RewardGrant> {
    let c = contents?;
    if c.kind != OfferContentsKind::Literal {
        return None;
    }
    let mut reward = RewardGrant {
        town_xp: c.town_xp,
        ..RewardGrant::default()
    };
    for entry in &c.contents {
        let slot = match entry.bucket.as_str() {
            "currencies" => &mut reward.currencies,
            "stackableItems" => &mut reward.stackable_items,
            // `items` needs a rolled instance, and anything else is a bucket
            // this build does not know. Either way the offer is not grantable.
            _ => return None,
        };
        *slot.entry(entry.item_template_id).or_insert(0) += entry.quantity;
    }
    // An offer that resolves to nothing must not read as a successful purchase:
    // the player would be charged and handed an empty reward.
    if reward.is_empty() {
        return None;
    }
    Some(reward)
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

    // What this product grants. The captures cover 159 of the storefront's 547
    // offers, because an offer's contents only ever appeared in a purchase
    // RESPONSE — so the other 388 used to 404 here, and the client answered that
    // by prompting the player to reconnect to Bethesda.
    //
    // A capture-derived grant always WINS: it is a recording of what retail
    // actually handed over, instance stats and all, where the fallback below
    // knows only templates and quantities.
    let reward = match app_state
        .static_data
        .global_shop_grants
        .get(&body.global_shop_product_id)
    {
        Some(r) => r.clone(),
        None => grant_from_offer_contents(
            app_state
                .static_data
                .global_shop_offer_contents
                .get(&body.global_shop_product_id),
        )
        .ok_or_else(|| map_purchase_err(PurchaseError::NoSuchProduct))?,
    };
    // The store has free offers — retail's daily giveaway — and the client sends
    // `quantity: 0` for them. `sanitize_prices` rejects a zero quantity, so every
    // attempt to claim one 400'd (report #58's reporter hit it; the corpus shows
    // retail answering 200 to exactly this request five times before shutdown,
    // and us answering 400 fifteen times after).
    //
    // Zero is allowed ONLY for an offer on the capture-derived free list. The
    // price is client-supplied, so a blanket "allow zero" would give away every
    // paid offer.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    validate_purchase_prices(
        &app_state.static_data,
        body.global_shop_product_id,
        &body.expected_prices,
        now,
    )
    .map_err(map_purchase_err)?;

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
            // Chest products (e.g. the `1275d959…` chest bucket) grant a treasury chest;
            // `apply_reward` doesn't handle chests (they land in the treasury, not the
            // backpack), so grant each one here — mirrors quest.rs / daily_reward.rs. A
            // chest product with NO grants entry 404'd → the client prompted to reconnect
            // to Bethesda; a chest reward that never lands would be a silent no-op.
            if !reward.chests.is_empty() {
                for chest in &reward.chests {
                    blades_lib::economy::grant_chest(
                        &mut entry.inventory.0,
                        chest.tier,
                        chest.level,
                        &mut tracker,
                    );
                }
                entry.inventory.0.treasury_version += 1;
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

#[cfg(test)]
mod replay_tests {
    use super::*;

    /// The committed catalogue, as the server would serve it.
    fn catalog() -> Value {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/global_shop_overrides.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn live_count(v: &Value, now: i64) -> usize {
        v["globalShopOverrides"]
            .as_object()
            .unwrap()
            .values()
            .filter(|e| {
                e.get("isActive").and_then(|b| b.as_bool()).unwrap_or(false)
                    && e["activeStartDate"].as_i64().unwrap_or(0) <= now
                    && now <= e["activeEndDate"].as_i64().unwrap_or(0)
            })
            .count()
    }

    /// The bug: 547 offers served, every one of them expired, so the shop is empty
    /// in game. This fails if the shift is removed.
    #[test]
    fn the_shop_is_not_empty_after_the_captured_schedule_expires() {
        let raw = catalog();
        // A year past the end of the corpus.
        let now = 1_783_000_000 + 365 * 86_400;
        assert_eq!(
            live_count(&raw, now),
            0,
            "precondition: the raw catalogue really is all expired by then"
        );
        let shifted = shift_to_now(&raw, now);
        assert!(
            live_count(&shifted, now) > 0,
            "after shifting, something must actually be on sale"
        );
    }

    /// Every offer moves by the SAME whole number of periods, so retail's relative
    /// timing survives — the daily block still turns over together.
    #[test]
    fn the_whole_catalogue_moves_by_one_constant_offset() {
        let raw = catalog();
        let now = 1_783_000_000 + 200 * 86_400;
        let shifted = shift_to_now(&raw, now);
        let a = raw["globalShopOverrides"].as_object().unwrap();
        let b = shifted["globalShopOverrides"].as_object().unwrap();
        assert_eq!(a.len(), b.len(), "no offer is dropped");
        let mut offsets = std::collections::HashSet::new();
        for (id, before) in a {
            let after = &b[id];
            offsets.insert(after["activeStartDate"].as_i64().unwrap() - before["activeStartDate"].as_i64().unwrap());
            assert_eq!(
                after["activeEndDate"].as_i64().unwrap() - before["activeEndDate"].as_i64().unwrap(),
                after["activeStartDate"].as_i64().unwrap() - before["activeStartDate"].as_i64().unwrap(),
                "an offer's duration must not change",
            );
        }
        assert_eq!(offsets.len(), 1, "one offset for the entire catalogue, got {offsets:?}");
        assert_eq!(*offsets.iter().next().unwrap() % 86_400, 0, "a whole number of days");
    }

    /// Rotation clock times must survive, or the daily block moves to the middle of
    /// the night. This is why the period is whole DAYS and not 66.5.
    #[test]
    fn rotation_times_of_day_are_preserved() {
        let raw = catalog();
        let now = 1_783_000_000 + 500 * 86_400;
        let shifted = shift_to_now(&raw, now);
        for (id, before) in raw["globalShopOverrides"].as_object().unwrap() {
            let s0 = before["activeStartDate"].as_i64().unwrap();
            let s1 = shifted["globalShopOverrides"][id]["activeStartDate"].as_i64().unwrap();
            assert_eq!(s0 % 86_400, s1 % 86_400, "offer {id} changed its time of day");
        }
    }

    /// The per-occurrence purchase cap embeds the window start in its tracking id.
    /// If that is not shifted with the window, a replayed cycle shares its allowance
    /// with the original and a player who bought once can never buy again.
    #[test]
    fn per_occurrence_purchase_allowances_are_renewed() {
        let raw = catalog();
        let now = 1_783_000_000 + 300 * 86_400;
        let shifted = shift_to_now(&raw, now);
        let mut checked = 0;
        for (id, before) in raw["globalShopOverrides"].as_object().unwrap() {
            let (Some(bl), Some(al)) = (
                before.get("maxPurchaseLimits").and_then(|l| l.as_array()),
                shifted["globalShopOverrides"][id].get("maxPurchaseLimits").and_then(|l| l.as_array()),
            ) else { continue };
            for (b, a) in bl.iter().zip(al.iter()) {
                let bt = b["purchaseTrackingId"].as_str().unwrap();
                let at = a["purchaseTrackingId"].as_str().unwrap();
                match bt.rsplit_once("::").and_then(|(_, t)| t.parse::<i64>().ok()) {
                    // The per-occurrence form: its timestamp must have moved.
                    Some(ts) => {
                        let new_ts: i64 = at.rsplit_once("::").unwrap().1.parse().unwrap();
                        assert!(new_ts > ts, "per-occurrence allowance not renewed for {id}");
                        checked += 1;
                    }
                    // The lifetime and per-override forms carry no timestamp and
                    // must be left exactly alone.
                    None => assert_eq!(bt, at, "a non-occurrence tracking id was rewritten"),
                }
            }
        }
        assert!(checked > 100, "expected many per-occurrence ids, saw {checked}");
    }

    /// Inside the original window the catalogue is served untouched, so refreshing
    /// the corpus with newer captures turns the shift off by itself.
    #[test]
    fn a_current_schedule_is_left_alone() {
        let raw = catalog();
        let now = 1_780_000_000; // inside the captured range
        assert_eq!(shift_to_now(&raw, now), raw);
    }

    fn authored(id: &str, start: i64, end: i64) -> Value {
        serde_json::json!({
            "globalShopOverrides": {
                id: {
                    "activeStartDate": start,
                    "activeEndDate": end,
                    "isActive": true,
                    "maxPurchaseLimits": [],
                    "maxPurchases": 0,
                    "prices": [{ "currencyId": "c64bcb53-41f4-41ba-892a-fe2cca423caa", "quantity": 5 }],
                    "purchaseTrackingId": null,
                }
            }
        })
    }

    /// The whole reason authored windows live in their own file: an authored future
    /// window folded into the main catalogue would become the shift's anchor, the
    /// shift would switch off, and every retail offer would revert to expired.
    ///
    /// This asserts the damage directly rather than asserting the plumbing.
    #[test]
    fn authoring_a_future_window_does_not_empty_the_shop() {
        let raw = catalog();
        let now = 1_783_000_000 + 365 * 86_400;

        // The trap, spelled out: merged into the base file FIRST, the way the
        // obvious implementation would have done it.
        let mut merged = raw.clone();
        merged["globalShopOverrides"]["11111111-1111-4111-8111-111111111111"] =
            authored("x", now + 30 * 86_400, now + 37 * 86_400)["globalShopOverrides"]["x"].clone();
        // Nothing at all is live: the authored window becomes the anchor, the shift
        // switches off, every retail offer reverts to expired, and the authored one
        // has not started yet either. An empty store, from adding one offer.
        assert_eq!(
            live_count(&shift_to_now(&merged, now), now),
            0,
            "precondition: merging first really does empty the shop",
        );

        // Applied after the shift, the rotation is untouched.
        let served = apply_authored(
            shift_to_now(&raw, now),
            &authored("11111111-1111-4111-8111-111111111111", now + 30 * 86_400, now + 37 * 86_400),
        );
        assert!(
            live_count(&served, now) > 1,
            "the retail rotation must still be live alongside the authored offer",
        );
    }

    /// An authored window means the dates someone typed — not those dates plus a
    /// replay offset.
    #[test]
    fn an_authored_window_is_never_shifted() {
        let now = 1_783_000_000 + 365 * 86_400;
        let (start, end) = (now + 86_400, now + 3 * 86_400);
        let served = apply_authored(
            shift_to_now(&catalog(), now),
            &authored("11111111-1111-4111-8111-111111111111", start, end),
        );
        let e = &served["globalShopOverrides"]["11111111-1111-4111-8111-111111111111"];
        assert_eq!(e["activeStartDate"].as_i64(), Some(start));
        assert_eq!(e["activeEndDate"].as_i64(), Some(end));
    }

    /// Authoring a window for an offer already on the rotation replaces its window
    /// rather than adding a duplicate — the human's decision is the newer fact.
    #[test]
    fn an_authored_window_overrides_the_replayed_one() {
        let raw = catalog();
        let now = 1_783_000_000 + 365 * 86_400;
        let id = raw["globalShopOverrides"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let before = shift_to_now(&raw, now);
        let served = apply_authored(before.clone(), &authored(&id, 7, 9));
        assert_eq!(
            served["globalShopOverrides"].as_object().unwrap().len(),
            before["globalShopOverrides"].as_object().unwrap().len(),
            "no duplicate entry",
        );
        assert_eq!(served["globalShopOverrides"][&id]["activeEndDate"].as_i64(), Some(9));
    }

    /// The normal case — an empty authored file must change nothing at all.
    #[test]
    fn an_empty_authored_file_is_a_no_op() {
        let now = 1_783_000_000 + 365 * 86_400;
        let shifted = shift_to_now(&catalog(), now);
        let empty = serde_json::json!({ "globalShopOverrides": {} });
        assert_eq!(apply_authored(shifted.clone(), &empty), shifted);
        assert_eq!(apply_authored(shifted.clone(), &Value::Null), shifted);
    }

    /// Every authored offer must have a reward definition, or buying it 404s and the
    /// client prompts the player to reconnect to Bethesda. The generator refuses to
    /// write one without; this is the guard on the committed file, and it starts
    /// vacuous because the file starts empty.
    #[test]
    fn authored_offers_are_purchasable() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let grants = &sd.global_shop_grants;
        for id in sd.global_shop_authored["globalShopOverrides"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            let uuid: Uuid = id.parse().unwrap_or_else(|e| panic!("authored id {id}: {e}"));
            assert!(
                grants.contains_key(&uuid),
                "authored offer {id} has no global_shop_grants entry — buying it would 404",
            );
        }
    }
}

#[cfg(test)]
mod authoritative_price_tests {
    use super::*;
    use blades_lib::{
        economy::{GEMS, SIGIL},
        static_data::{FreeProductIds, StaticData},
    };

    const PRODUCT: Uuid = Uuid::from_u128(0x1275d959_bbe5_460d_8f6a_1c31106a8eb2);
    const FREE_PRODUCT: Uuid = Uuid::from_u128(0x6135f729_1402_424c_b6c6_efcbc4e59955);
    const NOW: i64 = 1_780_000_000;

    fn base_data() -> StaticData {
        StaticData {
            global_shop_prices: [(PRODUCT, vec![Price::new(GEMS, 2500)])]
                .into_iter()
                .collect(),
            global_shop_overrides: serde_json::json!({"globalShopOverrides": {}}),
            global_shop_authored: serde_json::json!({"globalShopOverrides": {}}),
            ..StaticData::default()
        }
    }

    #[test]
    fn a_client_cannot_name_its_own_discount() {
        let data = base_data();
        assert_eq!(
            validate_purchase_prices(&data, PRODUCT, &[Price::new(GEMS, 1)], NOW),
            Err(PurchaseError::InvalidPrice)
        );
        assert_eq!(
            validate_purchase_prices(&data, PRODUCT, &[Price::new(GEMS, 2500)], NOW),
            Ok(())
        );
    }

    #[test]
    fn a_live_server_served_promotion_overrides_the_base_price() {
        let mut data = base_data();
        data.global_shop_overrides = serde_json::json!({
            "globalShopOverrides": {
                PRODUCT.to_string(): {
                    "activeStartDate": NOW - 100,
                    "activeEndDate": NOW + 100,
                    "isActive": true,
                    "prices": [{"currencyId": GEMS, "quantity": 1}]
                }
            }
        });
        assert_eq!(
            validate_purchase_prices(&data, PRODUCT, &[Price::new(GEMS, 1)], NOW),
            Ok(())
        );
        assert_eq!(
            validate_purchase_prices(&data, PRODUCT, &[Price::new(GEMS, 2500)], NOW),
            Err(PurchaseError::InvalidPrice)
        );
    }

    #[test]
    fn an_inactive_override_does_not_become_a_permanent_discount() {
        let mut data = base_data();
        data.global_shop_overrides = serde_json::json!({
            "globalShopOverrides": {
                PRODUCT.to_string(): {
                    "activeStartDate": NOW - 100,
                    "activeEndDate": NOW + 100,
                    "isActive": false,
                    "prices": [{"currencyId": GEMS, "quantity": 1}]
                }
            }
        });
        assert_eq!(
            validate_purchase_prices(&data, PRODUCT, &[Price::new(GEMS, 1)], NOW),
            Err(PurchaseError::InvalidPrice)
        );
        assert_eq!(
            validate_purchase_prices(&data, PRODUCT, &[Price::new(GEMS, 2500)], NOW),
            Ok(())
        );
    }

    #[test]
    fn the_capture_proven_free_offer_is_still_free() {
        let mut data = base_data();
        data.global_shop_free = FreeProductIds {
            free_product_ids: vec![FREE_PRODUCT],
        };
        assert_eq!(
            validate_purchase_prices(&data, FREE_PRODUCT, &[Price::new(SIGIL, 0)], NOW),
            Ok(())
        );
        assert_eq!(
            validate_purchase_prices(&data, FREE_PRODUCT, &[Price::new(SIGIL, 1)], NOW),
            Err(PurchaseError::InvalidPrice)
        );
    }

    #[test]
    fn an_unknown_product_has_no_client_chosen_price() {
        assert_eq!(
            validate_purchase_prices(
                &base_data(),
                Uuid::from_u128(0xdead),
                &[Price::new(GEMS, 1)],
                NOW,
            ),
            Err(PurchaseError::InvalidPrice)
        );
    }
}

/// The APK-derived offer-contents fallback (tracker #93).
#[cfg(test)]
/// Report #93 ("make it read then"): the file shipped, and the server threw it away.
///
/// `global_shop_offer_contents.json` holds 541 offers and ONE product id that is not
/// a UUID — `53c6f124-3603-4100-ba9a-e2fe23969f7p`, 36 characters with a `p` where
/// the last hex digit belongs. It comes that way from the game data itself
/// (`global_shop_overrides.json` carries the identical string), so regenerating the
/// file does not fix it.
///
/// `HashMap<Uuid, OfferContents>` failed the entire map on that key, serde aborted,
/// and `static_loader` fell back to `default()`. Prod said it out loud at 16:54 on
/// 2026-09-10 and nothing was watching:
///
/// ```text
/// [static] invalid "/data/static/global_shop_offer_contents.json":
///   UUID parsing failed: invalid character: found `p` at 36 at line 2794 column 41;
///   using default
/// ```
///
/// So every purchase was back to no contents to grant, which is exactly what the
/// reporter kept seeing after being told the data was shipped.
mod report93_bad_product_id {
    use blades_lib::static_data::OfferContentsFile;

    /// The real committed file: 540 good ids load and the one bad id is reported.
    #[test]
    fn the_shipped_file_loads_despite_its_one_bad_id() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/global_shop_offer_contents.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let file: OfferContentsFile = serde_json::from_str(&raw).expect("must parse at all");

        assert_eq!(
            file.unparseable_ids,
            vec!["53c6f124-3603-4100-ba9a-e2fe23969f7p".to_string()],
            "the one known-bad id must be reported, not silently dropped"
        );
        // THE regression. Before the fix this whole parse errored and the caller got
        // an empty map.
        assert_eq!(
            file.offers.len(),
            540,
            "541 entries minus the one unusable id — all the rest must load"
        );
    }

    /// The same shape in miniature, so a future data change cannot make the test
    /// above pass for the wrong reason.
    #[test]
    fn one_bad_key_costs_only_that_key() {
        let json = r#"{"offers":{
            "53c6f124-3603-4100-ba9a-e2fe23969f7p": {"kind":"literal"},
            "d07a8d30-9a1c-49b0-866d-97a8aa1534cf": {"kind":"literal"}
        }}"#;
        let file: OfferContentsFile = serde_json::from_str(json).expect("parses");
        assert_eq!(file.offers.len(), 1, "the good key survives");
        assert_eq!(file.unparseable_ids.len(), 1, "the bad key is reported");
    }

    /// The control: a file with no bad ids reports none, so `unparseable_ids` is a
    /// real signal rather than something always populated.
    #[test]
    fn a_clean_file_reports_nothing_skipped() {
        let json = r#"{"offers":{"d07a8d30-9a1c-49b0-866d-97a8aa1534cf": {"kind":"literal"}}}"#;
        let file: OfferContentsFile = serde_json::from_str(json).expect("parses");
        assert_eq!(file.offers.len(), 1);
        assert!(file.unparseable_ids.is_empty());
    }
}

#[cfg(test)]
mod offer_contents_fallback {
    use super::*;
    use blades_lib::static_data::OfferContentEntry;

    const GOLD: Uuid = Uuid::from_u128(0xf8d27767_a85e_4fd6_a5bb_bf8a13d0daa2);
    const CLAY: Uuid = Uuid::from_u128(0x42d91529_c88b_4c5b_815b_b55508b4e7ef);
    const SWORD: Uuid = Uuid::from_u128(0x0000_0001);

    fn entry(id: Uuid, qty: u64, bucket: &str) -> OfferContentEntry {
        OfferContentEntry { item_template_id: id, quantity: qty, bucket: bucket.into() }
    }

    fn offer(kind: OfferContentsKind, contents: Vec<OfferContentEntry>) -> OfferContents {
        OfferContents { kind, contents, town_xp: 0 }
    }

    /// The case this exists for: currencies and stackables, granted verbatim.
    /// Modelled on a real captured offer — gold plus three town resources.
    #[test]
    fn a_literal_offer_becomes_a_grantable_reward() {
        let o = offer(
            OfferContentsKind::Literal,
            vec![entry(GOLD, 10_000, "currencies"), entry(CLAY, 85, "stackableItems")],
        );
        let r = grant_from_offer_contents(Some(&o)).expect("literal must be grantable");
        assert_eq!(r.currencies.get(&GOLD), Some(&10_000));
        assert_eq!(r.stackable_items.get(&CLAY), Some(&85));
        assert!(r.items.is_empty(), "nothing may be invented into `items`");
    }

    /// The refusals. Each is a shape that would otherwise hand the player
    /// fabricated stats or a mis-bucketed item.
    #[test]
    fn every_other_kind_is_refused() {
        for kind in [
            OfferContentsKind::NeedsRoll,
            OfferContentsKind::ChestRoll,
            OfferContentsKind::Unclassified,
            OfferContentsKind::Unknown,
        ] {
            let o = offer(kind, vec![entry(GOLD, 1, "currencies")]);
            assert!(
                grant_from_offer_contents(Some(&o)).is_none(),
                "{kind:?} must not be grantable from template data"
            );
        }
        // control: the identical contents under `Literal` ARE grantable, so the
        // refusals above are about the kind and not about the contents.
        let o = offer(OfferContentsKind::Literal, vec![entry(GOLD, 1, "currencies")]);
        assert!(grant_from_offer_contents(Some(&o)).is_some());
    }

    /// A gear entry inside an otherwise-literal offer must sink the whole offer,
    /// not be quietly dropped — the player paid for all of it.
    #[test]
    fn an_unhandled_bucket_sinks_the_whole_offer() {
        let o = offer(
            OfferContentsKind::Literal,
            vec![entry(GOLD, 500, "currencies"), entry(SWORD, 1, "items")],
        );
        assert!(
            grant_from_offer_contents(Some(&o)).is_none(),
            "a partial grant is a silent short-change"
        );
    }

    /// No entry, or an unknown offer, must not read as a successful purchase:
    /// the price is charged before the reward is applied.
    #[test]
    fn an_empty_or_absent_offer_is_not_a_purchase() {
        assert!(grant_from_offer_contents(None).is_none(), "unknown product");
        let o = offer(OfferContentsKind::Literal, vec![]);
        assert!(grant_from_offer_contents(Some(&o)).is_none(), "empty reward");
    }

    /// townXp rides through, and on its own is enough to be a real reward —
    /// 16 of the captured grants carry a non-zero one.
    #[test]
    fn town_xp_rides_through() {
        let mut o = offer(OfferContentsKind::Literal, vec![entry(GOLD, 5, "currencies")]);
        o.town_xp = 40;
        let r = grant_from_offer_contents(Some(&o)).expect("grantable");
        assert_eq!(r.town_xp, 40);
    }

    /// A template listed twice sums rather than overwriting.
    #[test]
    fn a_repeated_template_sums() {
        let o = offer(
            OfferContentsKind::Literal,
            vec![entry(GOLD, 100, "currencies"), entry(GOLD, 25, "currencies")],
        );
        let r = grant_from_offer_contents(Some(&o)).expect("grantable");
        assert_eq!(r.currencies.get(&GOLD), Some(&125));
    }
}
