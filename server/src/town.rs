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
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
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

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current")]
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

/// The `typeId`/`styleId`/`level` of a building (read-only pre-scan before the
/// mutable borrow, so cost lookup doesn't tangle with the `&mut` on the town).
fn read_building_facts(town: &Value, building_id: Uuid) -> Option<(Uuid, Option<Uuid>, u64)> {
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
                let type_id = b
                    .get("typeId")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())?;
                let style_id = b
                    .get("styleId")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok());
                let level = b.get("level").and_then(Value::as_u64).unwrap_or(0);
                return Some((type_id, style_id, level));
            }
        }
    }
    None
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
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/upgrade"
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
    /// elapsed). We accept it but don't re-bill here — the timer is advisory on our
    /// server and the gem cost, if any, is charged client-flow-side.
    #[serde(default)]
    #[allow(dead_code)]
    speed_up: bool,
}

/// `complete`'s response: the wallet + inventory diff + updated town, plus the
/// building's `shop` (empty by default — we don't model per-building shop stock
/// generation) and the full character so the client refreshes town xp/level.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteResponse {
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
    town: Value,
    validation_flags: u64,
    /// Per-building shop the completed building unlocks. We don't generate stock, so
    /// this is an empty shop object; the client tolerates an empty `items` list.
    shop: Value,
    /// Full character JSONB (verbatim) so the client re-reads town xp/level etc.
    character: Value,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/complete"
)]
pub async fn complete_building(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<CompleteRequest>,
) -> Result<Json<CompleteResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, building_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await?;

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_town_economy(&mut conn, character_id, user_id).await?;
            let mut town = take_town(&mut entry)?;

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

                Ok::<_, BladeApiError>(Json(CompleteResponse {
                    wallet,
                    inventory,
                    town: town_col,
                    validation_flags: 1,
                    shop: json!({ "items": [] }),
                    character: serde_json::to_value(&character)
                        .unwrap_or_else(|_| json!(null)),
                }))
            }
        }
        .scope_boxed()
    })
    .await
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
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current/buildings"
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
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current/buildings/{building_id}/destroy"
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
}
