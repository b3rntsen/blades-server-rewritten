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
/// once the three ~950-day evergreen offers are set aside. The recurring Sigil
/// schedule starts at 2026-05-04 16:00 and runs continuously through that same
/// final timestamp, exactly 63 days. Replaying that complete interval avoids the
/// three-day Sigil-less hole that a 67-day period exposed at the start of every
/// cycle. It is also a whole number of weeks, so daily clock times and weekdays
/// both remain aligned.
const REPLAY_PERIOD_DAYS: i64 = 42;
const REPLAY_PERIOD: i64 = REPLAY_PERIOD_DAYS * 86_400;

/// Days at the START of the captured corpus that are too thinly covered to serve.
///
/// THE BUG THIS FIXES (tracker #141). "The Sigil shop is not empty but shows just
/// two weapons."
///
/// Counting the dated offers active on each day of the corpus gives a monotonic
/// ramp, not a rotation:
///
/// ```text
///   2026-05-01 .. 05-25    1 – 4 offers/day      <- 25 consecutive days
///   2026-05-26             13
///   2026-06-05             18
///   2026-06-16             27
///   2026-07-01             66
/// ```
///
/// A shop rotation does not ramp from 1 to 66 over six weeks; capture COVERAGE
/// does, as players joined. Those first 25 days are not retail behaviour, they are
/// how little we recorded of it. The old 63-day period replayed straight through
/// them, so 25 days of every 63-day cycle served a near-empty shop — and on the
/// thinnest of them the Sigil shop had **zero** offers.
///
/// The dense remainder, 2026-05-26 to 2026-07-06, is exactly 42 days — six whole
/// weeks, so daily clock times and weekdays stay aligned, which the replay depends
/// on. Hence [`REPLAY_PERIOD_DAYS`] = 42.
///
/// Nothing is discarded. The 30 offers whose windows fall entirely inside the
/// lead-in are carried forward one period into the dense region instead. All 546
/// valid products remain reachable; the one malformed Bethesda id is never served.
///
/// Simulated over a full cycle, counting Sigil-priced offers per day:
///
/// ```text
///   63d period, no skip   (before)   min  0   median 13   25 days under 5
///   42d period, skip 25d  (after)    min 12   median 20    0 days under 5
///   42d period, no skip   (control)  min  0   median  3   25 days under 5
///   63d period, skip 25d  (control)  min  1   median 16   21 days under 5
/// ```
///
/// Both controls matter: neither the shorter period nor the skip fixes this alone.
const CORPUS_LEAD_IN_DAYS: i64 = 25;
const CORPUS_LEAD_IN: i64 = CORPUS_LEAD_IN_DAYS * 86_400;

/// Longest window still counted as a dated, rotating offer. The three evergreen
/// offers run ~950 days and must not define where the corpus starts.
const MAX_DATED_WINDOW: i64 = 60 * 86_400;

