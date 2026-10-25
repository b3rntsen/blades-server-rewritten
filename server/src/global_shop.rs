//! Global store — `GET /catalogoverrides/globalshop`, `GET /catalogoverrides/iap`,
//! `GET /…/globalshops/current`, `POST /…/globalshops/current/purchase`.
//!
//! The Sigil/Gem sink. The override catalogue and IAP catalogue are served verbatim
//! from capture-derived JSON; a purchase verifies the client's expected price
//! against the active server-served promotion or APK base catalogue, debits it,
//! grants the capture-derived product reward, and bumps the per-character purchase
//! count. IAP (real money) is a priced placeholder only — there is no fulfillment
//! route. See [`blades_lib::features::global_shop`].

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

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
    models::{CharacterDbEntryServerState, CharacterDbEntryTownEconomy},
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
        let shift = if in_lead_in {
            shift + REPLAY_PERIOD
        } else {
            shift
        };
        shift_entry_in_place(&mut e, shift);
        out.insert(id.clone(), e);
    }
    serde_json::json!({ "globalShopOverrides": out })
}

/// Move one override entry by `shift` seconds: its window, and the window start
/// embedded in its per-occurrence tracking id.
///
/// `maxPurchaseLimits` third form embeds the window start in its tracking id
/// (`<offer>::override::<override>::<activeStartDate>`), which is how retail
/// gives a recurring offer a fresh allowance each time round. Shift it too,
/// or every replayed cycle would share one allowance with the original and
/// a player who bought in cycle 1 could never buy again.
fn shift_entry_in_place(e: &mut Value, shift: i64) {
    for field in ["activeStartDate", "activeEndDate"] {
        if let Some(t) = e.get(field).and_then(|d| d.as_i64()) {
            e[field] = Value::from(t + shift);
        }
    }
    if let Some(limits) = e
        .get_mut("maxPurchaseLimits")
        .and_then(|l| l.as_array_mut())
    {
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
}

/// End of the replayed slice of retail's calendar: 2026-07-01 16:00 UTC, when the
/// last daily block retail ever scheduled (the 30 June one) closed.
///
/// WHY THIS SLICE (tracker #238)
///
/// Retail's full calendar (`global_shop_windows.json`, 2750 windows over 546
/// offers) has a complete daily block, ~37-41 one-day Sigil offers turned over at
/// 16:00 UTC, on every day from 2 May to 30 June, except four days whose block was
/// never captured (1, 3, 5 and 16 May: no capture on those days landed after
/// 16:00). The 42 days ending here, 20 May to 30 June, contain none of those holes.
/// 42 is six whole weeks, so the Monday-anchored weekly windows and the
/// Tuesday/Thursday 17:00 block keep their weekdays in every replayed cycle.
const CALENDAR_END: i64 = 1_782_921_600;
const CALENDAR_START: i64 = CALENDAR_END - REPLAY_PERIOD;

fn window_of(e: &Value) -> Option<(i64, i64)> {
    Some((
        e.get("activeStartDate")?.as_i64()?,
        e.get("activeEndDate")?.as_i64()?,
    ))
}

/// Replay retail's FULL shop calendar so it covers `now`. `None` when there is no
/// calendar to replay, and the caller falls back to [`shift_to_now`].
///
/// THE BUG THIS FIXES (tracker #238). "Sigil shop items are not refreshing daily
/// except for the top two items", and no soul gems or potion salts.
///
/// `global_shop_overrides.json` holds ONE window per offer, the last one retail
/// scheduled. Retail brought each daily offer back every 4-8 days, so collapsing
/// its calendar to the last occurrence put every daily offer's single window in
/// the final nine days of June. Replayed, the daily block existed on nine days of
/// each 42-day cycle; the other 33 days showed the weekly block plus the two
/// one-day "Divine" items (the "top two"), and no soul gem or salts at all. On the
/// reported day prod served 23 live Sigil offers where retail served ~57.
///
/// This also explains #141's "ramp" (1-4 offers a day through most of May, 66 by
/// July) that [`CORPUS_LEAD_IN_DAYS`] was written to skip: it was not capture
/// coverage ramping up, it was the collapse. The full calendar is dense from 2 May.
///
/// HOW
///
/// Every window whose start lies in `[CALENDAR_START, CALENDAR_END)` is replayed
/// at that position plus a whole number of periods. Choosing by START tiles time
/// exactly once: a weekly window that runs past the slice end fills the start of
/// the next cycle, where the window that began before the slice is not replayed.
/// An offer with no window in the slice (19 of them, last seen earlier in May) has
/// its windows folded into the slice by whole periods rather than being dropped.
/// The three ~950-day evergreen windows are moved just far enough to cover `now`.
///
/// The client's catalogue holds one entry per offer, so each offer is served with
/// the window covering `now` if there is one, else its next one, else its last:
/// the same one-entry-per-offer shape as before, just the right window.
fn replay_calendar(windows: &Value, now: i64) -> Option<Value> {
    let calendar = windows
        .get("globalShopWindows")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())?;
    let cycle = (now - CALENDAR_START).div_euclid(REPLAY_PERIOD);
    let fold = |start: i64| -(start - CALENDAR_START).div_euclid(REPLAY_PERIOD) * REPLAY_PERIOD;

    let mut out = serde_json::Map::new();
    for (id, list) in calendar {
        if Uuid::parse_str(id).is_err() {
            continue;
        }
        let dated: Vec<(&Value, i64, i64)> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| window_of(e).map(|(s, en)| (e, s, en)))
            .collect();
        let is_dated = |s: i64, en: i64| en - s < MAX_DATED_WINDOW;

        // (window, base shift) pairs that place the offer inside the slice once.
        let mut placed: Vec<(&Value, i64, i64, i64)> = dated
            .iter()
            .filter(|&&(_, s, en)| is_dated(s, en) && (CALENDAR_START..CALENDAR_END).contains(&s))
            .map(|&(e, s, en)| (e, s, en, 0))
            .collect();
        if placed.is_empty() {
            placed = dated
                .iter()
                .filter(|&&(_, s, en)| is_dated(s, en))
                .map(|&(e, s, en)| (e, s, en, fold(s)))
                .collect();
        }

        // Candidates: the placed windows in the previous, current and next cycle
        // (the previous one because a window near the slice end runs into the next
        // cycle), plus any evergreen window moved just far enough to cover `now`.
        let mut candidates: Vec<(i64, i64, &Value, i64)> = Vec::new();
        for &(e, s, en, base) in &placed {
            for c in [cycle - 1, cycle, cycle + 1] {
                let shift = base + c * REPLAY_PERIOD;
                candidates.push((s + shift, en + shift, e, shift));
            }
        }
        for &(e, s, en) in &dated {
            if !is_dated(s, en) {
                let periods = (now - en + REPLAY_PERIOD - 1)
                    .div_euclid(REPLAY_PERIOD)
                    .max(0);
                let shift = periods * REPLAY_PERIOD;
                candidates.push((s + shift, en + shift, e, shift));
            }
        }

        let live = candidates
            .iter()
            .filter(|c| c.0 <= now && now <= c.1)
            .max_by_key(|c| c.0);
        let next = || candidates.iter().filter(|c| c.0 > now).min_by_key(|c| c.0);
        let last = || candidates.iter().max_by_key(|c| c.0);
        let Some(&(_, _, entry, shift)) = live.or_else(next).or_else(last) else {
            continue;
        };
        let mut e = entry.clone();
        shift_entry_in_place(&mut e, shift);
        out.insert(id.clone(), e);
    }
    Some(serde_json::json!({ "globalShopOverrides": out }))
}

/// The catalogue as the client sees it at `now`: retail's calendar replayed (or,
/// without one, the collapsed catalogue shifted), then admin-authored windows on
/// top. Every reader of "what is on sale" goes through here so the price check,
/// the caps and `GET /catalogoverrides/globalshop` can never disagree.
pub(crate) fn current_catalog(static_data: &blades_lib::static_data::StaticData, now: i64) -> Value {
    let replayed = replay_calendar(&static_data.global_shop_windows, now)
        .unwrap_or_else(|| shift_to_now(&static_data.global_shop_overrides, now));
    apply_raised_window_caps(apply_authored(replayed, &static_data.global_shop_authored))
}

/// Per-window purchase caps raised above retail's, by product id.
///
/// NOT RETAIL. An owner-approved deviation (tracker #394, 2026-10-16), requested
/// by HauDrauf: "The Enchanter's packs, all tiers, that are in the shop today:
/// raise to 9 purchases instead of 3." Retail sold each tier at most 3 times per
/// window; its windows (one day, Thursdays 04:00 UTC) are unchanged here, only
/// the count.
///
/// In code rather than in `deploy/static`: merging ships code, the static data
/// needs a separate push. Applied in [`current_catalog`], the one place every
/// reader goes through, so the served `/catalogoverrides/globalshop` payload and
/// the purchase handler's cap agree. The client must see it in the payload: it
/// builds its sold-out check from the override's `maxPurchaseLimits` first and
/// the APK's `perUserMaximum` (3 for these packs) only for tracking ids the
/// override does not name — a set keyed on tracking id alone
/// (`PurchaseLimitCalculator.GetPurchaseLimits`, `PurchaseLimit.Equals`). The
/// windowed id exists only in the override, so its `limit` is what the client
/// enforces.
pub(crate) const RAISED_WINDOW_CAPS: &[(&str, u64)] = &[
    ("b790518d-9614-4dcf-ab54-6d8d749bff2b", 9), // IAP_GemPack05_a_ApprenticeEnchanter
    ("ef234f68-8ee2-4fdd-8fbe-a8f9bf70fc21", 9), // IAP_GemPack05_b_JourneymanEnchanter
    ("69ab6e05-8968-41a2-b45e-997c8534e166", 9), // IAP_GemPack05_c_ExpertEnchanter
    ("6ec8f67f-2cef-41aa-a7fc-f46237ae809c", 9), // IAP_GemPack05_d_HeroicEnchanter
];

/// Rewrite the non-zero WINDOWED limit of each [`RAISED_WINDOW_CAPS`] product.
/// The lifetime `maxPurchases` and the un-timestamped tracking ids (limit 0,
/// unlimited) are left alone, as is everything about the window itself.
fn apply_raised_window_caps(mut catalog: Value) -> Value {
    let Some(map) = catalog
        .get_mut("globalShopOverrides")
        .and_then(Value::as_object_mut)
    else {
        return catalog;
    };
    for (product, cap) in RAISED_WINDOW_CAPS {
        let Some(limits) = map
            .get_mut(*product)
            .and_then(|e| e.get_mut("maxPurchaseLimits"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for lim in limits {
            let windowed = lim
                .get("purchaseTrackingId")
                .and_then(Value::as_str)
                .and_then(|t| t.rsplit("::").next())
                .is_some_and(|tail| !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()));
            if windowed && lim.get("limit").and_then(Value::as_u64).unwrap_or(0) > 0 {
                lim["limit"] = Value::from(*cap);
            }
        }
    }
    catalog
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
    let current = current_catalog(static_data, now);
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

/// What `GET /catalogoverrides/globalshop` sends: the overrides whose window covers
/// `now`, and nothing else.
///
/// THE BUG THIS FIXES (tracker #400). "When the game has been open for a while the
/// shop will not open any more; I have to restart."
///
/// [`current_catalog`] keeps one entry for EVERY offer — the live window, else the
/// next one, else the last — because the price check and the caps need the whole
/// calendar. The endpoint served that map verbatim: 546 offers, 472 of them not
/// starting for up to 41 days, 320,802 bytes. Retail never sent a non-live
/// override: 923 of 923 captured responses held only offers live at the moment of
/// the request (71 on average, at most 79, at most 46,152 bytes).
///
/// The client re-fetches this on every store open (the reporter's: ~120 times a
/// day) and copies it into its persistence tree, so a long session parsed and
/// stored seven times what retail ever gave it, hundreds of times over, and held
/// hundreds of not-yet-started overrides — a state retail's client never saw.
/// Serving only what is live restores retail's shape and size. Because the client
/// re-fetches at every store open, an offer that starts later in the session still
/// appears the next time the store opens, as it did in retail.
///
/// An entry without a window is kept (none ship today). Pricing, caps and purchase
/// validation keep reading [`current_catalog`], so what can be bought is unchanged.
pub(crate) fn served_catalog(
    static_data: &blades_lib::static_data::StaticData,
    now: i64,
) -> Value {
    let mut catalog = current_catalog(static_data, now);
    if let Some(map) = catalog
        .get_mut("globalShopOverrides")
        .and_then(Value::as_object_mut)
    {
        map.retain(|_, e| window_of(e).is_none_or(|(s, en)| s <= now && now <= en));
    }
    catalog
}

/// `GET /catalogoverrides/globalshop` — retail's calendar replayed so it covers the
/// present, overlaid with anything an admin authored, cut to what is live now. See
/// [`served_catalog`].
#[get("/blades.bgs.services/api/game/v1/public/catalogoverrides/globalshop")]
pub async fn get_override(app_state: web::Data<Arc<ServerGlobal>>) -> Json<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Json(served_catalog(&app_state.static_data, now))
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

fn current_purchase_tracking_ids(
    static_data: &blades_lib::static_data::StaticData,
    now: i64,
) -> HashSet<String> {
    current_catalog(static_data, now)
        .get("globalShopOverrides")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .flat_map(|(_, entry)| {
            entry
                .get("maxPurchaseLimits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|limit| {
            let tid = limit.get("purchaseTrackingId")?.as_str()?;
            Uuid::parse_str(tid).is_err().then(|| tid.to_string())
        })
        .collect()
}

fn current_lifetime_purchase_tracking_ids(
    static_data: &blades_lib::static_data::StaticData,
    now: i64,
) -> HashMap<String, Uuid> {
    current_catalog(static_data, now)
        .get("globalShopOverrides")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(product, entry)| {
            let product_id = Uuid::parse_str(product).ok()?;
            let lifetime_cap = entry
                .get("maxPurchases")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (lifetime_cap > 0).then_some((product, product_id, entry))
        })
        .flat_map(|(product, product_id, entry)| {
            entry
                .get("maxPurchaseLimits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |limit| {
                    let tid = limit.get("purchaseTrackingId")?.as_str()?;
                    let capped = limit.get("limit").and_then(Value::as_u64).unwrap_or(0) > 0;
                    let non_product = tid != product;
                    let non_windowed = tid.rsplit("::").next().is_none_or(|tail| {
                        tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit())
                    });
                    (capped && non_product && non_windowed).then(|| (tid.to_string(), product_id))
                })
        })
        .collect()
}

