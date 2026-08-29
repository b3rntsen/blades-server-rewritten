//! Town read + building lifecycle.
//!
//! `GET  /…/characters/{id}/towns/current`                                   — read
//! `POST /…/characters/{id}/towns/current/buildings`                         — place
//! `POST /…/characters/{id}/towns/current/buildings/{bid}/upgrade`           — upgrade
//! `POST /…/characters/{id}/towns/current/buildings/{bid}/complete`          — finish
//! `POST /…/characters/{id}/towns/current/buildings/{bid}/destroy`           — remove
//!
//! In Elder Scrolls: Blades the player rebuilds the town by placing and then
//! upgrading buildings (Smithy/Forge, Alchemy, etc.). Each upgrade costs GOLD +
//! town-resource MATERIALS (Lumber/Limestone/Clay/…) and takes real time; when the
//! timer expires (or the player pays gems to speed it up) the building "completes"
//! to its new level. The faithful cost/time tables were extracted from the APK
//! bundles + prod captures into `building_upgrades.json` and loaded into
//! `app_state.building_upgrades`.
//!
//! The town is stored verbatim as the `town` JSONB column (see [`CharacterDbEntryTown`]).
//! Buildings live under `town.districts[].segments{<segId>}.buildings{<bid>}` as
//! `{id, typeId, styleId, segmentGroupId, level, startIndex, constructionEnd,
//! customized, state}`. We mutate that JSON in place (read-modify-write, mirroring
//! `repair.rs`) and debit the wallet (gold/gems) + backpack stackables (materials).
//!
//! Every failure path is a clean [`BladeApiError`] (400/404/409) — a panic would drop
//! the connection and surface to the player as the reported "network error upgrading
//! smithy".

use std::sync::Arc;

