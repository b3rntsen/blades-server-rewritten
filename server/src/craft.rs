//! Blacksmith / alchemy crafting — `GET /crafts` (active jobs), `POST /crafts`
//! (start a craft), `POST /crafts/{id}/finish` (collect results).
//!
//! ## Cost
//! Recipe input costs are not in captures; this implementation is LENIENT — no
//! materials or gold are charged on start, and the gems speed-up (`speedUp`) is
//! accepted on finish but not charged.
//! TODO: recipe input cost not captured; lenient.
//!
//! ## Temper / enchant
//! Captures only show base crafts (temperingLevel = 0). `create_craft` applies the
//! requested `temperingLevel` to produced instanced items.
//! TODO: temper/enchant of an existing item not in captures.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    get, http::StatusCode, post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, RewardItem, apply_reward};
use blades_lib::server_state::CraftJob;
use blades_lib::user_data::{
    CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
    InventoryChangeTracker, Item, ItemPropertiesAll,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy, session::SessionLookedUpMaybe};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Wire types ────────────────────────────────────────────────────────────────

/// A craft job as sent to the client (`GET /crafts` list or `POST /crafts` response).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CraftJobWire<'a> {
    id: Uuid,
    user_id: Uuid,
    character_id: Uuid,
    building_id: Uuid,
    recipe_id: Uuid,
    crafting_type_id: Uuid,
    completed_at: i64,
    batch_size: u32,
    results: &'a Value,
    version: u32,
}

impl<'a> CraftJobWire<'a> {
    fn from_job(job: &'a CraftJob, user_id: Uuid, character_id: Uuid) -> Self {
        CraftJobWire {
            id: job.id,
            user_id,
            character_id,
            building_id: job.building_id,
            recipe_id: job.recipe_id,
            crafting_type_id: job.crafting_type_id,
            completed_at: job.completed_at_ms,
            batch_size: 1,
            results: &job.results,
            version: 1,
        }
    }
}

// ── GET /crafts ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct GetCraftsResponse {
    crafts: Vec<Value>,
}

/// `GET /crafts` — returns the character's active craft jobs.
/// The repair gate reads this list; an empty list unblocks repair.
#[get("blades.bgs.services/api/game/v1/public/characters/{character_id}/crafts")]
pub async fn get_crafts(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetCraftsResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let entry = load_owned(&mut conn, character_id, user_id).await?;
            let crafts = entry
                .server_state
                .0
                .craft_jobs
                .iter()
                .map(|job| {
                    serde_json::to_value(CraftJobWire::from_job(job, user_id, character_id))
                        .unwrap_or(Value::Null)
                })
                .collect();
            Ok::<_, BladeApiError>(Json(GetCraftsResponse { crafts }))
        }
        .scope_boxed()
    })
    .await
}

// ── POST /crafts ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCraftRequest {
    recipe_id: Uuid,
    building_id: Uuid,
    #[serde(default)]
    tempering_level: u64,
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
    #[serde(default)]
    #[allow(dead_code)]
    batch_size: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateCraftResponse {
    craft: Value,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
}