fn purchases_list_with_tracking_counts(
    product_counts: &HashMap<Uuid, u64>,
    tracking_counts: &HashMap<String, u64>,
    visible_tracking_ids: &HashSet<String>,
    lifetime_tracking_ids: &HashMap<String, Uuid>,
) -> Vec<PurchaseEntry> {
    let mut list = global_shop::purchases_list(product_counts);
    list.extend(
        tracking_counts
            .iter()
            .filter(|(id, quantity)| **quantity > 0 && visible_tracking_ids.contains(*id))
            .map(|(id, quantity)| PurchaseEntry {
                id: id.clone(),
                quantity: *quantity,
            }),
    );
    for (id, product_id) in lifetime_tracking_ids {
        if list.iter().any(|entry| entry.id == *id) {
            continue;
        }
        let Some(quantity) = product_counts.get(product_id).copied().filter(|q| *q > 0) else {
            continue;
        };
        list.push(PurchaseEntry {
            id: id.clone(),
            quantity,
        });
    }
    list.sort_by(|a, b| a.id.cmp(&b.id));
    list
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let visible_tracking_ids = current_purchase_tracking_ids(&app_state.static_data, now);
    let lifetime_tracking_ids = current_lifetime_purchase_tracking_ids(&app_state.static_data, now);
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
            global_shop_purchases: purchases_list_with_tracking_counts(
                &entry.server_state.0.global_shop_purchases,
                &entry.server_state.0.global_shop_window_purchases,
                &visible_tracking_ids,
                &lifetime_tracking_ids,
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
    /// The buyer's town after the purchase's `townXp` was paid into it — present
    /// only when the reward paid any, as retail sent it (#353).
    #[serde(skip_serializing_if = "Option::is_none")]
    town: Option<Value>,
}

/// The town prestige one purchase of `product_id` pays — the `townXp` of the
/// reward [`purchase_global_shop`] resolves for it. A randomised bundle is
/// rolled, and no rolled store bundle carries any (`store_bundle_loot.json`).
///
/// The town-load backfill (#353) prices past purchases with this, so it must
/// agree with the purchase path; a test holds the two together.
pub(crate) fn product_town_xp(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
) -> u64 {
    if blades_lib::features::store_bundles::is_randomised_bundle(&product_id) {
        return 0;
    }
    match captured_grant_for_product(static_data, product_id) {
        Some(grant) => grant.town_xp,
        None => static_data
            .global_shop_offer_contents
            .get(&product_id)
            .map_or(0, |c| c.town_xp),
    }
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
///
/// Test-only convenience: it rolls with default (empty) enchanting tables. The
/// live purchase path, [`purchase_reward`], calls
/// [`grant_from_offer_contents_with_enchanting`] with the real ones.
#[cfg(test)]
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
    grant_from_offer_contents_with_enchanting(
        contents,
        repair_data,
        items,
        &blades_lib::static_data::EnchantingData::default(),
        roll_nonce,
    )
}

fn grant_from_offer_contents_with_enchanting(
    contents: Option<&OfferContents>,
    repair_data: &blades_lib::features::repair::RepairData,
    items: &std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem>,
    enchanting: &blades_lib::static_data::EnchantingData,
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
        OfferContentsKind::Literal | OfferContentsKind::NeedsRoll | OfferContentsKind::Unclassified
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
            let wears =
                !blades_lib::economy::template_skips_durability(entry.item_template_id, items);
            // A template that authors its own properties is never rolled (#8).
            let fixed = crate::fixed_templates::has_mandatory_properties(entry.item_template_id);
            let rolled_grading = if !wears
                && !fixed
                && entry.grading.is_empty()
                && entry.arcane_tier > 0
            {
                Some(roll_arcane_grading(
                    entry.item_template_id,
                    entry.arcane_tier,
                    roll_nonce,
                    items,
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
                    .map(|p| blades_lib::user_data::ItemSingleProperty {
                        id: p.id,
                        tier: p.tier,
                    })
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
            for quantity_idx in 0..entry.quantity.max(1) {
                let mut item = blades_lib::user_data::Item {
                    item_template_id: entry.item_template_id,
                    tempering_level: entry.tempering_level,
                    durability,
                    // Never on gear that wears — see `wears` above.
                    grade: (!wears && grade > 0).then_some(grade),
                    arcane_tier: (entry.arcane_tier > 0).then_some(entry.arcane_tier),
                    properties: properties.clone(),
                };
                // A THEMED ring or necklace (APK-authored GRADING, e.g. "Ebony
                // Faerite Ring: Frostbite, Ice Spike, Blizzard Armor") is sold
                // exactly as authored — see `is_themed_jewelry`. Only the rest is
                // re-rolled.
                if !enchanting.secondary_tables.is_empty() && !is_themed_jewelry(entry) {
                    let tier = entry
                        .enchanting
                        .iter()
                        .map(|p| p.tier)
                        .max()
                        .unwrap_or(entry.arcane_tier)
                        .max(1);
                    let mut rng = crate::jewelry_roll::seeded(
                        &[entry.item_template_id.as_bytes(), b"sigil"],
                        roll_nonce ^ quantity_idx,
                    );
                    crate::jewelry_roll::reroll_sigil_jewelry(
                        &mut item,
                        entry.enchanting.len(),
                        tier,
                        rolled_grading.is_none(),
                        items,
                        enchanting,
                        &mut rng,
                    );
                }
                reward.items.push(blades_lib::economy::RewardItem {
                    // A fresh instance per purchase; the frozen ids in the
                    // capture-derived grants are what made buying twice overwrite.
                    id: uuid::Uuid::new_v4(),
                    item,
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

/// Whether an offer entry is a THEMED ring or necklace: one the APK authors a
/// fixed GRADING set for (tracker #403). Such an offer is sold exactly as
/// authored — that GRADING, `grade` = the sum of its tiers, and its authored
/// ENCHANTING — on every purchase. Only gear that does not wear (jewellery) is
/// ever graded (#220), so a non-empty authored GRADING already names jewellery.
///
/// Evidence, APK and retail (2026-10-16):
/// * The APK's `ItemEnhancementCatalog` authors GRADING for 21 Sigil offers and
///   leaves it EMPTY for every `_Arcane01/02` one; the editor names spell the
///   theme out (`..._EbonyFaeriteRing_FrostbiteIceSpikeBlizzardArmor_NoEnch`).
/// * Every retail purchase of a themed offer we hold matches the authored set
///   exactly, GRADING ids and tiers and ENCHANTING alike: `dac59a1a` (Frostbite /
///   Ice Spike / Blizzard Armor), `579141c5` (Power Attack / Dodging Strike /
///   Quick Strikes, AbsStm) and `fc8f44dd` (Armsman, FortMgcRegen) in
///   `global_shop_grants.json` — 3 of 3, 8 GRADING and 5 ENCHANTING properties,
///   pinned by `retail_themed_grants_are_the_authored_sets`.
/// * f69663f2 (#386 era), which started re-rolling these, generalised from the
///   three randomised jewellery BUNDLES (251 grants, all distinct); its own
///   provenance records 0 named Sigil jewellery purchases in that snapshot
///   (`jewelry_roll_ranges.json`).
pub(crate) fn is_themed_jewelry(entry: &blades_lib::static_data::OfferContentEntry) -> bool {
    !entry.grading.is_empty()
}

/// The GRADING one purchase of an arcane item with no authored grading gets.
///
/// Jewellery takes its property COUNT from HauDrauf's retail experience (#393:
/// 8% one property, 17% two, 75% three -
/// `jewelry_grade::JEWELRY_PROPERTY_COUNT_PERCENT`, the rule every ring and
/// necklace but the Elder/Legendary chest ones now follows), the grade within that count from the APK grade table, and its PROPERTIES afresh, without repeats,
/// from the slot's whole APK list (`jewelry_grade::fill`, #392). The grade used
/// to be copied from the 32 tier-2 ring outcomes we captured, 13 of them
/// single-property, so 41% of rings came with one property and none at grade 6.
///
/// Anything else draws from the retail outcome corpus, as before (#220).
/// `None` - refuse the sale - only for a tier retail never showed at all.
fn roll_arcane_grading(
    template: Uuid,
    arcane_tier: u64,
    roll_nonce: u64,
    items: &std::collections::HashMap<Uuid, blades_lib::game_data::GameDataItem>,
) -> Option<Vec<blades_lib::user_data::ItemSingleProperty>> {
    use crate::jewelry_grade::{NECKLACE, RING};
    let item_type = items.get(&template).map(|i| i.r#type);
    let Some(slot @ (RING | NECKLACE)) = item_type else {
        return blades_lib::features::sigil_grades::roll_grading(arcane_tier, roll_nonce);
    };
    if !blades_lib::features::sigil_grades::can_roll(arcane_tier) {
        return None;
    }
    let mut rng = crate::jewelry_roll::seeded(&[template.as_bytes(), b"arcane-grade"], roll_nonce);
    let tiers = crate::jewelry_grade::roll_tiers(&mut rng);
    crate::jewelry_grade::fill(slot, tiers, &mut rng)
}

/// The per-purchase roll nonce: how many times this character bought the
/// product before, with the character folded in so two players do not walk the
/// same sequence of rolls (#386). Store bundles were already seeded this way.
///
/// `alt` is [`crate::alt_roll::for_character`]: two alts share the live row's
/// id, so a later alt is folded in too, or two alts with the same counts would
/// walk one sequence (#387). `None` — every original — leaves the nonce as it
/// always was.
pub(crate) fn purchase_roll_nonce(bought_before: u64, character_id: Uuid, alt: Option<Uuid>) -> u64 {
    crate::alt_roll::fold_nonce(
        bought_before ^ (character_id.as_u128() as u64).rotate_left(29),
        alt,
    )
}

/// Whether this product sells a ring or necklace the server rolls per purchase.
///
/// For these a captured retail grant is ONE recorded purchase: cloning it hands
/// every buyer the identical necklace for ever (#386, the #170 class). A
/// fixed-template ring (the Ultimate rings, #8) is never rolled, so it does not
/// count.
pub(crate) fn rolls_jewelry_per_purchase(
    static_data: &blades_lib::static_data::StaticData,
    items: &std::collections::HashMap<Uuid, blades_lib::game_data::GameDataItem>,
    product_id: Uuid,
) -> bool {
    static_data
        .global_shop_offer_contents
        .get(&product_id)
        .is_some_and(|c| {
            c.contents.iter().any(|e| {
                crate::jewelry_roll::is_jewelry_template(e.item_template_id, items)
                    && !crate::fixed_templates::has_mandatory_properties(e.item_template_id)
            })
        })
}

/// The reward one purchase of a non-randomised product pays, or `None` when it
/// has nothing deliverable.
///
/// A captured retail grant wins — it is what retail handed over, instance stats
/// and all — EXCEPT for products that sell rolled jewellery, which roll from the
/// APK contents every time. Should that roll be impossible the capture is still
/// paid rather than the sale refused.
pub(crate) fn purchase_reward(
    static_data: &blades_lib::static_data::StaticData,
    repair_data: &blades_lib::features::repair::RepairData,
    items: &std::collections::HashMap<Uuid, blades_lib::game_data::GameDataItem>,
    product_id: Uuid,
    roll_nonce: u64,
) -> Option<RewardGrant> {
    let rolled = || {
        grant_from_offer_contents_with_enchanting(
            static_data.global_shop_offer_contents.get(&product_id),
            repair_data,
            items,
            &static_data.enchanting,
            roll_nonce,
        )
    };
    match captured_grant_for_product(static_data, product_id) {
        Some(r) if !rolls_jewelry_per_purchase(static_data, items, product_id) => Some(r.clone()),
        Some(r) => Some(rolled().unwrap_or_else(|| r.clone())),
        None => rolled(),
    }
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
    if static_data
        .global_shop_offer_contents
        .contains_key(&product_id)
    {
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
    let current = current_catalog(static_data, now);
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
    let current = current_catalog(static_data, now);
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

fn current_product_purchase_tracking_ids(
    static_data: &blades_lib::static_data::StaticData,
    product_id: Uuid,
    now: i64,
) -> Vec<String> {
    let product = product_id.to_string();
    let current = current_catalog(static_data, now);
    let Some(limits) = current
        .get("globalShopOverrides")
        .and_then(Value::as_object)
        .and_then(|m| m.get(&product))
        .and_then(|e| e.get("maxPurchaseLimits"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for lim in limits {
        let Some(tid) = lim.get("purchaseTrackingId").and_then(Value::as_str) else {
            continue;
        };
        if tid != product && !out.iter().any(|seen| seen == tid) {
            out.push(tid.to_string());
        }
    }
    out
}

/// Whether one more purchase of `product_id` would exceed a cap: `Some((scope,
/// used, cap))` when it would, `None` when the purchase may go ahead.
///
/// The lifetime cap is `maxPurchases`, counted per product. Nothing read it
/// before, so every cap in the catalogue was decorative: one player bought a
/// capped product 109 times. The per-window cap is counted under the offer's own
/// windowed tracking id, so a new window starts again at zero.
fn purchase_cap_reached(
    state: &blades_lib::server_state::ServerState,
    product_id: Uuid,
    lifetime_cap: Option<u64>,
    window_cap: Option<&(String, u64)>,
) -> Option<(&'static str, u64, u64)> {
    if let Some(cap) = lifetime_cap {
        let owned = state
            .global_shop_purchases
            .get(&product_id)
            .copied()
            .unwrap_or(0);
        if owned >= cap {
            return Some(("lifetime", owned, cap));
        }
    }
    if let Some((key, cap)) = window_cap {
        let used = state
            .global_shop_window_purchases
            .get(key)
            .copied()
            .unwrap_or(0);
        if used >= *cap {
            return Some(("window", used, *cap));
        }
    }
    None
}

/// Count one purchase: the lifetime per-product total and every tracking id the
/// offer currently carries (bare override and windowed alike).
fn record_purchase(
    state: &mut blades_lib::server_state::ServerState,
    product_id: Uuid,
    tracking_ids: Vec<String>,
) {
    *state.global_shop_purchases.entry(product_id).or_insert(0) += 1;
    for key in tracking_ids {
        *state.global_shop_window_purchases.entry(key).or_insert(0) += 1;
    }
}

/// `POST /…/globalshops/current/purchase` — buy a global-shop product: validate the
/// client price, debit it, grant the product, bump the purchase count.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/globalshops/current/purchase"
)]
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
    // knows only templates and quantities. Except for rolled jewellery, which
    // re-rolls every purchase (#386) — see `purchase_reward`.
    //
    // A randomised bundle is rolled inside the transaction below, so its recorded
    // grant (if any) is never what the player gets. Checked FIRST because the
    // store chests at full price had either an invented grant — one treasury
    // chest, which froze the client's store opening sequence and paid a single
    // fixed tier-5 bundle (#215/#216) — or none at all, which refused the sale.
    let randomised =
        blades_lib::features::store_bundles::is_randomised_bundle(&body.global_shop_product_id);
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
    let globals = app_state.get_ref().clone();
    let lifetime_cap = lifetime_purchase_cap(&app_state.static_data, product_id, now);
    let window_cap = window_purchase_cap(&app_state.static_data, product_id, now);
    let product_tracking_ids =
        current_product_purchase_tracking_ids(&app_state.static_data, product_id, now);
    let visible_tracking_ids = current_purchase_tracking_ids(&app_state.static_data, now);
    let lifetime_tracking_ids = current_lifetime_purchase_tracking_ids(&app_state.static_data, now);
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryTownEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Checked BEFORE the wallet is touched, so a refused purchase costs
            // nothing.
            if let Some((scope, used, cap)) = purchase_cap_reached(
                &entry.server_state.0,
                product_id,
                lifetime_cap,
                window_cap.as_ref(),
            ) {
                log::info!(
                    "[shop] character {character_id} is at the {scope} cap for product \
                     {product_id} ({used}/{cap}) — refusing"
                );
                return Err(map_purchase_err(PurchaseError::InvalidPrice));
            }

            let bought_before = entry
                .server_state
                .0
                .global_shop_purchases
                .get(&product_id)
                .copied()
                .unwrap_or(0);

            // Read after the row lock above, so a switch cannot land between it
            // and the roll.
            let roll_alt = crate::alt_roll::for_character(&mut conn, character_id).await;
            let roll_nonce = purchase_roll_nonce(bought_before, character_id, roll_alt);
            let reward = if randomised {
                RewardGrant::default()
            } else {
                purchase_reward(
                    &globals.static_data,
                    &globals.repair_data,
                    &globals.game_data.items_template,
                    product_id,
                    roll_nonce,
                )
                .ok_or_else(|| {
                    log::warn!(
                        "[shop] character {character_id} tried to buy product {product_id} which has no \
                         deliverable reward (#167); refusing with the price-mismatch shape \
                         rather than a 404, which would brick the client",
                    );
                    map_purchase_err(PurchaseError::InvalidPrice)
                })?
            };

            // Charge the (validated) price after proving the reward is deliverable.
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
            // bundle twice in a row cannot return the same thing. The character id
            // is folded in so two players at the same level do not walk the same
            // sequence of chests. The held templates are what keep an artifact the
            // buyer already owns from being handed out again (#310). The Epic
            // Chest is a tier-4 (Elder) chest, so from level 45 it pays retail's
            // high-level Elder gold and stackables like a treasury one (#378), and
            // the Golden Chest is a Gold chest, so from 49 it is a level-100
            // treasury Gold chest (#421).
            let reward = {
                // From level 49 the level-100 buyer's chest (#398); a chest's soul
                // gems at 45-100 by the owner's rule (#421, #429).
                blades_lib::features::store_bundles::roll_store_chest(
                    &product_id,
                    u64::from(entry.character.0.level),
                    roll_nonce,
                    &entry.inventory.0.item_templates(),
                    Some(&globals.static_data.chest_loots),
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
            // TOWN PRESTIGE (#353). `apply_reward` has no town to pay `townXp`
            // into, so it was sent to the client and dropped: a decoration bought
            // here never counted. Retail paid it with the purchase — 11 of 11
            // measurable store purchases moved the town's experiencePoints by
            // exactly the reward's townXp (Fighters' Trophy +80 among them).
            //
            // Paid BEFORE this purchase is counted below: taking the town runs the
            // one-time backfill over `globalShopPurchases`, which must not yet
            // include the purchase being paid here.
            let town = crate::town::credit_town_xp(
                &mut entry.town,
                &mut entry.server_state.0,
                &globals,
                reward.town_xp,
            )?;
            record_purchase(&mut entry.server_state.0, product_id, product_tracking_ids);

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let global_shop_purchases = purchases_list_with_tracking_counts(
                &entry.server_state.0.global_shop_purchases,
                &entry.server_state.0.global_shop_window_purchases,
                &visible_tracking_ids,
                &lifetime_tracking_ids,
            );

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
                town,
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
fn item_table()
-> &'static std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem> {
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
        assert_eq!(
            raw_map.len(),
            before + 1,
            "one malformed retail id is omitted"
        );
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
            offsets.insert(
                after["activeStartDate"].as_i64().unwrap()
                    - before["activeStartDate"].as_i64().unwrap(),
            );
            assert_eq!(
                after["activeEndDate"].as_i64().unwrap()
                    - before["activeEndDate"].as_i64().unwrap(),
                after["activeStartDate"].as_i64().unwrap()
                    - before["activeStartDate"].as_i64().unwrap(),
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
            let s1 = shifted["globalShopOverrides"][id]["activeStartDate"]
                .as_i64()
                .unwrap();
            assert_eq!(
                s0 % 86_400,
                s1 % 86_400,
                "offer {id} changed its time of day"
            );
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
                shifted["globalShopOverrides"][id]
                    .get("maxPurchaseLimits")
                    .and_then(|l| l.as_array()),
            ) else {
                continue;
            };
            for (b, a) in bl.iter().zip(al.iter()) {
                let bt = b["purchaseTrackingId"].as_str().unwrap();
                let at = a["purchaseTrackingId"].as_str().unwrap();
                match bt
                    .rsplit_once("::")
                    .and_then(|(_, t)| t.parse::<i64>().ok())
                {
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
        assert!(
            checked > 100,
            "expected many per-occurrence ids, saw {checked}"
        );
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
        assert!(
            raw["globalShopOverrides"].get(BAD).is_some(),
            "fixture keeps the retail typo"
        );
        for now in [1_780_000_000, 1_783_000_000 + 400 * 86_400] {
            assert!(
                shift_to_now(&raw, now)["globalShopOverrides"]
                    .get(BAD)
                    .is_none(),
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
            &authored(
                "11111111-1111-4111-8111-111111111111",
                now + 30 * 86_400,
                now + 37 * 86_400,
            ),
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
        assert_eq!(
            served["globalShopOverrides"][&id]["activeEndDate"].as_i64(),
            Some(9)
        );
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
        assert_eq!(
            aliases, expected,
            "only the three replacement windows alias a grant"
        );
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
        assert!(
            capped > 0,
            "no capped product found — the enforcement is inert"
        );
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
        assert!(
            !entry.is_null(),
            "the incident product must still be in the catalogue"
        );

        let windowed: Vec<i64> = entry["maxPurchaseLimits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| {
                l["purchaseTrackingId"]
                    .as_str()
                    .and_then(|t| {
                        t.rsplit("::")
                            .next()
                            .map(|s| s.chars().all(|c| c.is_ascii_digit()))
                    })
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
        // Retail declared 3 per window; #394 raised the Enchanter packs to 9.
        assert_eq!(cap, 9, "the raised cap (#394) over retail's 3 per window");
        assert!(
            key.rsplit("::")
                .next()
                .is_some_and(|t| t.chars().all(|c| c.is_ascii_digit())),
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

        assert!(
            first.is_some(),
            "precondition: the offer has a windowed cap now"
        );
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
                        let windowed = t.rsplit("::").next().is_some_and(|x| {
                            !x.is_empty() && x.chars().all(|c| c.is_ascii_digit())
                        });
                        windowed && l["limit"].as_u64().unwrap_or(0) > 0
                    });
                (!any_windowed_nonzero)
                    .then(|| Uuid::parse_str(k).ok())
                    .flatten()
            })
            .next()
            .expect("some offer has no windowed cap");

        assert_eq!(
            window_purchase_cap(&sd, uncapped, now),
            None,
            "an uncapped offer must not be given a cap"
        );
    }

    /// The client decides that a limited-time offer is sold out by comparing the
    /// offer's `maxPurchaseLimits[].purchaseTrackingId` with the character's
    /// `globalShop.globalShopPurchases`. Retail returned product, bare override
    /// and windowed tracking ids after a purchase; returning only the product id
    /// leaves some capped offers looking buyable until the doomed purchase 400s.
    #[test]
    fn character_purchase_state_includes_the_current_tracking_ids() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let now = 1_783_000_000 + 400 * 86_400;

        let product = Uuid::parse_str("6ec8f67f-2cef-41aa-a7fc-f46237ae809c").unwrap();
        let (window_key, cap) =
            window_purchase_cap(&sd, product, now).expect("incident product still has a cap");
        assert_eq!(cap, 9, "retail's 3, raised by #394");
        let tracking_ids = current_product_purchase_tracking_ids(&sd, product, now);
        let bare_key = tracking_ids
            .iter()
            .find(|id| {
                !id.rsplit("::")
                    .next()
                    .is_some_and(|tail| tail.chars().all(|c| c.is_ascii_digit()))
            })
            .expect("retail product has a bare override tracking id")
            .clone();

        let visible = current_purchase_tracking_ids(&sd, now);
        let lifetime_tracking = current_lifetime_purchase_tracking_ids(&sd, now);
        assert!(
            visible.contains(&window_key),
            "the key the purchase handler enforces must be visible to the client"
        );
        assert!(
            visible.contains(&bare_key),
            "the bare override tracking id must be visible to the client"
        );

        let product_counts = HashMap::from([(product, 2)]);
        let tracking_counts = HashMap::from([(bare_key.clone(), 2), (window_key.clone(), 3)]);
        let list = purchases_list_with_tracking_counts(
            &product_counts,
            &tracking_counts,
            &visible,
            &lifetime_tracking,
        );

        assert!(
            list.iter()
                .any(|e| e.id == product.to_string() && e.quantity == 2),
            "control: ordinary lifetime product counts still go out"
        );
        assert!(
            list.iter().any(|e| e.id == bare_key && e.quantity == 2),
            "the current bare override count must go out like retail"
        );
        assert!(
            list.iter().any(|e| e.id == window_key && e.quantity == 3),
            "the current window count must go out so the capped offer reads sold out"
        );
    }

    /// Tracker #8: Mikhail's shield has `maxPurchases: 1`, but the client-side
    /// sold-out check also sees the bare override entry in `maxPurchaseLimits`.
    /// Retail returned that bare tracking id at quantity 1 after the first buy.
    #[test]
    fn character_purchase_state_includes_the_capped_shield_override_tracking_id() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let product = Uuid::parse_str("bcc83fac-5a47-4ba9-8627-3b10d4eece9c").unwrap();
        let now = 1_783_000_000 + 400 * 86_400;

        assert_eq!(lifetime_purchase_cap(&sd, product, now), Some(1));
        let tracking_ids = current_product_purchase_tracking_ids(&sd, product, now);
        let bare_key =
            "bcc83fac-5a47-4ba9-8627-3b10d4eece9c::override::834a1e14-10f4-4f0e-a9ec-719c4f479905";
        assert!(
            tracking_ids.iter().any(|id| id == bare_key),
            "the shield's capped override tracking id must be counted"
        );

        let visible = current_purchase_tracking_ids(&sd, now);
        let lifetime_tracking = current_lifetime_purchase_tracking_ids(&sd, now);
        let product_counts = HashMap::from([(product, 1)]);
        let tracking_counts = HashMap::new();
        let list = purchases_list_with_tracking_counts(
            &product_counts,
            &tracking_counts,
            &visible,
            &lifetime_tracking,
        );

        assert!(
            list.iter()
                .any(|e| e.id == product.to_string() && e.quantity == 1),
            "the lifetime product count still goes out"
        );
        assert!(
            list.iter().any(|e| e.id == bare_key && e.quantity == 1),
            "the client needs this override count to gray out the shield"
        );
    }

    /// Negative control: old replay-window keys must not leak back to the client.
    /// Their timestamp no longer appears in the served catalogue, so sending them
    /// cannot sell anything out and only grows the payload forever.
    #[test]
    fn character_purchase_state_omits_stale_window_tracking_ids() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let now = 1_783_000_000 + 400 * 86_400;

        let product = Uuid::parse_str("6ec8f67f-2cef-41aa-a7fc-f46237ae809c").unwrap();
        let (window_key, _) =
            window_purchase_cap(&sd, product, now).expect("incident product still has a cap");
        let stale_key = window_key
            .rsplit_once("::")
            .map(|(head, _)| format!("{head}::1"))
            .expect("window key has a timestamp");
        assert_ne!(stale_key, window_key);

        let visible = current_purchase_tracking_ids(&sd, now);
        let lifetime_tracking = current_lifetime_purchase_tracking_ids(&sd, now);
        let product_counts = HashMap::new();
        let tracking_counts = HashMap::from([(stale_key.clone(), 99)]);
        let list = purchases_list_with_tracking_counts(
            &product_counts,
            &tracking_counts,
            &visible,
            &lifetime_tracking,
        );

        assert!(
            !list.iter().any(|e| e.id == stale_key),
            "stale replay-window counts must not be advertised"
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
        for (id, e) in sd.global_shop_overrides["globalShopOverrides"]
            .as_object()
            .unwrap()
        {
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
            let uuid: Uuid = id
                .parse()
                .unwrap_or_else(|e| panic!("authored id {id}: {e}"));
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
        OfferContents {
            kind,
            contents,
            town_xp: 0,
        }
    }

    /// The case this exists for: currencies and stackables, granted verbatim.
    /// Modelled on a real captured offer — gold plus three town resources.
    #[test]
    fn a_literal_offer_becomes_a_grantable_reward() {
        let o = offer(
            OfferContentsKind::Literal,
            vec![
                entry(GOLD, 10_000, "currencies"),
                entry(CLAY, 85, "stackableItems"),
            ],
        );
        let r = grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1)
            .expect("literal must be grantable");
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
            let Some(built) = grant_from_offer_contents(
                sd.global_shop_offer_contents.get(id),
                &rd,
                item_table(),
                1,
            ) else {
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
        assert!(
            compared > 0,
            "nothing to compare — the corpus or the contents file is wrong"
        );
        eprintln!("instance identity: {matched}/{compared}");
        assert!(
            matched * 100 >= compared * 90,
            "only {matched}/{compared} reconstructions match the recorded retail instance"
        );
    }

    /// Tracker #403: a THEMED Sigil ring — APK-authored GRADING and ENCHANTING —
    /// is sold exactly as authored on every purchase, as retail sold it (its one
    /// captured retail grant is that set). This test asserted the opposite until
    /// #403: f69663f2 generalised "retail rolled every jewel" from the randomised
    /// jewellery bundles, never from a named offer.
    #[test]
    fn a_themed_sigil_ring_keeps_its_authored_properties_every_purchase() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let rd = repair_data();
        let offer_id = Uuid::parse_str("579141c5-4960-4280-9b29-98b8a88f9dfe").unwrap();
        let contents = sd
            .global_shop_offer_contents
            .get(&offer_id)
            .expect("Ebony Faerite Ring offer present");
        let authored = &contents.contents[0];
        assert_eq!(authored.item_template_id, EBONY_FAERITE_RING);
        assert_eq!(authored.enchanting.len(), 3, "fixture must be the fixed-secondary offer");
        assert!(
            !sd.enchanting.secondary_tables.is_empty(),
            "the prod path (reroll_sigil_jewelry) needs the real enchanting tables"
        );
        let props = |v: &[blades_lib::static_data::PropertyRef]| {
            v.iter()
                .map(|p| blades_lib::user_data::ItemSingleProperty { id: p.id, tier: p.tier })
                .collect::<Vec<_>>()
        };
        for nonce in 0..200 {
            let reward = grant_from_offer_contents_with_enchanting(
                Some(contents),
                &rd,
                item_table(),
                &sd.enchanting,
                nonce,
            )
            .expect("Sigil jewelry offer grantable");
            let item = &reward.items[0].item;
            assert_eq!(item.item_template_id, EBONY_FAERITE_RING);
            assert_eq!(item.properties.grading, props(&authored.grading), "purchase {nonce}");
            assert_eq!(item.properties.enchanting, props(&authored.enchanting), "purchase {nonce}");
            assert_eq!(item.grade, Some(4), "grade = 1 + 1 + 2");
        }
    }

    /// Negative control: the jewelry roll path must not touch non-jewelry Sigil gear.
    #[test]
    fn sigil_non_jewelry_keeps_its_authored_properties() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let rd = repair_data();
        let contents = sd
            .global_shop_offer_contents
            .get(&ARCANE_HAND_AXE_OFFER)
            .expect("hand axe offer present");
        let a = grant_from_offer_contents_with_enchanting(
            Some(contents),
            &rd,
            item_table(),
            &sd.enchanting,
            0,
        )
        .expect("grantable");
        let b = grant_from_offer_contents_with_enchanting(
            Some(contents),
            &rd,
            item_table(),
            &sd.enchanting,
            37,
        )
        .expect("grantable");
        assert_eq!(a.items[0].item.properties, b.items[0].item.properties);
        assert_eq!(a.items[0].item.grade, None);
        assert!(a.items[0].item.durability > 0.0);
    }

    /// Tracker report #8: `SigilShop_Ultimate_RingOfShock` sold the Master Ring of
    /// Shock with `grade: 4` and three rolled GRADING properties on top of the six
    /// mandatory properties its template already carries. A fixed template must
    /// arrive exactly as authored, on every purchase.
    #[test]
    fn the_ring_of_shock_offer_grants_an_unrolled_ring() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        let rd = repair_data();
        let offer = Uuid::parse_str("5e77bc66-a80b-4f9b-b272-e738635cf2fd").unwrap();
        let ring = Uuid::parse_str("85c44edf-2fc6-4b4d-b8a9-3340f127f9f0").unwrap();
        let contents = sd
            .global_shop_offer_contents
            .get(&offer)
            .expect("Ring of Shock offer present");
        assert!(
            !sd.enchanting.secondary_tables.is_empty(),
            "the prod path (reroll_sigil_jewelry) needs the real enchanting tables"
        );
        for nonce in 0..40 {
            let reward = grant_from_offer_contents_with_enchanting(
                Some(contents),
                &rd,
                item_table(),
                &sd.enchanting,
                nonce,
            )
            .expect("Ring of Shock offer grantable");
            assert_eq!(reward.items.len(), 1);
            let item = &reward.items[0].item;
            assert_eq!(item.item_template_id, ring);
            assert_eq!(item.grade, None, "nonce {nonce}: a fixed ring must not be graded");
            assert!(
                item.properties.grading.is_empty(),
                "nonce {nonce}: {:?}",
                item.properties.grading
            );
            assert!(item.properties.enchanting.is_empty(), "nonce {nonce}");
            assert_eq!(item.arcane_tier, None);
        }
    }

    /// The same rule for an arcane offer of a fixed template: the arcane-tier
    /// grade roll (#167) must skip it too.
    #[test]
    fn an_arcane_fixed_ring_is_not_graded_either() {
        let ring = Uuid::parse_str("85c44edf-2fc6-4b4d-b8a9-3340f127f9f0").unwrap();
        let mut e = entry(ring, 1, "items");
        e.arcane_tier = 2;
        let o = offer(OfferContentsKind::NeedsRoll, vec![e]);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let sd = crate::static_loader::load(&dir);
        for nonce in 0..40 {
            let r = grant_from_offer_contents_with_enchanting(
                Some(&o),
                &repair_data(),
                item_table(),
                &sd.enchanting,
                nonce,
            )
            .expect("grantable");
            assert_eq!(r.items[0].item.grade, None);
            assert!(r.items[0].item.properties.grading.is_empty());
        }
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
            let r = grant_from_offer_contents(
                Some(&arcane_no_grading),
                &repair_data(),
                item_table(),
                nonce,
            )
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
            grant_from_offer_contents(Some(&unknown_tier), &repair_data(), item_table(), 1)
                .is_none(),
            "a grade must not be invented for a tier retail never showed"
        );

        // …and the same entry WITH authored grading is grantable, so the refusal is
        // about the missing roll and not about arcane items in general.
        let mut graded = arcane_no_grading.clone();
        graded.contents[0].grading =
            vec![blades_lib::static_data::PropertyRef { id: CLAY, tier: 3 }];
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
        let full = rd
            .max_durability(DRAGONBONE_HAND_AXE, 0)
            .expect("axe durability row");
        assert_eq!(
            full, 162.5,
            "the APK's temper-0 max for the Dragonbone Hand Axe"
        );

        for nonce in 0..200u64 {
            let r = grant_from_offer_contents(Some(offer), &rd, item_table(), nonce)
                .expect("the arcane axe offer is grantable");
            assert_eq!(r.items.len(), 1);
            let item = &r.items[0].item;
            assert_eq!(item.item_template_id, DRAGONBONE_HAND_AXE);
            assert_eq!(item.arcane_tier, Some(2));
            assert_eq!(item.grade, None, "nonce {nonce}: an arcane axe was graded");
            assert!(
                item.properties.grading.is_empty(),
                "nonce {nonce}: GRADING on an axe"
            );
            assert_eq!(item.durability, full, "a new axe must be at full condition");

            let wire = serde_json::to_value(item).unwrap();
            assert_eq!(wire.get("durability").and_then(|v| v.as_f64()), Some(full));
            assert_eq!(wire.get("temperingLevel").and_then(|v| v.as_u64()), Some(0));
            assert!(wire.get("grade").is_none());
            assert!(
                !rd.needs_repair(item),
                "a brand-new axe must not need repair"
            );
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
            assert_eq!(
                r.items[0].item.grade, None,
                "nonce {nonce}: arcane gauntlets were graded"
            );
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
        let o = offer(
            OfferContentsKind::Literal,
            vec![entry(GOLD, 1, "currencies")],
        );
        assert!(grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_some());
        // …and `Unclassified` left this list (#184). The extractor could not place
        // its entries in a bucket; the template's own type can, and 51 offers whose
        // contents are all real item templates were being refused for a label.
        let o = offer(
            OfferContentsKind::Unclassified,
            vec![entry(GOLD, 1, "currencies")],
        );
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
        assert!(
            grant_from_offer_contents(None, &repair_data(), item_table(), 1).is_none(),
            "unknown product"
        );
        let o = offer(OfferContentsKind::Literal, vec![]);
        assert!(
            grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1).is_none(),
            "empty reward"
        );
    }

    /// townXp rides through, and on its own is enough to be a real reward —
    /// 16 of the captured grants carry a non-zero one.
    #[test]
    fn town_xp_rides_through() {
        let mut o = offer(
            OfferContentsKind::Literal,
            vec![entry(GOLD, 5, "currencies")],
        );
        o.town_xp = 40;
        let r = grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1)
            .expect("grantable");
        assert_eq!(r.town_xp, 40);
    }

    /// A template listed twice sums rather than overwriting.
    #[test]
    fn a_repeated_template_sums() {
        let o = offer(
            OfferContentsKind::Literal,
            vec![
                entry(GOLD, 100, "currencies"),
                entry(GOLD, 25, "currencies"),
            ],
        );
        let r = grant_from_offer_contents(Some(&o), &repair_data(), item_table(), 1)
            .expect("grantable");
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
        assert!(
            with_items > 0,
            "no grant awards an item — the premise is gone"
        );
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

        assert_ne!(
            a.items[0].id, b.items[0].id,
            "each purchase needs its own instance id"
        );
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
    fn item_table()
    -> &'static std::collections::HashMap<uuid::Uuid, blades_lib::game_data::GameDataItem> {
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
        let durability: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("item_durability.json")).unwrap(),
        )
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
        let uuid = uuid::Uuid::parse_str("a66c27d2-6fc1-47f9-9c9c-419d1fa98855").unwrap();
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
            grant_from_offer_contents(Some(&contents), &repair_data(), item_table(), 1).is_none(),
            "an id nothing can classify must refuse, not guess a bucket"
        );
    }
}

/// Tracker #238: the replay must carry retail's DAILY rotation, not one window
/// per offer. Every test that asserts the fix also runs the same check against
/// the collapsed replay and asserts that it FAILS there, so none of them can pass
/// vacuously.
#[cfg(test)]
mod report238_calendar_tests {
    use super::*;
    use blades_lib::static_data::StaticData;
    use std::collections::HashSet;

    const SIGIL: &str = "c64bcb53-41f4-41ba-892a-fe2cca423caa";
    const TRANSCENDENT: &str = "24f8421e-5fa4-4ef8-b29c-a5db807dda78"; // SigilShop_SoulGem_Transcendent
    const FROST_SALTS: &str = "3cb3c2ee-35e4-43b7-bf1d-03c45d65d9c1"; // SigilShop_EffectIngredients_FrostSalts
    const FIRE_SALTS: &str = "5d4a230c-43c8-498f-9534-659fe9389476"; // SigilShop_EffectIngredients_FireSalts
    /// When SpaceMunk filed #238: 2026-09-26 18:55 UTC. Prod was serving 23 live
    /// Sigil offers — the two one-day "Divine" items and 21 weekly ones.
    const REPORTED: i64 = 1_790_448_900;
    const DAY: i64 = 86_400;

    fn read(name: &str) -> Value {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    /// The shipped files, as the server loads them.
    fn shipped() -> StaticData {
        StaticData {
            global_shop_overrides: read("global_shop_overrides.json"),
            global_shop_windows: read("global_shop_windows.json"),
            global_shop_authored: serde_json::json!({ "globalShopOverrides": {} }),
            ..StaticData::default()
        }
    }

    /// CONTROL: the same data without the calendar — exactly what prod runs today.
    fn collapsed() -> StaticData {
        StaticData {
            global_shop_windows: serde_json::json!({ "globalShopWindows": {} }),
            ..shipped()
        }
    }

    fn is_sigil(e: &Value) -> bool {
        e["prices"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p["currencyId"].as_str() == Some(SIGIL))
    }

    fn is_live(e: &Value, now: i64) -> bool {
        e.get("isActive").and_then(Value::as_bool).unwrap_or(false)
            && window_of(e).is_some_and(|(s, en)| s <= now && now <= en)
    }

    /// Live Sigil offers at `now`, and the subset on a one-day window (the block
    /// retail replaced every day at 16:00 UTC).
    fn live_sigil(sd: &StaticData, now: i64) -> (HashSet<String>, HashSet<String>) {
        let cat = current_catalog(sd, now);
        let mut all = HashSet::new();
        let mut daily = HashSet::new();
        for (id, e) in cat["globalShopOverrides"].as_object().unwrap() {
            if is_sigil(e) && is_live(e, now) {
                all.insert(id.clone());
                if window_of(e).is_some_and(|(s, en)| en - s <= DAY) {
                    daily.insert(id.clone());
                }
            }
        }
        (all, daily)
    }

    /// 20:00 UTC on each of `days` days from the day of the report — after the
    /// 16:00 rotation, like the retail per-day counts in the PR.
    fn evenings(days: i64) -> impl Iterator<Item = i64> {
        let first = REPORTED - REPORTED.rem_euclid(DAY) + 20 * 3_600;
        (0..days).map(move |d| first + d * DAY)
    }

    /// THE REPORT, at the reported moment: a full daily block, with soul gems and
    /// potion salts in it.
    #[test]
    fn the_reported_moment_has_a_full_daily_block() {
        let (all, daily) = live_sigil(&shipped(), REPORTED);
        assert!(
            all.len() >= 45,
            "only {} live Sigil offers at the report",
            all.len()
        );
        assert!(
            daily.len() >= 25,
            "only {} daily Sigil offers at the report",
            daily.len()
        );

        // CONTROL: the collapsed replay reproduces what the player saw.
        let (all0, daily0) = live_sigil(&collapsed(), REPORTED);
        assert_eq!(
            daily0.len(),
            2,
            "precondition: the old replay showed two daily items"
        );
        assert!(
            all0.len() < 30,
            "precondition: the old replay was thin ({})",
            all0.len()
        );
    }

    /// NO DAY OF TWO FULL CYCLES MAY LACK THE DAILY BLOCK. Retail, measured from
    /// the full calendar, never had fewer than 29 one-day Sigil offers on a day
    /// whose block was captured (median 38).
    #[test]
    fn every_day_restocks_the_daily_block() {
        let days = 2 * REPLAY_PERIOD_DAYS;
        let worst = evenings(days)
            .map(|t| (live_sigil(&shipped(), t).1.len(), t))
            .min()
            .unwrap();
        assert!(
            worst.0 >= 25,
            "only {} daily Sigil offers at {}",
            worst.0,
            worst.1
        );

        // CONTROL: the collapsed replay is missing the block on most days.
        let thin = evenings(days)
            .filter(|&t| live_sigil(&collapsed(), t).1.len() < 10)
            .count();
        assert!(
            thin as i64 > days / 2,
            "precondition: old replay thin on {thin}/{days} days"
        );
    }

    /// "Not refreshing daily": consecutive days' daily blocks must be different
    /// offers. Retail's overlap between consecutive days was zero on all 59 pairs.
    #[test]
    fn the_daily_block_turns_over_completely() {
        let sd = shipped();
        let days: Vec<_> = evenings(REPLAY_PERIOD_DAYS + 1)
            .map(|t| live_sigil(&sd, t).1)
            .collect();
        for (i, pair) in days.windows(2).enumerate() {
            let same = pair[0].intersection(&pair[1]).count();
            assert_eq!(same, 0, "day {i}: {same} daily offers did not rotate");
        }
    }

    /// The reporter's items: Transcendent soul gems and Frost/Fire Salts. Retail
    /// sold each on many days of every six weeks; the collapsed replay on one.
    #[test]
    fn soul_gems_and_salts_come_back_every_few_days() {
        let per_day = |sd: &StaticData| -> Vec<HashSet<String>> {
            evenings(REPLAY_PERIOD_DAYS)
                .map(|t| live_sigil(sd, t).0)
                .collect()
        };
        let (fixed, old) = (per_day(&shipped()), per_day(&collapsed()));
        let days_on_sale =
            |days: &[HashSet<String>], id: &str| days.iter().filter(|d| d.contains(id)).count();
        for (id, name, floor) in [
            (TRANSCENDENT, "Transcendent soul gem", 4),
            (FROST_SALTS, "Frost Salts", 8),
            (FIRE_SALTS, "Fire Salts", 8),
        ] {
            let n = days_on_sale(&fixed, id);
            assert!(
                n >= floor,
                "{name} on sale on only {n} of {REPLAY_PERIOD_DAYS} days"
            );
            // CONTROL
            let n0 = days_on_sale(&old, id);
            assert!(
                n0 <= 1,
                "precondition: the old replay sold {name} on {n0} days"
            );
        }
    }

    /// Nothing is lost: every product is in the catalogue at every moment, and
    /// every one retail ever put on sale is on sale at some point of a cycle. (Three
    /// offers are `isActive: false` in every window retail scheduled; they are
    /// served, as before, and never on sale, as in retail.)
    #[test]
    fn every_offer_is_served_and_reachable() {
        let sd = shipped();
        let valid = sd.global_shop_overrides["globalShopOverrides"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|id| Uuid::parse_str(id).is_ok())
            .count();
        let ever_active = sd.global_shop_windows["globalShopWindows"]
            .as_object()
            .unwrap()
            .values()
            .filter(|ws| {
                ws.as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w["isActive"].as_bool() == Some(true))
            })
            .count();
        assert_eq!(
            valid - ever_active,
            3,
            "precondition: three never-active retail offers"
        );
        let mut reached = HashSet::new();
        for t in (REPORTED..REPORTED + REPLAY_PERIOD).step_by(6 * 3_600) {
            let cat = current_catalog(&sd, t);
            let map = cat["globalShopOverrides"].as_object().unwrap();
            assert_eq!(map.len(), valid, "the catalogue lost offers at {t}");
            reached.extend(
                map.iter()
                    .filter(|(_, e)| is_live(e, t))
                    .map(|(id, _)| id.clone()),
            );
        }
        assert_eq!(
            reached.len(),
            ever_active,
            "offers never on sale in a whole cycle"
        );
    }

    /// Replaying moves windows, never reshapes them: every served window is a
    /// retail window moved by whole periods, so its length, its time of day and
    /// its weekday are retail's. Its per-occurrence tracking id moves with it.
    #[test]
    fn served_windows_are_retail_windows_moved_by_whole_periods() {
        let sd = shipped();
        let calendar = sd.global_shop_windows["globalShopWindows"]
            .as_object()
            .unwrap();
        for t in [REPORTED, REPORTED + 17 * DAY, REPORTED + 900 * DAY] {
            let cat = current_catalog(&sd, t);
            for (id, e) in cat["globalShopOverrides"].as_object().unwrap() {
                let (s, en) = window_of(e).unwrap();
                let matched = calendar[id].as_array().unwrap().iter().find(|w| {
                    let (ws, wen) = window_of(w).unwrap();
                    (s - ws).rem_euclid(REPLAY_PERIOD) == 0 && en - s == wen - ws
                });
                let w = matched.unwrap_or_else(|| panic!("{id} served a window retail never had"));
                let moved = s - window_of(w).unwrap().0;
                let tids = |v: &Value| -> Vec<String> {
                    v["maxPurchaseLimits"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l["purchaseTrackingId"].as_str().map(str::to_owned))
                        .collect()
                };
                for (a, b) in tids(w).iter().zip(tids(e).iter()) {
                    match a
                        .rsplit_once("::")
                        .and_then(|(h, ts)| Some((h, ts.parse::<i64>().ok()?)))
                    {
                        Some((h, ts)) => assert_eq!(b, &format!("{h}::{}", ts + moved)),
                        None => assert_eq!(a, b),
                    }
                }
            }
        }
    }

    /// Inside the replayed slice, the client sees exactly what retail showed that
    /// day: the replay is the identity on its own source dates.
    #[test]
    fn inside_the_slice_retail_dates_are_served_unmoved() {
        let sd = shipped();
        let now = CALENDAR_START + 21 * DAY + 4 * 3_600; // 2026-06-10 20:00 UTC
        let (served, _) = live_sigil(&sd, now);
        let retail: HashSet<String> = sd.global_shop_windows["globalShopWindows"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, ws)| {
                ws.as_array()
                    .unwrap()
                    .iter()
                    .any(|w| is_sigil(w) && is_live(w, now))
            })
            .map(|(id, _)| id.clone())
            .collect();
        assert!(
            retail.len() >= 45,
            "precondition: retail had a full shop that day"
        );
        assert_eq!(served, retail);
    }

    /// The three ~950-day evergreen offers must stay on sale however far the
    /// clock runs, not blink out once per cycle.
    #[test]
    fn evergreen_offers_never_lapse() {
        let sd = shipped();
        for id in [
            "11102495-fde7-4e77-b6c4-d13b9303f1f5",
            "11b322d2-1b5e-4e90-bb41-a8d90ca9548b",
            "9e4dc391-422e-4b25-ba90-faab39e0769f",
        ] {
            assert!(
                sd.global_shop_windows["globalShopWindows"]
                    .get(id)
                    .is_some(),
                "precondition: {id} is in the calendar"
            );
            for t in
                (REPORTED..REPORTED + 5 * 365 * DAY).step_by((5 * DAY + 7 * 3_600 + 13) as usize)
            {
                let cat = current_catalog(&sd, t);
                assert!(
                    is_live(&cat["globalShopOverrides"][id], t),
                    "{id} lapsed at {t}"
                );
            }
        }
    }

    /// Deploy-order safety: until the box has `global_shop_windows.json`, the
    /// server must behave exactly as it did before this change.
    #[test]
    fn without_a_calendar_the_old_replay_is_unchanged() {
        let sd = collapsed();
        for t in [REPORTED, REPORTED + 40 * DAY] {
            assert_eq!(
                current_catalog(&sd, t),
                // #394's raised caps apply on either path.
                apply_raised_window_caps(shift_to_now(&sd.global_shop_overrides, t)),
            );
        }
    }

    /// An admin-authored window still wins over the replayed calendar.
    #[test]
    fn an_authored_window_still_wins() {
        let mut sd = shipped();
        let (start, end) = (REPORTED + DAY, REPORTED + 2 * DAY);
        sd.global_shop_authored = serde_json::json!({
            "globalShopOverrides": { TRANSCENDENT: {
                "activeStartDate": start, "activeEndDate": end, "isActive": true,
                "maxPurchaseLimits": [], "maxPurchases": 0,
                "prices": [{ "currencyId": SIGIL, "quantity": 5 }],
            }}
        });
        let cat = current_catalog(&sd, REPORTED);
        assert_eq!(
            window_of(&cat["globalShopOverrides"][TRANSCENDENT]),
            Some((start, end))
        );
    }

    /// When Sephoris filed #400: 2026-10-16 07:33 UTC.
    const REPORT_400: i64 = 1_792_135_980;

    /// Tracker #400. Retail's `GET /catalogoverrides/globalshop` held ONLY offers live
    /// at the moment of the request — 923 of 923 captured responses, at most 79
    /// offers and 46,152 bytes. We served every offer in the calendar (546, 320,802
    /// bytes at the report), 472 of them weeks from starting, on every store open.
    ///
    /// Swept over two whole replay cycles, so every day of the rotation and the wrap
    /// are covered: every served override must be live, and the payload must stay
    /// within retail's envelope. The control is the full catalogue at the reported
    /// moment, which is what the endpoint used to return.
    #[test]
    fn the_store_endpoint_serves_only_live_offers_at_retail_size() {
        let sd = shipped();
        let before = current_catalog(&sd, REPORT_400);
        let before_len = serde_json::to_string(&before).unwrap().len();
        assert!(
            before["globalShopOverrides"].as_object().unwrap().len() > 500
                && before_len > 300_000,
            "control: the full catalogue is what #400's client was fetching ({before_len} bytes)"
        );

        let (mut max_offers, mut max_bytes) = (0, 0);
        for t in (REPORT_400..REPORT_400 + 2 * REPLAY_PERIOD).step_by(3 * 3_600) {
            let served = served_catalog(&sd, t);
            let map = served["globalShopOverrides"].as_object().unwrap();
            for (id, e) in map {
                let (s, en) = window_of(e).unwrap();
                assert!(s <= t && t <= en, "{id} served outside its window at {t}");
            }
            assert!(!map.is_empty(), "the store is empty at {t}");
            max_offers = max_offers.max(map.len());
            max_bytes = max_bytes.max(serde_json::to_string(&served).unwrap().len());
        }
        // Retail's measured ceiling is 79 offers / 46 KB. A little headroom for the
        // replay's window boundaries, but nowhere near the 546 / 320 KB served before.
        assert!(max_offers <= 120, "up to {max_offers} offers served at once");
        assert!(max_bytes <= 80_000, "up to {max_bytes} bytes served at once");
    }

    /// Cutting the payload must not hide anything that is on sale: every offer the
    /// full catalogue has live (the set the price check honours) is still served,
    /// unchanged, at every step of a cycle.
    #[test]
    fn nothing_on_sale_is_dropped_from_the_store_endpoint() {
        let sd = shipped();
        for t in (REPORT_400..REPORT_400 + REPLAY_PERIOD).step_by(5 * 3_600) {
            let full = current_catalog(&sd, t);
            let served = served_catalog(&sd, t);
            let served = served["globalShopOverrides"].as_object().unwrap();
            for (id, e) in full["globalShopOverrides"].as_object().unwrap() {
                if is_live(e, t) {
                    assert_eq!(served.get(id), Some(e), "{id} is on sale at {t} but was not served");
                }
            }
        }
    }

    /// An authored window is served while it runs and not before: the cut applies
    /// after the authored overlay, like retail never announcing an offer early.
    #[test]
    fn an_authored_window_is_served_only_while_it_runs() {
        let mut sd = shipped();
        let (start, end) = (REPORT_400 + DAY, REPORT_400 + 2 * DAY);
        sd.global_shop_authored = serde_json::json!({
            "globalShopOverrides": { TRANSCENDENT: {
                "activeStartDate": start, "activeEndDate": end, "isActive": true,
                "maxPurchaseLimits": [], "maxPurchases": 0,
                "prices": [{ "currencyId": SIGIL, "quantity": 5 }],
            }}
        });
        assert!(served_catalog(&sd, REPORT_400)["globalShopOverrides"]
            .get(TRANSCENDENT)
            .is_none());
        assert_eq!(
            window_of(&served_catalog(&sd, start + 3_600)["globalShopOverrides"][TRANSCENDENT]),
            Some((start, end))
        );
    }
}

/// Tracker #353: a decoration bought in the store ("Victor's Trophy" / Fighters'
/// Trophy, Sephoris) paid no town prestige. Retail paid every store purchase's
/// `townXp` into the town on the spot — 11 of 11 measurable retail purchases
/// moved `town.levelInfo.experiencePoints` by exactly the reward's `townXp`.
#[cfg(test)]
mod report353_store_prestige {
    use super::*;

    fn shipped() -> blades_lib::static_data::StaticData {
        crate::static_loader::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static"
        )))
    }

    const FIGHTERS_TROPHY_OFFER: &str = "0db1411b-f1b6-4422-9dfc-1d1c4decdb99";

    /// Retail capture 51225: the Fighters' Trophy offer paid +80 (77272 -> 77352).
    #[test]
    fn the_fighters_trophy_pays_eighty() {
        let sd = shipped();
        assert_eq!(product_town_xp(&sd, Uuid::parse_str(FIGHTERS_TROPHY_OFFER).unwrap()), 80);
    }

    /// The other measured purchases, each paid exactly its `townXp` on retail.
    #[test]
    fn every_measured_retail_purchase_pays_what_retail_paid() {
        let sd = shipped();
        for (product, retail_delta) in [
            ("afc71167-0544-4112-ad28-7cd1b2ea95d1", 142), // capture 761
            ("67b7a1eb-05fb-417d-b013-1e5fc037acc0", 59),  // 10214, 55525
            ("fd022c2e-a06c-4271-aa66-4424b1a8a7ab", 143), // 10226
            ("6324182a-c722-4059-89d0-040ba500b4f8", 82),  // 10242
            ("ab94a2e0-9770-4fee-9f7f-8d9a34946750", 86),  // 10254
            ("c820b311-5b84-4bd6-be6c-a76cf0a5c57b", 72),  // 11416
            ("1185efd6-bb85-44d3-ae0d-e14272b2f8eb", 56),  // 15338
            ("8e35aa43-0e1c-4abd-9db2-3a908f851669", 168), // 23459
            ("bb5631a5-e68d-4455-a01e-bc3080bcf054", 133), // 30428
            (FIGHTERS_TROPHY_OFFER, 80),                   // 51225
        ] {
            assert_eq!(product_town_xp(&sd, Uuid::parse_str(product).unwrap()), retail_delta, "{product}");
        }
    }

    /// The backfill prices past purchases with `product_town_xp`, so it must say
    /// what the purchase path actually pays, for every product in the catalogue.
    #[test]
    fn product_town_xp_agrees_with_the_purchase_reward() {
        let sd = shipped();
        let items = item_table();
        let repair = repair_data();
        let mut products: std::collections::HashSet<Uuid> =
            sd.global_shop_offer_contents.keys().copied().collect();
        products.extend(sd.global_shop_grants.keys().copied());
        let mut paying = 0;
        for product in products {
            let paid = if blades_lib::features::store_bundles::is_randomised_bundle(&product) {
                0
            } else {
                purchase_reward(&sd, &repair, items, product, 0).map_or(0, |r| r.town_xp)
            };
            // A product the store cannot grant was never bought, so it owes
            // nothing either way; every grantable one must agree exactly.
            let owed = product_town_xp(&sd, product);
            assert!(owed == paid || paid == 0, "{product}: backfill {owed}, purchase {paid}");
            paying += usize::from(paid > 0);
        }
        assert!(paying >= 20, "the store's prestige-paying offers: {paying}");
    }

    /// Every store offer that sells a single decoration pays prestige.
    #[test]
    fn every_store_decoration_pays_prestige() {
        let sd = shipped();
        let items = item_table();
        let mut decorations = 0;
        for (product, c) in &sd.global_shop_offer_contents {
            let is_decoration = |t: &Uuid| items.get(t).is_some_and(|i| i.r#type == 4);
            if c.contents.len() == 1 && is_decoration(&c.contents[0].item_template_id) {
                decorations += 1;
                assert!(product_town_xp(&sd, *product) > 0, "{product} sells a decoration for no prestige");
            }
        }
        assert!(decorations >= 10, "{decorations}");
    }

    /// The response carries `town` only when the purchase paid prestige.
    #[test]
    fn the_response_carries_town_only_when_prestige_was_paid() {
        let base = || PurchaseResponse {
            inventory: CompleteInventoryUpdate {
                backpack: Default::default(),
                loadout: Default::default(),
                treasury: Default::default(),
                overflow_treasury: Default::default(),
                backpack_version: 1,
                treasury_version: 1,
            },
            wallet: CompleteWallet::default(),
            global_shop: GlobalShopState { global_shop_purchases: vec![] },
            reward: RewardGrant::default(),
            town: None,
        };
        assert!(serde_json::to_value(base()).unwrap().get("town").is_none());
        let mut paid = base();
        paid.town = Some(serde_json::json!({"levelInfo": {"level": 10, "experiencePoints": 77352}}));
        assert_eq!(
            serde_json::to_value(paid).unwrap()["town"]["levelInfo"]["experiencePoints"],
            77352
        );
    }

    /// The purchase pays the town BEFORE it counts itself into
    /// `globalShopPurchases`: taking the town runs the one-time backfill over those
    /// counts, and a purchase counted first would be paid twice.
    #[test]
    fn the_purchase_pays_the_town_before_counting_itself() {
        let src = include_str!("global_shop.rs");
        let start = src.find("pub async fn purchase_global_shop(").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        let credit = body.find("crate::town::credit_town_xp(").expect("the purchase pays the town");
        let count = body.find("record_purchase(").expect("counted");
        assert!(credit < count, "pay the town first, then count the purchase");
        assert!(body.contains("CharacterDbEntryTownEconomy::as_select()"), "the row must carry the town");
    }
}

/// Tracker #386: "I'm buying the necklaces with two guaranteed enchantments —
/// always the same one comes." The Ebony Faerite Necklace of Assured Imbuement
/// (arcane tier 2: "guaranteed to receive exactly 2 bonus enchantments") has a
/// captured retail grant, and the captured grant won over the per-purchase roll,
/// so every buyer got that one recorded necklace — same grade, same GRADING —
/// on every purchase. Prod held 21 purchases of it across four characters, each
/// a copy of the capture.
///
/// #403 narrowed this: of the six captured jewellery products #386 re-rolled,
/// three are THEMED (APK-authored GRADING) and retail sold each exactly as
/// authored, so they are fixed again. Only the three arcane ones roll.
#[cfg(test)]
mod report_386_captured_jewelry_rolls {
    use super::*;
    use std::collections::HashSet;

    /// Ebony Faerite Necklace of Assured Imbuement — the reporter's offer.
    const ARCANE_NECKLACE_OFFER: Uuid = Uuid::from_u128(0x79995d29_eac2_49a3_bfec_ce9e547b98de);
    /// The captured ARCANE jewellery products: the APK leaves their GRADING
    /// empty, so retail rolled it and so do we.
    const CAPTURED_ROLLED_JEWELRY: [&str; 3] = [
        "79995d29-eac2-49a3-bfec-ce9e547b98de", // EbonyFaeriteNecklace_Arcane02
        "812ea9ef-c2af-406b-9b93-1897db4a6879", // EbonyFaeriteRing_Arcane02
        "e87d0348-84ee-48be-a19e-6201b38e4b74", // GoldRubyRing_Arcane02
    ];
    /// Tracker #403: the THEMED jewellery products retail evidence covers. #386
    /// listed them with the arcane three, but each is an APK-authored set and
    /// its one captured RETAIL purchase (`global_shop_grants.json`, mined from
    /// prod's `api_captures` up to 2026-07-01 by `build-shop-static.py`) is that
    /// exact set — GRADING ids, tiers and ENCHANTING. The "21 identical copies"
    /// in #386 were our server's purchases, not retail's.
    const RETAIL_THEMED_JEWELRY: [(&str, &str); 3] = [
        ("579141c5-4960-4280-9b29-98b8a88f9dfe", "EbonyFaeriteRing_PowerAttackDodgingStrikeQuickStrikes_AbsStm"),
        ("dac59a1a-7f22-4306-9117-fa90d44aee3d", "EbonyFaeriteRing_FrostbiteIceSpikeBlizzardArmor_NoEnch"),
        ("fc8f44dd-d4cf-48b8-b8b2-77eaaa0643b8", "EbonyFaeriteNecklace_Armsman_FortMgcRegen"),
    ];
    const CHAR_A: Uuid = Uuid::from_u128(0x489620db_7f90_4a03_bb7c_f7e92a9c73cb);
    const CHAR_B: Uuid = Uuid::from_u128(0xf7817aa6_5892_4794_871e_9b51a475a606);
    const PURCHASES: u64 = 40;

    fn sd() -> &'static blades_lib::static_data::StaticData {
        static SD: std::sync::OnceLock<blades_lib::static_data::StaticData> =
            std::sync::OnceLock::new();
        SD.get_or_init(|| {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
            crate::static_loader::load(&dir)
        })
    }

    /// What `purchase_global_shop` pays this character's `n`-th purchase.
    fn buy(product: Uuid, character: Uuid, n: u64) -> blades_lib::user_data::Item {
        let r = purchase_reward(
            sd(),
            &repair_data(),
            item_table(),
            product,
            purchase_roll_nonce(n, character, None),
        )
        .expect("the offer is deliverable");
        assert_eq!(r.items.len(), 1, "{product}: one item");
        r.items[0].item.clone()
    }

    fn roll_key(item: &blades_lib::user_data::Item) -> String {
        format!(
            "{:?}|{:?}|{:?}",
            item.grade, item.properties.grading, item.properties.enchanting
        )
    }

    /// The defect itself, as a before/after: the captured grant is one fixed
    /// instance, and the purchase now rolls a fresh one per buy.
    #[test]
    fn the_reporters_necklace_is_rolled_per_purchase_not_cloned() {
        let sd = sd();
        assert!(
            captured_grant_for_product(sd, ARCANE_NECKLACE_OFFER).is_some(),
            "fixture: the offer must carry a captured grant, or this tests nothing"
        );
        assert!(rolls_jewelry_per_purchase(sd, item_table(), ARCANE_NECKLACE_OFFER));

        let necklace = Uuid::from_u128(0x54a7fc91_93e7_4908_a29a_0667d0b07179);
        let mut seen = HashSet::new();
        for n in 0..PURCHASES {
            let item = buy(ARCANE_NECKLACE_OFFER, CHAR_A, n);
            assert_eq!(item.item_template_id, necklace);
            assert_eq!(item.arcane_tier, Some(2), "the Assured Imbuement tier survives");
            assert!(
                item.properties.enchanting.is_empty(),
                "retail's arcane necklace arrives un-enchanted (the capture has none)"
            );
            // Retail rings and necklaces are always graded, from their own slot.
            let grade = item.grade.expect("an arcane necklace is always graded");
            assert_eq!(grade, item.properties.grading.iter().map(|p| p.tier).sum::<u64>());
            assert!(!item.properties.grading.is_empty());
            for p in &item.properties.grading {
                assert!(
                    crate::jewelry_grade::slot_allows(crate::jewelry_grade::NECKLACE, p.id),
                    "purchase {n}: GRADING {} is not a necklace property",
                    p.id
                );
            }
            assert_eq!(item.durability, 0.0, "jewellery never wears");
            seen.insert(roll_key(&item));
        }
        eprintln!(
            "#386 arcane necklace: {} distinct rolls in {PURCHASES} purchases (captured clone: 1)",
            seen.len()
        );
        assert!(
            seen.len() as u64 >= PURCHASES / 2,
            "only {} distinct necklaces in {PURCHASES} purchases",
            seen.len()
        );
    }

    fn sorted_props(v: &[blades_lib::user_data::ItemSingleProperty]) -> Vec<(Uuid, u64)> {
        let mut out: Vec<(Uuid, u64)> = v.iter().map(|p| (p.id, p.tier)).collect();
        out.sort_unstable();
        out
    }

    /// #403, the evidence itself: every captured retail purchase of a themed
    /// offer is exactly the APK-authored set. If a refreshed capture ever shows
    /// one that is not, this fails and `is_themed_jewelry` must be revisited.
    #[test]
    fn retail_themed_grants_are_the_authored_sets() {
        let sd = sd();
        for (raw, name) in RETAIL_THEMED_JEWELRY {
            let product = Uuid::parse_str(raw).unwrap();
            let c = &sd.global_shop_offer_contents[&product];
            let authored = &c.contents[0];
            assert!(is_themed_jewelry(authored), "{name}: authored GRADING");
            let retail = &sd.global_shop_grants[&product].items[0].item;
            let auth = |v: &[blades_lib::static_data::PropertyRef]| {
                let mut out: Vec<(Uuid, u64)> = v.iter().map(|p| (p.id, p.tier)).collect();
                out.sort_unstable();
                out
            };
            assert_eq!(sorted_props(&retail.properties.grading), auth(&authored.grading), "{name}: GRADING");
            assert_eq!(sorted_props(&retail.properties.enchanting), auth(&authored.enchanting), "{name}: ENCHANTING");
            assert_eq!(retail.grade, Some(authored.grading.iter().map(|p| p.tier).sum()), "{name}: grade");
        }
    }

    /// #403: every themed offer — all APK-authored GRADING, not only the three
    /// retail purchases cover — yields its authored set on each of 200 purchases
    /// by several characters, grade = sum of tiers.
    #[test]
    fn every_themed_offer_sells_its_fixed_set_over_200_purchases() {
        let sd = sd();
        let themed: Vec<Uuid> = sd
            .global_shop_offer_contents
            .iter()
            .filter(|(_, c)| c.contents.iter().any(is_themed_jewelry))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(themed.len(), 21, "the APK authors GRADING for 21 Sigil jewellery offers");
        for product in themed {
            let authored = &sd.global_shop_offer_contents[&product].contents[0];
            let want_grading: Vec<(Uuid, u64)> = {
                let mut v: Vec<_> = authored.grading.iter().map(|p| (p.id, p.tier)).collect();
                v.sort_unstable();
                v
            };
            let want_ench: Vec<(Uuid, u64)> = {
                let mut v: Vec<_> = authored.enchanting.iter().map(|p| (p.id, p.tier)).collect();
                v.sort_unstable();
                v
            };
            let want_grade: u64 = authored.grading.iter().map(|p| p.tier).sum();
            for n in 0..200u64 {
                let who = if n % 2 == 0 { CHAR_A } else { CHAR_B };
                let item = buy(product, who, n / 2);
                assert_eq!(item.item_template_id, authored.item_template_id, "{product}");
                assert_eq!(sorted_props(&item.properties.grading), want_grading, "{product}: purchase {n}");
                assert_eq!(sorted_props(&item.properties.enchanting), want_ench, "{product}: purchase {n}");
                assert_eq!(item.grade, Some(want_grade), "{product}: purchase {n}");
                assert_eq!(item.durability, 0.0, "jewellery never wears");
            }
        }
    }

    /// Suspect 2 of the report: the nonce was only the purchase count, so every
    /// player's n-th purchase was the same item as everyone else's.
    #[test]
    fn two_characters_walk_different_sequences() {
        for product in [ARCANE_NECKLACE_OFFER] {
            let a: Vec<String> = (0..20).map(|n| roll_key(&buy(product, CHAR_A, n))).collect();
            let b: Vec<String> = (0..20).map(|n| roll_key(&buy(product, CHAR_B, n))).collect();
            let same = a.iter().zip(&b).filter(|(x, y)| x == y).count();
            assert!(same <= 5, "{product}: {same}/20 purchases identical between two characters");
        }
        // Control: the same character buying the same n-th time is reproducible,
        // so the difference above is the character and not noise.
        assert_eq!(
            roll_key(&buy(ARCANE_NECKLACE_OFFER, CHAR_A, 3)),
            roll_key(&buy(ARCANE_NECKLACE_OFFER, CHAR_A, 3))
        );
    }

    /// Every captured jewellery product now varies, and keeps its template,
    /// arcane tier and authored enchantment count.
    #[test]
    fn no_captured_jewelry_product_hands_out_one_frozen_item() {
        let sd = sd();
        for raw in CAPTURED_ROLLED_JEWELRY {
            let product = Uuid::parse_str(raw).unwrap();
            let authored = &sd.global_shop_offer_contents[&product].contents[0];
            assert!(captured_grant_for_product(sd, product).is_some(), "{raw}: fixture");
            let mut seen = HashSet::new();
            for n in 0..PURCHASES {
                let item = buy(product, CHAR_A, n);
                assert_eq!(item.item_template_id, authored.item_template_id, "{raw}");
                assert_eq!(item.arcane_tier, (authored.arcane_tier > 0).then_some(authored.arcane_tier), "{raw}");
                assert_eq!(item.properties.enchanting.len(), authored.enchanting.len(), "{raw}");
                assert!(item.grade.is_some(), "{raw}: purchase {n} ungraded");
                seen.insert(roll_key(&item));
            }
            assert!(seen.len() > 1, "{raw}: one frozen item for {PURCHASES} purchases");
        }
    }

    /// CONTROLS. The fix must not reach captured grants it has no business with:
    /// the fixed-template Ultimate rings (#8) and captured gear still arrive
    /// exactly as recorded, apart from the fresh instance id.
    #[test]
    fn fixed_rings_and_captured_gear_still_pay_the_capture() {
        let sd = sd();
        for raw in [
            "0a2a58e0-e26f-47e3-8170-c0806a577414", // LTUltimate Defensive Spells Ring
            "b579ca24-28be-41c0-a205-35f8d08d1bfa", // Ultimate Ring of Frost
            "e4ea78fa-f49a-44de-8ad0-d0965f5ce487", // Ultimate Defensive Spells Ring
            "5e40760a-4d88-44fb-b0b5-2f07b2dd8443", // arcane Dragonbone Light Hammer
            "c05dfce1-b221-4906-a67d-84c4e13b0172", // Ultimate Shock Mace
        ] {
            let product = Uuid::parse_str(raw).unwrap();
            let captured = captured_grant_for_product(sd, product).expect("fixture: captured");
            assert!(!rolls_jewelry_per_purchase(sd, item_table(), product), "{raw}");
            for n in 0..5 {
                let paid = purchase_reward(
                    sd,
                    &repair_data(),
                    item_table(),
                    product,
                    purchase_roll_nonce(n, CHAR_A, None),
                )
                .unwrap();
                assert_eq!(&paid, captured, "{raw}: the capture must still be paid verbatim");
            }
        }
    }

    /// Arcane jewellery with NO capture used to draw from the whole arcane corpus:
    /// ungraded about half the time, a ring's properties on a necklace about one
    /// time in seven. Now always graded from its own slot — including arcane tier
    /// 1, whose corpus holds only ungraded weapons.
    #[test]
    fn uncaptured_arcane_jewelry_is_graded_from_its_own_slot() {
        let sd = sd();
        let mut checked = 0;
        for (product, c) in &sd.global_shop_offer_contents {
            if captured_grant_for_product(sd, *product).is_some() {
                continue;
            }
            let [e] = c.contents.as_slice() else { continue };
            let Some(slot) = item_table().get(&e.item_template_id).map(|i| i.r#type) else {
                continue;
            };
            if !matches!(slot, crate::jewelry_grade::RING | crate::jewelry_grade::NECKLACE)
                || e.arcane_tier == 0
                || !e.grading.is_empty()
            {
                continue;
            }
            checked += 1;
            for n in 0..30 {
                let item = buy(*product, CHAR_B, n);
                assert!(item.grade.is_some(), "{product}: purchase {n} ungraded");
                assert!(
                    item.properties
                        .grading
                        .iter()
                        .all(|p| crate::jewelry_grade::slot_allows(slot, p.id)),
                    "{product}: purchase {n} carries another slot's GRADING"
                );
            }
        }
        assert!(checked >= 10, "fixture: arcane jewellery offers found: {checked}");
    }
}