use actix_web::{
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{GEMS, GOLD};
use blades_lib::user_data::{CompleteInventory, CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{fs::File, io::AsyncReadExt};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    json_db::JsonDbWrapper,
    models::{CharacterDbEntryTown, CharacterDbEntryTownEconomy},
    session::SessionLookedUpMaybe,
    util::get_only_single_character_and_check_permission,
};

/// Out-of-band service id for town-lifecycle error envelopes (not a real Blades
/// service id; the client pre-checks affordability/level so these rarely fire).
const TOWN_SERVICE_ID: u64 = 9005;

/// Service ID for props-related errors
const PROPS_SERVICE_ID: u64 = 9006;

/// Wall-clock milliseconds since the unix epoch. This is a real server — no
/// determinism requirement — so `SystemTime` is fine (a pre-1970 clock, which can't
/// happen, would saturate to 0 rather than panic).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Serialize)]
struct GetTownResponse {
    town: Value,
}

#[get("/api/game/v1/public/characters/{character_id}/towns/current")]
pub async fn get_town(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetTownResponse>, BladeApiError> {
    let character_id = path.into_inner();

    // Best-effort personalization: serve the requesting character's OWN captured
    // town when we have one. Any miss (no session, character not found, not owned,
    // or no stored town) falls through to the static default — serving the town
    // must never regress the menu/town load into an error.
    if let Some(town) = load_personal_town(&session, &app_state, character_id).await {
        return Ok(Json(GetTownResponse { town }));
    }

    // Fallback: the static default town (previously the ONLY behaviour). This used
    // to unwrap() every step, so a missing/invalid default_town.json PANICKED the
    // actix worker and the client saw a dropped connection ("Communication/Network
    // error"). Handle each failure as a 500 so the worker survives and logs why.
    let path = app_state.static_data_path.join("default_town.json");
    let mut file = File::open(&path).await.map_err(|e| {
        eprintln!("[town] cannot open {path:?}: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    let mut content = String::new();
    file.read_to_string(&mut content).await.map_err(|e| {
        eprintln!("[town] cannot read {path:?}: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    let town = serde_json::from_str(&content).map_err(|e| {
        eprintln!("[town] invalid json in {path:?}: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    Ok(Json(GetTownResponse { town }))
}

/// Look up the character's stored, ownership-checked town. Returns `None` on any
/// miss (no session / not found / not owned / null town / db error) so the caller
/// falls back to the default town.
async fn load_personal_town(
    session: &SessionLookedUpMaybe,
    app_state: &ServerGlobal,
    character_id: Uuid,
) -> Option<Value> {
    let session = session.get_session_or_error().ok()?;
    let mut conn = app_state.db_pool.get().await.ok()?;
    let rows = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(id.eq(character_id))
            .select(CharacterDbEntryTown::as_select())
            .load(&mut conn)
            .await
            .ok()?
    };
    let entry = get_only_single_character_and_check_permission(rows, &session.session).ok()?;
    match entry.town {
        Some(JsonDbWrapper(v)) if !v.is_null() => Some(v),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Cost model (parsed out of the raw `building_upgrades.json` `Value`).
// ─────────────────────────────────────────────────────────────────────────────

/// The faithful cost to build/upgrade one building to a specific level, resolved
/// from `app_state.building_upgrades`.
#[derive(Debug, Clone, PartialEq)]
struct LevelCost {
    gold: u64,
    construction_time_ms: u64,
    /// `(materialTemplateId, quantity)` — base build inputs plus the chosen style's
    /// extra inputs, already merged.
    materials: Vec<(Uuid, u64)>,
    require_town_level: u64,
    max_level: u64,
}

/// Errors from resolving/validating a building operation against the cost table.
#[derive(Debug, PartialEq)]
enum CostError {
    /// No entry for this building `typeId` in the cost table.
    UnknownBuilding,
    /// No cost row for the requested level (e.g. already at maxLevel, or a level the
    /// APK data doesn't cover).
    NoSuchLevel,
    /// Requested level exceeds the building's `maxLevel`.
    AtMaxLevel,
    /// The town isn't high enough level for this building level yet.
    TownLevelTooLow { need: u64, have: u64 },
}

impl CostError {
    fn to_api(&self) -> BladeApiError {
        match self {
            // 404: we simply don't know this building — treat as not-found rather
            // than a client error the player could "fix".
            CostError::UnknownBuilding | CostError::NoSuchLevel => {
                BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1)
            }
            // 409: the request conflicts with the building's current state.
            CostError::AtMaxLevel => {
                BladeApiError::new(StatusCode::CONFLICT, TOWN_SERVICE_ID, 2)
            }
            CostError::TownLevelTooLow { .. } => {
                BladeApiError::new(StatusCode::CONFLICT, TOWN_SERVICE_ID, 3)
            }
        }
    }
}

/// Look up the cost to bring `type_id` to `target_level` (the "level built toward":
/// `0` = initial placement on an empty lot, `N` = upgrade result). `style_id` picks
/// the per-style extra `styleInputs`; a style not present in the table contributes
/// no extra materials (the building's stored style is often the plain default that
/// carries no style inputs — charge base only rather than fail).
fn lookup_level_cost(
    building_upgrades: &Value,
    type_id: Uuid,
    target_level: u64,
    style_id: Option<Uuid>,
) -> Result<LevelCost, CostError> {
    let building = building_upgrades
        .get("buildings")
        .and_then(|b| b.get(type_id.to_string()))
        .ok_or(CostError::UnknownBuilding)?;

    let max_level = building
        .get("maxLevel")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if target_level > max_level {
        return Err(CostError::AtMaxLevel);
    }

    let level = building
        .get("levels")
        .and_then(|l| l.get(target_level.to_string()))
        .ok_or(CostError::NoSuchLevel)?;

    let gold = level.get("goldCost").and_then(Value::as_u64).unwrap_or(0);
    let construction_time_ms = level
        .get("constructionTimeMs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let require_town_level = level
        .get("requireTownLevel")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut materials: std::collections::HashMap<Uuid, u64> = std::collections::HashMap::new();
    if let Some(inputs) = level.get("buildInputs").and_then(Value::as_object) {
        for (k, v) in inputs {
            if let (Ok(id), Some(q)) = (Uuid::parse_str(k), v.as_u64()) {
                *materials.entry(id).or_insert(0) += q;
            }
        }
    }
    // Per-style extra inputs. Only the chosen style's row is charged; an unknown /
    // absent style just adds nothing.
    if let (Some(style), Some(style_inputs)) =
        (style_id, level.get("styleInputs").and_then(Value::as_object))
    {
        if let Some(row) = style_inputs.get(&style.to_string()).and_then(Value::as_object) {
            for (k, v) in row {
                if let (Ok(id), Some(q)) = (Uuid::parse_str(k), v.as_u64()) {
                    *materials.entry(id).or_insert(0) += q;
                }
            }
        }
    }

    Ok(LevelCost {
        gold,
        construction_time_ms,
        materials: materials.into_iter().collect(),
        require_town_level,
        max_level,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Town JSON navigation (buildings live in nested districts/segments).
// ─────────────────────────────────────────────────────────────────────────────

/// The town's current level, from `town.levelInfo.level` (0 if absent).
fn town_level(town: &Value) -> u64 {
    town.get("levelInfo")
        .and_then(|l| l.get("level"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Mutably borrow a building object by its `id`, searching every district/segment.
fn find_building_mut<'a>(town: &'a mut Value, building_id: Uuid) -> Option<&'a mut Value> {
    let bid = building_id.to_string();
    let districts = town.get_mut("districts")?.as_array_mut()?;
    for district in districts {
        let segments = match district.get_mut("segments").and_then(Value::as_object_mut) {
            Some(s) => s,
            None => continue,
        };
        for (_seg_id, seg) in segments.iter_mut() {
            if let Some(buildings) = seg.get_mut("buildings").and_then(Value::as_object_mut) {
                if buildings.contains_key(&bid) {
                    return buildings.get_mut(&bid);
                }
            }
        }
    }
    None
}

/// Immutably borrow a building object by its `id` — the read-only twin of
/// [`find_building_mut`], for pre-scans that must not tangle with the `&mut`.
fn find_building<'a>(town: &'a Value, building_id: Uuid) -> Option<&'a Value> {
    let bid = building_id.to_string();
    let districts = town.get("districts")?.as_array()?;
    for district in districts {
        let segments = match district.get("segments").and_then(Value::as_object) {
            Some(s) => s,
            None => continue,
        };
        for seg in segments.values() {
            if let Some(b) = seg
                .get("buildings")
                .and_then(Value::as_object)
                .and_then(|m| m.get(&bid))
            {
                return Some(b);
            }
        }
    }
    None
}

/// The `typeId`/`styleId`/`level` of a building (read-only pre-scan before the
/// mutable borrow, so cost lookup doesn't tangle with the `&mut` on the town).
fn read_building_facts(town: &Value, building_id: Uuid) -> Option<(Uuid, Option<Uuid>, u64)> {
    let b = find_building(town, building_id)?;
    let type_id = b
        .get("typeId")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())?;
    let style_id = b
        .get("styleId")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok());
    let level = b.get("level").and_then(Value::as_u64).unwrap_or(0);
    Some((type_id, style_id, level))
}

/// How much of a building's construction timer is still to run, in milliseconds, as
/// of `now`. `constructionEnd` is stored as epoch MILLISECONDS (see
/// [`apply_upgrade_transition`]); an absent, zero or already-past value gives `0`.
///
/// This is the ONLY input to the speed-up price. The client sends `speedUp: true`
/// and nothing else — never a cost — so the price is derived server-side from stored
/// state and a client cannot name its own.
fn remaining_construction_ms(town: &Value, building_id: Uuid, now: u64) -> i64 {
    let end = find_building(town, building_id)
        .and_then(|b| b.get("constructionEnd"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (end as i64) - (now as i64)
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared debit: charge a `LevelCost` against the wallet + backpack.
// ─────────────────────────────────────────────────────────────────────────────

/// Charge one building operation's cost. Two-phase (verify-then-debit) so a
/// partially-affordable cost never leaves the wallet/backpack half-charged. When
/// `gems_payment`, the gold portion is billed in GEMS instead (materials are still
/// consumed — retail gems buy the timer, not the raw materials, but the reported
/// flow only sends `gemsPayment` to swap the currency; we keep materials real).
fn charge_cost(
    cost: &LevelCost,
    gems_payment: bool,
    wallet: &mut CompleteWallet,
    inventory: &mut blades_lib::user_data::CompleteInventory,
    tracker: &mut InventoryChangeTracker,
) -> Result<(), BladeApiError> {
    let currency = if gems_payment { GEMS } else { GOLD };

    // Phase 1: verify affordability (no mutation) so we fail cleanly.
    if wallet.balance(currency) < cost.gold {
        return Err(BladeApiError::new(
            StatusCode::BAD_REQUEST,
            TOWN_SERVICE_ID,
            4,
        ));
    }
    for (mat, qty) in &cost.materials {
        if inventory.backpack.stackable_items.count(*mat) < *qty {
            return Err(BladeApiError::new(
                StatusCode::BAD_REQUEST,
                TOWN_SERVICE_ID,
                5,
            ));
        }
    }

    // Phase 2: commit. Every step was checked above, so these cannot fail.
    wallet
        .debit(currency, cost.gold)
        .map_err(BladeApiError::from_economy)?;
    for (mat, qty) in &cost.materials {
        inventory
            .backpack
            .stackable_items
            .remove(*mat, *qty)
            .map_err(|_have| BladeApiError::new(StatusCode::BAD_REQUEST, TOWN_SERVICE_ID, 5))?;
        tracker.modified_backpack.stackable_items.insert(*mat);
    }
    if !cost.materials.is_empty() {
        inventory.backpack_version += 1;
    }
    Ok(())
}

/// The common `{wallet, inventory, town, validationFlags}` response the client
/// consumes after a build/upgrade/destroy.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TownMutationResponse {
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
    town: Value,
    validation_flags: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. UPGRADE — the reported "network error upgrading smithy".
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct UpgradeRequest {
    #[serde(default)]
    gems_payment: bool,
}

#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/upgrade"
)]
pub async fn upgrade_building(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<UpgradeRequest>,
) -> Result<Json<TownMutationResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, building_id) = path.into_inner();
    let gems_payment = body.into_inner().gems_payment;
    let globals: Arc<ServerGlobal> = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            // Resolve the building's current facts, then the cost of the NEXT level.
            let (type_id, style_id, cur_level) = read_building_facts(&town, building_id)
                .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1))?;
            let target_level = cur_level + 1;
            let cost = lookup_level_cost(
                &globals.building_upgrades,
                type_id,
                target_level,
                style_id,
            )
            .map_err(|e| e.to_api())?;

            // Town-level gate.
            let tl = town_level(&town);
            if tl < cost.require_town_level {
                return Err(CostError::TownLevelTooLow {
                    need: cost.require_town_level,
                    have: tl,
                }
                .to_api());
            }

            // Charge (gold/gems + materials); fails cleanly on insufficient funds.
            let mut tracker = InventoryChangeTracker::default();
            charge_cost(
                &cost,
                gems_payment,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut tracker,
            )?;

            // Apply the pending upgrade: the building enters UPGRADING with the timer
            // running and the target level set (retail shows the new level while under
            // construction; `complete` just clears the timer/state).
            let building = find_building_mut(&mut town, building_id).ok_or_else(|| {
                BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1)
            })?;
            apply_upgrade_transition(building, target_level, cost.construction_time_ms, now_ms());

            finish_town_mutation(&mut conn, entry, town, &tracker).await
        }
        .scope_boxed()
    })
    .await
}

/// Mutate a building object into the "UPGRADING to `target_level`" state with the
/// construction timer set. Pure JSON edit so it's unit-testable without a db.
fn apply_upgrade_transition(
    building: &mut Value,
    target_level: u64,
    construction_time_ms: u64,
    now: u64,
) {
    if let Some(obj) = building.as_object_mut() {
        obj.insert("state".to_string(), json!("UPGRADING"));
        obj.insert("level".to_string(), json!(target_level));
        obj.insert(
            "constructionEnd".to_string(),
            json!(now + construction_time_ms),
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. COMPLETE — finalize an in-progress upgrade/construction.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct CompleteRequest {
    /// `true` when the player paid gems to finish instantly (the timer hadn't
    /// elapsed). This is BILLED — see [`blades_lib::economy::skip_time`] for the
    /// curve and its provenance. The flag is the only thing the client sends; the
    /// price comes from the stored `constructionEnd`, never from the request.
    #[serde(default)]
    speed_up: bool,
}

/// `complete`'s response.
///
/// Retail's shape depends on the flag, measured across 159 captured completions:
///
/// ```text
/// speedUp=false → { "town" }
/// speedUp=true  → { "character", "inventory", "town", "wallet" }   // post-deduction
/// ```
///
/// The wallet on the speed-up path is REQUIRED: it is how the client learns the gems
/// left it. The other three keys are `Option` so the no-speed-up response is the
/// bare `{town}` retail sends.
///
/// We used to send six keys unconditionally, including a `shop` that retail sends on
/// this endpoint 0 times out of 159. That was not merely noise — a `shop` key here
/// hands the client an EMPTY stock list for the building it just finished, which is
/// the shape of a vendor that has nothing to sell. It is dropped, along with the
/// `validationFlags` retail also does not send here (the town object carries its own
/// `validationFlags` field, so nothing is lost).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteResponse {
    /// Always sent — the completed building's new state.
    town: Value,
    /// Post-deduction balance. Speed-up only; the client reads the gem debit here.
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
    /// Inventory diff. Speed-up only (empty in practice — gems live in the wallet —
    /// but retail sends the key and the client's parser expects the quartet).
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<CompleteInventoryUpdate>,
    /// Full character JSONB (verbatim) so the client re-reads town xp/level etc.
    /// Speed-up only.
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<Value>,
}

#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/complete"
)]
pub async fn complete_building(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<CompleteRequest>,
) -> Result<Json<CompleteResponse>, BladeApiError> {
    
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, building_id) = path.into_inner();
    let speed_up = body.into_inner().speed_up;
    let globals: Arc<ServerGlobal> = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            // A 404 before anything is charged: an unknown building must never cost
            // gems. (`find_building` is the read-only pre-scan; the mutation borrows
            // separately below.)
            if find_building(&town, building_id).is_none() {
                return Err(BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1));
            }

            // Bill the speed-up BEFORE clearing the timer — the price is a function
            // of the time that is still to run, so the order matters. Insufficient
            // gems fails the whole request (the transaction rolls back and the
            // building stays under construction) rather than completing for free.
            charge_construction_speed_up(
                speed_up,
                globals.skip_time_costs.as_ref(),
                &town,
                building_id,
                now_ms(),
                &mut entry.wallet.0,
            )?;

            let building = find_building_mut(&mut town, building_id).ok_or_else(|| {
                BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1)
            })?;

            apply_complete_transition(building);

            // No inventory mutation on complete → empty diff, no version bump.
            let tracker = InventoryChangeTracker::default();
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let character = entry.character.0.clone();

            {
                use crate::schema::characters;
                let town_col = town.clone();
                let mut changeset = entry;
                changeset.town = Some(JsonDbWrapper(town_col.clone()));
                diesel::update(characters::table)
                    .filter(characters::id.eq(character_id))
                    .set(changeset)
                    .execute(&mut conn)
                    .await?;

                Ok::<_, BladeApiError>(Json(complete_response(
                    speed_up,
                    town_col,
                    wallet,
                    inventory,
                    serde_json::to_value(&character).unwrap_or_else(|_| json!(null)),
                )))
            }
        }
        .scope_boxed()
    })
    .await
}

