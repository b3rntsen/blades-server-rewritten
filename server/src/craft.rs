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
//! A `POST /crafts` request that carries an `itemId` MODIFIES an existing backpack item
//! rather than minting a new one: the item is pulled from the backpack into a timed job
//! and re-added (mutated) by `/finish`. `temperingLevel > 0` tempers (sets the level,
//! keeping existing enchants); otherwise it enchants — applying one of the recipe's
//! observed `ENCHANTING` outcomes (`item_mod_recipes.json`), picked deterministically per
//! item. `arcaneTier` is not modelled on `Item` (the server drops it for every item).

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    get, http::StatusCode, post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, RewardItem, apply_reward, remove_backpack_item};
use blades_lib::server_state::CraftJob;
use blades_lib::static_data::ItemModRecipe;
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
    /// Present for temper/enchant — the existing backpack item to modify. Absent for a
    /// plain craft (which mints a new item).
    #[serde(default)]
    item_id: Option<Uuid>,
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

/// `POST /crafts` — start a craft job. Two captured shapes:
///
/// - **plain craft** (no `itemId`): look up the recipe in `static_data.recipes`, mint a
///   new `CraftJob` whose `results` is the recipe output (with the requested
///   `temperingLevel` applied to produced items).
/// - **temper / enchant** (`itemId` present): pull that item out of the backpack and
///   store the MUTATED item as the job's `results` — temper sets `temperingLevel`
///   (keeping enchants), enchant applies one of the recipe's observed `ENCHANTING`
///   outcomes (`item_mod_recipes.json`). `/finish` re-adds the mutated item.
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

    // A plain craft (no itemId) looks up `recipes.json`; a temper/enchant (itemId
    // present) modifies an existing item and uses `item_mod_recipes.json`. The captured
    // recipe set is PARTIAL, so an unknown plain-craft recipe must NOT 404 — that crashed
    // the client mid-craft (user repro: "craft a potion → error + game restarted").
    // Unknown recipe → lenient empty job (handled in the transaction below).
    let plain_recipe = if req.item_id.is_none() {
        globals.static_data.recipes.get(&req.recipe_id).cloned()
    } else {
        None
    };
    let mod_recipe = globals.static_data.item_mod_recipes.get(&req.recipe_id).cloned();

    let recipe_id = req.recipe_id;
    let building_id = req.building_id;
    let tempering_level: u64 = req.tempering_level;
    let item_id = req.item_id;

    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();

            let (results, crafting_type_id, duration_ms) = if let Some(item_id) = item_id {
                // ── temper / enchant: modify an existing backpack item ──
                let existing =
                    remove_backpack_item(&mut entry.inventory.0, item_id, &mut tracker)
                        .map_err(BladeApiError::from_economy)?;
                let mutated = apply_item_mod(&existing, tempering_level, mod_recipe.as_ref(), item_id);
                entry.inventory.0.backpack_version += 1;
                let reward_item = RewardItem { id: item_id, item: mutated };
                let results = serde_json::json!({ "items": [reward_item] });
                // crafting_type_id MUST be the universal temper/enchant CraftingType, never the
                // recipe id. item_mod_recipes.json is a tiny subset of the real recipes
                // (retail has one recipe per item per level); for an unknown recipe we
                // previously fell back to recipe_id, which the client cannot map to a
                // CraftingStation → the temper UI spun forever. EVERY real on-device
                // temper recipe is outside our captured 23 (verified against the
                // arena-mitm transcript). Derive the type from temperingLevel; keep the
                // captured duration when the recipe is known, else 0.
                //
                // Both branches fixed this independently and identically (e5659c9 on
                // main, f73f425 here) — same two UUIDs, same rule. Keeping this side
                // because the logic lives in a named helper with its own test rather
                // than inline, so there is one place for it to be wrong.
                let crafting_type_id = item_mod_crafting_type(tempering_level);
                let duration_ms = mod_recipe.as_ref().map(|m| m.duration_ms).unwrap_or(0);
                (results, crafting_type_id, duration_ms)
            } else {
                // ── plain craft: mint from the recipe; unknown recipe → derive a valid
                //    crafting_type_id + a well-formed result (never 404, never echo
                //    recipe_id — both crash/freeze the client mid-craft) ──
                match &plain_recipe {
                    Some(recipe) => {
                        // Mint fresh, unique item ids now (the recipe's are shared
                        // placeholders); finish preserves whatever id is stored.
                        let results = remint_result_item_ids(
                            apply_tempering_to_results(&recipe.results, tempering_level),
                        );
                        (results, recipe.crafting_type_id, recipe.duration_ms)
                    }
                    None => {
                        // recipes.json is a PARTIAL capture (retail has far more alchemy
                        // recipes than we captured). For an unknown recipe we previously
                        // echoed crafting_type_id = recipe_id — exactly the temper-hang
                        // class of bug (fix e5659c9): the client can't map that id to a
                        // CraftingStation, so the Alchemist screen freezes and the app
                        // restarts. Derive a VALID crafting_type_id from context instead,
                        // and return a well-formed (non-empty) result so the craft-
                        // completion flow can finish.
                        let crafting_type_id =
                            derive_plain_craft_type(building_id, &globals.static_data);
                        // Approximate the brew's output as one stackable of the recipe's
                        // own id (the true potion template for an un-captured recipe is
                        // unknown; granting a single stackable is well-formed and lets the
                        // completion flow finish — flagged as an approximation).
                        let results = serde_json::json!({
                            "stackableItems": { recipe_id.to_string(): 1 }
                        });
                        (results, crafting_type_id, 0)
                    }
                }
            };

            let completed_at_ms = now_ms() + duration_ms;
            let job = CraftJob {
                id: Uuid::new_v4(),
                recipe_id,
                building_id,
                crafting_type_id,
                completed_at_ms,
                results,
            };
            entry.server_state.0.craft_jobs.push(job.clone());

            let craft_wire =
                serde_json::to_value(CraftJobWire::from_job(&job, user_id, character_id))
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