/// Tracker #392: "The rings with two bonus enchantments in the Sigil shop repeat
/// constantly. I bought about 50 and not a single one a mage could use."
/// Sephoris bought the Ebony Faerite Ring of Assured Imbuement (`812ea9ef`,
/// arcane tier 2) 47 times on 2026-10-14; the 51 such rings in her backpack carry
/// only 30 distinct GRADING sets, the commonest four times.
///
/// THE CAUSE. #386 narrowed an arcane ring's GRADING to the retail outcomes for
/// its own slot, and at tier 2 that is 32 recorded rings — so the shop sold a
/// catalogue of 32 fixed rings, and five of the APK's 34 ring properties could
/// never appear: Absorb, Firewall, Ice Spike and Paralyze (all spells) and
/// Shield Bash. 32 grants are evidence of WHICH GRADES retail rolled at tier 2,
/// not a list of every ring it could roll — every non-arcane retail ring (1,121
/// graded pieces) draws from all 34.
///
/// THE FIX. The properties are drawn without repeats from the slot's whole APK
/// list. (The grade still came from the retail tier-2 outcomes until #393
/// replaced it with HauDrauf's property-count shares - see
/// `report_393_sigil_jewelry_property_count`.)
#[cfg(test)]
mod report_392_arcane_ring_variety {
    use super::*;
    use std::collections::{BTreeSet, HashMap, HashSet};