/// Bill a `/complete` speed-up against the wallet. The whole billed path minus the
/// database, so tests exercise exactly what the handler runs.
///
/// * `speed_up == false` → nothing is charged, full stop.
/// * no table (static data not pushed yet) → nothing is charged; see
///   [`blades_lib::economy::skip_time::SkipTimeCostTable::from_static`].
/// * timer already elapsed → nothing is charged; the player is not paying to skip
///   time that has already passed.
/// * not enough gems → `400` (the same envelope [`charge_cost`] uses for "you cannot
///   afford this") and the wallet is untouched. It must FAIL: silently skipping the
///   charge would hand out free speed-ups, which is the bug being fixed.
fn charge_construction_speed_up(
    speed_up: bool,
    table: Option<&blades_lib::economy::skip_time::SkipTimeCostTable>,
    town: &Value,
    building_id: Uuid,
    now: u64,
    wallet: &mut CompleteWallet,
) -> Result<Vec<blades_lib::economy::Price>, BladeApiError> {
    if !speed_up {
        return Ok(Vec::new());
    }
    let remaining_ms = remaining_construction_ms(town, building_id, now);
    blades_lib::economy::skip_time::charge_skip_time(table, remaining_ms, wallet)
        .map_err(|_| BladeApiError::new(StatusCode::BAD_REQUEST, TOWN_SERVICE_ID, 4))
}

/// Assemble `/complete`'s response for the retail shape (see [`CompleteResponse`]):
/// `{town}` on a plain completion, `{character, inventory, town, wallet}` when gems
/// were spent. Split out so the shape is unit-testable without a database.
fn complete_response(
    speed_up: bool,
    town: Value,
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
    character: Value,
) -> CompleteResponse {
    if speed_up {
        CompleteResponse {
            town,
            wallet: Some(wallet),
            inventory: Some(inventory),
            character: Some(character),
        }
    } else {
        CompleteResponse {
            town,
            wallet: None,
            inventory: None,
            character: None,
        }
    }
}

/// Finalize a building: clear the timer, back to NORMAL. The `level` was already
/// bumped at `upgrade`/`place` time (retail carries the target level through the
/// UPGRADING state), so completion is purely a state/timer clear.
fn apply_complete_transition(building: &mut Value) {
    if let Some(obj) = building.as_object_mut() {
        obj.insert("state".to_string(), json!("NORMAL"));
        obj.insert("constructionEnd".to_string(), json!(0));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. PLACE — build a new (level-0) building on a segment lot.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PlaceRequest {
    building_type: Uuid,
    style_id: Uuid,
    segment_group_id: Uuid,
    #[serde(default)]
    start_index: u64,
    #[serde(default)]
    #[allow(dead_code)]
    npc_index: Option<u64>,
    #[serde(default)]
    gems_payment: bool,
}

#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/buildings"
)]
pub async fn place_building(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<PlaceRequest>,
) -> Result<Json<TownMutationResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let req = body.into_inner();
    let globals: Arc<ServerGlobal> = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            // Placement is the level-0 build (initial construction on an empty lot).
            let cost = lookup_level_cost(
                &globals.building_upgrades,
                req.building_type,
                0,
                Some(req.style_id),
            )
            .map_err(|e| e.to_api())?;

            let tl = town_level(&town);
            if tl < cost.require_town_level {
                return Err(CostError::TownLevelTooLow {
                    need: cost.require_town_level,
                    have: tl,
                }
                .to_api());
            }

            let mut tracker = InventoryChangeTracker::default();
            charge_cost(
                &cost,
                req.gems_payment,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut tracker,
            )?;

            let new_building_id = Uuid::new_v4();
            insert_building(
                &mut town,
                req.segment_group_id,
                new_building_id,
                req.building_type,
                req.style_id,
                req.start_index,
                cost.construction_time_ms,
                now_ms(),
            )
            .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 6))?;

            finish_town_mutation(&mut conn, entry, town, &tracker).await
        }
        .scope_boxed()
    })
    .await
}

/// Insert a freshly-placed level-0 building into the segment identified by
/// `segment_group_id`. Returns `None` if that segment isn't in the town (unknown lot).
#[allow(clippy::too_many_arguments)]
fn insert_building(
    town: &mut Value,
    segment_group_id: Uuid,
    building_id: Uuid,
    type_id: Uuid,
    style_id: Uuid,
    start_index: u64,
    construction_time_ms: u64,
    now: u64,
) -> Option<()> {
    let seg_key = segment_group_id.to_string();
    let districts = town.get_mut("districts")?.as_array_mut()?;
    for district in districts {
        let segments = match district.get_mut("segments").and_then(Value::as_object_mut) {
            Some(s) => s,
            None => continue,
        };
        if let Some(seg) = segments.get_mut(&seg_key) {
            let obj = seg.as_object_mut()?;
            let buildings = obj
                .entry("buildings".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()?;
            buildings.insert(
                building_id.to_string(),
                json!({
                    "id": building_id.to_string(),
                    "typeId": type_id.to_string(),
                    "styleId": style_id.to_string(),
                    "segmentGroupId": segment_group_id.to_string(),
                    "level": 0,
                    "startIndex": start_index,
                    "constructionEnd": now + construction_time_ms,
                    "customized": false,
                    "state": "UPGRADING",
                }),
            );
            return Some(());
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. DESTROY — remove a building; refund nothing (retail doesn't).
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct DestroyRequest {
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
}

#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/destroy"
)]
pub async fn destroy_building(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<DestroyRequest>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, building_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            if !remove_building(&mut town, building_id) {
                return Err(BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1));
            }

            let tracker = InventoryChangeTracker::default();
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();

            {
                use crate::schema::characters;
                let town_col = town.clone();
                let mut changeset = entry;
                changeset.town = Some(JsonDbWrapper(town_col.clone()));
                diesel::update(characters::table)
                    .filter(characters::id.eq(character_id))
                    .set(changeset)
                    .execute(&mut conn)
                    .await?;

                // `town` echoes the updated town with a `removedBuilding` marker so
                // the client drops the building from its view.
                let mut town_resp = town_col;
                if let Some(obj) = town_resp.as_object_mut() {
                    obj.insert("removedBuilding".to_string(), json!(building_id.to_string()));
                }
                Ok::<_, BladeApiError>(Json(json!({
                    "wallet": wallet,
                    "inventory": inventory,
                    "town": town_resp,
                    "validationFlags": 1,
                })))
            }
        }
        .scope_boxed()
    })
    .await
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. STYLE — the reported "stuck loading, can't leave" on a town building.
// ─────────────────────────────────────────────────────────────────────────────