/// The universal ALCHEMY crafting type id (capture-confirmed: the craftingTypeId shared
/// by the captured alchemy recipes in recipes.json). Used as the derived type for an
/// un-captured plain craft — the reported freeze is the Alchemist, and alchemy is the
/// plain-craft station most likely to hit an un-captured recipe.
const ALCHEMY_CRAFTING_TYPE_ID: &str = "c9d3b3aa-6f27-4869-9523-c10861f3e292";

/// Derive a VALID `craftingTypeId` for an unknown plain-craft recipe, so the client can
/// map it to a `CraftingStation` (echoing the recipe_id can't be mapped → the Alchemist
/// screen freezes and the app restarts — the temper-hang class of bug, fix e5659c9).
///
/// recipes.json carries no `buildingId`, so we can't build a `building_id → craftingType`
/// lookup from static data today; if the passed `building_id` ever matches a KNOWN
/// recipe's building we'd prefer that, but absent that data we default to the alchemy
/// crafting type (the reported symptom + the common un-captured plain craft). `building_id`
/// and `static_data` are threaded through so a future building→type map can refine this
/// without changing the call site. NEVER returns the recipe id.
fn derive_plain_craft_type(
    _building_id: Uuid,
    static_data: &blades_lib::static_data::StaticData,
) -> Uuid {
    let alchemy = Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).expect("valid alchemy craft type uuid");
    // Prefer the alchemy type only if it actually appears in the loaded recipes (so a
    // future data set that renames it still yields a client-mappable type); else fall
    // back to ANY known recipe's craftingTypeId (still a real CraftingStation), and only
    // as a last resort the hardcoded alchemy id.
    if static_data.recipes.values().any(|r| r.crafting_type_id == alchemy) {
        return alchemy;
    }
    static_data
        .recipes
        .values()
        .map(|r| r.crafting_type_id)
        .next()
        .unwrap_or(alchemy)
}