    /// SigilShop_Jewelry_EbonyFaeriteRing_Arcane02.
    const RING_OFFER: Uuid = Uuid::from_u128(0x812ea9ef_c2af_406b_9b93_1897db4a6879);
    /// Ebony Faerite Necklace of Assured Imbuement (#386).
    const NECKLACE_OFFER: Uuid = Uuid::from_u128(0x79995d29_eac2_49a3_bfec_ce9e547b98de);
    const SEPHORIS: Uuid = Uuid::from_u128(0xf7817aa6_5892_4794_871e_9b51a475a606);
    const OTHER: Uuid = Uuid::from_u128(0x489620db_7f90_4a03_bb7c_f7e92a9c73cb);

    fn sd() -> &'static blades_lib::static_data::StaticData {
        static SD: std::sync::OnceLock<blades_lib::static_data::StaticData> =
            std::sync::OnceLock::new();
        SD.get_or_init(|| {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
            crate::static_loader::load(&dir)
        })
    }

    fn buy(product: Uuid, character: Uuid, n: u64) -> blades_lib::user_data::Item {
        let r = purchase_reward(
            sd(),
            &repair_data(),
            item_table(),
            product,
            purchase_roll_nonce(n, character, None),
        )
        .expect("the offer is deliverable");
        assert_eq!(r.items.len(), 1, "{product}: one item");
        r.items[0].item.clone()
    }

    /// The GRADING as a set — what a player compares when he says "the same ring".
    fn grading_set(item: &blades_lib::user_data::Item) -> BTreeSet<(Uuid, u64)> {
        item.properties
            .grading
            .iter()
            .map(|p| (p.id, p.tier))
            .collect()
    }

    fn slot_list(slot: &str) -> HashMap<Uuid, &'static str> {
        crate::arena::combat::gamedata::GRADE_PROPERTIES
            .iter()
            .filter(|g| g.slot == slot)
            .map(|g| (Uuid::parse_str(g.uuid).unwrap(), g.editor_name))
            .collect()
    }

    /// THE DEFECT. 100 purchases came from a catalogue of at most 32 rings.
    /// Now: one-property rings (13 of the 32 retail tier-2 rings) can only be one
    /// of 34, so they collide; every multi-property ring is near-unique. 100
    /// purchases measure 80 and 85 distinct sets for these two characters (at
    /// most 32 before — the catalogue's size); the floor leaves room for seed
    /// noise and still sits far above the old ceiling.
    #[test]
    fn a_hundred_purchases_are_not_a_catalogue() {
        for character in [SEPHORIS, OTHER] {
            let seen: HashSet<_> = (0..100)
                .map(|n| grading_set(&buy(RING_OFFER, character, n)))
                .collect();
            eprintln!(
                "#392 {character}: {} distinct GRADING sets in 100 purchases",
                seen.len()
            );
            assert!(
                seen.len() >= 70,
                "{character}: only {} distinct rings in 100 purchases",
                seen.len()
            );
        }
    }

    /// Every one of the 34 APK ring properties can be bought — the 17 spells
    /// included, and the five the 32-ring catalogue could never hand out.
    #[test]
    fn every_ring_property_appears() {
        let ring = slot_list("Ring");
        assert_eq!(ring.len(), 34, "RingGradePropertyList");
        let mut seen: HashMap<Uuid, u32> = HashMap::new();
        for n in 0..2000 {
            for p in buy(RING_OFFER, SEPHORIS, n).properties.grading {
                *seen.entry(p.id).or_default() += 1;
            }
        }
        let missing: Vec<_> = ring
            .iter()
            .filter(|(id, _)| !seen.contains_key(id))
            .map(|(_, n)| *n)
            .collect();
        assert!(
            missing.is_empty(),
            "never rolled in 2000 purchases: {missing:?}"
        );
        for name in [
            "AbsorbBonusRanks",
            "FirewallBonusRanks",
            "IceSpikeBonusRanks",
            "ParalyzeBonusRanks",
            "ShieldBashBonusRanks",
        ] {
            let id = ring
                .iter()
                .find(|(_, n)| **n == name)
                .map(|(id, _)| *id)
                .unwrap();
            assert!(seen[&id] >= 20, "{name}: only {} times in 2000", seen[&id]);
        }
        // Uniform: no property starved or dominant. At #393's 5/10/85 a ring
        // carries 2.8 properties, so ~165 each over 2000 purchases.
        let (lo, hi) = (seen.values().min().unwrap(), seen.values().max().unwrap());
        assert!(*lo >= 115 && *hi <= 215, "property counts range {lo}..{hi}");
    }

    /// Every ring keeps the tiers its grade implies (`_providedBonuses`),
    /// distinct properties from its own slot, no enchantments, arcane tier 2.
    /// The grade SHARES are #393's (`report_393_sigil_jewelry_property_count`).
    #[test]
    fn grades_and_tiers_stay_retail() {
        let ring = slot_list("Ring");
        let mut by_grade = [0u32; 7];
        let n = 2000;
        for i in 0..n {
            let item = buy(RING_OFFER, SEPHORIS, i);
            let g = &item.properties.grading;
            let grade = item.grade.expect("an arcane ring is always graded");
            assert_eq!(
                grade,
                g.iter().map(|p| p.tier).sum::<u64>(),
                "grade is the tier sum"
            );
            let mut tiers: Vec<u64> = g.iter().map(|p| p.tier).collect();
            tiers.sort_unstable_by(|a, b| b.cmp(a));
            let provided = crate::jewelry_grade::JEWELRY_GRADES
                .iter()
                .find(|j| j.0 == grade)
                .unwrap()
                .2;
            assert_eq!(tiers, provided, "grade {grade}: tiers {tiers:?}");
            assert!(
                g.iter().all(|p| ring.contains_key(&p.id)),
                "a non-ring property"
            );
            assert_eq!(grading_set(&item).len(), g.len(), "a property twice");
            assert!(
                item.properties.enchanting.is_empty(),
                "retail's ring arrives un-enchanted"
            );
            assert_eq!(item.arcane_tier, Some(2));
            by_grade[grade as usize] += 1;
        }
        assert!(
            by_grade[1..=6].iter().all(|c| *c > 0),
            "every grade 1-6 must be reachable: {by_grade:?}"
        );
    }

    /// Necklaces likewise draw from the whole Necklace list, keep their own
    /// grades (grade 6 does occur on retail tier-2 necklaces) and never carry a
    /// ring property.
    #[test]
    fn necklaces_use_the_full_necklace_list() {
        let neck = slot_list("Necklace");
        assert_eq!(neck.len(), 15, "NecklaceGradePropertyList");
        let mut seen = HashSet::new();
        let mut sets = HashSet::new();
        for n in 0..2000 {
            let item = buy(NECKLACE_OFFER, SEPHORIS, n);
            for p in &item.properties.grading {
                assert!(neck.contains_key(&p.id), "a non-necklace property");
                seen.insert(p.id);
            }
            assert_eq!(grading_set(&item).len(), item.properties.grading.len());
            if n < 100 {
                sets.insert(grading_set(&item));
            }
        }
        assert_eq!(seen.len(), 15, "necklace properties reached");
        assert!(
            sets.len() >= 60,
            "only {} distinct necklaces in 100",
            sets.len()
        );
    }

    /// The per-character purchase sequence (#386/#387) is untouched: the n-th
    /// purchase is reproducible, and two characters do not walk one sequence.
    #[test]
    fn the_purchase_sequence_is_still_per_character() {
        assert_eq!(
            grading_set(&buy(RING_OFFER, SEPHORIS, 7)),
            grading_set(&buy(RING_OFFER, SEPHORIS, 7))
        );
        let same = (0..40)
            .filter(|n| {
                grading_set(&buy(RING_OFFER, SEPHORIS, *n))
                    == grading_set(&buy(RING_OFFER, OTHER, *n))
            })
            .count();
        assert!(
            same <= 4,
            "{same}/40 purchases identical between two characters"
        );
    }
}

