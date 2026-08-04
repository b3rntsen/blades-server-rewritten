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

/// `GET /catalogoverrides/globalshop` — the override catalogue, shifted so retail's
/// rotation covers the present. See [`shift_to_now`].
#[get("/blades.bgs.services/api/game/v1/public/catalogoverrides/globalshop")]
pub async fn get_override(app_state: web::Data<Arc<ServerGlobal>>) -> Json<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Json(shift_to_now(&app_state.static_data.global_shop_overrides, now))
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
}