/// The universal temper / enchant `craftingTypeId`s — the ONLY two that appear across
/// `item_mod_recipes.json`. A mod-craft (`itemId` present) MUST report one of these, never
/// the recipe id: `item_mod_recipes.json` is a tiny captured subset (retail has one recipe
/// per item per level), so an unknown recipe reporting `recipe_id` leaves the client unable
/// to map the `CraftingStation` → the temper UI spins forever (fix e5659c9). Every real
/// on-device temper recipe is outside our captured 23, so this path is the common one.
fn item_mod_crafting_type(tempering_level: u64) -> Uuid {
    if tempering_level > 0 {
        // temper
        Uuid::parse_str("06c8087b-ede4-4ce7-8103-6c2067d18498").expect("valid temper craft type")
    } else {
        // enchant
        Uuid::parse_str("aaef180b-8ee7-474a-a7eb-0156aa5529ba").expect("valid enchant craft type")
    }
}

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

/// Give each instanced item in a `{"items":[...]}` results object a fresh unique `id`.
/// Plain-craft recipe ids are shared placeholders (from `recipes.json`), so two crafts
/// of the same recipe would collide once `finish` preserves the stored id — minting
/// here keeps them unique. Temper/enchant do NOT go through this (they intentionally
/// keep the original backpack item id).
fn remint_result_item_ids(mut results: Value) -> Value {
    if let Some(items) = results.get_mut("items").and_then(|v| v.as_array_mut()) {
        for item in items.iter_mut() {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("id".to_string(), Value::from(Uuid::new_v4().to_string()));
            }
        }
    }
    results
}

/// Apply a temper or enchant to an existing item, returning the mutated copy.
///
/// - `tempering_level > 0` → **temper**: set `temperingLevel`, keeping everything else
///   (including existing enchants — matches the captured temper response).
/// - otherwise → **enchant**: replace `properties.enchanting` with one of the recipe's
///   observed `ENCHANTING` outcomes, picked deterministically by `item_id` (retail rolls
///   randomly from a pool; we pick a real observed outcome). With no recipe / no
///   outcomes the item is returned unchanged (lenient).
fn apply_item_mod(
    existing: &Item,
    tempering_level: u64,
    recipe: Option<&ItemModRecipe>,
    item_id: Uuid,
) -> Item {
    let mut item = existing.clone();
    if tempering_level > 0 {
        item.tempering_level = tempering_level;
        return item;
    }
    if let Some(rec) = recipe {
        if !rec.outcomes.is_empty() {
            let idx = (item_id.as_u128() % rec.outcomes.len() as u128) as usize;
            item.properties.enchanting = rec.outcomes[idx].enchanting.clone();
        }
    }
    item
}