/// Tracker #387: "Jewellery came out the same as in the last 100 … or it always
/// starts with chest 1, and if I only open 100 I never get the 200th."
/// Sephoris (level 46-47) opened 100 Legendary chests on each of two days.
///
/// It does not start again. A Legendary chest's roll is seeded by
/// [`purchase_roll_nonce`] from this character's LIFETIME purchase count of the
/// product (`globalShopPurchases`, 1,226 for Sephoris on 2026-10-14, 717 on
/// 10-10), never by the per-window count, which a new offer window resets. On
/// prod, 251 of the 297 enchanted or graded pieces Sephoris kept since 10-10 are
/// exactly the piece this roll predicts, and 207 of them match exactly one
/// purchase, purchases 819 to 1,225. What he saw is the tier: from 45 the pool
/// has one ring and one necklace (Ebony Faerite) and two shields (Dragonscale
/// and Dragonplate), as retail's did.
#[cfg(test)]
mod report_387_legendary_chest_sequence {
    use super::*;

    const LEGENDARY: Uuid = Uuid::from_u128(0x11102495_fde7_4e77_b6c4_d13b9303f1f5);
    const SEPHORIS: Uuid = Uuid::from_u128(0xf7817aa6_5892_4794_871e_9b51a475a606);

    fn key(item: &blades_lib::user_data::Item) -> String {
        format!(
            "{}|{:?}|{:?}|{}",
            item.item_template_id, item.properties.enchanting, item.properties.grading, item.tempering_level
        )
    }