/// Apply a cosmetic STYLE to a town building.
///
/// Reported as: upgrading the city wall or crafting armour hangs on the loading
/// screen with no way out but restarting the game (tracker #75). The client was
/// POSTing here and getting a 404 — we served every neighbouring town route
/// (`buildings`, `/upgrade`, `/complete`, `/destroy`) but never this one, so the
/// client sat waiting for a response that never came. It bites on the cosmetic
/// step of a build/upgrade, which is why it looked like an upgrade bug.
///
/// Shape is capture-derived: 128 retail 200s exist for this route (922-5800 B).
/// A retail response echoes the post-transaction `{wallet, inventory, town}` with
/// the building's `styleId` set and `customized: true` — e.g. building
/// `af0a05c7…` coming back with `"styleId":"aa133662…","customized":true`.
///
/// No cost is charged. Retail's captured responses return the wallet UNCHANGED
/// across the call, so whatever a restyle costs is not settled here; inventing a
/// price would be worse than charging nothing, and the client is authoritative
/// for what it offers the player.
#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/styles/{style_id}"
)]
pub async fn set_building_style(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid, Uuid)>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, building_id, style_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            // 404 on an unknown building — same as `destroy`. Silently succeeding
            // would tell the client a style was applied that the town does not
            // carry, and it would snap back on the next load.
            if !apply_building_style(&mut town, building_id, style_id) {
                return Err(BladeApiError::new(StatusCode::NOT_FOUND, TOWN_SERVICE_ID, 1));
            }

            let tracker = InventoryChangeTracker::default();
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();

            {
                use crate::schema::characters;
                let town_col = town.clone();
                let mut changeset = entry;
                changeset.town = Some(JsonDbWrapper(town_col.clone()));
                diesel::update(characters::table)
                    .filter(characters::id.eq(character_id))
                    .set(changeset)
                    .execute(&mut conn)
                    .await?;

                Ok::<_, BladeApiError>(Json(json!({
                    "wallet": wallet,
                    "inventory": inventory,
                    "town": town_col,
                    "validationFlags": 1,
                })))
            }
        }
        .scope_boxed()
    })
    .await
}

/// Set a building's cosmetic `styleId` (and mark it `customized`). `true` if the
/// building was found.
///
/// Split out of the handler for the same reason `remove_building` is: the town
/// JSON walk is where the bugs live, and it is the only part reachable from a test
/// without a database.
fn apply_building_style(town: &mut Value, building_id: Uuid, style_id: Uuid) -> bool {
    let Some(building) = find_building_mut(town, building_id) else {
        return false;
    };
    let Some(obj) = building.as_object_mut() else {
        return false;
    };
    obj.insert("styleId".to_string(), json!(style_id.to_string()));
    // Retail sets this alongside the style; the client reads it to know the
    // building is no longer showing its default appearance.
    obj.insert("customized".to_string(), json!(true));
    true
}

/// Remove a building by id from whichever segment holds it. `true` if removed.
fn remove_building(town: &mut Value, building_id: Uuid) -> bool {
    let bid = building_id.to_string();
    let districts = match town.get_mut("districts").and_then(Value::as_array_mut) {
        Some(d) => d,
        None => return false,
    };
    for district in districts {
        let segments = match district.get_mut("segments").and_then(Value::as_object_mut) {
            Some(s) => s,
            None => continue,
        };
        for seg in segments.values_mut() {
            if let Some(buildings) = seg.get_mut("buildings").and_then(Value::as_object_mut) {
                if buildings.remove(&bid).is_some() {
                    return true;
                }
            }
        }
    }
    false
}

// ─────────────────────────────────────────────────────────────────────────────
// Prop related Endpoints.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedProp {
    pub prop_id: Uuid,
    pub district_id: Uuid,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RemovePropsRequest {
    pub deleted_props: Vec<DeletedProp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovePropsResponse {
    pub wallet: CompleteWallet,
    pub inventory: CompleteInventoryUpdate,
    pub town: Value,
    pub validation_flags: u64,
    pub removed_count: usize,
    pub failed_props: Vec<String>,
    pub removed_props: Vec<String>,
    pub placed_props: Vec<String>,
    pub success: bool,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacedProp {
    pub anchor_id: Uuid,
    pub decoration_id: Uuid,
    pub district_id: Uuid,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacePropsRequest {
    pub placed_props: Vec<PlacedProp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacePropsResponse {
    pub wallet: CompleteWallet,
    pub inventory: CompleteInventoryUpdate,
    pub town: Value,
    pub validation_flags: u64,
    pub placed_count: usize,
    pub failed_props: Vec<String>,
    pub placed_props: Vec<String>,
}

/// POST /api/game/v1/public/characters/{character_id}/towns/current/props/remove
///
/// Remove one or more props from the character's current town.
/// Props are located in districts under `town.districts[].props`.
///
/// The client sends a list of {propId, districtId} pairs to remove.
/// Returns updated wallet, inventory, and town state.
#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/props/remove"
)]
pub async fn remove_town_props(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<RemovePropsRequest>,
) -> Result<Json<RemovePropsResponse>, BladeApiError> {
    use crate::schema::characters;
    use diesel::update;
    
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let req = body.into_inner();
    let mut conn = app_state.db_pool.get().await?;

    if req.deleted_props.is_empty() {
        return Err(BladeApiError::new(
            StatusCode::BAD_REQUEST,
            PROPS_SERVICE_ID,
            1,
        ));
    }

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            // Process prop removals and get decoration IDs removed
            let (removed_count, failed_props, removed_decoration_ids, removed_prop_ids) = 
                remove_props_from_town(&mut town, &req.deleted_props);

            let mut tracker = InventoryChangeTracker::default();
            
            // RETURN decorations to inventory (add them back)
            for decoration_id_str in &removed_decoration_ids {
                if let Ok(decoration_uuid) = Uuid::parse_str(decoration_id_str) {
                    entry
                        .inventory
                        .0
                        .backpack
                        .stackable_items
                        .add(decoration_uuid, 1);
                    tracker.modified_backpack.stackable_items.insert(decoration_uuid);
                }
            }
            
            if !removed_decoration_ids.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();

            // Add a removedProps marker to the town response (like removedBuilding for buildings)
            let mut town_col = town.clone();
            if let Some(obj) = town_col.as_object_mut() {
                // The client expects the prop IDs that were removed
                obj.insert("removedProps".to_string(), json!(removed_prop_ids));
            }

            let mut changeset = entry;
            changeset.town = Some(JsonDbWrapper(town.clone()));
            update(characters::table)
                .filter(characters::id.eq(character_id))
                .set(changeset)
                .execute(&mut conn)
                .await?;

            // The client needs the prop IDs that were removed
            let client_prop_ids: Vec<String> = req.deleted_props
                .iter()
                .map(|p| p.prop_id.to_string())
                .collect();

            Ok(Json(RemovePropsResponse {
                wallet,
                inventory,
                town: town_col,
                validation_flags: 1,
                removed_count,
                failed_props,
                removed_props: client_prop_ids,
                placed_props: vec![],
                success: removed_count > 0,
            }))
        }
        .scope_boxed()
    })
    .await
}

/// Remove props from the town JSON structure and track what was removed.
/// 
/// Returns (removed_count, failed_prop_ids, removed_decoration_ids, removed_prop_ids)
fn remove_props_from_town(
    town: &mut Value,
    props_to_remove: &[DeletedProp],
) -> (usize, Vec<String>, Vec<String>, Vec<String>) {
    let mut removed_count = 0;
    let mut failed_props = Vec::new();
    let mut removed_decoration_ids = Vec::new();
    let mut removed_prop_ids = Vec::new(); // Track the actual prop IDs (id field) that were removed

    let districts = match town.get_mut("districts").and_then(Value::as_array_mut) {
        Some(d) => d,
        None => return (0, props_to_remove.iter().map(|p| p.prop_id.to_string()).collect(), vec![], vec![]),
    };

    for deleted_prop in props_to_remove {
        let target_prop_id = deleted_prop.prop_id.to_string();
        let target_district_id = deleted_prop.district_id.to_string();
        let mut found = false;

        for district in districts.iter_mut() {
            let district_id = match district.get("id").and_then(Value::as_str) {
                Some(id) => id,
                None => continue,
            };

            if district_id != target_district_id {
                continue;
            }

            let props = match district.get_mut("props").and_then(Value::as_object_mut) {
                Some(p) => p,
                None => continue,
            };

            let prop_key_to_remove = props.iter().find_map(|(key, prop_obj)| {
                let matches_id = prop_obj
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|&id| id == target_prop_id)
                    .is_some();
                
                let matches_prop_id = prop_obj
                    .get("propId")
                    .and_then(Value::as_str)
                    .filter(|&prop_id| prop_id == target_prop_id)
                    .is_some();
                
                if matches_id || matches_prop_id {
                    Some(key.clone())
                } else {
                    None
                }
            });

            if let Some(key) = prop_key_to_remove {
                // Get the prop data before removing
                if let Some(prop_obj) = props.get(&key) {
                    // Track the decoration_id (propId field)
                    if let Some(decoration_id) = prop_obj
                        .get("propId")
                        .and_then(Value::as_str)
                    {
                        removed_decoration_ids.push(decoration_id.to_string());
                    }
                    // Track the actual prop ID (id field) - this is what the client sent
                    if let Some(prop_id) = prop_obj
                        .get("id")
                        .and_then(Value::as_str)
                    {
                        removed_prop_ids.push(prop_id.to_string());
                    }
                }
                
                props.remove(&key);
                removed_count += 1;
                found = true;
                break;
            }
        }

        if !found {
            failed_props.push(target_prop_id);
        }
    }

    (removed_count, failed_props, removed_decoration_ids, removed_prop_ids)
}