/// `POST /crafts` — start a craft job.
///
/// Looks up the recipe from `static_data.recipes`; mints a new `CraftJob` with a
/// fresh id and stores it in `server_state.craft_jobs`. Returns the job wire-shape,
/// an empty inventory diff, and the wallet.
///
/// TODO: recipe input cost not captured; lenient (no materials/gold charged).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/crafts")]
pub async fn create_craft(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<CreateCraftRequest>,
) -> Result<Json<CreateCraftResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let req = body.into_inner();
    let globals = app_state.get_ref().clone();

    // Look up the recipe before entering the transaction.
    let recipe = globals
        .static_data
        .recipes
        .get(&req.recipe_id)
        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 4))?
        .clone();

    let tempering_level: u64 = req.tempering_level;
    let building_id = req.building_id;
    let recipe_id = req.recipe_id;

    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let tracker = InventoryChangeTracker::default();

            // Apply the requested temperingLevel to any instanced item in the results.
            let results = apply_tempering_to_results(&recipe.results, tempering_level);

            let now = now_ms();
            let completed_at_ms = now + recipe.duration_ms;

            let job = CraftJob {
                id: Uuid::new_v4(),
                recipe_id,
                building_id,
                crafting_type_id: recipe.crafting_type_id,
                completed_at_ms,
                results,
            };

            entry.server_state.0.craft_jobs.push(job.clone());

            let craft_wire = serde_json::to_value(CraftJobWire::from_job(
                &job,
                user_id,
                character_id,
            ))
            .unwrap_or(Value::Null);

            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(CreateCraftResponse {
                craft: craft_wire,
                inventory,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}

// ── POST /crafts/{id}/finish ──────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FinishCraftRequest {
    #[serde(default)]
    #[allow(dead_code)]
    speed_up: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FinishCraftResponse {
    character: CompleteCharacterWithIdWithoutData,
    reward: RewardGrant,
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
}

/// `POST /crafts/{id}/finish` — collect the results of a completed craft job.
///
/// Finds the job by id in `server_state.craft_jobs`, builds a `RewardGrant` from
/// its stored `results` (re-minting instanced item ids with `Uuid::new_v4()`),
/// calls `apply_reward`, removes the job, and returns the character + reward.
///
/// `speedUp: true` is accepted but gems are NOT charged (lenient).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/crafts/{craft_id}/finish")]
pub async fn finish_craft(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<FinishCraftRequest>,
) -> Result<Json<FinishCraftResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, craft_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;

            // Find and remove the job.
            let job_pos = entry
                .server_state
                .0
                .craft_jobs
                .iter()
                .position(|j| j.id == craft_id)
                .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 5))?;
            let job = entry.server_state.0.craft_jobs.remove(job_pos);

            // Build a RewardGrant from the job's results, re-minting item ids.
            let reward = reward_from_results(&job.results);

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

            let character = entry.character.0.clone();
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(FinishCraftResponse {
                character: CompleteCharacterWithIdWithoutData { id: character_id, character },
                reward,
                wallet,
                inventory,
            }))
        }
        .scope_boxed()
    })
    .await
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Apply the requested `tempering_level` to every item in an `{"items":[...]}` results
/// object. Stackable results are returned unchanged.
fn apply_tempering_to_results(results: &Value, tempering_level: u64) -> Value {
    if tempering_level == 0 {
        return results.clone();
    }
    let mut out = results.clone();
    if let Some(items) = out.get_mut("items").and_then(|v| v.as_array_mut()) {
        for item in items.iter_mut() {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("temperingLevel".to_string(), Value::from(tempering_level));
            }
        }
    }
    out
}

/// Build a `RewardGrant` from a craft job's stored `results` value. Instanced items
/// get fresh `Uuid::new_v4()` ids so they never collide with the placeholder ids
/// stored in the recipe. Stackable items are carried over verbatim.
fn reward_from_results(results: &Value) -> RewardGrant {
    let mut reward = RewardGrant::default();

    // Instanced items branch: `{"items": [{id, itemTemplateId, temperingLevel, durability, properties?}]}`
    if let Some(items_val) = results.get("items").and_then(|v| v.as_array()) {
        for item_val in items_val {
            let template_id = item_val
                .get("itemTemplateId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Uuid>().ok())
                .unwrap_or_else(Uuid::nil);
            let tempering_level = item_val
                .get("temperingLevel")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let durability = item_val
                .get("durability")
                .and_then(|v| v.as_f64())
                .unwrap_or(100.0);
            // Carry properties verbatim if present, else default (empty).
            let properties: ItemPropertiesAll = item_val
                .get("properties")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            reward.items.push(RewardItem {
                id: Uuid::new_v4(), // re-mint so ids never collide
                item: Item { item_template_id: template_id, tempering_level, durability, properties },
            });
        }
    }

    // Stackable items branch: `{"stackableItems": {"<templateId>": <count>}}`
    if let Some(stacks) = results.get("stackableItems").and_then(|v| v.as_object()) {
        for (tmpl_str, count_val) in stacks {
            if let (Ok(tmpl), Some(count)) = (tmpl_str.parse::<Uuid>(), count_val.as_u64()) {
                reward.stackable_items.insert(tmpl, count);
            }
        }
    }

    reward
}

// ── DB helpers (identical pattern to shop.rs / challenge.rs) ─────────────────

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