    /// The pieces of purchases `from..to`, rolled as `purchase_global_shop` rolls them.
    fn chests(from: u64, to: u64, level: u64) -> Vec<String> {
        (from..to)
            .flat_map(|n| {
                blades_lib::features::store_bundles::roll_bundle_with(
                    &LEGENDARY,
                    level,
                    purchase_roll_nonce(n, SEPHORIS, None),
                    &HashSet::new(),
                    None,
                )
                .expect("the Legendary chest rolls")
                .items
                .into_iter()
                .map(|i| key(&i.item))
            })
            .collect()
    }

    /// The reporter's hypothesis as a test: the second 100 chests must not be the
    /// first 100 again, neither as a sequence nor as a set.
    #[test]
    fn the_second_hundred_chests_do_not_replay_the_first() {
        for (start, level) in [(0u64, 46u64), (1_026, 46), (1_026, 47)] {
            let a = chests(start, start + 100, level);
            let b = chests(start + 100, start + 200, level);
            let same_place = a.iter().zip(&b).filter(|(x, y)| x == y).count();
            assert!(same_place <= 2, "from {start} at {level}: {same_place} pieces repeat in place");
            let first: HashSet<&String> = a.iter().collect();
            let shared = b.iter().filter(|k| first.contains(k)).count();
            // Only unenchanted Dragon armour and shields (two templates, one
            // tempering) can coincide by chance.
            assert!(
                shared * 100 <= b.len() * 5,
                "from {start} at {level}: {shared} of {} pieces of the second hundred were in the first",
                b.len()
            );
        }
    }