/// Remove props from inventory after they're removed from the town.
fn remove_decoration_from_inventory(
    inventory: &mut CompleteInventory,
    decoration_id: Uuid,
    tracker: &mut InventoryChangeTracker,
) -> Result<(), BladeApiError> {
    // Remove 1 of the decoration item from stackable items
    // The decoration is stored as a stackable item in the backpack
    let count = inventory.backpack.stackable_items.count(decoration_id);
    if count > 0 {
        inventory
            .backpack
            .stackable_items
            .remove(decoration_id, 1)
            .map_err(|_| {
                BladeApiError::new(
                    StatusCode::BAD_REQUEST,
                    PROPS_SERVICE_ID,
                    3,
                )
            })?;
        tracker.modified_backpack.stackable_items.insert(decoration_id);
        inventory.backpack_version += 1;
        Ok(())
    } else {
        // The decoration might be in the treasury or elsewhere
        // For now, just log and continue
        log::warn!("Decoration {} not found in inventory", decoration_id);
        Ok(())
    }
}

#[post(
    "/api/game/v1/public/characters/{character_id}/towns/current/props"
)]
pub async fn place_town_props(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<PlacePropsRequest>,
) -> Result<Json<PlacePropsResponse>, BladeApiError> {
    use crate::schema::characters;
    use diesel::update;
    
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let req = body.into_inner();
    let mut conn = app_state.db_pool.get().await?;

    if req.placed_props.is_empty() {
        return Err(BladeApiError::new(
            StatusCode::BAD_REQUEST,
            PROPS_SERVICE_ID,
            2,
        ));
    }

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

            // Process prop placements and get decoration IDs placed
            let (placed_count, failed_props, placed_decoration_ids) = 
                place_props_in_town(&mut town, &req.placed_props);

            // REMOVE decorations from inventory (consuming the items)
            let mut tracker = InventoryChangeTracker::default();
            
            for decoration_id_str in &placed_decoration_ids {
                if let Ok(decoration_uuid) = Uuid::parse_str(decoration_id_str) {
                    remove_decoration_from_inventory(
                        &mut entry.inventory.0,
                        decoration_uuid,
                        &mut tracker,
                    )?;
                }
            }
            
            if !placed_decoration_ids.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();

            let town_col = town.clone();
            let mut changeset = entry;
            changeset.town = Some(JsonDbWrapper(town_col.clone()));
            update(characters::table)
                .filter(characters::id.eq(character_id))
                .set(changeset)
                .execute(&mut conn)
                .await?;

            Ok(Json(PlacePropsResponse {
                wallet,
                inventory,
                town: town_col,
                validation_flags: 1,
                placed_count,
                failed_props,
                placed_props: placed_decoration_ids, // Send back what was placed
            }))
        }
        .scope_boxed()
    })
    .await
}

/// Place props into the town JSON structure.
/// 
/// Returns (placed_count, failed_prop_anchor_ids_as_strings, placed_decoration_ids)
/// 
/// Props are stored in `town.districts[].props` as an object/dictionary.
fn place_props_in_town(
    town: &mut Value,
    props_to_place: &[PlacedProp],
) -> (usize, Vec<String>, Vec<String>) {
    let mut placed_count = 0;
    let mut failed_props = Vec::new();
    let mut placed_decoration_ids = Vec::new(); // Track decoration IDs placed

    let districts = match town.get_mut("districts").and_then(Value::as_array_mut) {
        Some(d) => d,
        None => return (0, props_to_place.iter().map(|p| p.anchor_id.to_string()).collect(), vec![]),
    };

    for placed_prop in props_to_place {
        let anchor_id = placed_prop.anchor_id;
        let decoration_id = placed_prop.decoration_id;
        let district_id = placed_prop.district_id;
        let anchor_id_str = anchor_id.to_string();
        let decoration_id_str = decoration_id.to_string();
        let mut found_district = false;

        for district in districts.iter_mut() {
            let district_id_field = match district.get("id").and_then(Value::as_str) {
                Some(id) => id,
                None => continue,
            };

            if district_id_field != district_id.to_string() {
                continue;
            }

            found_district = true;

            let props = match district.get_mut("props") {
                Some(Value::Object(obj)) => obj,
                _ => {
                    let new_props = serde_json::Map::new();
                    district.as_object_mut()
                        .expect("district must be an object")
                        .insert("props".to_string(), Value::Object(new_props));
                    
                    district.get_mut("props")
                        .and_then(Value::as_object_mut)
                        .expect("props should exist now")
                }
            };

            if props.contains_key(&anchor_id_str) {
                log::warn!("Prop with anchor_id {} already exists, overwriting", anchor_id_str);
            }

            let prop_id = Uuid::new_v4();
            let prop_type = "2a529107-9561-4d23-91a8-becfd7fe76fa";

            let prop_obj = json!({
                "anchorId": anchor_id_str,
                "id": prop_id.to_string(),
                "propId": decoration_id_str,
                "type": prop_type,
            });

            props.insert(anchor_id_str.clone(), prop_obj);
            placed_count += 1;
            
            // Track the decoration ID that was placed
            placed_decoration_ids.push(decoration_id_str);
            break;
        }

        if !found_district {
            failed_props.push(anchor_id_str);
        }
    }

    (placed_count, failed_props, placed_decoration_ids)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_town_with_props() -> Value {
        json!({
            "levelInfo": { "level": 6 },
            "districts": [
                {
                    "id": "9a12c0d3-218c-4ef2-b78c-b6e3bca60719",
                    "props": {
                        "key1": {
                            "id": "e915e0f4-3f86-41cb-b9e2-1ead10025c06",
                            "propId": "e915e0f4-3f86-41cb-b9e2-1ead10025c06",
                            "districtId": "9a12c0d3-218c-4ef2-b78c-b6e3bca60719",
                            "typeId": "some-prop-type",
                            "x": 10.0,
                            "y": 20.0,
                            "rotation": 45.0
                        },
                        "key2": {
                            "id": "f1234567-89ab-cdef-0123-456789abcdef",
                            "propId": "f1234567-89ab-cdef-0123-456789abcdef",
                            "districtId": "9a12c0d3-218c-4ef2-b78c-b6e3bca60719",
                            "typeId": "another-prop",
                            "x": 30.0,
                            "y": 40.0,
                            "rotation": 90.0
                        }
                    }
                },
                {
                    "id": "another-district-id",
                    "props": {
                        "key3": {
                            "id": "g9876543-21ab-cdef-0123-456789abcdef",
                            "propId": "g9876543-21ab-cdef-0123-456789abcdef",
                            "districtId": "another-district-id",
                            "typeId": "prop-in-other-district",
                            "x": 50.0,
                            "y": 60.0,
                            "rotation": 0.0
                        }
                    }
                }
            ]
        })
    }

    #[test]
    fn remove_props_removes_specified_props() {
        let mut town = sample_town_with_props();
        let prop_id = Uuid::parse_str("e915e0f4-3f86-41cb-b9e2-1ead10025c06").unwrap();
        let district_id = Uuid::parse_str("9a12c0d3-218c-4ef2-b78c-b6e3bca60719").unwrap();
        
        let props_to_remove = vec![
            DeletedProp { prop_id, district_id }
        ];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 1);
        assert!(failed.is_empty());
        
        // Verify the prop was removed
        let district = &town["districts"][0];
        let props = district["props"].as_array().unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(props[0]["id"], "f1234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn remove_props_handles_multiple_props() {
        let mut town = sample_town_with_props();
        let prop1 = Uuid::parse_str("e915e0f4-3f86-41cb-b9e2-1ead10025c06").unwrap();
        let prop2 = Uuid::parse_str("f1234567-89ab-cdef-0123-456789abcdef").unwrap();
        let district_id = Uuid::parse_str("9a12c0d3-218c-4ef2-b78c-b6e3bca60719").unwrap();
        
        let props_to_remove = vec![
            DeletedProp { prop_id: prop1, district_id },
            DeletedProp { prop_id: prop2, district_id },
        ];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 2);
        assert!(failed.is_empty());
        
        // Verify all props were removed from the district
        let district = &town["districts"][0];
        let props = district["props"].as_array().unwrap();
        assert_eq!(props.len(), 0);
    }

    #[test]
    fn remove_props_reports_failed_removals() {
        let mut town = sample_town_with_props();
        let existing_prop = Uuid::parse_str("e915e0f4-3f86-41cb-b9e2-1ead10025c06").unwrap();
        let non_existing_prop = Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap();
        let district_id = Uuid::parse_str("9a12c0d3-218c-4ef2-b78c-b6e3bca60719").unwrap();
        
        let props_to_remove = vec![
            DeletedProp { prop_id: existing_prop, district_id },
            DeletedProp { prop_id: non_existing_prop, district_id },
        ];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 1);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0], "00000000-0000-0000-0000-000000000000");
        
        // Verify the existing prop was removed
        let district = &town["districts"][0];
        let props = district["props"].as_array().unwrap();
        assert_eq!(props.len(), 1);
    }

    #[test]
    fn remove_props_handles_district_without_props() {
        let mut town = json!({
            "levelInfo": { "level": 6 },
            "districts": [
                {
                    "id": "9a12c0d3-218c-4ef2-b78c-b6e3bca60719",
                    // No props array
                }
            ]
        });

        let prop_id = Uuid::parse_str("e915e0f4-3f86-41cb-b9e2-1ead10025c06").unwrap();
        let district_id = Uuid::parse_str("9a12c0d3-218c-4ef2-b78c-b6e3bca60719").unwrap();
        
        let props_to_remove = vec![
            DeletedProp { prop_id, district_id }
        ];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 0);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0], "e915e0f4-3f86-41cb-b9e2-1ead10025c06");
    }

    #[test]
    fn remove_props_handles_wrong_district() {
        let mut town = sample_town_with_props();
        let prop_id = Uuid::parse_str("e915e0f4-3f86-41cb-b9e2-1ead10025c06").unwrap();
        let wrong_district_id = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        
        let props_to_remove = vec![
            DeletedProp { prop_id, district_id: wrong_district_id }
        ];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 0);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0], "e915e0f4-3f86-41cb-b9e2-1ead10025c06");
        
        // Verify the prop is still there (unchanged)
        let district = &town["districts"][0];
        let props = district["props"].as_array().unwrap();
        assert_eq!(props.len(), 2);
    }

    #[test]
    fn remove_props_handles_empty_request() {
        let mut town = sample_town_with_props();
        let props_to_remove: Vec<DeletedProp> = vec![];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 0);
        assert!(failed.is_empty());
        
        // Town should be unchanged
        let district = &town["districts"][0];
        let props = district["props"].as_array().unwrap();
        assert_eq!(props.len(), 2);
    }

    #[test]
    fn remove_props_handles_props_in_multiple_districts() {
        let mut town = sample_town_with_props();
        let prop1 = Uuid::parse_str("e915e0f4-3f86-41cb-b9e2-1ead10025c06").unwrap();
        let district1 = Uuid::parse_str("9a12c0d3-218c-4ef2-b78c-b6e3bca60719").unwrap();
        let prop2 = Uuid::parse_str("g9876543-21ab-cdef-0123-456789abcdef").unwrap();
        let district2 = Uuid::parse_str("another-district-id").unwrap();
        
        let props_to_remove = vec![
            DeletedProp { prop_id: prop1, district_id: district1 },
            DeletedProp { prop_id: prop2, district_id: district2 },
        ];

        let (removed_count, failed) = remove_props_from_town(&mut town, &props_to_remove);
        
        assert_eq!(removed_count, 2);
        assert!(failed.is_empty());
        
        // Verify first district props
        let district = &town["districts"][0];
        let props = district["props"].as_array().unwrap();
        assert_eq!(props.len(), 1); // Only prop2 remains
        assert_eq!(props[0]["id"], "f1234567-89ab-cdef-0123-456789abcdef");
        
        // Verify second district props
        let district2 = &town["districts"][1];
        let props2 = district2["props"].as_array().unwrap();
        assert_eq!(props2.len(), 0);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared db plumbing.
// ─────────────────────────────────────────────────────────────────────────────

/// Load the character's town+wallet+inventory+character row, locked for the
/// read-modify-write and ownership-checked by the `user_id` filter.
async fn load_town_economy(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
    user_id: Uuid,
) -> Result<CharacterDbEntryTownEconomy, BladeApiError> {
    use crate::schema::characters;
    characters::table
        .filter(characters::id.eq(character_id))
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryTownEconomy::as_select())
        .for_no_key_update()
        .load(conn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))
}