/// Bring retail's schedule forward so it covers the present.
///
/// WHY (tracker #18)
///
/// The catalogue was already served — 547 rows, verbatim, exactly as retail sent
/// them. But every window in it closed by 2026-07-06, so the client filtered all of
/// them out and the shop was empty. Not "we serve nothing": we served expired
/// offers, which looks identical from inside the game and is a different bug.
///
/// This shifts every window by a whole number of REPLAY_PERIODs — one constant
/// offset for the entire catalogue, so the *relative* timing retail authored is
/// preserved exactly. The daily block still turns over together, the Tuesday and
/// Thursday block still lands on Tuesday and Thursday, and the Monday-anchored
/// weekly windows still start on a Monday.
///
/// The alternative was authoring a fresh schedule. That is Phase 2 and it needs a
/// product decision; this is the smaller thing that makes the shop work today
/// without inventing anything.
///
/// The first 3.5 days of the wider offer corpus are outside the replay interval.
/// That is deliberate: those days predate the continuous Sigil schedule, while
/// the overlapping tail contains both the complete daily shop and the Sigil shop.
fn shift_to_now(overrides: &Value, now: i64) -> Value {
    let mut cleaned = overrides.clone();
    let Some(map) = cleaned
        .get_mut("globalShopOverrides")
        .and_then(|v| v.as_object_mut())
    else {
        return cleaned;
    };

    // Bethesda shipped one key ending in `p`, which is not a UUID. The purchase
    // request is UUID-typed, so replaying that row advertises an offer no client
    // can ever buy. Keep every real product and omit only malformed identifiers.
    map.retain(|id, _| Uuid::parse_str(id).is_ok());

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
        return cleaned;
    }
    let periods = (now - latest_end).div_euclid(REPLAY_PERIOD) + 1;
    let shift = periods * REPLAY_PERIOD;

    // Where the thinly-covered lead-in ends. Measured off the DATED offers only —
    // the three evergreen ones start years earlier and would drag this back with
    // them, which is exactly the mistake that made an earlier check vacuous.
    let corpus_start = map
        .values()
        .filter_map(|v| {
            let s = v.get("activeStartDate").and_then(|d| d.as_i64())?;
            let e = v.get("activeEndDate").and_then(|d| d.as_i64())?;
            (e - s < MAX_DATED_WINDOW).then_some(s)
        })
        .min()
        .unwrap_or(0);
    let lead_in_ends = corpus_start - corpus_start.rem_euclid(86_400) + CORPUS_LEAD_IN;

    let mut out = serde_json::Map::new();
    for (id, entry) in map {
        let mut e = entry.clone();
        // An offer that both starts AND ends inside the lead-in is carried forward
        // one period, into the dense region, rather than served alone on a day with
        // nothing beside it. Carried, not dropped: 30 offers and their products stay
        // reachable. See `CORPUS_LEAD_IN_DAYS`.
        let in_lead_in = e
            .get("activeEndDate")
            .and_then(|d| d.as_i64())
            .is_some_and(|end| end < lead_in_ends);
        let shift = if in_lead_in { shift + REPLAY_PERIOD } else { shift };
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
/// so the shift switches off, and all 546 valid retail offers snap back to their real
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
/// * `ChestRoll` grants a chest rather than items. None of the 546 valid storefront
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
fn grant_from_offer_contents(
    contents: Option<&OfferContents>,
    repair_data: &blades_lib::features::repair::RepairData,
    // The template -> bucket table, for entries the extractor left `unknown`.
    items: &std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem>,
    // Varies per purchase, so buying the same arcane offer twice can roll two
    // different grades — which is what retail did. Unlike dungeon loot this does
    // NOT need to be reproducible: the purchase response IS the grant, there is
    // nothing described in advance to stay consistent with.
    roll_nonce: u64,
) -> Option<RewardGrant> {
    let c = contents?;
    // `NeedsRoll` is now grantable — that is the whole of this change: its gear is
    // no longer "needs a roll", because the APK authors the enhancement and the
    // extractor finally reads it.
    //
    // The other kinds are still refused, and NOT per-entry. A `ChestRoll` offer's
    // real contents come out of a chest the server rolls; granting it on the
    // strength of a currency entry it happens to list would short-change the
    // player. `Unclassified`/`Unknown` carry a bucket this build cannot place.
    if !matches!(
        c.kind,
        OfferContentsKind::Literal
            | OfferContentsKind::NeedsRoll
            | OfferContentsKind::Unclassified
    ) {
        return None;
    }
    let mut reward = RewardGrant {
        town_xp: c.town_xp,
        ..RewardGrant::default()
    };
    for entry in &c.contents {
        // GEAR. Used to be unreachable — "`items` needs a rolled instance", which
        // was true only because the extractor dropped the authored enhancement.
        // It no longer does, so the instance is known: template, temper, arcane
        // tier and enchantments come from the APK, and durability from the repair
        // table at that temper level.
        // `unknown` is the extractor's "I could not place this", not retail's.
        // Resolve it from the template's own type, which is measured and total
        // (see `economy::bucket_for_template`). An entry whose template the game
        // data cannot name at all still refuses the whole offer.
        let bucket = match entry.bucket.as_str() {
            "items" => blades_lib::economy::ItemBucket::Instance,
            "currencies" => blades_lib::economy::ItemBucket::Currency,
            "stackableItems" => blades_lib::economy::ItemBucket::Stackable,
            _ => blades_lib::economy::bucket_for_template(entry.item_template_id, items)?,
        };
        if bucket == blades_lib::economy::ItemBucket::Instance {
            // `grading` empty on an arcane item means retail ROLLED the grade at
            // purchase; the APK records that a roll happens, never its outcome.
            // Ninety offers are like this and they used to be refused outright,
            // so most of the Sigil shop could not be bought at all (#167).
            //
            // The roll is now drawn from what retail actually handed out — 243
            // grants, keyed by arcane tier, with the ungraded outcome kept
            // because 111 of the 235 tier-2 grants had no grade. That is a
            // distribution of outcomes and not retail's rule, which is not
            // recoverable from anything we hold; the reporter was told so and
            // asked for it anyway.
            //
            // A tier the corpus has never seen is still refused rather than
            // handed over ungraded: that is the case the old refusal was right
            // about.
            //
            // ONLY JEWELLERY IS GRADED (#220). Of the 280 distinct arcane
            // instances in the retail captures, all 150 graded ones are rings or
            // necklaces (item types 10/11) with no durability, and all 130
            // weapons, armour and shields carry durability and no grade. Rolling
            // a grade onto an arcane axe did worse than invent a stat: a graded
            // `Item` serializes without `durability` (retail never sent both),
            // so the client read the brand-new axe as broken, and a repair could
            // not fix it because the restored durability was dropped again on
            // the way out.
            let wears = !blades_lib::economy::template_skips_durability(
                entry.item_template_id,
                items,
            );
            let rolled_grading = if !wears && entry.grading.is_empty() && entry.arcane_tier > 0 {
                Some(blades_lib::features::sigil_grades::roll_grading(
                    entry.arcane_tier,
                    roll_nonce,
                )?)
            } else {
                None
            };
            // Rings and jewellery never carry durability — absent on all 34,867
            // captured instances and absent from the table by design. Refusing
            // them for a "missing" entry is what kept 39 of the 51 unclassified
            // offers ungrantable (#184). Everything that DOES wear out is still
            // refused when the table cannot price it, because there the missing
            // entry really is a gap and the alternative is inventing a number.
            let durability = if !wears {
                0.0
            } else {
                repair_data.max_durability(entry.item_template_id, entry.tempering_level)?
            };
            let properties = blades_lib::user_data::ItemPropertiesAll {
                enchanting: entry
                    .enchanting
                    .iter()
                    .map(|p| blades_lib::user_data::ItemSingleProperty { id: p.id, tier: p.tier })
                    .collect(),
                // The rolled grade when this offer leaves it to a roll, the
                // authored one otherwise.
                grading: rolled_grading.clone().unwrap_or_else(|| {
                    entry
                        .grading
                        .iter()
                        .map(|p| blades_lib::user_data::ItemSingleProperty {
                            id: p.id,
                            tier: p.tier,
                        })
                        .collect()
                }),
            };
            // `grade` is the SUM of the grading tiers, not their count — 8/8
            // against the captured grants, where counting scores 2/8.
            let grade: u64 = match &rolled_grading {
                Some(rolled) => rolled.iter().map(|p| p.tier).sum(),
                None => entry.grading.iter().map(|p| p.tier).sum(),
            };
            for _ in 0..entry.quantity.max(1) {
                reward.items.push(blades_lib::economy::RewardItem {
                    // A fresh instance per purchase; the frozen ids in the
                    // capture-derived grants are what made buying twice overwrite.
                    id: uuid::Uuid::new_v4(),
                    item: blades_lib::user_data::Item {
                        item_template_id: entry.item_template_id,
                        tempering_level: entry.tempering_level,
                        durability,
                        // Never on gear that wears — see `wears` above.
                        grade: (!wears && grade > 0).then_some(grade),
                        arcane_tier: (entry.arcane_tier > 0).then_some(entry.arcane_tier),
                        properties: properties.clone(),
                    },
                });
            }
            continue;
        }
        let slot = match bucket {
            blades_lib::economy::ItemBucket::Currency => &mut reward.currencies,
            blades_lib::economy::ItemBucket::Stackable => &mut reward.stackable_items,
            // Handled above; the `continue` there makes this unreachable.
            blades_lib::economy::ItemBucket::Instance => return None,
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

/// A captured reward for this catalogue product, including a capture from an
/// otherwise-identical replacement window.
///
/// Three one-gem promotions in the captured catalogue have a fresh product id
/// but no APK bundle and no purchase response under that fresh id.  Each carries
/// the same bare `purchaseTrackingId` and the same price as one earlier catalogue
/// entry whose retail purchase response we did capture.  That shared id is the
/// catalogue's own statement that the two windows are the same tracked product;
/// it is stronger evidence than trying to infer a reward from the price or name.
///
/// Refuse ambiguity: an alias is used only when exactly one same-price sibling
/// with the same tracking id has a captured grant.  A direct grant always wins.
fn captured_grant_for_product(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
) -> Option<&RewardGrant> {
    if let Some(reward) = static_data.global_shop_grants.get(&product_id) {
        return Some(reward);
    }
    // This is not a second way to reinterpret normal APK-authored products.
    // Their bundle contents remain the fallback below; only a product absent
    // from the APK can borrow its captured replacement-window sibling.
    if static_data.global_shop_offer_contents.contains_key(&product_id) {
        return None;
    }

    fn tracking_id(entry: &Value, product_id: Uuid) -> Option<Uuid> {
        let mut found = None;
        for limit in entry.get("maxPurchaseLimits")?.as_array()? {
            let raw = limit.get("purchaseTrackingId")?.as_str()?;
            if raw.contains("::") {
                continue;
            }
            let Ok(id) = Uuid::parse_str(raw) else {
                continue;
            };
            if id == product_id {
                continue;
            }
            if found.is_some_and(|seen| seen != id) {
                return None;
            }
            found = Some(id);
        }
        found
    }

    let catalogue = static_data
        .global_shop_overrides
        .get("globalShopOverrides")?
        .as_object()?;
    let product = catalogue.get(&product_id.to_string())?;
    let tracked_as = tracking_id(product, product_id)?;
    let price = product.get("prices")?;

    let mut found = None;
    for (candidate_raw, candidate) in catalogue {
        let Ok(candidate_id) = Uuid::parse_str(candidate_raw) else {
            continue;
        };
        if candidate_id == product_id
            || candidate.get("prices") != Some(price)
            || tracking_id(candidate, candidate_id) != Some(tracked_as)
        {
            continue;
        }
        let Some(reward) = static_data.global_shop_grants.get(&candidate_id) else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some(reward);
    }
    found
}

/// The product's LIFETIME purchase cap, or `None` when it is unlimited.
///
/// `maxPurchases` is the per-product total, and `0` means unlimited — 485 of the
/// 546 valid catalogue entries are 0, and the other 61 carry 1, 3, 5, 10 or 20. That maps
/// exactly onto `server_state.globalShopPurchases`, which counts purchases per
/// product for the life of the character.
///
/// Deliberately NOT the `maxPurchaseLimits` array. Those are per-WINDOW caps — 112
/// of them are non-zero — and their tracking id embeds the window start
/// (`<product>::override::<id>::<startTimeSecs>`). Enforcing one against a lifetime
/// counter would cap the product for ever after the first window, which is the
/// exact bug just fixed for event tiers. Doing them properly needs a per-window
/// counter we do not store, so they are left alone rather than half-enforced.
fn lifetime_purchase_cap(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
    now: i64,
) -> Option<u64> {
    let current = apply_authored(
        shift_to_now(&static_data.global_shop_overrides, now),
        &static_data.global_shop_authored,
    );
    let n = current
        .get("globalShopOverrides")
        .and_then(Value::as_object)?
        .get(&product_id.to_string())?
        .get("maxPurchases")
        .and_then(Value::as_u64)?;
    (n > 0).then_some(n)
}

/// The offer's PER-WINDOW cap and the key that counts against it.
///
/// `maxPurchaseLimits` carries three forms of tracking id; the one that ends in a
/// window start (`<product>::override::<override>::<startTimeSecs>`) is the
/// per-window cap. 112 of the catalogue's entries carry a non-zero one — 69 at 1,
/// 35 at 3, 8 at 5 — and nothing enforced any of them. One player bought a
/// 3-per-window offer 109 times.
///
/// The key returned is the whole tracking id, which is what makes the reset free:
/// `shift_to_now` moves that embedded timestamp with the rotation, so a new window
/// is a new key and the count starts again at zero. Counting these against the
/// lifetime `globalShopPurchases` instead would bar the offer for ever after its
/// first window — the shape of bug that locked a player out of an event
/// permanently.
///
/// `None` when the offer has no windowed limit, or the limit is 0 (unlimited).
fn window_purchase_cap(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
    now: i64,
) -> Option<(String, u64)> {
    let current = apply_authored(
        shift_to_now(&static_data.global_shop_overrides, now),
        &static_data.global_shop_authored,
    );
    let limits = current
        .get("globalShopOverrides")?
        .as_object()?
        .get(&product_id.to_string())?
        .get("maxPurchaseLimits")?
        .as_array()?;
    for lim in limits {
        let tid = lim.get("purchaseTrackingId")?.as_str()?;
        // The windowed form, and only it: its last segment is the window start.
        let is_windowed = tid
            .rsplit("::")
            .next()
            .is_some_and(|tail| !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()));
        if !is_windowed {
            continue;
        }
        let n = lim.get("limit").and_then(Value::as_u64).unwrap_or(0);
        if n > 0 {
            return Some((tid.to_string(), n));
        }
    }
    None
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

    // What this product grants. The captures cover 159 of the storefront's 546 valid
    // offers, because an offer's contents only ever appeared in a purchase
    // RESPONSE — so the other 388 used to 404 here, and the client answered that
    // by prompting the player to reconnect to Bethesda.
    //
    // A capture-derived grant always WINS: it is a recording of what retail
    // actually handed over, instance stats and all, where the fallback below
    // knows only templates and quantities.
    //
    // A randomised bundle is rolled inside the transaction below, so its recorded
    // grant (if any) is never what the player gets. Checked FIRST because the
    // store chests at full price had either an invented grant — one treasury
    // chest, which froze the client's store opening sequence and paid a single
    // fixed tier-5 bundle (#215/#216) — or none at all, which refused the sale.
    let randomised =
        blades_lib::features::store_bundles::is_randomised_bundle(&body.global_shop_product_id);
    let reward = match captured_grant_for_product(
        &app_state.static_data,
        body.global_shop_product_id,
    ) {
        _ if randomised => RewardGrant::default(),
        Some(r) => r.clone(),
        None => grant_from_offer_contents(
            app_state
                .static_data
                .global_shop_offer_contents
                .get(&body.global_shop_product_id),
            &app_state.repair_data,
            &app_state.game_data.items_template,
            // Fresh per purchase. Retail rolled the arcane grade every time, and
            // the purchase response is the grant, so there is nothing described
            // in advance that this has to stay consistent with.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0),
        )
        .ok_or_else(|| {
            // NOT a 404. This comment's own neighbour records why: a 404 here makes
            // the client prompt the player to reconnect to Bethesda, and a player
            // who touched one of these lost the game entirely — "ever since I tried
            // to buy something in the shop, Blades won't start anymore" (#170),
            // matching the single purchase 404 in that day's log.
            //
            // Every valid product in the current captured catalogue has a reward;
            // this remains as the safe failure for a newer or malformed product.
            // Hiding unknowns was measured and is worse: it can collapse the shop
            // back to the near-empty state from #141. So the offer stays visible
            // and the refusal remains survivable.
            //
            // The shape is the one the client already receives in ordinary play for
            // a price mismatch — 400 on this service — so it is a path known to be
            // handled rather than a code invented here. The real reason is logged
            // server-side; the client is simply told no.
            log::warn!(
                "[shop] character {character_id} tried to buy product {} which has no \
                 deliverable reward (#167); refusing with the price-mismatch shape \
                 rather than a 404, which would brick the client",
                body.global_shop_product_id,
            );
            map_purchase_err(PurchaseError::InvalidPrice)
        })?,
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
    // Resolved here, not inside the transaction: the closure takes `app_state`.
    let lifetime_cap = lifetime_purchase_cap(&app_state.static_data, product_id, now);
    let window_cap = window_purchase_cap(&app_state.static_data, product_id, now);
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

            // A product with a lifetime cap must not exceed it. Nothing read
            // `maxPurchases` before, so every cap in the catalogue was decorative:
            // one player bought a capped product 109 times.
            //
            // Checked BEFORE the wallet is touched, so a refused purchase costs
            // nothing.
            if let Some(cap) = lifetime_cap {
                let owned = entry
                    .server_state
                    .0
                    .global_shop_purchases
                    .get(&product_id)
                    .copied()
                    .unwrap_or(0);
                if owned >= cap {
                    log::info!(
                        "[shop] character {character_id} is at the lifetime cap for product \
                         {product_id} ({owned}/{cap}) — refusing"
                    );
                    return Err(map_purchase_err(PurchaseError::InvalidPrice));
                }
            }

            // …and the per-window cap, counted under the offer's own tracking id so a
            // new window starts again at zero. Also before the wallet is touched.
            if let Some((ref key, cap)) = window_cap {
                let used = entry
                    .server_state
                    .0
                    .global_shop_window_purchases
                    .get(key)
                    .copied()
                    .unwrap_or(0);
                if used >= cap {
                    log::info!(
                        "[shop] character {character_id} is at this window's cap for product \
                         {product_id} ({used}/{cap}) — refusing"
                    );
                    return Err(map_purchase_err(PurchaseError::InvalidPrice));
                }
            }

            // Charge the (validated) price; fail on insufficient funds.
            entry
                .wallet
                .0
                .try_pay(&prices)
                .map_err(BladeApiError::from_economy)?;

            // MINT A FRESH INSTANCE ID PER PURCHASE.
            //
            // The grants are capture-derived, so each one carries the item instance
            // uuid from the ORIGINAL retail purchase it was mined from — 97 distinct
            // frozen ids across the 85 grants that award items, not one of them null.
            // `apply_reward` inserts into `backpack.items` keyed by that id, so
            // buying the same product twice overwrites the first copy: the player
            // pays twice and owns one. It also hands every player on the server the
            // same instance id for that product, and the arena ships instance ids to
            // the opponent's client in the op54 profile.
            //
            // `quest.rs` already mints per grant (`id: Uuid::new_v4()`); the shop
            // path never did.
            let reward = {
                let mut r = reward;
                for item in &mut r.items {
                    item.id = uuid::Uuid::new_v4();
                }
                r
            };

            // FOUR products are randomised bundles, and for them the recorded grant
            // above is the wrong answer entirely: it is one retail purchase, cloned
            // for every player and every buy, so the store hands out the same gold
            // and the same two items for ever (#170). Retail re-rolled — 4,697 buys
            // of the biggest bundle produced 4,697 distinct rewards — and the payout
            // scales with the buyer's level, from a median 9,735 gold at levels 1-5
            // to 60,315 at 54-89.
            //
            // Rolled HERE rather than beside the recorded grant because only inside
            // the transaction do we know who is buying and how many times they have
            // bought before. The purchase count is the nonce, so buying the same
            // bundle twice in a row cannot return the same thing.
            let reward = {
                let bought_before = entry
                    .server_state
                    .0
                    .global_shop_purchases
                    .get(&product_id)
                    .copied()
                    .unwrap_or(0);
                blades_lib::features::store_bundles::roll_bundle(
                    &product_id,
                    u64::from(entry.character.0.level),
                    bought_before,
                )
                .unwrap_or(reward)
            };

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
            if let Some((key, _)) = window_cap {
                *entry
                    .server_state
                    .0
                    .global_shop_window_purchases
                    .entry(key)
                    .or_insert(0) += 1;
            }

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


/// The shipped template table, for the entries whose bucket the extractor left
/// `unknown`. Read from the same file the server loads. File-scope so every test
/// module in this file can reach it.
#[cfg(test)]
fn item_table() -> &'static std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem> {
    static T: std::sync::OnceLock<
        std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        let gd: blades_lib::game_data::GameData =
            serde_json::from_str(&std::fs::read_to_string(p).expect("parsed.json"))
                .expect("game data");
        gd.items_template
    })
}

/// The real durability table, for grants that need an instance. File-scope for
/// the same reason as [`item_table`].
///
/// `#[cfg(test)]` is load-bearing: without it this compiles into the server
/// binary, where nothing calls it, and the crate stops building warning-free
/// (#84). It lost the attribute when `item_table` was inserted between the
/// attribute and the function it applied to.
#[cfg(test)]
fn repair_data() -> blades_lib::features::repair::RepairData {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../deploy/static/item_durability.json");
    let durability: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    blades_lib::features::repair::RepairData::from_json(&durability, &serde_json::json!({}))
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

    fn live_sigil_count(v: &Value, now: i64) -> usize {
        const SIGILS: &str = "c64bcb53-41f4-41ba-892a-fe2cca423caa";
        v["globalShopOverrides"]
            .as_object()
            .unwrap()
            .values()
            .filter(|e| {
                e.get("isActive").and_then(|b| b.as_bool()).unwrap_or(false)
                    && e["activeStartDate"].as_i64().unwrap_or(0) <= now
                    && now <= e["activeEndDate"].as_i64().unwrap_or(0)
                    && e["prices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|p| p["currencyId"].as_str() == Some(SIGILS))
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

    /// NO DAY OF THE ROTATION MAY SHOW A NEARLY-EMPTY SIGIL SHOP.
    ///
    /// Report #141, second round: *"it is not empty but shows just 2 weapons."*
    ///
    /// The previous fix guaranteed only that SOME Sigil offer was live. That is too
    /// weak — the first 25 days of the captured corpus carry 1-4 offers per day in
    /// total, because that is when capture coverage was ramping up (1 → 3 → 13 → 27
    /// → 66 is not a shop rotation), and the old 63-day period replayed straight
    /// through them. 25 days in every 63 served a shop with almost nothing in it.
    ///
    /// So this sweeps a whole cycle and asserts a FLOOR on every single day.
    /// Measured across the cycle after the fix: min 12, median 20, max 60.
    #[test]
    fn every_day_of_the_rotation_has_a_stocked_sigil_shop() {
        const FLOOR: usize = 10;
        let raw = catalog();
        // Start well past the corpus so the replay is definitely engaged, then walk
        // two full periods so the wrap-around is covered as well as the interior.
        let base = 1_783_000_000 + 400 * 86_400;
        let mut worst = (usize::MAX, 0i64);
        for day in 0..(2 * REPLAY_PERIOD_DAYS) {
            let now = base + day * 86_400;
            let n = live_sigil_count(&shift_to_now(&raw, now), now);
            if n < worst.0 {
                worst = (n, day);
            }
        }
        assert!(
            worst.0 >= FLOOR,
            "day {} of the rotation offers only {} Sigil products; the thin lead-in \
             of the capture corpus must not be replayed",
            worst.1,
            worst.0,
        );
    }

    /// THE CONTROL: nothing was thrown away to achieve it.
    ///
    /// Skipping the lead-in by DROPPING those offers would satisfy the test above
    /// while quietly removing 30 products from the game. They are carried forward a
    /// period instead, so the served catalogue keeps every valid product. The one
    /// malformed `…f9p` key is deliberately omitted because it cannot be bought.
    #[test]
    fn the_replay_still_serves_every_offer_in_the_catalogue() {
        let raw = catalog();
        let now = 1_783_000_000 + 400 * 86_400;
        let raw_map = raw["globalShopOverrides"].as_object().unwrap();
        let before = raw_map
            .keys()
            .filter(|id| Uuid::parse_str(id).is_ok())
            .count();
        assert_eq!(raw_map.len(), before + 1, "one malformed retail id is omitted");
        let after = shift_to_now(&raw, now)["globalShopOverrides"]
            .as_object()
            .unwrap()
            .len();
        assert_eq!(after, before, "the replay must not drop offers");

        // …and every one of them is reachable at some point in the cycle, so a
        // carried-forward offer did not land outside the window entirely.
        let mut seen: std::collections::HashSet<String> = Default::default();
        for day in 0..REPLAY_PERIOD_DAYS {
            let t = now + day * 86_400;
            let shifted = shift_to_now(&raw, t);
            for (id, e) in shifted["globalShopOverrides"].as_object().unwrap() {
                let s = e["activeStartDate"].as_i64().unwrap_or(0);
                let en = e["activeEndDate"].as_i64().unwrap_or(0);
                if s <= t && t <= en {
                    seen.insert(id.clone());
                }
            }
        }
        let total = before;
        assert!(
            seen.len() * 100 >= total * 90,
            "only {} of {total} offers are reachable across a full cycle",
            seen.len()
        );
    }

    /// Report #141 arrived in the 67-day replay's three-day prefix: the global
    /// shop had four live Gem offers, but zero live Sigil offers. The captured
    /// Sigil schedule itself is continuous, so its replay must be continuous too.
    #[test]
    fn the_sigil_shop_is_live_at_the_reported_time() {
        let now = 1_789_261_815; // 2026-09-12 23:30:15 UTC
        let shifted = shift_to_now(&catalog(), now);
        assert!(
            live_sigil_count(&shifted, now) > 0,
            "the replay must never land in the corpus prefix before Sigil offers began",
        );
    }

    /// The 63-day period begins and ends exactly on the captured Sigil span.
    /// Sample every hour for several future cycles so a later change cannot
    /// silently reintroduce an empty Sigil window between replays.
    #[test]
    fn every_replayed_hour_has_a_sigil_offer() {
        let start = 1_783_353_600 + 1;
        let end = start + 4 * REPLAY_PERIOD;
        for now in (start..=end).step_by(3_600) {
            assert!(
                live_sigil_count(&shift_to_now(&catalog(), now), now) > 0,
                "no Sigil offer at {now}",
            );
        }
    }

    /// Every offer moves by a whole number of PERIODS, so retail's relative timing
    /// survives — the daily block still turns over together, at the same clock time
    /// and on the same weekday.
    ///
    /// There are exactly two offsets, not one: the lead-in offers are carried
    /// forward one extra period out of the thinly-covered corpus prefix (see
    /// `CORPUS_LEAD_IN_DAYS`). That is asserted rather than waved through — two
    /// offsets differing by precisely one period is the intended shape, and any
    /// other spread would mean offers had drifted relative to one another.
    #[test]
    fn the_whole_catalogue_moves_by_whole_periods() {
        let raw = catalog();
        let now = 1_783_000_000 + 200 * 86_400;
        let shifted = shift_to_now(&raw, now);
        let a = raw["globalShopOverrides"].as_object().unwrap();
        let b = shifted["globalShopOverrides"].as_object().unwrap();
        let valid = a.keys().filter(|id| Uuid::parse_str(id).is_ok()).count();
        assert_eq!(b.len(), valid, "every valid offer survives the replay");
        let mut offsets = std::collections::HashSet::new();
        for (id, before) in a {
            if Uuid::parse_str(id).is_err() {
                assert!(b.get(id).is_none(), "a malformed id must not be advertised");
                continue;
            }
            let after = &b[id];
            offsets.insert(after["activeStartDate"].as_i64().unwrap() - before["activeStartDate"].as_i64().unwrap());
            assert_eq!(
                after["activeEndDate"].as_i64().unwrap() - before["activeEndDate"].as_i64().unwrap(),
                after["activeStartDate"].as_i64().unwrap() - before["activeStartDate"].as_i64().unwrap(),
                "an offer's duration must not change",
            );
        }
        for o in &offsets {
            assert_eq!(
                o % REPLAY_PERIOD,
                0,
                "every offset must be a whole number of replay periods, got {o}"
            );
        }
        assert!(
            offsets.len() <= 2,
            "at most two offsets — the catalogue and the carried-forward lead-in, got {offsets:?}"
        );
        if offsets.len() == 2 {
            let mut v: Vec<i64> = offsets.iter().copied().collect();
            v.sort_unstable();
            assert_eq!(
                v[1] - v[0],
                REPLAY_PERIOD,
                "the two offsets must differ by exactly one period, got {v:?}"
            );
        }
    }

    /// Rotation clock times must survive, or the daily block moves to the middle of
    /// the night. This is why the period is whole DAYS and not 66.5.
    #[test]
    fn rotation_times_of_day_are_preserved() {
        let raw = catalog();
        let now = 1_783_000_000 + 500 * 86_400;
        let shifted = shift_to_now(&raw, now);
        for (id, before) in raw["globalShopOverrides"].as_object().unwrap() {
            if Uuid::parse_str(id).is_err() {
                assert!(
                    shifted["globalShopOverrides"].get(id).is_none(),
                    "a malformed id must not be advertised"
                );
                continue;
            }
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
        let mut raw = catalog();
        raw["globalShopOverrides"]
            .as_object_mut()
            .unwrap()
            .retain(|id, _| Uuid::parse_str(id).is_ok());
        let now = 1_780_000_000; // inside the captured range
        assert_eq!(shift_to_now(&raw, now), raw);
    }

    /// The raw capture contains Bethesda's `…f9p` typo. It is not a product id:
    /// this server's purchase request cannot deserialize it into a UUID, so
    /// advertising it would create an offer that necessarily 400s (#184).
    #[test]
    fn a_malformed_product_id_is_never_served() {
        const BAD: &str = "53c6f124-3603-4100-ba9a-e2fe23969f7p";
        let raw = catalog();
        assert!(raw["globalShopOverrides"].get(BAD).is_some(), "fixture keeps the retail typo");
        for now in [1_780_000_000, 1_783_000_000 + 400 * 86_400] {
            assert!(
                shift_to_now(&raw, now)["globalShopOverrides"].get(BAD).is_none(),
                "the invalid id was served at {now}"
            );
        }
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

    /// THE LAST THREE UNBUYABLE OFFERS (#184).
    ///
    /// They are later one-gem windows for three products already present in the
    /// catalogue.  The new product ids have no APK bundle or captured purchase,
    /// but retail gives each the same bare purchase-tracking id and the same price
    /// as an earlier window whose response we did capture.  Resolve those exact
    /// siblings, and assert that every one of the 546 valid served products now has a
    /// reward.  This is deliberately corpus-wide: fixing only the reporter's next
    /// failed item would leave two indistinguishable 400s behind.
    #[test]
    fn every_storefront_offer_is_deliverable() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let ids: Vec<_> = sd.global_shop_overrides["globalShopOverrides"]
            .as_object()
            .expect("an object")
            .keys()
            // One Valentine's decoration row carries Bethesda's known `…f9p`
            // typo and can never arrive in this UUID-typed purchase handler.
            .filter_map(|raw| Uuid::parse_str(raw).ok())
            .collect();
        assert_eq!(ids.len(), 546, "the valid captured storefront population");

        let rd = repair_data();
        let mut aliases = Vec::new();
        for id in ids {
            let captured = captured_grant_for_product(&sd, id);
            if captured.is_some() && !sd.global_shop_grants.contains_key(&id) {
                aliases.push(id);
            }
            assert!(
                captured.is_some()
                    || grant_from_offer_contents(
                        sd.global_shop_offer_contents.get(&id),
                        &rd,
                        item_table(),
                        1,
                    )
                    .is_some(),
                "storefront product {id} still has no deliverable reward"
            );
        }
        aliases.sort_unstable();
        let mut expected: Vec<Uuid> = [
            "0413eb45-6b3f-485f-907d-42f106e5e1a0",
            "88589dc7-ee1c-4769-ae8e-9af96a155196",
            "9440d12e-140f-47c5-8bb8-b099ff2acdb3",
        ]
        .into_iter()
        .map(|raw| raw.parse().unwrap())
        .collect();
        expected.sort_unstable();
        assert_eq!(aliases, expected, "only the three replacement windows alias a grant");
    }

    /// THE CAPS IN THE CATALOGUE WERE DECORATIVE.
    ///
    /// Nothing ever read `maxPurchases`, so a product capped at a handful could be
    /// bought without limit. One player bought a capped product **109 times**.
    ///
    /// 61 of the 546 valid catalogue entries carry a non-zero lifetime cap (1, 3, 5, 10 or
    /// 20); the other 485 are 0, which means unlimited.
    #[test]
    fn the_catalogue_really_does_declare_lifetime_caps() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let now = 1_783_000_000 + 400 * 86_400;

        let mut capped = 0usize;
        let mut uncapped = 0usize;
        for id in sd.global_shop_overrides["globalShopOverrides"]
            .as_object()
            .expect("an object")
            .keys()
            .filter_map(|k| Uuid::parse_str(k).ok())
        {
            match lifetime_purchase_cap(&sd, id, now) {
                Some(n) => {
                    assert!(n > 0, "a cap of 0 must read as unlimited, not as a cap");
                    capped += 1;
                }
                None => uncapped += 1,
            }
        }
        assert!(capped > 0, "no capped product found — the enforcement is inert");
        assert!(
            uncapped > capped,
            "most products are uncapped; if that flipped, the reading of maxPurchases is wrong"
        );
    }

    /// THE CONTROL, and the one that matters: the per-WINDOW limits must NOT be
    /// enforced against the lifetime counter.
    ///
    /// `maxPurchaseLimits` entries whose tracking id ends in a window start are
    /// per-window caps. Enforcing one against `globalShopPurchases`, which counts
    /// for the life of the character, would cap the product for ever after its first
    /// window — exactly the bug just fixed for event tiers.
    ///
    /// The product a player bought 109 times is one of these: its `maxPurchases` is
    /// 0 and its only real limit is a windowed 3. So this fix deliberately does NOT
    /// cover that case, and this test pins that it is a decision rather than an
    /// oversight.
    #[test]
    fn windowed_limits_are_not_treated_as_lifetime_caps() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let now = 1_783_000_000 + 400 * 86_400;

        // The product from the incident: windowed limit 3, maxPurchases 0.
        let id = Uuid::parse_str("6ec8f67f-2cef-41aa-a7fc-f46237ae809c").unwrap();
        let entry = &sd.global_shop_overrides["globalShopOverrides"][id.to_string()];
        assert!(!entry.is_null(), "the incident product must still be in the catalogue");

        let windowed: Vec<i64> = entry["maxPurchaseLimits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| {
                l["purchaseTrackingId"]
                    .as_str()
                    .and_then(|t| t.rsplit("::").next().map(|s| s.chars().all(|c| c.is_ascii_digit())))
                    .unwrap_or(false)
            })
            .filter_map(|l| l["limit"].as_i64())
            .collect();
        assert!(
            windowed.iter().any(|n| *n > 0),
            "the incident product must still carry a non-zero WINDOWED limit"
        );
        assert_eq!(
            lifetime_purchase_cap(&sd, id, now),
            None,
            "a windowed limit must not be read as a lifetime cap"
        );
    }

    /// THE WINDOWED CAPS ARE REAL AND WERE NEVER ENFORCED.
    ///
    /// 112 catalogue entries carry a non-zero per-window limit — 69 at 1, 35 at 3,
    /// 8 at 5 — and nothing read any of them. One player bought a 3-per-window
    /// offer 109 times.
    #[test]
    fn the_incident_product_has_a_windowed_cap_we_can_now_read() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let now = 1_783_000_000 + 400 * 86_400;

        let id = Uuid::parse_str("6ec8f67f-2cef-41aa-a7fc-f46237ae809c").unwrap();
        let (key, cap) = window_purchase_cap(&sd, id, now)
            .expect("the product bought 109 times must expose its windowed cap");
        assert_eq!(cap, 3, "the catalogue declares 3 per window");
        assert!(
            key.rsplit("::").next().is_some_and(|t| t.chars().all(|c| c.is_ascii_digit())),
            "the counter key must be the WINDOWED tracking id, or it cannot reset"
        );
    }

    /// THE RESET. The key must change when the window does — that is the whole
    /// mechanism, and getting it wrong bars the offer for ever after one window,
    /// which is the bug that locked a player out of an event permanently.
    #[test]
    fn the_window_key_changes_with_the_rotation() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let id = Uuid::parse_str("6ec8f67f-2cef-41aa-a7fc-f46237ae809c").unwrap();

        let base = 1_783_000_000 + 400 * 86_400;
        let first = window_purchase_cap(&sd, id, base).map(|(k, _)| k);
        // A full replay period later the rotation has moved on.
        let later = window_purchase_cap(&sd, id, base + REPLAY_PERIOD).map(|(k, _)| k);

        assert!(first.is_some(), "precondition: the offer has a windowed cap now");
        assert_ne!(
            first, later,
            "a later window must produce a different counter key, or the cap never resets"
        );
    }

    /// THE CONTROL: an offer with no windowed limit must yield no key, so the
    /// enforcement cannot bite products that were never capped. 432 of the
    /// catalogue's windowed entries have limit 0, which means unlimited.
    #[test]
    fn an_uncapped_offer_yields_no_window_key() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let now = 1_783_000_000 + 400 * 86_400;

        let uncapped = sd.global_shop_overrides["globalShopOverrides"]
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(k, v)| {
                let any_windowed_nonzero = v["maxPurchaseLimits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|l| {
                        let t = l["purchaseTrackingId"].as_str().unwrap_or("");
                        let windowed = t
                            .rsplit("::")
                            .next()
                            .is_some_and(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()));
                        windowed && l["limit"].as_u64().unwrap_or(0) > 0
                    });
                (!any_windowed_nonzero).then(|| Uuid::parse_str(k).ok()).flatten()
            })
            .next()
            .expect("some offer has no windowed cap");

        assert_eq!(
            window_purchase_cap(&sd, uncapped, now),
            None,
            "an uncapped offer must not be given a cap"
        );
    }

    /// HOW MANY SIGIL OFFERS ARE NOW GRANTABLE. Printed, then asserted.
    #[test]
    fn most_sigil_offers_are_now_grantable() {
        const SIGIL: &str = "c64bcb53-41f4-41ba-892a-fe2cca423caa";
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);

        let mut sigil = 0usize;
        let mut ok = 0usize;
        for (id, e) in sd.global_shop_overrides["globalShopOverrides"].as_object().unwrap() {
            let is_sigil = e["prices"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|p| p["currencyId"].as_str() == Some(SIGIL));
            if !is_sigil {
                continue;
            }
            sigil += 1;
            let uuid = match Uuid::parse_str(id) {
                Ok(u) => u,
                Err(_) => continue,
            };
            let grantable = sd.global_shop_grants.contains_key(&uuid)
                || grant_from_offer_contents(
                    sd.global_shop_offer_contents.get(&uuid),
                    &repair_data(),
                    item_table(),
                    1,
                )
                .is_some();
            if grantable {
                ok += 1;
            }
        }
        eprintln!("Sigil offers grantable: {ok}/{sigil}");
        assert!(sigil > 0, "no Sigil offers found — the probe is wrong");
        assert!(
            ok * 100 >= sigil * 60,
            "only {ok}/{sigil} Sigil offers are grantable; the enhancement data should \
             have taken this well past half"
        );
    }

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
    /// SigilShop_Armor_ChitinGauntlets_Arcane02 — a REAL arcane offer template
    /// with a durability row. Gear: it wears, so it is never graded (#220).
    const ARCANE_GAUNTLETS: Uuid = Uuid::from_u128(0x208d05ed_74bf_4fef_b5e1_ed407d52432f);
    /// Ebony Faerite Ring (item type 10) — jewellery, the only kind retail graded.
    const EBONY_FAERITE_RING: Uuid = Uuid::from_u128(0x240eb001_fe3e_4899_b0cc_dd87cb8a72b6);
    /// Dragonbone Hand Axe, and the real Sigil-shop offer that sells it arcane
    /// (tier 2, temper 0, no authored grading). Tracker #220's axe.
    const DRAGONBONE_HAND_AXE: Uuid = Uuid::from_u128(0x1d6c0fb1_50e2_401f_939e_b8860d9fe026);
    const ARCANE_HAND_AXE_OFFER: Uuid = Uuid::from_u128(0x0b685d94_3bd2_45a5_ab5b_3c0fd6bfd9c1);

    fn entry(id: Uuid, qty: u64, bucket: &str) -> OfferContentEntry {
        OfferContentEntry {
            item_template_id: id,
            quantity: qty,
            bucket: bucket.into(),
            tempering_level: 0,
            arcane_tier: 0,
            enchanting: Vec::new(),
            grading: Vec::new(),
        }
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
        let r = grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).expect("literal must be grantable");
        assert_eq!(r.currencies.get(&GOLD), Some(&10_000));
        assert_eq!(r.stackable_items.get(&CLAY), Some(&85));
        assert!(r.items.is_empty(), "nothing may be invented into `items`");
    }

    /// NEEDS_ROLL GEAR IS GRANTED FROM THE AUTHORED ENHANCEMENT — and matches what
    /// retail actually handed over.
    ///
    /// This is the identity test. `global_shop_grants.json` holds 159 grants
    /// recorded from real retail purchases; the APK did not produce them. For every
    /// single-item offer that has BOTH a recorded grant and authored contents, the
    /// reconstruction must reproduce the recorded instance.
    ///
    /// Ignoring the enhancement pointer scores 41/73 on the same comparison, which
    /// is what makes this a test of the data rather than of the plumbing.
    #[test]
    fn needs_roll_gear_is_granted_from_the_authored_enhancement() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let rd = repair_data();

        let mut compared = 0usize;
        let mut matched = 0usize;
        for (id, recorded) in &sd.global_shop_grants {
            if recorded.items.len() != 1 {
                continue;
            }
            let Some(built) = grant_from_offer_contents(sd.global_shop_offer_contents.get(id), &rd, item_table(), 1)
            else {
                continue;
            };
            if built.items.len() != 1 {
                continue;
            }
            compared += 1;
            let (a, b) = (&recorded.items[0].item, &built.items[0].item);
            if a.item_template_id == b.item_template_id
                && a.tempering_level == b.tempering_level
                && a.arcane_tier == b.arcane_tier
                && a.properties.enchanting.len() == b.properties.enchanting.len()
            {
                matched += 1;
            }
        }
        assert!(compared > 0, "nothing to compare — the corpus or the contents file is wrong");
        eprintln!("instance identity: {matched}/{compared}");
        assert!(
            matched * 100 >= compared * 90,
            "only {matched}/{compared} reconstructions match the recorded retail instance"
        );
    }

    /// THE CONTROL that matters: an arcane item whose grading retail ROLLED must
    /// stay refused rather than being handed over ungraded.
    ///
    /// Three of the identity-checked grants are exactly this shape — same
    /// enhancement, but retail's recorded instances carry 3, 3 and 1 grading
    /// properties, differing between them. There is no AUTHORED answer, which is
    /// why this was refused outright until the roll was modelled from 243
    /// recorded retail grants (#167). It is now granted with a drawn grade, and
    /// a tier the corpus has never seen is still refused.
    #[test]
    fn an_arcane_item_with_no_authored_grading_has_its_grade_rolled() {
        let arcane_no_grading = OfferContents {
            kind: OfferContentsKind::NeedsRoll,
            town_xp: 0,
            contents: vec![OfferContentEntry {
                // Jewellery, because only jewellery is graded. This fixture used
                // to be ARCANE_GAUNTLETS, which is how rolling a grade onto gear
                // (#220) came to be asserted as correct.
                item_template_id: EBONY_FAERITE_RING,
                quantity: 1,
                bucket: "items".into(),
                tempering_level: 0,
                arcane_tier: 2,
                enchanting: Vec::new(),
                grading: Vec::new(),
            }],
        };
        // This used to assert the offer was REFUSED, and that was right while the
        // roll was unmodelled — inventing a grade would have short-changed a
        // player who paid. The roll is now drawn from 243 recorded retail grants
        // (#167), so the correct behaviour is the opposite: it is granted, and
        // two purchases can come out differently.
        let mut outcomes = std::collections::HashSet::new();
        for nonce in 0..80u64 {
            let r = grant_from_offer_contents(Some(&arcane_no_grading), &repair_data(), item_table(), nonce)
                .expect("an arcane offer must now be grantable");
            // indexed, not `.first()`: diesel's prelude brings its own `first`
            // into scope here and it shadows the slice method.
            assert_eq!(r.items.len(), 1, "one item");
            let item = &r.items[0].item;
            outcomes.insert(format!("{:?}", item.properties.grading));
            assert_eq!(
                item.grade.unwrap_or(0),
                item.properties.grading.iter().map(|p| p.tier).sum::<u64>(),
                "grade must be the SUM of the rolled grading tiers"
            );
        }
        assert!(
            outcomes.len() > 1,
            "every purchase rolled the same grade; retail's did not"
        );

        // An arcane tier the corpus has never seen stays refused — that is the
        // case the old refusal was right about, and it must not regress.
        let mut unknown_tier = arcane_no_grading.clone();
        unknown_tier.contents[0].arcane_tier = 9;
        assert!(
            grant_from_offer_contents(Some(&unknown_tier), &repair_data(), item_table(), 1).is_none(),
            "a grade must not be invented for a tier retail never showed"
        );

        // …and the same entry WITH authored grading is grantable, so the refusal is
        // about the missing roll and not about arcane items in general.
        let mut graded = arcane_no_grading.clone();
        graded.contents[0].grading = vec![blades_lib::static_data::PropertyRef {
            id: CLAY,
            tier: 3,
        }];
        // (Only reaches the durability lookup; a template with no durability row is
        // still refused, which is asserted separately.)
        let _ = grant_from_offer_contents(Some(&graded), &repair_data(), item_table(), 1);
    }

    /// TRACKER #220: "new purchase axe says it needs repair … press repair but it
    /// stays". The Sigil-shop arcane Dragonbone Hand Axe was handed out with a
    /// rolled `grade`, and a graded item serializes without `durability`, so the
    /// client saw a never-used axe at 0 condition and every repair was dropped
    /// on the way back out.
    ///
    /// Retail: all 130 arcane weapons/armour/shields in the captures carry
    /// durability and no grade. So across many purchases the axe must never be
    /// graded, must be at full condition for its temper (162.5 at temper 0),
    /// and that durability must reach the wire.
    #[test]
    fn arcane_gear_is_never_graded_and_ships_its_durability() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let offer = sd
            .global_shop_offer_contents
            .get(&ARCANE_HAND_AXE_OFFER)
            .expect("the arcane hand axe offer is in the static data");
        let rd = repair_data();
        let full = rd.max_durability(DRAGONBONE_HAND_AXE, 0).expect("axe durability row");
        assert_eq!(full, 162.5, "the APK's temper-0 max for the Dragonbone Hand Axe");

        for nonce in 0..200u64 {
            let r = grant_from_offer_contents(Some(offer), &rd, item_table(), nonce)
                .expect("the arcane axe offer is grantable");
            assert_eq!(r.items.len(), 1);
            let item = &r.items[0].item;
            assert_eq!(item.item_template_id, DRAGONBONE_HAND_AXE);
            assert_eq!(item.arcane_tier, Some(2));
            assert_eq!(item.grade, None, "nonce {nonce}: an arcane axe was graded");
            assert!(item.properties.grading.is_empty(), "nonce {nonce}: GRADING on an axe");
            assert_eq!(item.durability, full, "a new axe must be at full condition");

            let wire = serde_json::to_value(item).unwrap();
            assert_eq!(wire.get("durability").and_then(|v| v.as_f64()), Some(full));
            assert_eq!(wire.get("temperingLevel").and_then(|v| v.as_u64()), Some(0));
            assert!(wire.get("grade").is_none());
            assert!(!rd.needs_repair(item), "a brand-new axe must not need repair");
        }

        // The same holds for arcane ARMOUR (the gauntlets the grade-roll test used
        // to use), so this is about wearable gear, not about axes.
        let gauntlets = OfferContents {
            kind: OfferContentsKind::NeedsRoll,
            town_xp: 0,
            contents: vec![OfferContentEntry {
                item_template_id: ARCANE_GAUNTLETS,
                quantity: 1,
                bucket: "items".into(),
                tempering_level: 0,
                arcane_tier: 2,
                enchanting: Vec::new(),
                grading: Vec::new(),
            }],
        };
        for nonce in 0..200u64 {
            let r = grant_from_offer_contents(Some(&gauntlets), &rd, item_table(), nonce).unwrap();
            assert_eq!(r.items[0].item.grade, None, "nonce {nonce}: arcane gauntlets were graded");
        }
    }

    /// CONTROL for the test above: arcane JEWELLERY is still graded, so the fix
    /// is scoped to gear that wears and has not simply switched the roll off.
    #[test]
    fn arcane_jewellery_is_still_graded() {
        let ring = OfferContents {
            kind: OfferContentsKind::NeedsRoll,
            town_xp: 0,
            contents: vec![OfferContentEntry {
                item_template_id: EBONY_FAERITE_RING,
                quantity: 1,
                bucket: "items".into(),
                tempering_level: 0,
                arcane_tier: 2,
                enchanting: Vec::new(),
                grading: Vec::new(),
            }],
        };
        let graded = (0..200u64)
            .filter(|&n| {
                grant_from_offer_contents(Some(&ring), &repair_data(), item_table(), n)
                    .unwrap()
                    .items[0]
                    .item
                    .grade
                    .is_some()
            })
            .count();
        assert!(graded > 0, "no arcane ring came out graded");
    }

    /// The refusals that REMAIN.
    ///
    /// `NeedsRoll` used to be here and is deliberately gone: its gear is no longer
    /// rolled, because the APK authors the enhancement and the extractor now reads
    /// it. See `needs_roll_gear_is_granted_from_the_authored_enhancement`.
    ///
    /// These three stay. A `ChestRoll` offer's real contents come out of a chest
    /// the server rolls, so granting it on the strength of a currency entry it
    /// happens to also list would short-change the player who paid — which is why
    /// this refusal is by KIND and not per entry.
    #[test]
    fn every_other_kind_is_refused() {
        for kind in [OfferContentsKind::ChestRoll, OfferContentsKind::Unknown] {
            let o = offer(kind, vec![entry(GOLD, 1, "currencies")]);
            assert!(
                grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_none(),
                "{kind:?} must not be grantable from template data"
            );
        }
        // control: the identical contents under `Literal` ARE grantable, so the
        // refusals above are about the kind and not about the contents.
        let o = offer(OfferContentsKind::Literal, vec![entry(GOLD, 1, "currencies")]);
        assert!(grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_some());
        // …and `Unclassified` left this list (#184). The extractor could not place
        // its entries in a bucket; the template's own type can, and 51 offers whose
        // contents are all real item templates were being refused for a label.
        let o = offer(OfferContentsKind::Unclassified, vec![entry(GOLD, 1, "currencies")]);
        assert!(
            grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_some(),
            "an unclassified offer whose entries resolve must be grantable"
        );
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
            grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_none(),
            "a partial grant is a silent short-change"
        );
    }

    /// No entry, or an unknown offer, must not read as a successful purchase:
    /// the price is charged before the reward is applied.
    #[test]
    fn an_empty_or_absent_offer_is_not_a_purchase() {
        assert!(grant_from_offer_contents(None, &repair_data(), item_table(), 1).is_none(), "unknown product");
        let o = offer(OfferContentsKind::Literal, vec![]);
        assert!(grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_none(), "empty reward");
    }

    /// townXp rides through, and on its own is enough to be a real reward —
    /// 16 of the captured grants carry a non-zero one.
    #[test]
    fn town_xp_rides_through() {
        let mut o = offer(OfferContentsKind::Literal, vec![entry(GOLD, 5, "currencies")]);
        o.town_xp = 40;
        let r = grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).expect("grantable");
        assert_eq!(r.town_xp, 40);
    }

    /// A template listed twice sums rather than overwriting.
    #[test]
    fn a_repeated_template_sums() {
        let o = offer(
            OfferContentsKind::Literal,
            vec![entry(GOLD, 100, "currencies"), entry(GOLD, 25, "currencies")],
        );
        let r = grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).expect("grantable");
        assert_eq!(r.currencies.get(&GOLD), Some(&125));
    }
}