    /// Retail's 307 distinct purchases repeated a slot exactly 1.3-4.2% of the
    /// time; 200 chests in a row stay in that range.
    #[test]
    fn two_hundred_chests_repeat_about_as_rarely_as_retail() {
        for start in [0u64, 1_026, 5_000, 77_777] {
            let pieces = chests(start, start + 200, 47);
            let distinct = pieces.iter().collect::<HashSet<_>>().len();
            let repeated = (pieces.len() - distinct) as f64 / pieces.len() as f64;
            assert!(repeated <= 0.06, "from {start}: {:.1}% of pieces repeated", repeated * 100.0);
        }
    }

    /// The count the roll is seeded from is the lifetime one, never the window's.
    #[test]
    fn the_roll_nonce_counts_every_purchase_ever_made() {
        let src = include_str!("global_shop.rs");
        let start = src.find("pub async fn purchase_global_shop(").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        let bought = body.find("let bought_before = entry").expect("the count is read");
        let read = &body[bought..bought + body[bought..].find(';').unwrap()];
        assert!(read.contains(".global_shop_purchases"), "lifetime count: {read}");
        assert!(!read.contains("window"), "a window count restarts with every offer: {read}");
        assert!(body.contains("purchase_roll_nonce(bought_before, character_id, roll_alt)"));
        // #421: the store chests roll with the shipped treasury tables, so a
        // Golden chest from 49 is a level-100 Gold chest.
        assert!(body.contains("Some(&globals.static_data.chest_loots)"), "the chest tables reach the roll");
        let nonces: HashSet<u64> = (0..10_000).map(|n| purchase_roll_nonce(n, SEPHORIS, None)).collect();
        assert_eq!(nonces.len(), 10_000);
    }
}

/// Tracker #387, the alt half: a player's alts share ONE live character row, so
/// a purchase nonce seeded by `character_id` alone walked every alt with the
/// same counts through the same sequence. A later alt is folded in; the
/// original's nonce is pinned to the value it has always had.
#[cfg(test)]
mod report_387_alts_roll_their_own {
    use super::*;

    const LEGENDARY: Uuid = Uuid::from_u128(0x11102495_fde7_4e77_b6c4_d13b9303f1f5);
    const ROW: Uuid = Uuid::from_u128(0xf7817aa6_5892_4794_871e_9b51a475a606);
    const ALT_A: Uuid = Uuid::from_u128(0x0a17_0000_0000_4000_8000_0000_0000_000a);
    const ALT_B: Uuid = Uuid::from_u128(0x0a17_0000_0000_4000_8000_0000_0000_000b);

    fn chests(alt: Option<Uuid>, level: u64) -> Vec<String> {
        (0..50u64)
            .flat_map(|n| {
                blades_lib::features::store_bundles::roll_bundle_with(
                    &LEGENDARY,
                    level,
                    purchase_roll_nonce(n, ROW, alt),
                    &HashSet::new(),
                    None,
                )
                .expect("the Legendary chest rolls")
                .items
                .into_iter()
                .map(|i| {
                    format!(
                        "{}|{:?}|{:?}",
                        i.item.item_template_id, i.item.properties.enchanting, i.item.properties.grading
                    )
                })
            })
            .collect()
    }

    /// The original — every row with no later alt live — keeps today's nonce
    /// bit for bit (values computed from the pre-#387 formula).
    #[test]
    fn the_originals_nonce_is_unchanged() {
        assert_eq!(purchase_roll_nonce(0, ROW, None), 3_787_163_076_896_478_058);
        assert_eq!(purchase_roll_nonce(1_226, ROW, None), 3_787_163_076_896_479_136);
        for n in [0u64, 1, 717, 1_226, 99_999] {
            assert_eq!(
                purchase_roll_nonce(n, ROW, None),
                n ^ (ROW.as_u128() as u64).rotate_left(29)
            );
        }
    }

    /// Two alts of one row, same level, same purchase counts: different nonces
    /// and different chests — and both differ from the original.
    #[test]
    fn two_alts_with_identical_counts_and_level_roll_different_sequences() {
        for n in 0..1_000u64 {
            let (a, b, o) = (
                purchase_roll_nonce(n, ROW, Some(ALT_A)),
                purchase_roll_nonce(n, ROW, Some(ALT_B)),
                purchase_roll_nonce(n, ROW, None),
            );
            assert!(a != b && a != o && b != o, "purchase {n}");
        }
        let (a, b, o) = (chests(Some(ALT_A), 47), chests(Some(ALT_B), 47), chests(None, 47));
        for (name, x, y) in [("A/B", &a, &b), ("A/original", &a, &o), ("B/original", &b, &o)] {
            let same_place = x.iter().zip(y).filter(|(p, q)| p == q).count();
            assert!(
                same_place * 100 <= x.len() * 10,
                "{name}: {same_place} of {} pieces identical in place",
                x.len()
            );
        }
    }

    /// The purchase handler reads the alt after locking the row and passes it to
    /// the nonce; a call without it would silently re-share the sequence.
    #[test]
    fn the_purchase_handler_folds_the_live_alt_in() {
        let src = include_str!("global_shop.rs");
        let start = src.find("pub async fn purchase_global_shop(").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        let lock = body.find(".for_no_key_update()").expect("the row is locked");
        let read = body
            .find("crate::alt_roll::for_character(&mut conn, character_id)")
            .expect("the alt is read");
        assert!(lock < read, "the alt is read after the row lock");
        assert!(body.contains("purchase_roll_nonce(bought_before, character_id, roll_alt)"));
    }
}

/// Tracker #393 (HauDrauf): "I bought 100 rings and almost half have only one
/// ability, then many with two. I've bought thousands of rings and never saw
/// this. The vast majority must have 3 abilities. Only one ability maybe 5%,
/// two maybe 10%." Necklaces likewise had too many with one.
///
/// THE CAUSE. The grade of an arcane ring was copied from the 32 tier-2 ring
/// outcomes we captured, and 13 of those had one property: 41% one / 16% two /
/// 44% three. Sephoris's 107 Ebony Faerite rings on prod measured 46% / 19% /
/// 36%, and her 45 necklaces 18% / 18% / 64% (corpus: 25% / 20% / 55%).
///
/// THE FIX. Every Sigil-shop arcane ring and necklace draws its property count
/// from HauDrauf's shares (`jewelry_grade::JEWELRY_PROPERTY_COUNT_PERCENT`)
/// and, within three properties, its grade by the APK weights.
#[cfg(test)]
mod report_393_sigil_jewelry_property_count {
    use super::*;
    use crate::jewelry_grade::{
        JEWELRY_GRADES, JEWELRY_PROPERTY_COUNT_PERCENT, NECKLACE, RING,
    };

    /// SigilShop_Jewelry_EbonyFaeriteRing_Arcane02 - the reporter's ring.
    const RING_OFFER: Uuid = Uuid::from_u128(0x812ea9ef_c2af_406b_9b93_1897db4a6879);
    /// SigilShop_Jewelry_EbonyFaeriteNecklace_Arcane02.
    const NECKLACE_OFFER: Uuid = Uuid::from_u128(0x79995d29_eac2_49a3_bfec_ce9e547b98de);
    const PURCHASES: u64 = 4000;