/// Take the character's town JSON out of the loaded row for in-place mutation.
/// A character with no captured town (or a null town) can't have buildings modified
/// — 409 rather than fabricate one.
fn take_town(entry: &mut CharacterDbEntryTownEconomy) -> Result<Value, BladeApiError> {
    match entry.town.take() {
        Some(JsonDbWrapper(v)) if !v.is_null() => Ok(v),
        _ => Err(BladeApiError::new(StatusCode::CONFLICT, TOWN_SERVICE_ID, 7)),
    }
}

/// Persist the mutated town + charged wallet/inventory back to the row and build the
/// standard `{wallet, inventory, town, validationFlags}` response. Takes the
/// `tracker` `charge_cost` populated so the inventory diff carries the consumed
/// materials. Consumes the entry (moved into the diesel changeset).
async fn finish_town_mutation(
    conn: &mut diesel_async::AsyncPgConnection,
    entry: CharacterDbEntryTownEconomy,
    town: Value,
    tracker: &InventoryChangeTracker,
) -> Result<Json<TownMutationResponse>, BladeApiError> {
    let inventory = entry.inventory.0.generate_client_update(tracker);
    let wallet = entry.wallet.0.clone();
    let character_id = entry.id;

    use crate::schema::characters;
    let mut changeset = entry;
    changeset.town = Some(JsonDbWrapper(town.clone()));
    diesel::update(characters::table)
        .filter(characters::id.eq(character_id))
        .set(changeset)
        .execute(conn)
        .await?;

    Ok(Json(TownMutationResponse {
        wallet,
        inventory,
        town,
        validation_flags: 1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_upgrades() -> Value {
        json!({
            "buildings": {
                "26fdb92f-a4df-4928-a97b-dee8699af605": {
                    "editorName": "Forge",
                    "maxLevel": 9,
                    "styleIds": ["c462a43a-0547-4cd0-a755-5c0aff0f74f8"],
                    "levels": {
                        "0": {
                            "goldCost": 15000,
                            "constructionTimeMs": 3604904,
                            "buildInputs": {"e7193116-d761-479b-8a20-5633737977f5": 43},
                            "styleInputs": {},
                            "requireTownLevel": 0
                        },
                        "1": {
                            "goldCost": 1140,
                            "constructionTimeMs": 1199911,
                            "buildInputs": {"e7193116-d761-479b-8a20-5633737977f5": 43},
                            "styleInputs": {
                                "c462a43a-0547-4cd0-a755-5c0aff0f74f8": {
                                    "38d32048-ce01-4390-a4f0-cdb94ef3ce72": 5
                                }
                            },
                            "requireTownLevel": 5
                        }
                    }
                }
            }
        })
    }

    const FORGE: &str = "26fdb92f-a4df-4928-a97b-dee8699af605";
    const LUMBER: &str = "e7193116-d761-479b-8a20-5633737977f5";
    const BRONZE: &str = "38d32048-ce01-4390-a4f0-cdb94ef3ce72";
    const STYLE: &str = "c462a43a-0547-4cd0-a755-5c0aff0f74f8";

    #[test]
    fn cost_lookup_reads_gold_time_materials_and_gate() {
        let up = sample_upgrades();
        let cost = lookup_level_cost(
            &up,
            Uuid::parse_str(FORGE).unwrap(),
            1,
            Some(Uuid::parse_str(STYLE).unwrap()),
        )
        .unwrap();
        assert_eq!(cost.gold, 1140);
        assert_eq!(cost.construction_time_ms, 1199911);
        assert_eq!(cost.require_town_level, 5);
        assert_eq!(cost.max_level, 9);
        // base 43 lumber + style 5 bronze
        let lumber = Uuid::parse_str(LUMBER).unwrap();
        let bronze = Uuid::parse_str(BRONZE).unwrap();
        assert_eq!(cost.materials.iter().find(|(m, _)| *m == lumber).unwrap().1, 43);
        assert_eq!(cost.materials.iter().find(|(m, _)| *m == bronze).unwrap().1, 5);
    }

    /// A town with one building, in the nested districts/segments shape the real
    /// payload uses.
    fn sample_town(bid: &str) -> Value {
        serde_json::json!({
            "levelInfo": { "level": 6 },
            "districts": [{
                "id": "9a12c0d3-218c-4ef2-b78c-b6e3bca60719",
                "segments": {
                    "71c4b321-825a-4fde-bdcc-c584f1d2db83": {
                        "id": "71c4b321-825a-4fde-bdcc-c584f1d2db83",
                        "buildings": {
                            bid: {
                                "id": bid,
                                "typeId": "52291bce-3585-49e5-85e4-afbdfa5ba422",
                                "level": 0,
                                "state": "NORMAL"
                            }
                        }
                    }
                }
            }]
        })
    }

    /// tracker #75: the client POSTed `…/buildings/{id}/styles/{id}` and got a 404,
    /// then hung on the loading screen with no way out but restarting. The style has
    /// to land on the building, and `customized` has to flip — retail's captured
    /// response carries both.
    #[test]
    fn applying_a_style_sets_style_id_and_customized() {
        let bid = "af0a05c7-9765-4c59-8799-6fc00f8a16c8";
        let style = "aa133662-053d-434e-8779-3f2a41d1271e";
        let mut town = sample_town(bid);

        assert!(super::apply_building_style(
            &mut town,
            bid.parse().unwrap(),
            style.parse().unwrap()
        ));

        let b = &town["districts"][0]["segments"]["71c4b321-825a-4fde-bdcc-c584f1d2db83"]
            ["buildings"][bid];
        assert_eq!(b["styleId"], serde_json::json!(style), "the style must be applied");
        assert_eq!(b["customized"], serde_json::json!(true), "retail also sets customized");
        // Untouched fields survive — this rewrites two keys, not the building.
        assert_eq!(b["level"], serde_json::json!(0));
        assert_eq!(b["state"], serde_json::json!("NORMAL"));
    }

    /// An unknown building must REPORT failure so the handler can 404, rather than
    /// silently succeeding — a style the town does not carry would snap back on the
    /// next load and look like the bug we are fixing.
    #[test]
    fn applying_a_style_to_an_unknown_building_fails() {
        let mut town = sample_town("af0a05c7-9765-4c59-8799-6fc00f8a16c8");
        let before = town.clone();
        assert!(!super::apply_building_style(
            &mut town,
            "00000000-0000-0000-0000-000000000000".parse().unwrap(),
            "aa133662-053d-434e-8779-3f2a41d1271e".parse().unwrap()
        ));
        assert_eq!(town, before, "a failed lookup must not modify the town");
    }

    #[test]
    fn cost_lookup_without_matching_style_charges_base_only() {
        let up = sample_upgrades();
        // A style id not present in styleInputs → no extra materials, no error.
        let cost = lookup_level_cost(
            &up,
            Uuid::parse_str(FORGE).unwrap(),
            1,
            Some(Uuid::new_v4()),
        )
        .unwrap();
        assert_eq!(cost.materials.len(), 1); // only base lumber
    }

    #[test]
    fn cost_lookup_rejects_beyond_max_level() {
        let up = sample_upgrades();
        let err = lookup_level_cost(&up, Uuid::parse_str(FORGE).unwrap(), 10, None).unwrap_err();
        assert_eq!(err, CostError::AtMaxLevel);
    }

    #[test]
    fn cost_lookup_unknown_building() {
        let up = sample_upgrades();
        let err = lookup_level_cost(&up, Uuid::new_v4(), 1, None).unwrap_err();
        assert_eq!(err, CostError::UnknownBuilding);
    }

    #[test]
    fn upgrade_transition_sets_state_level_and_construction_end() {
        let mut building = json!({
            "id": "718e3d02-07a3-432d-a808-65a9d25e0540",
            "typeId": FORGE,
            "level": 0,
            "state": "NORMAL",
            "constructionEnd": 0
        });
        apply_upgrade_transition(&mut building, 1, 1199911, 1_000_000);
        assert_eq!(building["state"], json!("UPGRADING"));
        assert_eq!(building["level"], json!(1));
        assert_eq!(building["constructionEnd"], json!(1_000_000u64 + 1199911));
    }

    #[test]
    fn complete_transition_clears_timer_and_state() {
        let mut building = json!({
            "level": 3,
            "state": "UPGRADING",
            "constructionEnd": 999_999
        });
        apply_complete_transition(&mut building);
        assert_eq!(building["state"], json!("NORMAL"));
        assert_eq!(building["constructionEnd"], json!(0));
        // level is left as-is (already the target level).
        assert_eq!(building["level"], json!(3));
    }

    fn empty_inventory() -> blades_lib::user_data::CompleteInventory {
        use blades_lib::user_data::*;
        CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        }
    }

    #[test]
    fn charge_cost_debits_gold_and_materials_and_bumps_version() {
        let lumber = Uuid::parse_str(LUMBER).unwrap();
        let cost = LevelCost {
            gold: 1000,
            construction_time_ms: 100,
            materials: vec![(lumber, 10)],
            require_town_level: 0,
            max_level: 9,
        };
        let mut wallet = CompleteWallet::default();
        wallet.credit(GOLD, 5000);
        let mut inv = empty_inventory();
        inv.backpack.stackable_items.add(lumber, 30);
        let mut tracker = InventoryChangeTracker::default();

        charge_cost(&cost, false, &mut wallet, &mut inv, &mut tracker).unwrap();

        assert_eq!(wallet.balance(GOLD), 4000);
        assert_eq!(inv.backpack.stackable_items.count(lumber), 20);
        assert_eq!(inv.backpack_version, 2);
        assert!(tracker.modified_backpack.stackable_items.contains(&lumber));
    }

    #[test]
    fn charge_cost_insufficient_gold_errors_without_mutating() {
        let cost = LevelCost {
            gold: 1000,
            construction_time_ms: 0,
            materials: vec![],
            require_town_level: 0,
            max_level: 9,
        };
        let mut wallet = CompleteWallet::default();
        wallet.credit(GOLD, 500);
        let mut inv = empty_inventory();
        let mut tracker = InventoryChangeTracker::default();
        let err = charge_cost(&cost, false, &mut wallet, &mut inv, &mut tracker);
        assert!(err.is_err());
        // Wallet untouched.
        assert_eq!(wallet.balance(GOLD), 500);
    }

    #[test]
    fn charge_cost_insufficient_materials_errors_before_debiting_gold() {
        let lumber = Uuid::parse_str(LUMBER).unwrap();
        let cost = LevelCost {
            gold: 100,
            construction_time_ms: 0,
            materials: vec![(lumber, 50)],
            require_town_level: 0,
            max_level: 9,
        };
        let mut wallet = CompleteWallet::default();
        wallet.credit(GOLD, 10_000);
        let mut inv = empty_inventory();
        inv.backpack.stackable_items.add(lumber, 5); // not enough
        let mut tracker = InventoryChangeTracker::default();
        let err = charge_cost(&cost, false, &mut wallet, &mut inv, &mut tracker);
        assert!(err.is_err());
        // Neither gold nor materials consumed (phase-1 check failed first).
        assert_eq!(wallet.balance(GOLD), 10_000);
        assert_eq!(inv.backpack.stackable_items.count(lumber), 5);
    }

    #[test]
    fn find_and_remove_building_navigate_nested_town() {
        let real = Uuid::new_v4();
        let mut town = json!({
            "levelInfo": {"level": 7},
            "districts": [
                {"segments": {
                    "seg-a": {"buildings": {
                        real.to_string(): {"id": real.to_string(), "typeId": FORGE, "styleId": STYLE, "level": 2, "state": "NORMAL"}
                    }},
                    "seg-b": {"buildings": {}}
                }}
            ]
        });

        // town_level reads levelInfo.level.
        assert_eq!(town_level(&town), 7);

        let facts = read_building_facts(&town, real).unwrap();
        assert_eq!(facts.0, Uuid::parse_str(FORGE).unwrap());
        assert_eq!(facts.1, Some(Uuid::parse_str(STYLE).unwrap()));
        assert_eq!(facts.2, 2);
        assert!(find_building_mut(&mut town, real).is_some());
        assert!(remove_building(&mut town, real));
        assert!(find_building_mut(&mut town, real).is_none());
    }

    #[test]
    fn insert_building_adds_level0_upgrading_building_to_segment() {
        let mut town = json!({
            "levelInfo": {"level": 5},
            "districts": [{"segments": {"seg-a": {"id": "seg-a"}}}]
        });
        let seg = Uuid::new_v4();
        // rename the segment key to a real uuid we can address
        town["districts"][0]["segments"] = json!({ seg.to_string(): {"id": seg.to_string()} });
        let bid = Uuid::new_v4();
        let ty = Uuid::parse_str(FORGE).unwrap();
        let st = Uuid::parse_str(STYLE).unwrap();
        insert_building(&mut town, seg, bid, ty, st, 0, 5000, 1_000).unwrap();
        let b = &town["districts"][0]["segments"][seg.to_string()]["buildings"][bid.to_string()];
        assert_eq!(b["level"], json!(0));
        assert_eq!(b["state"], json!("UPGRADING"));
        assert_eq!(b["constructionEnd"], json!(6000));
        assert_eq!(b["typeId"], json!(ty.to_string()));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Speed-up (tracker #88): paying gems to finish a building instantly.
    //
    // The curve itself is unit-tested in `blades_lib::economy::skip_time`. What
    // is pinned HERE is the wiring: that the table we SHIP parses and prices the
    // captured numbers, that `/complete` actually debits it, that a player who
    // cannot pay is refused, and that the response carries the new balance.
    // ─────────────────────────────────────────────────────────────────────────

    use blades_lib::economy::Price;
    use blades_lib::economy::skip_time::SkipTimeCostTable;

    /// The building id `sample_town` puts in the one addressable segment.
    const TIMED_BID: &str = "af0a05c7-9765-4c59-8799-6fc00f8a16c8";
    const TIMED_SEG: &str = "71c4b321-825a-4fde-bdcc-c584f1d2db83";

    /// The static file the SERVER reads in production (the `deploy/static` bind
    /// mount), not a fixture — so a regeneration that drops the table fails here.
    fn shipped_prod_static() -> Value {
        serde_json::from_str(include_str!("../../deploy/static/building_upgrades.json"))
            .expect("deploy/static/building_upgrades.json is valid JSON")
    }

    /// The local-dev copy (`--static-data server/data/static`).
    fn shipped_dev_static() -> Value {
        serde_json::from_str(include_str!("../data/static/building_upgrades.json"))
            .expect("server/data/static/building_upgrades.json is valid JSON")
    }

    fn shipped_table() -> SkipTimeCostTable {
        SkipTimeCostTable::from_static(&shipped_prod_static())
            .expect("the shipped static carries _meta.skipTimeCostTable")
    }

    fn town_with_timer(construction_end: u64) -> Value {
        let mut town = sample_town(TIMED_BID);
        let b = &mut town["districts"][0]["segments"][TIMED_SEG]["buildings"][TIMED_BID];
        b["state"] = json!("UPGRADING");
        b["level"] = json!(1);
        b["constructionEnd"] = json!(construction_end);
        town
    }

    fn wallet_with(gems: u64) -> CompleteWallet {
        let mut w = CompleteWallet::default();
        w.credit(GEMS, gems);
        w
    }

    /// The DATA, not just the code: the table we ship has to be the measured one.
    /// If `extract_town_static.py` is re-run and drops `_meta.skipTimeCostTable`,
    /// or someone edits the rates, this is the test that goes red.
    #[test]
    fn the_shipped_static_carries_the_measured_skip_time_table() {
        let t = shipped_table();
        assert_eq!(t.rate_list.len(), 2, "two bands: 12/hr to 12 h, then 6/hr");
        assert!(
            t.rate_list.iter().all(|b| b.currency == GEMS),
            "the skip-time table prices in Gem"
        );

        // The two captured points that straddle the 12 h join.
        assert_eq!(t.cost_for_time(38_394.0), vec![Price::new(GEMS, 128)]);
        assert_eq!(t.cost_for_time(47_994.0), vec![Price::new(GEMS, 152)]);
        // And the f32 boundary case (an f64 walk would say 200 here).
        assert_eq!(t.cost_for_time(76_800.0), vec![Price::new(GEMS, 201)]);
    }

    /// The prod bind-mount copy and the local-dev copy must agree, or a developer
    /// tests against a price players are not charged.
    #[test]
    fn both_shipped_static_copies_carry_the_same_table() {
        assert_eq!(
            shipped_prod_static()["_meta"]["skipTimeCostTable"],
            shipped_dev_static()["_meta"]["skipTimeCostTable"],
            "deploy/static and server/data/static disagree on the skip-time price"
        );
    }

    /// The billed path: a timer with 47 994 s left costs 152 gems, exactly the
    /// captured retail debit, and the wallet ends 152 lighter.
    #[test]
    fn speed_up_debits_the_measured_gem_price() {
        let now = 1_800_000_000_000u64;
        let town = town_with_timer(now + 47_994_000);
        let mut wallet = wallet_with(1_000);

        let charged = charge_construction_speed_up(
            true,
            Some(&shipped_table()),
            &town,
            TIMED_BID.parse().unwrap(),
            now,
            &mut wallet,
        )
        .expect("affordable");

        assert_eq!(charged, vec![Price::new(GEMS, 152)]);
        assert_eq!(wallet.balance(GEMS), 848);
    }

    /// Without the flag, nothing is charged — a normal completion of an elapsed
    /// timer is free, and always was.
    #[test]
    fn completing_without_speed_up_charges_nothing() {
        let now = 1_800_000_000_000u64;
        let town = town_with_timer(now + 47_994_000);
        let mut wallet = wallet_with(1_000);

        let charged = charge_construction_speed_up(
            false,
            Some(&shipped_table()),
            &town,
            TIMED_BID.parse().unwrap(),
            now,
            &mut wallet,
        )
        .unwrap();

        assert!(charged.is_empty());
        assert_eq!(wallet.balance(GEMS), 1_000, "speedUp:false must not debit");
    }

    /// An already-elapsed timer costs nothing even with `speedUp: true` — the
    /// player is not buying anything, and a faithful walk over a negative
    /// remainder would CREDIT gems.
    #[test]
    fn speed_up_on_an_elapsed_timer_is_free() {
        let now = 1_800_000_000_000u64;
        for end in [now, now - 1, now - 86_400_000, 0] {
            let town = town_with_timer(end);
            let mut wallet = wallet_with(1_000);
            let charged = charge_construction_speed_up(
                true,
                Some(&shipped_table()),
                &town,
                TIMED_BID.parse().unwrap(),
                now,
                &mut wallet,
            )
            .unwrap();
            assert!(charged.is_empty(), "constructionEnd {end} should be free");
            assert_eq!(wallet.balance(GEMS), 1_000);
        }
    }

    /// Not enough gems must FAIL with a 400 and leave the balance untouched. If
    /// this ever "passes" by skipping the charge, players get free speed-ups —
    /// which is precisely the bug this fixes.
    #[test]
    fn speed_up_without_enough_gems_fails_and_debits_nothing() {
        use actix_web::ResponseError;

        let now = 1_800_000_000_000u64;
        let town = town_with_timer(now + 47_994_000);
        let mut wallet = wallet_with(151); // one gem short of 152

        let err = charge_construction_speed_up(
            true,
            Some(&shipped_table()),
            &town,
            TIMED_BID.parse().unwrap(),
            now,
            &mut wallet,
        )
        .expect_err("151 gems cannot buy a 152-gem skip");

        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(wallet.balance(GEMS), 151, "a refused charge must not debit");
    }

    /// `deploy/static/` is a bind mount: merging this ships the CODE but not the
    /// JSON. Between the merge and `deploy/arena.sh static` the table is absent,
    /// and the server must charge NOTHING rather than crash or invent a price.
    #[test]
    fn without_the_static_table_speed_up_is_free_not_broken() {
        let now = 1_800_000_000_000u64;
        let town = town_with_timer(now + 47_994_000);
        let mut wallet = wallet_with(1_000);

        let charged = charge_construction_speed_up(
            true,
            None,
            &town,
            TIMED_BID.parse().unwrap(),
            now,
            &mut wallet,
        )
        .expect("a missing table must not error");

        assert!(charged.is_empty());
        assert_eq!(wallet.balance(GEMS), 1_000);
    }

    /// The price comes from stored state only. A building the town does not carry
    /// has no timer, so it has no price — and the handler 404s before this runs.
    #[test]
    fn remaining_time_of_an_unknown_building_is_not_positive() {
        let now = 1_800_000_000_000u64;
        let town = town_with_timer(now + 47_994_000);
        assert_eq!(
            remaining_construction_ms(&town, Uuid::new_v4(), now),
            -(now as i64)
        );
        assert_eq!(
            remaining_construction_ms(&town, TIMED_BID.parse().unwrap(), now),
            47_994_000
        );
    }

    /// Retail's `/complete` sends `{town}` alone without the flag, and
    /// `{character, inventory, town, wallet}` with it — measured over 159
    /// captures. In particular it NEVER sends `shop`, which we used to send on
    /// every completion and which tells the client the finished building's vendor
    /// has nothing to sell.
    #[test]
    fn complete_response_matches_the_captured_retail_shape() {
        use blades_lib::user_data::{Backpack, CompleteInventory, Loadout, Treasury};
        let inv = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        }
        .generate_client_update(&InventoryChangeTracker::default());
        let plain = serde_json::to_value(complete_response(
            false,
            json!({"levelInfo": {"level": 6}}),
            wallet_with(848),
            inv.clone(),
            json!({"name": "Swanne"}),
        ))
        .unwrap();
        let keys: Vec<&str> = plain
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["town"], "speedUp:false sends the town alone");

        let sped = serde_json::to_value(complete_response(
            true,
            json!({"levelInfo": {"level": 6}}),
            wallet_with(848),
            inv,
            json!({"name": "Swanne"}),
        ))
        .unwrap();
        let obj = sped.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["character", "inventory", "town", "wallet"]);
        assert!(!obj.contains_key("shop"), "retail sends no shop here");
        // The post-deduction balance is the point of returning the wallet at all.
        // The wire wallet is an array of `{currencyId, balance}`.
        let gem_line = sped["wallet"]
            .as_array()
            .expect("wallet serializes as an array")
            .iter()
            .find(|e| e["currencyId"] == json!(GEMS.to_string()))
            .expect("the gem line is present");
        assert_eq!(
            gem_line["balance"],
            json!(848),
            "the client learns the gem debit from this field"
        );
    }
}