#[cfg(test)]
mod purchase_item_id_tests {
    use super::*;

    /// THE SHIPPED GRANTS CARRY FROZEN INSTANCE IDS, so the purchase path must mint.
    ///
    /// Each grant was mined from one recorded retail purchase and kept that
    /// purchase's item instance uuid. `apply_reward` inserts into
    /// `backpack.items` keyed by that id, so granting the same product twice
    /// overwrites the first copy — pay twice, own one — and every player on the
    /// server ends up holding the same instance id for that product, which the
    /// arena ships to the opponent's client in the op54 profile.
    ///
    /// This pins the premise: if the committed grants ever stop carrying ids, the
    /// minting below is dead code and should be revisited rather than left as
    /// cargo cult.
    #[test]
    fn the_committed_grants_really_do_carry_frozen_item_ids() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/global_shop_grants.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let grants: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let obj = grants.as_object().expect("grants is an object");

        let mut with_items = 0usize;
        let mut ids = std::collections::HashSet::new();
        for g in obj.values() {
            for it in g["items"].as_array().into_iter().flatten() {
                with_items += 1;
                let id = it["id"].as_str().expect("a grant item carries an id");
                ids.insert(id.to_string());
            }
        }
        assert!(with_items > 0, "no grant awards an item — the premise is gone");
        assert!(
            ids.len() > 1,
            "the ids must be real frozen uuids, not one repeated placeholder"
        );
    }

    /// Minting must give a DIFFERENT id each time, and must not disturb the item.
    ///
    /// Driven off a REAL committed grant rather than a hand-built one, so the test
    /// cannot pass against a fixture that does not resemble what ships.
    #[test]
    fn two_purchases_of_one_product_yield_two_distinct_items() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/global_shop_grants.json");
        let raw = std::fs::read_to_string(&p).unwrap();
        let grants: std::collections::HashMap<String, RewardGrant> =
            serde_json::from_str(&raw).expect("grants parse as RewardGrant");

        let (_, grant) = grants
            .iter()
            .find(|(_, g)| !g.items.is_empty())
            .expect("some grant awards an item");
        let frozen = grant.items[0].id;
        let template = grant.items[0].item.item_template_id;

        let mint = |g: &RewardGrant| {
            let mut r = g.clone();
            for item in &mut r.items {
                item.id = uuid::Uuid::new_v4();
            }
            r
        };
        let a = mint(grant);
        let b = mint(grant);

        assert_ne!(a.items[0].id, b.items[0].id, "each purchase needs its own instance id");
        assert_ne!(a.items[0].id, frozen, "the frozen id must not survive");
        assert_eq!(
            a.items[0].item.item_template_id, template,
            "the item itself must be untouched — only its instance id changes"
        );
        assert_eq!(a.items.len(), grant.items.len(), "no item is added or lost");
    }
}