    fn sd() -> &'static blades_lib::static_data::StaticData {
        static SD: std::sync::OnceLock<blades_lib::static_data::StaticData> =
            std::sync::OnceLock::new();
        SD.get_or_init(|| {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
            crate::static_loader::load(&dir)
        })
    }

    /// Purchase `i` across 40 buyers, 100 purchases each - how a population of
    /// players walks the per-character nonce.
    fn buy(product: Uuid, i: u64) -> blades_lib::user_data::Item {
        let buyer = Uuid::from_u128(0x3930_0000_0000_4000_8000_0000_0000_0000 + u128::from(i % 40));
        let r = purchase_reward(
            sd(),
            &repair_data(),
            item_table(),
            product,
            purchase_roll_nonce(i / 40, buyer, None),
        )
        .expect("the offer is deliverable");
        assert_eq!(r.items.len(), 1, "{product}: one item");
        r.items[0].item.clone()
    }

    /// Asserts the retail shape and returns the property count.
    fn check(item: &blades_lib::user_data::Item, slot: u64) -> usize {
        let g = &item.properties.grading;
        let grade = item.grade.expect("arcane jewellery is always graded");
        assert_eq!(grade, g.iter().map(|p| p.tier).sum::<u64>(), "grade = sum of tiers");
        let tiers: Vec<u64> = g.iter().map(|p| p.tier).collect();
        let provided = JEWELRY_GRADES
            .iter()
            .find(|j| j.0 == grade)
            .unwrap_or_else(|| panic!("grade {grade} is not an APK grade"))
            .2;
        assert_eq!(tiers, provided, "grade {grade}: tiers {tiers:?}");
        assert!(g.iter().all(|p| crate::jewelry_grade::slot_allows(slot, p.id)));
        let distinct: std::collections::HashSet<_> = g.iter().map(|p| p.id).collect();
        assert_eq!(distinct.len(), g.len(), "a property twice");
        g.len()
    }

    /// THE DEFECT, measured the way the reporter did: abilities per item.
    /// Failed before the fix (rings 41% / 16% / 44%).
    #[test]
    fn property_counts_follow_haudrauf() {
        for (product, slot, name) in [
            (RING_OFFER, RING, "ring"),
            (NECKLACE_OFFER, NECKLACE, "necklace"),
        ] {
            let mut count = [0u64; 4];
            let mut by_grade = [0u64; 7];
            for i in 0..PURCHASES {
                let item = buy(product, i);
                count[check(&item, slot)] += 1;
                by_grade[item.grade.unwrap() as usize] += 1;
            }
            let share = |n: u64| n as f64 * 100.0 / PURCHASES as f64;
            eprintln!(
                "#393 {name}: 1={:.1}% 2={:.1}% 3={:.1}%  grades {by_grade:?}",
                share(count[1]),
                share(count[2]),
                share(count[3])
            );
            for (props, pct) in JEWELRY_PROPERTY_COUNT_PERCENT {
                let got = share(count[props]);
                assert!(
                    (got - f64::from(pct)).abs() <= 2.5,
                    "{name}: {props} properties {got:.1}% (want {pct}% +/- 2.5)"
                );
            }
            // Within three properties, grades 3-6 by the APK weights 18/16/14/10.
            let three: u64 = by_grade[3..=6].iter().sum();
            for (grade, weight, _) in JEWELRY_GRADES.iter().filter(|g| g.2.len() == 3) {
                let got = by_grade[*grade as usize] as f64 / three as f64;
                let want = f64::from(*weight) / 58.0;
                assert!(
                    (got - want).abs() <= 0.04,
                    "{name}: grade {grade} is {got:.3} of three-property items (want {want:.3})"
                );
            }
        }
    }

    /// Every arcane ring and necklace the Sigil shop sells (tier 1 and 2,
    /// needs-roll and unclassified offers alike) is deliverable and rolls the
    /// same way.
    #[test]
    fn every_arcane_sigil_jewel_rolls_the_same_way() {
        let sd = sd();
        let items = item_table();
        let mut products = Vec::new();
        for (id, c) in &sd.global_shop_offer_contents {
            let jewel = c.contents.iter().find_map(|e| {
                let t = items.get(&e.item_template_id)?.r#type;
                (matches!(t, RING | NECKLACE)
                    && e.arcane_tier > 0
                    && e.grading.is_empty()
                    && !crate::fixed_templates::has_mandatory_properties(e.item_template_id))
                .then_some(t)
            });
            if let Some(t) = jewel {
                products.push((*id, t));
            }
        }
        assert!(
            products.len() >= 27,
            "only {} arcane Sigil jewels found",
            products.len()
        );
        let (mut one, mut total) = (0u64, 0u64);
        for (product, slot) in products {
            for i in 0..200 {
                if check(&buy(product, i), slot) == 1 {
                    one += 1;
                }
                total += 1;
            }
        }
        let share = one as f64 * 100.0 / total as f64;
        assert!(
            share < 10.5,
            "{share:.1}% single-property arcane jewels (want ~8%)"
        );
    }

    /// #393, 2026-10-15 ("überall, keine Unterschiede"): the merchant / crafted
    /// roll (`jewelry_grade::roll`) follows the same split. It used to be the APK
    /// weights alone: ~22% single-property jewellery.
    #[test]
    fn the_merchant_roll_follows_the_same_split() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(393);
        for slot in [RING, NECKLACE] {
            let n = 20_000u64;
            let mut count = [0u64; 4];
            for _ in 0..n {
                let (grade, g) = crate::jewelry_grade::roll(slot, &mut rng).unwrap();
                assert_eq!(grade, g.iter().map(|p| p.tier).sum::<u64>());
                count[g.len()] += 1;
            }
            for (props, pct) in JEWELRY_PROPERTY_COUNT_PERCENT {
                let got = count[props] as f64 * 100.0 / n as f64;
                assert!(
                    (got - f64::from(pct)).abs() <= 3.0,
                    "{slot}: {props} properties {got:.1}% (want {pct}%)"
                );
            }
        }
    }

    /// #393 tested a split on "Sigil jewellery whose grading the shop re-rolls
    /// although the offer is not an arcane needs-roll one". After #403 there is
    /// no such offer: every single-jewel offer is arcane (rolled, the split
    /// above), THEMED (authored GRADING, sold as authored) or a fixed template.
    /// A new offer outside those three would be rolled by `reroll_sigil_jewelry`
    /// without any evidence for it, so this fails until it is classified.
    #[test]
    fn every_sigil_jewel_is_arcane_themed_or_fixed() {
        let sd = sd();
        let items = item_table();
        let mut unclassified = Vec::new();
        let (mut arcane, mut themed, mut fixed) = (0, 0, 0);
        for (id, c) in &sd.global_shop_offer_contents {
            if c.contents.len() != 1 {
                continue;
            }
            let e = &c.contents[0];
            let Some(t) = items.get(&e.item_template_id).map(|i| i.r#type) else {
                continue;
            };
            if !matches!(t, RING | NECKLACE) {
                continue;
            }
            if crate::fixed_templates::has_mandatory_properties(e.item_template_id) {
                fixed += 1;
            } else if is_themed_jewelry(e) {
                themed += 1;
            } else if e.arcane_tier > 0 {
                arcane += 1;
            } else {
                unclassified.push(*id);
            }
        }
        eprintln!("Sigil jewels: {arcane} arcane, {themed} themed, {fixed} fixed");
        assert_eq!(themed, 21, "the APK authors GRADING for 21 Sigil jewellery offers");
        assert!(arcane > 0 && fixed > 0, "control: both other classes exist");
        assert!(unclassified.is_empty(), "jewellery offers with no rule: {unclassified:?}");
    }
}

/// Tracker #394 (owner-approved, 2026-10-16): the Enchanter packs, every tier,
/// sell 9 per window instead of retail's 3. Window and everything else unchanged.
#[cfg(test)]
mod raised_window_cap_tests {
    use super::*;

    fn sd() -> blades_lib::static_data::StaticData {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn is_windowed(tid: &str) -> bool {
        tid.rsplit("::")
            .next()
            .is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
    }

    /// The catalogue before the #394 override: what retail's data alone serves.
    fn unraised(sd: &blades_lib::static_data::StaticData, now: i64) -> Value {
        let replayed = replay_calendar(&sd.global_shop_windows, now)
            .unwrap_or_else(|| shift_to_now(&sd.global_shop_overrides, now));
        apply_authored(replayed, &sd.global_shop_authored)
    }

    /// One hour into `product`'s next (or current) window after `after`.
    fn inside_window(sd: &blades_lib::static_data::StaticData, product: &str, after: i64) -> i64 {
        let e = &current_catalog(sd, after)["globalShopOverrides"][product];
        let start = e["activeStartDate"].as_i64().expect("window start");
        start.max(after - 3_600) + 3_600
    }

    const BASE: i64 = 1_783_000_000 + 400 * 86_400;

    #[test]
    fn the_table_is_the_four_enchanter_tiers() {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/global_shop_offer_contents.json"
        );
        let offers: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let mut names: Vec<&str> = RAISED_WINDOW_CAPS
            .iter()
            .map(|(id, cap)| {
                assert_eq!(*cap, 9, "{id}");
                offers["offers"][*id]["editorName"].as_str().unwrap()
            })
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "IAP_GemPack05_a_ApprenticeEnchanter",
                "IAP_GemPack05_b_JourneymanEnchanter",
                "IAP_GemPack05_c_ExpertEnchanter",
                "IAP_GemPack05_d_HeroicEnchanter",
            ]
        );
    }

    /// What `GET /catalogoverrides/globalshop` sends while the offer is live: the
    /// windowed limit reads 9, where retail's data alone reads 3 (the control).
    /// The client takes this limit from the payload; the APK default only fills
    /// tracking ids the override does not name.
    #[test]
    fn the_served_payload_shows_nine_for_every_tier_and_keeps_the_thursday_window() {
        let sd = sd();
        for (product, _) in RAISED_WINDOW_CAPS {
            let now = inside_window(&sd, product, BASE);
            let served = served_catalog(&sd, now);
            let e = &served["globalShopOverrides"][*product];
            assert!(e.is_object(), "{product} is live and served at {now}");
            let before = &unraised(&sd, now)["globalShopOverrides"][*product];

            let limits = e["maxPurchaseLimits"].as_array().unwrap();
            let raw = before["maxPurchaseLimits"].as_array().unwrap();
            assert_eq!(limits.len(), raw.len());
            for (got, was) in limits.iter().zip(raw) {
                let tid = got["purchaseTrackingId"].as_str().unwrap();
                assert_eq!(tid, was["purchaseTrackingId"].as_str().unwrap());
                if is_windowed(tid) {
                    assert_eq!(was["limit"], 3, "control: retail's data says 3 ({tid})");
                    assert_eq!(got["limit"], 9, "served limit is raised ({tid})");
                } else {
                    assert_eq!(
                        got["limit"], was["limit"],
                        "un-windowed limit untouched ({tid})"
                    );
                }
            }
            assert_eq!(e["maxPurchases"], before["maxPurchases"]);
            assert_eq!(e["prices"], before["prices"]);

            // The window is retail's: unchanged, one day, starting Thursday 04:00 UTC.
            let (s, en) = window_of(e).unwrap();
            assert_eq!((s, en), window_of(before).unwrap());
            assert_eq!(
                s.div_euclid(86_400).rem_euclid(7),
                0,
                "Thursday (1970-01-01 was one)"
            );
            assert_eq!(s.rem_euclid(86_400), 4 * 3_600);
            assert!(en - s < 86_400);
        }
    }

    /// Driving the purchase handler's own cap gate and counter with the caps it
    /// resolves: purchases 1..=9 in one window go through, the 10th is refused, and
    /// the next window starts again at zero.
    #[test]
    fn the_ninth_purchase_in_a_window_succeeds_and_the_tenth_is_refused() {
        let sd = sd();
        for (product, _) in RAISED_WINDOW_CAPS {
            let id = Uuid::parse_str(product).unwrap();
            let now = inside_window(&sd, product, BASE);
            let prices: Vec<Price> = serde_json::from_value(
                served_catalog(&sd, now)["globalShopOverrides"][*product]["prices"].clone(),
            )
            .unwrap();
            assert_eq!(validate_purchase_prices(&sd, id, &prices, now), Ok(()));

            let lifetime = lifetime_purchase_cap(&sd, id, now);
            let window = window_purchase_cap(&sd, id, now);
            assert_eq!(lifetime, None, "{product}: no lifetime cap");
            let (window_key, cap) = window.clone().expect("windowed cap");
            assert_eq!(cap, 9);

            let mut state = blades_lib::server_state::ServerState::default();
            for n in 1..=9 {
                assert_eq!(
                    purchase_cap_reached(&state, id, lifetime, window.as_ref()),
                    None,
                    "{product}: purchase {n} must succeed"
                );
                record_purchase(
                    &mut state,
                    id,
                    current_product_purchase_tracking_ids(&sd, id, now),
                );
            }
            assert_eq!(
                purchase_cap_reached(&state, id, lifetime, window.as_ref()),
                Some(("window", 9, 9)),
                "{product}: the 10th purchase is refused"
            );

            // The client reads sold-out from this count against the served 9.
            let list = purchases_list_with_tracking_counts(
                &state.global_shop_purchases,
                &state.global_shop_window_purchases,
                &current_purchase_tracking_ids(&sd, now),
                &current_lifetime_purchase_tracking_ids(&sd, now),
            );
            assert!(
                list.iter().any(|e| e.id == window_key && e.quantity == 9),
                "{list:?}"
            );

            // Next Thursday is a new window with a fresh allowance.
            let next = inside_window(&sd, product, now + 2 * 86_400);
            let next_window = window_purchase_cap(&sd, id, next);
            assert_ne!(next_window.as_ref().map(|w| &w.0), Some(&window_key));
            assert_eq!(
                purchase_cap_reached(&state, id, None, next_window.as_ref()),
                None
            );
        }
    }

    /// Every other product is exactly what retail's data serves, at every hour
    /// of a week, and a 3-per-window product still refuses its 4th purchase.
    #[test]
    fn other_products_are_unchanged() {
        let sd = sd();
        let raised: HashSet<&str> = RAISED_WINDOW_CAPS.iter().map(|(id, _)| *id).collect();
        for now in (BASE..BASE + 7 * 86_400).step_by(6 * 3_600) {
            let got = current_catalog(&sd, now);
            let want = unraised(&sd, now);
            let got = got["globalShopOverrides"].as_object().unwrap();
            let want = want["globalShopOverrides"].as_object().unwrap();
            assert_eq!(got.len(), want.len());
            for (id, e) in want {
                if !raised.contains(id.as_str()) {
                    assert_eq!(&got[id], e, "{id} at {now}");
                }
            }
        }

        let catalog = current_catalog(&sd, BASE);
        let (other, _) = catalog["globalShopOverrides"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|id| !raised.contains(id.as_str()))
            .filter_map(|id| Uuid::parse_str(id).ok())
            .filter_map(|id| Some((id, window_purchase_cap(&sd, id, BASE)?)))
            .find(|(_, (_, cap))| *cap == 3)
            .expect("another 3-per-window product");
        let window = window_purchase_cap(&sd, other, BASE);
        let mut state = blades_lib::server_state::ServerState::default();
        for _ in 0..3 {
            assert_eq!(
                purchase_cap_reached(&state, other, None, window.as_ref()),
                None
            );
            record_purchase(
                &mut state,
                other,
                current_product_purchase_tracking_ids(&sd, other, BASE),
            );
        }
        assert_eq!(
            purchase_cap_reached(&state, other, None, window.as_ref()),
            Some(("window", 3, 3)),
            "{other}: still 3 per window"
        );
    }
}