/// Build a `RewardGrant` from a craft job's stored `results` value. Instanced items
/// keep the `id` stored in the job — for temper/enchant that is the ORIGINAL backpack
/// item id (retail preserves it through the craft, and the client tracks the item in
/// the smithy by that id; re-minting it here desynced the client → the temper "hung"
/// after the gem speed-up). Plain-craft ids are made unique at create time (see
/// `remint_result_item_ids`), so preserving them here never collides. Stackable items
/// are carried over verbatim.
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
            let id = item_val
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Uuid>().ok())
                .unwrap_or_else(Uuid::new_v4);
            reward.items.push(RewardItem {
                id, // preserve the stored id (temper/enchant keep the item's own id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::static_data::EnchantOutcome;
    use blades_lib::user_data::{ItemPropertiesAll, ItemSingleProperty};

    fn prop(n: u128) -> ItemSingleProperty {
        ItemSingleProperty { id: Uuid::from_u128(n), tier: 10 }
    }

    fn item_with(tempering: u64, enchants: Vec<ItemSingleProperty>) -> Item {
        Item {
            item_template_id: Uuid::from_u128(0xABCD),
            tempering_level: tempering,
            durability: 300.0,
            properties: ItemPropertiesAll { enchanting: enchants, grading: vec![] },
        }
    }

    fn enchant_recipe(outcomes: Vec<EnchantOutcome>) -> ItemModRecipe {
        ItemModRecipe {
            crafting_type_id: Uuid::from_u128(0x1),
            duration_ms: 0,
            kind: "enchant".into(),
            outcomes,
        }
    }

    #[test]
    fn temper_sets_level_and_keeps_enchants() {
        let existing = item_with(0, vec![prop(1), prop(2)]);
        let out = apply_item_mod(&existing, 10, None, Uuid::from_u128(0x99));
        assert_eq!(out.tempering_level, 10);
        assert_eq!(out.properties.enchanting.len(), 2, "existing enchants preserved");
        assert_eq!(out.durability, 300.0);
        assert_eq!(out.item_template_id, existing.item_template_id);
    }

    #[test]
    fn enchant_applies_outcome_and_keeps_tempering() {
        let existing = item_with(5, vec![]);
        let recipe = enchant_recipe(vec![EnchantOutcome {
            enchanting: vec![prop(0xAA), prop(0xBB), prop(0xCC)],
            arcane_tier: Some(2),
        }]);
        let out = apply_item_mod(&existing, 0, Some(&recipe), Uuid::from_u128(0x7));
        assert_eq!(out.properties.enchanting.len(), 3, "enchants applied from outcome");
        assert_eq!(out.tempering_level, 5, "tempering preserved on enchant");
    }

    #[test]
    fn enchant_pick_is_deterministic_per_item() {
        let recipe = enchant_recipe(vec![
            EnchantOutcome { enchanting: vec![prop(1)], arcane_tier: None },
            EnchantOutcome { enchanting: vec![prop(2), prop(3)], arcane_tier: None },
        ]);
        let existing = item_with(0, vec![]);
        // idx = item_id % 2 → id 0 picks outcome 0 (len 1), id 1 picks outcome 1 (len 2)
        let a = apply_item_mod(&existing, 0, Some(&recipe), Uuid::from_u128(0));
        let b = apply_item_mod(&existing, 0, Some(&recipe), Uuid::from_u128(1));
        assert_eq!(a.properties.enchanting.len(), 1);
        assert_eq!(b.properties.enchanting.len(), 2);
        // same id → same outcome (deterministic, no state)
        let a2 = apply_item_mod(&existing, 0, Some(&recipe), Uuid::from_u128(0));
        assert_eq!(a2.properties.enchanting.len(), 1);
    }

    #[test]
    fn enchant_without_recipe_is_lenient_noop() {
        let existing = item_with(3, vec![prop(1)]);
        let out = apply_item_mod(&existing, 0, None, Uuid::from_u128(0x5));
        assert_eq!(out.tempering_level, 3);
        assert_eq!(out.properties.enchanting.len(), 1, "unchanged when no recipe");
    }

    #[test]
    fn finish_preserves_stored_item_id() {
        // temper/enchant store the ORIGINAL backpack item id in results; finish must
        // return it unchanged (re-minting it desynced the client → temper hang).
        let item_id = "fad31819-b941-4446-a229-e22b3647b142";
        let results = serde_json::json!({"items":[{
            "id": item_id, "itemTemplateId": "616b64ef-4184-4efb-af55-1a3f122431dc",
            "temperingLevel": 10, "durability": 675.0
        }]});
        let r = reward_from_results(&results);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].id.to_string(), item_id, "finish must preserve the item id");
        assert_eq!(r.items[0].item.tempering_level, 10);
    }

    #[test]
    fn plain_craft_remint_makes_ids_unique() {
        // plain-craft recipe ids are shared placeholders → must be unique per craft.
        let results = serde_json::json!({"items":[{
            "id": "00000000-0000-0000-0000-000000000001",
            "itemTemplateId": "616b64ef-4184-4efb-af55-1a3f122431dc"
        }]});
        let a = remint_result_item_ids(results.clone());
        let b = remint_result_item_ids(results);
        let ida = a["items"][0]["id"].as_str().unwrap();
        let idb = b["items"][0]["id"].as_str().unwrap();
        assert_ne!(ida, idb, "each craft gets a unique id");
        assert_ne!(ida, "00000000-0000-0000-0000-000000000001", "placeholder replaced");
    }

    /// An unknown plain-craft recipe (e.g. an un-captured alchemy brew) must NOT get
    /// crafting_type_id == recipe_id (the temper-hang / Alchemist-freeze bug). With no
    /// known recipes at all, the derived type still falls back to the real alchemy type
    /// — never the recipe id.
    #[test]
    fn unknown_recipe_derives_a_valid_craft_type_not_recipe_id() {
        use blades_lib::static_data::StaticData;
        let recipe_id = Uuid::from_u128(0xDEAD_BEEF);

        // No recipes loaded → alchemy fallback, and it must not equal the recipe id.
        let empty = StaticData::default();
        let ctid = derive_plain_craft_type(Uuid::from_u128(1), &empty);
        assert_ne!(ctid, recipe_id, "must never echo the recipe id");
        assert_eq!(
            ctid.to_string(),
            ALCHEMY_CRAFTING_TYPE_ID,
            "empty recipe set → alchemy fallback"
        );
    }

    /// Temper/enchant (itemId present) reports the universal craftingTypeId, NEVER the recipe
    /// id — an unknown mod-recipe (all real on-device tempers are outside our captured 23) must
    /// still map to a CraftingStation or the temper UI spins forever (fix e5659c9).
    #[test]
    fn item_mod_crafting_type_is_universal_never_recipe_id() {
        assert_eq!(
            item_mod_crafting_type(3).to_string(),
            "06c8087b-ede4-4ce7-8103-6c2067d18498",
            "temper (level>0) → universal temper type"
        );
        assert_eq!(
            item_mod_crafting_type(0).to_string(),
            "aaef180b-8ee7-474a-a7eb-0156aa5529ba",
            "enchant (level==0) → universal enchant type"
        );
    }

    /// When the alchemy crafting type is present in the loaded recipes, the derived type
    /// is exactly that (a client-mappable CraftingStation), regardless of building id.
    #[test]
    fn derive_prefers_alchemy_type_when_present() {
        use blades_lib::static_data::{Recipe, StaticData};
        let alchemy = Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap();
        let mut sd = StaticData::default();
        sd.recipes.insert(
            Uuid::from_u128(1),
            Recipe { crafting_type_id: alchemy, results: serde_json::json!({}), duration_ms: 0 },
        );
        // A different (forge) recipe also present — alchemy must still win.
        sd.recipes.insert(
            Uuid::from_u128(2),
            Recipe { crafting_type_id: Uuid::from_u128(0xF0), results: serde_json::json!({}), duration_ms: 0 },
        );
        assert_eq!(derive_plain_craft_type(Uuid::from_u128(9), &sd), alchemy);
    }

    /// The unknown-recipe result object is well-formed and NON-empty (a single stackable),
    /// so the client's craft-completion flow can finish instead of freezing on `{}`.
    #[test]
    fn unknown_recipe_result_is_well_formed_nonempty() {
        let recipe_id = Uuid::from_u128(0xC0FFEE);
        let results = serde_json::json!({ "stackableItems": { recipe_id.to_string(): 1 } });
        // finish must be able to build a non-empty reward from it.
        let reward = reward_from_results(&results);
        assert!(!reward.stackable_items.is_empty(), "unknown-recipe result yields a real grant");
        assert_eq!(reward.stackable_items.get(&recipe_id), Some(&1));
    }
}