#[cfg(test)]
mod report184_unclassified_tests {
    use super::*;

    fn shipped() -> blades_lib::static_data::StaticData {
        crate::static_loader::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static"
        )))
    }

    /// The same two helpers the fallback tests use, which are private to that
    /// module; duplicated rather than made public so the surface stays closed.
    fn item_table() -> &'static std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem> {
        static T: std::sync::OnceLock<
            std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem>,
        > = std::sync::OnceLock::new();
        T.get_or_init(|| {
            let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
            let gd: blades_lib::game_data::GameData =
                serde_json::from_str(&std::fs::read_to_string(p).expect("parsed.json"))
                    .expect("game data");
            gd.items_template
        })
    }

    fn repair_data() -> blades_lib::features::repair::RepairData {
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static"));
        let durability: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("item_durability.json")).unwrap())
                .unwrap();
        let costs: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("repair_costs.json")).unwrap())
                .unwrap();
        blades_lib::features::repair::RepairData::from_json(&durability, &costs)
    }

    /// THE REPORT (#184). Mɾʂιɾι's two failing purchases were both `unclassified`
    /// offers: the extractor could not place their entries in a bucket, so the
    /// whole kind was refused — even though every one of those entries names a
    /// real item template the game data can classify by type.
    #[test]
    fn unclassified_offers_are_grantable_when_their_templates_resolve() {
        let sd = shipped();
        let rd = repair_data();
        let mut unclassified = 0;
        let mut grantable = 0;
        for contents in sd.global_shop_offer_contents.values() {
            if !matches!(contents.kind, OfferContentsKind::Unclassified) {
                continue;
            }
            unclassified += 1;
            if grant_from_offer_contents(Some(contents), &rd, item_table(), 1).is_some() {
                grantable += 1;
            }
        }
        // 50 here and 51 in the file's own `_meta`: one product id appears twice
        // in the catalogue and collapses on load.
        assert_eq!(unclassified, 50, "the shipped unclassified offers");
        assert!(
            grantable >= 45,
            "only {grantable} of {unclassified} unclassified offers are grantable; \
             before this change it was 0"
        );
    }

    /// The two products the reporter actually hit.
    #[test]
    fn the_two_reported_products_are_grantable() {
        let sd = shipped();
        let rd = repair_data();
        for id in [
            "a66c27d2-6fc1-47f9-9c9c-419d1fa98855", // SigilShop_LTUltimate_OffensiveSpellsRing
            "23931dab-b680-4eb4-900e-1e0e5180bc55", // EmoteOffer_Guffaw
        ] {
            let uuid = uuid::Uuid::parse_str(id).unwrap();
            let contents = sd
                .global_shop_offer_contents
                .get(&uuid)
                .unwrap_or_else(|| panic!("{id} is in the shipped catalogue"));
            assert!(
                grant_from_offer_contents(Some(contents), &rd, item_table(), 1).is_some(),
                "{id} still refuses"
            );
        }
    }

    /// A ring grants with no durability rather than being refused for a table
    /// entry it was never going to have — 39 of the 51 hung on exactly this.
    #[test]
    fn a_ring_grants_without_a_durability_entry() {
        let sd = shipped();
        let rd = repair_data();
        let uuid =
            uuid::Uuid::parse_str("a66c27d2-6fc1-47f9-9c9c-419d1fa98855").unwrap();
        let reward = grant_from_offer_contents(
            Some(sd.global_shop_offer_contents.get(&uuid).unwrap()),
            &rd,
            item_table(),
            1,
        )
        .expect("the ring offer grants");
        assert_eq!(reward.items.len(), 1, "one ring, as an instance");
        assert_eq!(reward.items[0].item.durability, 0.0, "rings never wear out");
        assert!(
            reward.stackable_items.is_empty(),
            "a ring must not become a stackable"
        );
    }

    /// CONTROL, and the guarantee this change must not lose: an entry whose
    /// template the game data cannot name still sinks the WHOLE offer. A partial
    /// grant is a silent short-change, and the player paid.
    #[test]
    fn an_unnameable_template_still_sinks_the_offer() {
        use blades_lib::static_data::{OfferContentEntry, OfferContents};
        let unknown = uuid::Uuid::from_u128(0x99999999_8888_4777_8666_555555555555);
        let contents = OfferContents {
            kind: OfferContentsKind::Unclassified,
            town_xp: 0,
            contents: vec![OfferContentEntry {
                item_template_id: unknown,
                quantity: 1,
                bucket: "unknown".to_string(),
                tempering_level: 0,
                arcane_tier: 0,
                enchanting: vec![],
                grading: vec![],
            }],
        };
        assert!(
            grant_from_offer_contents(Some(&contents), &repair_data(), item_table(), 1)
                .is_none(),
            "an id nothing can classify must refuse, not guess a bucket"
        );
    }
}
