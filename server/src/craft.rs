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
//! item, whose `arcaneTier` is then stamped onto the item (retail's enchanted items
//! carry the tier they end at; `Item::arcane_tier` holds it, absent staying absent).

use std::{
    borrow::Cow,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    get, http::StatusCode, post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, RewardItem, apply_reward, remove_backpack_item};
use blades_lib::features::repair::RepairData;
use blades_lib::server_state::CraftJob;
use blades_lib::static_data::{CraftResultShape, ItemModRecipe};
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
    results: Cow<'a, Value>,
    version: u32,
}

impl<'a> CraftJobWire<'a> {
    /// Build the wire record for a stored job, running it through
    /// [`repaired_craft_fields`] first. Every craft record the client ever sees goes
    /// through here, so this is the single choke point for the
    /// `craftingTypeId != recipeId` + non-empty-`results` invariants.
    fn from_job(
        job: &'a CraftJob,
        user_id: Uuid,
        character_id: Uuid,
        static_data: &blades_lib::static_data::StaticData,
        repair_data: &RepairData,
    ) -> Self {
        let (crafting_type_id, results) = repaired_craft_fields(job, static_data, repair_data);
        CraftJobWire {
            id: job.id,
            user_id,
            character_id,
            building_id: job.building_id,
            recipe_id: job.recipe_id,
            crafting_type_id,
            completed_at: job.completed_at_ms,
            batch_size: 1,
            results,
            version: 1,
        }
    }
}

/// Build the wire records for a character's craft jobs, then enforce the one invariant
/// that only the whole list can see: **retail never showed two craft jobs on the same
/// station.**
///
/// Across the 135 captured retail `GET /crafts` snapshots, every single
/// `(buildingId, craftingTypeId)` group holds exactly one job — 238 groups, 238
/// singletons, no exceptions. Our own snapshots reach EIGHT on one group, because the
/// owner's Forge holds a pile of forge crafts that older builds all stored as Alchemy.
///
/// While they say "Alchemy" that pile is inert: their building is the Forge, whose
/// stations are Smithing / Tempering / Repair / Salvaging, so there is no Alchemy station
/// for the client to bind them to and it never looks at them. Correcting each one
/// individually — which [`repaired_craft_fields`] now can, since the results can be
/// rebuilt as `items` — would bind all eight to the Forge's one real Smithing station at
/// once. That is a second shape retail never emitted, on top of the pair mismatch that
/// PR #40 was about, and it would be reached on exactly the character that got stuck on
/// the startup screen. Making the `(craftingTypeId, results)` pair legal is not licence
/// to invent a station occupancy instead.
///
/// So the correction is rationed rather than abandoned. The guarantee is precisely: **a
/// job we corrected is always alone on the station we moved it to.**
///
///   * A job whose type we did NOT change always wins — it is the one the client stored,
///     and a repair may not shoulder a real job off its own bench.
///   * Among corrected jobs, the deterministic winner is the earliest `completedAt`,
///     tie-broken by job id, so the same list renders the same way on every poll instead
///     of following whatever order the rows came back from Postgres in.
///   * Every other corrected job reverts to its stored `craftingTypeId` AND its stored
///     `results` together — back to the inert pile that loads today. Both fields revert,
///     never one: reverting the name alone would emit Alchemy + `items`, the original
///     defect wearing the other shoe.
///
/// What this deliberately does NOT do is thin out the inert pile itself. After one row is
/// promoted the others are still five jobs nominally on "Alchemy" at the Forge — exactly
/// the state production serves today, and it loads, because that station does not exist
/// there for them to bind to. The only way to reduce that group further would be to drop
/// craft jobs from the response, which would delete a player's crafts to tidy a number.
/// So the bar here is "never make a bindable station hold more than retail did, and never
/// make any group worse than it already is", not "force every group to one".
///
/// The player therefore gets one honestly-named forge bench where they had none, and no
/// station the client can actually bind ends up holding a crowd.
/// True when a job's STORED `craftingTypeId` cannot be served to the client at all: an
/// item uuid (the recipe's own id) or nil in the `CraftingStation` slot. Never a
/// legitimate value — a CraftingType and a Recipe are different game-data objects and
/// never share an id — and emitting one is report #34's hang, because the client's
/// `GetCraftingStation(<uuid>)` returns null and the town-build coroutine never completes.
fn stored_crafting_type_is_unserveable(job: &CraftJob) -> bool {
    job.crafting_type_id == job.recipe_id || job.crafting_type_id.is_nil()
}

fn craft_wires<'a>(
    jobs: &'a [CraftJob],
    user_id: Uuid,
    character_id: Uuid,
    static_data: &blades_lib::static_data::StaticData,
    repair_data: &RepairData,
) -> Vec<CraftJobWire<'a>> {
    let mut wires: Vec<CraftJobWire<'a>> = jobs
        .iter()
        .map(|job| CraftJobWire::from_job(job, user_id, character_id, static_data, repair_data))
        .collect();

    // Demoting a job puts it back on its STORED station, and that station may be one
    // another job was corrected onto — so this runs to a fixed point rather than in a
    // single pass. Each round demotes at least one job and a demoted job is never
    // re-promoted, so it settles within `jobs.len()` rounds.
    for _ in 0..=jobs.len() {
        // (buildingId, emitted craftingTypeId) -> indices, in a stable order.
        let mut groups: std::collections::BTreeMap<(Uuid, Uuid), Vec<usize>> = Default::default();
        for (i, w) in wires.iter().enumerate() {
            groups.entry((w.building_id, w.crafting_type_id)).or_default().push(i);
        }

        let mut demote: Vec<usize> = Vec::new();
        for (_, members) in groups {
            if members.len() < 2 {
                continue;
            }
            // Anything we did not relabel holds its station unconditionally: it is what
            // the client stored, and a repair may not shoulder a real job aside.
            let untouched_present = members
                .iter()
                .any(|&i| wires[i].crafting_type_id == jobs[i].crafting_type_id);
            let mut corrected: Vec<usize> = members
                .iter()
                .copied()
                .filter(|&i| wires[i].crafting_type_id != jobs[i].crafting_type_id)
                .collect();
            corrected.sort_by_key(|&i| (jobs[i].completed_at_ms, jobs[i].id));

            // A job whose STORED type cannot be served at all has nowhere safe to go
            // back to: reverting it would put the unmappable `craftingTypeId` back on the
            // wire, which is report #34's hang — `GetCraftingStation` returns null and the
            // town-build coroutine never finishes. A crowded station is an unproven risk;
            // an unmappable type is a measured one. So these keep their correction and
            // only the genuinely demotable rows yield.
            let (pinned, demotable): (Vec<usize>, Vec<usize>) = corrected
                .into_iter()
                .partition(|&i| stored_crafting_type_is_unserveable(&jobs[i]));

            // `.iter().copied().next()` rather than `.first()`: diesel's prelude puts its
            // own `first` in scope and it shadows the slice method.
            let keep = if untouched_present || !pinned.is_empty() {
                None
            } else {
                demotable.iter().copied().next()
            };
            demote.extend(demotable.into_iter().filter(|&i| Some(i) != keep));
        }
        if demote.is_empty() {
            break;
        }
        for i in demote {
            wires[i].crafting_type_id = jobs[i].crafting_type_id;
            wires[i].results = Cow::Borrowed(&jobs[i].results);
        }
    }
    wires
}

// ── GET /crafts ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct GetCraftsResponse {
    crafts: Vec<Value>,
}

/// `GET /crafts` — returns the character's active craft jobs.
/// The repair gate reads this list; an empty list unblocks repair.
#[get("/api/game/v1/public/characters/{character_id}/crafts")]
pub async fn get_crafts(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetCraftsResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let entry = load_owned(&mut conn, character_id, user_id).await?;
            let crafts = craft_wires(
                &entry.server_state.0.craft_jobs,
                user_id,
                character_id,
                &globals.static_data,
                &globals.repair_data,
            )
            .into_iter()
            .map(|w| serde_json::to_value(w).unwrap_or(Value::Null))
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
#[post("/api/game/v1/public/characters/{character_id}/crafts")]
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
                        // A smith (Forge) craft resolves to a REAL craftable: the client's
                        // RecipeManager gates the list by forge level and sends its own
                        // recipeId; recipes.json captured only ~4 smithing recipes, so most
                        // forge crafts land here. Mint the REAL item at its gradeIndex (a
                        // proper instanced backpack item, like a known recipe) and ALWAYS
                        // report the Smithing craftingTypeId — never the recipe id (echoing
                        // recipe_id hangs the client, fix e5659c9).
                        if let Some(craftable) =
                            globals.static_data.smith_craftables.resolve(&recipe_id)
                        {
                            // Prefer the APK's per-recipe answer; the Smithing constant
                            // stays as the fallback for a craftable resolved by template
                            // id (which is not a recipe id and so is not in the table).
                            let crafting_type_id =
                                apk_crafting_type(&recipe_id, &globals.static_data)
                                    .unwrap_or_else(|| smithing_crafting_type(&globals.static_data));
                            let results = mint_smith_craftable(craftable, tempering_level);
                            (results, crafting_type_id, craftable.duration_ms)
                        } else {
                            // Not a smith craftable → the alchemy/other plain-craft path.
                            // recipes.json is a PARTIAL capture (retail has far more alchemy
                            // recipes than we captured). For an unknown recipe we previously
                            // echoed crafting_type_id = recipe_id — exactly the temper-hang
                            // class of bug (fix e5659c9): the client can't map that id to a
                            // CraftingStation, so the Alchemist screen freezes and the app
                            // restarts. Derive a VALID crafting_type_id from context instead,
                            // and return a well-formed (non-empty) result so the craft-
                            // completion flow can finish.
                            // The APK table answers this outright for every recipe the
                            // client ships; `derive_plain_craft_type` is now only for a
                            // recipe absent from the shipped data entirely.
                            let crafting_type_id =
                                apk_crafting_type(&recipe_id, &globals.static_data).unwrap_or_else(
                                    || derive_plain_craft_type(building_id, &globals.static_data),
                                );
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

            let craft_wire = serde_json::to_value(CraftJobWire::from_job(
                &job,
                user_id,
                character_id,
                &globals.static_data,
                &globals.repair_data,
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
    /// `true` when the player paid gems to collect before the timer elapsed. This is
    /// BILLED, from the SAME global curve as town construction: retail's
    /// `RecipeData._skipTimeData` and `BuildingConstructionDataList._skipTimeData`
    /// point at one `SkipTimeCostTable` asset. See [`blades_lib::economy::skip_time`].
    #[serde(default)]
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
/// `speedUp: true` charges gems for the time still on the clock — the same
/// [`blades_lib::economy::skip_time`] curve town construction uses. A player who
/// cannot afford it gets an error and keeps the job; the alternative (collect early
/// for free) is what this endpoint used to do.
#[post("/api/game/v1/public/characters/{character_id}/crafts/{craft_id}/finish")]
pub async fn finish_craft(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<FinishCraftRequest>,
) -> Result<Json<FinishCraftResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, craft_id) = path.into_inner();
    let speed_up = body.into_inner().speed_up;
    let globals = app_state.get_ref().clone();
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

            // Bill the speed-up from the job's own stored `completedAt` — never from
            // anything the client sent. A job whose timer already elapsed is free
            // (the player is just collecting), so a client that sets `speedUp` on a
            // finished craft is not overcharged.
            charge_craft_speed_up(
                speed_up,
                globals.skip_time_costs.as_ref(),
                job.completed_at_ms,
                now_ms(),
                &mut entry.wallet.0,
            )?;

            // Build a RewardGrant from the job's results, re-minting item ids. Repair
            // first, for the same reason `GET /crafts` does: a pre-fix job stored
            // `results: {}`, so collecting it granted nothing and left the player with a
            // permanently uncollectable craft.
            let (_, repaired_results) = repaired_craft_fields(&job, &globals.static_data, &globals.repair_data);
            let reward = reward_from_results(&repaired_results);

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

/// Bill a craft speed-up against the wallet — the whole billed path minus the
/// database, so tests exercise exactly what the handler runs.
///
/// The price comes from the job's STORED `completedAt`, never from the request: the
/// client sends only the `speedUp` flag, and a client-supplied cost is not a cost.
/// The curve is the shared [`blades_lib::economy::skip_time`] one (retail's
/// `RecipeData._skipTimeData` is the same asset as the town's).
///
/// * `speed_up == false`, no table, or an already-elapsed job → charge nothing.
/// * not enough gems → the economy 400, wallet untouched; the job stays uncollected
///   rather than being handed over for free.
fn charge_craft_speed_up(
    speed_up: bool,
    table: Option<&blades_lib::economy::skip_time::SkipTimeCostTable>,
    completed_at_ms: i64,
    now: i64,
    wallet: &mut CompleteWallet,
) -> Result<Vec<blades_lib::economy::Price>, BladeApiError> {
    if !speed_up {
        return Ok(Vec::new());
    }
    blades_lib::economy::skip_time::charge_skip_time(table, completed_at_ms - now, wallet)
        .map_err(BladeApiError::from_economy)
}

/// Repair a STORED craft job on the way out to the client.
///
/// The create path has been hardened three times (temper e5659c9/985cc31, alchemy
/// b874c26, forge 462c279) so it no longer emits a bad record — but nothing ever
/// repaired the jobs already sitting in `server_state.craft_jobs`. `GET /crafts`
/// echoed them verbatim, so a row written by a pre-fix build is re-served forever:
///
///   * `craftingTypeId == recipeId` — an *item* uuid in the CraftingStation slot.
///     `GetCraftingStation(<item uuid>)` returns null, the town-build coroutine
///     never completes, and the player is stuck on the loading screen with no
///     client-side way out (report #34).
///   * `results: {}` on a job whose `completedAt` is in the past — the client shows
///     a collectable craft with nothing to collect, and `finish` grants nothing.
///
///   * a MAPPABLE but WRONG `craftingTypeId` — the third defect, only visible now that
///     the APK table exists. Every craft written before this table fell back to
///     Alchemy when its recipe was not one of the ~34 captured ones, so the live DB
///     holds forge crafts (Iron Hand Axe, Iron Greatsword, Dragonbone Longsword, …)
///     stored as ALCHEMY jobs. They load fine and name the wrong bench forever.
///     **Correcting one of these is only safe when the corrected record is still a
///     record retail could have emitted** — see [`reconcile_type_with_results`]. It
///     was not, and shipping the correction hung the loading screen for every player
///     holding such a row.
///
/// All three are repaired here with the SAME resolution order the create path uses,
/// and only from mappings that already exist in the extracted game data: the APK's
/// `recipe_crafting_types.json` first (per-recipe, authoritative, covers every recipe
/// the client ships), else a known recipe's own `craftingTypeId`, else the Smithing
/// type for a resolvable forge craftable, else [`derive_plain_craft_type`]. No mapping
/// is invented — a recipe absent from the APK data keeps the old fallback chain.
///
/// One deliberate exception: a stored **Tempering / Enchanting** type is never
/// second-guessed. Those two come from the request's `temperingLevel`
/// ([`item_mod_crafting_type`]), which the recipe table cannot see, so the stored
/// value carries information the table does not have.
///
/// A well-formed job is returned borrowed and untouched.
fn repaired_craft_fields<'a>(
    job: &'a CraftJob,
    static_data: &blades_lib::static_data::StaticData,
    repair_data: &RepairData,
) -> (Uuid, Cow<'a, Value>) {
    // An item uuid (or nil) in the CraftingStation slot — never a legitimate value:
    // a CraftingType and a Recipe are different game-data objects and never share an id.
    let crafting_type_is_unmappable = stored_crafting_type_is_unserveable(job);
    let from_apk = apk_crafting_type(&job.recipe_id, static_data);
    // A stored type the APK contradicts is a MISLABEL — written by a build that had no
    // recipe→station table and guessed. Correct it, except for the two mod-craft types,
    // which encode the request's `temperingLevel` rather than the recipe.
    let crafting_type_is_mislabelled = match from_apk {
        Some(right) => {
            right != job.crafting_type_id
                && job.crafting_type_id != item_mod_crafting_type(1)
                && job.crafting_type_id != item_mod_crafting_type(0)
        }
        None => false,
    };
    let results_are_empty = match &job.results {
        Value::Object(map) => map.is_empty(),
        Value::Null => true,
        _ => false,
    };
    if !crafting_type_is_unmappable && !crafting_type_is_mislabelled && !results_are_empty {
        return (job.crafting_type_id, Cow::Borrowed(&job.results));
    }

    let known_recipe = static_data.recipes.get(&job.recipe_id);
    let smith_craftable = static_data.smith_craftables.resolve(&job.recipe_id);

    let candidate_crafting_type = if crafting_type_is_mislabelled {
        from_apk.expect("mislabelled implies the APK has an answer")
    } else if !crafting_type_is_unmappable {
        job.crafting_type_id
    } else if let Some(right) = from_apk {
        // The APK's own recipe -> CraftingType table: the RIGHT bench, not merely a
        // mappable one. Ahead of the captured recipe and the smith guess because it is
        // per-recipe data rather than a capture subset or a template-id coincidence
        // (the loader test pins that it agrees with all 34 captured rows).
        right
    } else if let Some(recipe) = known_recipe {
        recipe.crafting_type_id
    } else if smith_craftable.is_some() {
        smithing_crafting_type(static_data)
    } else {
        derive_plain_craft_type(job.building_id, static_data)
    };

    // The recipe's real output, from the APK. Lets a forge row whose results are only
    // our own approximation be rebuilt into the shape its bench can actually restore,
    // which is the one thing that makes correcting the bench name safe.
    let output_template = static_data
        .recipe_crafting_types
        .output_template_of(&job.recipe_id);
    let rebuildable = results_are_our_own_approximation(job)
        && output_template.is_some()
        && static_data
            .recipe_crafting_types
            .result_shape_of_type(&candidate_crafting_type)
            == Some(CraftResultShape::Instanced);

    let results = if !results_are_empty && !rebuildable {
        Cow::Borrowed(&job.results)
    } else if let Some(recipe) = known_recipe {
        // Fresh instanced ids, exactly as a newly-created job would get.
        Cow::Owned(remint_result_item_ids(recipe.results.clone()))
    } else if let Some(craftable) = smith_craftable {
        // Tempering level is not stored on the job, so a repaired forge result mints at
        // the base level; the item itself is real and finish-able.
        Cow::Owned(mint_smith_craftable(craftable, 0))
    } else if let Some(template) = output_template {
        // Reached only when the results are empty or rebuildable, both of which mean we
        // are free to mint: the recipe's REAL output, in the shape its bench restores.
        Cow::Owned(mint_recipe_output(
            job,
            template,
            candidate_crafting_type,
            static_data,
            repair_data,
        ))
    } else {
        // Same approximation the unknown-recipe create path makes: one stackable of the
        // recipe's own id — well-formed and grantable, so the completion flow can finish.
        Cow::Owned(serde_json::json!({ "stackableItems": { job.recipe_id.to_string(): 1 } }))
    };

    reconcile_type_with_results(
        job,
        candidate_crafting_type,
        results,
        static_data,
        crafting_type_is_unmappable,
    )
}

/// True when a job's `results` are OUR OWN unknown-recipe approximation rather than
/// anything retail sent — i.e. a single `stackableItems` entry keyed by the job's own
/// `recipeId`.
///
/// This is the discriminator that makes rebuilding safe, and it is exact rather than
/// heuristic. A recipe id in a `stackableItems` key is a category error — that slot holds
/// an `itemTemplateId`, and the two are disjoint namespaces — so it can only have been
/// written by our own fallback. Measured over every captured craft record:
///
/// | source                              | stackable entries | keyed by the recipe id |
/// |-------------------------------------|-------------------|------------------------|
/// | retail (`blades.bgs.services`)      | 108               | **0**                  |
/// | ours (`127.0.0.1:8087`)             | 328               | **328**                |
///
/// So a self-keyed stackable is never data we must preserve, and a stackable keyed by
/// anything else is never ours to overwrite. Rebuilding is gated on this and nothing
/// else: a genuine retail Alchemy result keyed by a real template is left untouched even
/// though it is also "a stackable on a job we are relabelling".
fn results_are_our_own_approximation(job: &CraftJob) -> bool {
    job.results
        .get("stackableItems")
        .and_then(Value::as_object)
        .is_some_and(|m| {
            m.len() == 1 && m.contains_key(&job.recipe_id.to_string())
        })
}

/// Mint a recipe's REAL output into the `results` shape its bench restores.
///
/// The APK's `Recipe._outputs[]._itemTemplate` gives the item template a recipe produces
/// (`recipe_crafting_types.json`, 617/617 Smithing recipes). With it we can finally do
/// what [`reconcile_type_with_results`] had to refuse: correct a forge craft's bench name
/// AND give it results the Smithing station can actually restore, instead of choosing
/// between a wrong name and a shape retail never sent.
///
/// Which shape depends on the station, exactly as retail paired them (482/482 captured
/// craft records, no exceptions):
///
///   * Instanced (Smithing / Tempering / Enchanting) → `{"items":[{id, itemTemplateId,
///     temperingLevel, durability}]}`, matching both the captured smithing records and
///     `recipes.json`.
///   * Stackable (Alchemy / DecorationCrafting) → `{"stackableItems":{"<template>": 1}}`
///     — the template id, which is what retail keyed it by, not the recipe id.
///
/// The instanced item's `id` is derived deterministically from the craft job id, NOT
/// randomly. We are reconstructing a record whose real item id was never stored, and this
/// function runs on every read: a fresh `v4` would hand the client a different item id on
/// every `GET /crafts` poll for the same job. A v8 uuid over the job id is stable, unique
/// per job, and self-evidently synthesized.
///
/// `temperingLevel` is 0 because a job does not store the level it was crafted at, and
/// `durability` is that template's real level-0 maximum from the APK's own
/// `item_durability.json` ladder rather than a constant — a flat 150.0 on an Iron Dagger
/// (max 50.0) would hand the client an item at 300% condition. An unknown template falls
/// back to [`SMITH_MINT_DURABILITY`], the value the existing forge mint already ships.
fn mint_recipe_output(
    job: &CraftJob,
    template: Uuid,
    crafting_type_id: Uuid,
    static_data: &blades_lib::static_data::StaticData,
    repair_data: &RepairData,
) -> Value {
    if static_data
        .recipe_crafting_types
        .result_shape_of_type(&crafting_type_id)
        == Some(CraftResultShape::Stackable)
    {
        return serde_json::json!({ "stackableItems": { template.to_string(): 1 } });
    }
    serde_json::json!({
        "items": [{
            "id": Uuid::new_v8(job.id.into_bytes()).to_string(),
            "itemTemplateId": template.to_string(),
            "temperingLevel": 0,
            "durability": repair_data
                .max_durability(template, 0)
                .unwrap_or(SMITH_MINT_DURABILITY),
        }]
    })
}

/// The shape of a `results` object as it will go on the wire, or `None` when it is
/// empty / neither shape (the caller then has nothing to check against).
fn observed_result_shape(results: &Value) -> Option<CraftResultShape> {
    let obj = results.as_object()?;
    if obj.get("items").and_then(Value::as_array).map_or(false, |a| !a.is_empty()) {
        return Some(CraftResultShape::Instanced);
    }
    if obj
        .get("stackableItems")
        .and_then(Value::as_object)
        .map_or(false, |m| !m.is_empty())
    {
        return Some(CraftResultShape::Stackable);
    }
    None
}

/// Refuse to emit a `(craftingTypeId, results)` pair that retail never emitted.
///
/// ## Why this exists
///
/// `craftingTypeId` does not merely *name a bench for the UI*. The client binds the
/// job to the `CraftingStation` of that type and then restores the station's
/// in-progress craft FROM `results` — and each station knows only one result shape.
/// The captures are unambiguous (482/482 retail craft records, see
/// [`blades_lib::static_data::RecipeCraftingTypes::result_shape_of_type`]): Smithing,
/// Tempering and Enchanting always carry `results.items`; Alchemy and
/// DecorationCrafting always carry `results.stackableItems`.
///
/// Report #35 relabelled a "mappable but wrong" stored type from the APK table and
/// deliberately left `results` alone ("only the bench changes"). For the eleven live
/// forge crafts stored as Alchemy that meant emitting **Smithing + `stackableItems`**,
/// a pair that appears nowhere in retail. The affected characters stalled in loading
/// pass 2 with `town_level == -1`: the same stall site as report #34, reached a
/// different way. As Alchemy those rows were inert — the Forge (building type
/// `26fdb92f-…`, whose stations are Smithing / Tempering / Repair / Salvaging) has no
/// Alchemy station for them to bind to — so relabelling them is what *activated* a
/// malformed record that had been harmlessly ignored for weeks.
///
/// ## The rule
///
/// If the type we are about to emit expects a different result shape than the results
/// we actually have, the type loses, not the results — we can always fabricate a
/// plausible bench NAME, but we cannot fabricate the item a Smithing station wants
/// (`smith_craftables.json` resolves none of the live forge recipes, and the recipe id
/// is not an `itemTemplateId`: `items.json` has no entry for `b949b05f-…`). So:
///
///   * stored type still **mappable** → keep it. Wrong bench name, working game. This
///     is exactly what production has been serving, and it loads.
///   * stored type **unmappable** (report #34 — it cannot be served at all) → pick a
///     mappable type whose shape matches the results we have, which is what the
///     pre-table build did and what unblocked #34 in the first place.
///
/// The cost is cosmetic and bounded: some forge crafts keep saying "Alchemy". Naming
/// them correctly needs a recipe→`itemTemplateId` mapping so the results can be
/// rebuilt into `items` alongside the relabel; that is a data-extraction change, not
/// something to guess at here.
///
/// With no table loaded `result_shape_of_type` returns `None` and this is a no-op, so
/// the gate can never make an un-tabled deployment worse.
fn reconcile_type_with_results<'a>(
    job: &'a CraftJob,
    candidate: Uuid,
    results: Cow<'a, Value>,
    static_data: &blades_lib::static_data::StaticData,
    stored_type_is_unmappable: bool,
) -> (Uuid, Cow<'a, Value>) {
    let (Some(wanted), Some(have)) = (
        static_data.recipe_crafting_types.result_shape_of_type(&candidate),
        observed_result_shape(&results),
    ) else {
        return (candidate, results);
    };
    if wanted == have {
        return (candidate, results);
    }

    if !stored_type_is_unmappable {
        // A real CraftingType is already stored; the only thing wrong with it is the
        // name. Leave it — the player loads.
        return (job.crafting_type_id, results);
    }

    // The stored type cannot be served at all, so something must be chosen. Choose one
    // that matches the results, rather than one that matches the recipe.
    let fallback = match have {
        CraftResultShape::Stackable => derive_plain_craft_type(job.building_id, static_data),
        // Not observed in production (an instanced result under a stackable-producing
        // type), but the same rule applies in the mirror direction.
        CraftResultShape::Instanced => smithing_crafting_type(static_data),
    };
    (fallback, results)
}

/// The bench a recipe belongs to, from the APK-extracted `recipe_crafting_types.json`
/// (`RecipeData._recipeMap` — every recipe the client ships, 2,978 across the 7
/// `CraftingType`s). `None` only when the recipe is not in the shipped client data at
/// all, in which case the caller keeps its fallback.
///
/// This is what closes the hole the create-path and read-path fixes left open. Before
/// the table existed the only recipe→bench data on the server was the ~34 captured rows
/// in `recipes.json`, so an un-captured craft could be given a *valid* CraftingType (any
/// real station unblocks `GetCraftingStation`) but not the *right* one — report #34's
/// second job is a forge craft that fell through to the Alchemy fallback, which unblocks
/// the loading screen while naming the wrong bench to the player. With the table it
/// resolves to Smithing, from data, with nothing guessed.
fn apk_crafting_type(
    recipe_id: &Uuid,
    static_data: &blades_lib::static_data::StaticData,
) -> Option<Uuid> {
    static_data.recipe_crafting_types.crafting_type_of(recipe_id)
}

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

/// The Smithing station's `craftingTypeId`, ALWAYS echoed for a forge craft (never the
/// recipe id — echoing recipe_id hangs the client, fix e5659c9). Prefer the value loaded
/// from `smith_craftables.json`; fall back to the known Smithing station UUID so the
/// client can still map the CraftingStation if the file is missing.
const SMITHING_CRAFTING_TYPE_ID: &str = "a47707e6-59e9-43b0-a29f-6d703acd8171";

fn smithing_crafting_type(static_data: &blades_lib::static_data::StaticData) -> Uuid {
    static_data
        .smith_craftables
        .smithing_crafting_type_id
        .unwrap_or_else(|| {
            Uuid::parse_str(SMITHING_CRAFTING_TYPE_ID).expect("valid smithing craft type uuid")
        })
}

/// Full durability stamped on a freshly-forged item. The captured smithing recipe result
/// carried a per-tier durability that `smith_craftables.json` does not model per item;
/// 150.0 matches the captured Fine-tier smithing result and is a sane full-durability
/// value (the repair endpoint restores from `item_durability.json` at use time anyway).
const SMITH_MINT_DURABILITY: f64 = 150.0;

/// Mint a smith craftable into a well-formed `{"items":[...]}` results object: a fresh
/// instanced item at the craftable's `itemTemplateId`, with the requested `temperingLevel`
/// and full durability — mirroring how a known smithing recipe's result is shaped
/// (`{id, itemTemplateId, temperingLevel, durability}`). Grade-specific `GRADING` affix
/// property UUIDs are not in this config, so no (malformed) synthetic grading is attached.
/// A unique id is minted now so repeated crafts of the same template never collide when
/// `finish` preserves the id.
///
/// DELIBERATELY still emits no `grade`, and the older claim that grade "is not a separate
/// wire field" was wrong: retail plainly sends a per-item `grade` (32 of the 131 instanced
/// items in `reference/capture-599.jsonl` carry one), and `Item` now models it. What is
/// still missing is the MAPPING. `smith_craftables.json`'s `gradeIndex` runs 0..=8 across
/// its 137 entries while every observed wire `grade` is 1..=6, so they are on different
/// scales and equating them would stamp values retail never emitted onto forge output —
/// worse than omitting the key, which at least matches the pre-existing shape. Resolving
/// this needs either a captured forge result carrying both, or the client's grade table.
fn mint_smith_craftable(craftable: &blades_lib::static_data::SmithCraftable, tempering_level: u64) -> Value {
    serde_json::json!({
        "items": [{
            "id": Uuid::new_v4().to_string(),
            "itemTemplateId": craftable.item_template_id.to_string(),
            "temperingLevel": tempering_level,
            "durability": SMITH_MINT_DURABILITY,
        }]
    })
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
            // The captured outcome carries the arcane tier the item ENDS at, and it was
            // parsed into `EnchantOutcome::arcane_tier` all along — there was simply no
            // field on `Item` to assign it to, so every enchant produced an item retail
            // would have stamped with an arcaneTier and we returned without one.
            item.arcane_tier = rec.outcomes[idx].arcane_tier;
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
            // `grade` / `arcaneTier` ride through VERBATIM, absent staying absent. Both
            // are real captured keys in `recipes.json`'s results (5 grades, 6 arcane
            // tiers) and were silently dropped here for as long as `Item` had no field
            // to put them in — the craft handed the client back a rarer item than the
            // capture said it was.
            let grade = item_val.get("grade").and_then(|v| v.as_u64());
            let arcane_tier = item_val.get("arcaneTier").and_then(|v| v.as_u64());
            reward.items.push(RewardItem {
                id, // preserve the stored id (temper/enchant keep the item's own id)
                item: Item {
                    item_template_id: template_id,
                    grade,
                    tempering_level,
                    durability,
                    properties,
                    arcane_tier,
                },
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
            grade: None,
            arcane_tier: None,
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
        // The outcome's arcane tier must land ON THE ITEM. This fixture already declared
        // `arcane_tier: Some(2)` before the field existed on `Item`, so the value was
        // parsed out of `item_mod_recipes.json` and then dropped: retail stamps an
        // enchanted item with the tier it ends at, and we returned it bare.
        assert_eq!(out.arcane_tier, Some(2), "enchant must stamp the outcome's arcaneTier");
    }

    /// An enchant outcome with NO arcane tier must leave the item without one, rather
    /// than defaulting it to 0 — `arcaneTier: 0` is a value retail never sent.
    #[test]
    fn enchant_without_an_arcane_tier_leaves_the_item_bare() {
        let existing = item_with(5, vec![]);
        let recipe = enchant_recipe(vec![EnchantOutcome {
            enchanting: vec![prop(0xAA)],
            arcane_tier: None,
        }]);
        let out = apply_item_mod(&existing, 0, Some(&recipe), Uuid::from_u128(0x7));
        assert_eq!(out.arcane_tier, None);
        let j = serde_json::to_string(&out).unwrap();
        assert!(!j.contains("arcaneTier"), "absent arcane tier must be omitted: {j}");
    }

    /// `reward_from_results` hand-rolls an `Item` out of the stored results `Value`, so it
    /// is its own drop risk independent of the struct: a captured result carrying a grade
    /// and an arcane tier must arrive with both. `recipes.json` really does carry these (5
    /// grades, 6 arcane tiers), and every one of them was lost here.
    #[test]
    fn reward_from_results_carries_grade_and_arcane_tier() {
        let results = serde_json::json!({
            "items": [{
                "id": "3ad24023-f66f-4ef4-8f96-63166533bce5",
                "itemTemplateId": "9e0714d7-2a10-406d-a3eb-9b8ca95e14ad",
                "grade": 4,
                "arcaneTier": 2
            }]
        });
        let reward = reward_from_results(&results);
        assert_eq!(reward.items.len(), 1);
        assert_eq!(reward.items[0].item.grade, Some(4), "grade lost in reward_from_results");
        assert_eq!(reward.items[0].item.arcane_tier, Some(2), "arcaneTier lost in reward_from_results");

        // A gear result with neither key stays bare (no invented zeros).
        let plain = serde_json::json!({
            "items": [{
                "id": "3ad24023-f66f-4ef4-8f96-63166533bce5",
                "itemTemplateId": "9e0714d7-2a10-406d-a3eb-9b8ca95e14ad",
                "temperingLevel": 3,
                "durability": 90.0
            }]
        });
        let bare = reward_from_results(&plain);
        assert_eq!(bare.items[0].item.grade, None);
        assert_eq!(bare.items[0].item.arcane_tier, None);
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

    /// A smith craftable mints the REAL itemTemplateId at its grade (a proper instanced
    /// backpack item, `finish`-able) and reports the Smithing craftingTypeId — never the
    /// recipe id (echoing recipe_id hangs the client, fix e5659c9).
    #[test]
    fn smith_craftable_mints_real_item_and_smithing_type_not_recipe_id() {
        use blades_lib::static_data::{SmithCraftable, SmithCraftables, StaticData};

        let template = Uuid::from_u128(0x5A17_C0DE);
        let request_recipe_id = template; // client sends its own (== template) id
        let craftable = SmithCraftable {
            item_template_id: template,
            grade_index: 3,
            recipe_id: None,
            duration_ms: 12_345,
            name: Some("Ebony Longsword".into()),
        };
        let mut sd = StaticData::default();
        let smithing_type = Uuid::parse_str(SMITHING_CRAFTING_TYPE_ID).unwrap();
        sd.smith_craftables = SmithCraftables {
            smithing_crafting_type_id: Some(smithing_type),
            forge_building_type_id: Some(Uuid::from_u128(0xF0_47E)),
            by_recipe: Default::default(),
            by_template: {
                let mut m = std::collections::HashMap::new();
                m.insert(template, craftable.clone());
                m
            },
        };

        // The unknown-recipe smith path resolves + mints.
        let resolved = sd.smith_craftables.resolve(&request_recipe_id).expect("resolves by template");
        let results = mint_smith_craftable(resolved, 10);
        let ctid = smithing_crafting_type(&sd);

        // Crafting type is Smithing, NOT the recipe id.
        assert_eq!(ctid, smithing_type, "reports the Smithing crafting type");
        assert_ne!(ctid, request_recipe_id, "must never echo the recipe id");

        // The minted item is the real template, is finish-able, and carries the tempering.
        let reward = reward_from_results(&results);
        assert_eq!(reward.items.len(), 1, "one instanced item minted");
        assert_eq!(reward.items[0].item.item_template_id, template, "real itemTemplateId");
        assert_eq!(reward.items[0].item.tempering_level, 10, "requested tempering applied");
        assert!(reward.items[0].item.durability > 0.0, "minted with durability");
    }

    /// Resolution prefers the captured recipe id over the template id, and the two smith
    /// crafting-type consts agree (loaded value used when present, hardcoded fallback else).
    #[test]
    fn smith_resolve_by_recipe_id_and_type_fallback() {
        use blades_lib::static_data::{SmithCraftable, SmithCraftables, StaticData};
        let template = Uuid::from_u128(0xAA);
        let recipe = Uuid::from_u128(0xBB);
        let craftable = SmithCraftable {
            item_template_id: template,
            grade_index: 0,
            recipe_id: Some(recipe),
            duration_ms: 5,
            name: None,
        };
        let mut sd = StaticData::default();
        sd.smith_craftables = SmithCraftables {
            smithing_crafting_type_id: None, // force fallback
            forge_building_type_id: None,
            by_recipe: { let mut m = std::collections::HashMap::new(); m.insert(recipe, craftable.clone()); m },
            by_template: { let mut m = std::collections::HashMap::new(); m.insert(template, craftable.clone()); m },
        };
        assert!(sd.smith_craftables.resolve(&recipe).is_some(), "resolves by captured recipe id");
        // No loaded type → the known Smithing station UUID fallback (a real CraftingStation).
        assert_eq!(smithing_crafting_type(&sd).to_string(), SMITHING_CRAFTING_TYPE_ID);
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

    // ── Report #34: stored craft jobs must be REPAIRED on the way out ─────────
    //
    // The create path (tests above) has been hardened three times; the READ path
    // never was. `GET /crafts` echoes `server_state.craft_jobs` verbatim, so a job
    // written by an older build — `craftingTypeId == recipeId`, `results: {}` — is
    // re-served forever. `GetCraftingStation(<item uuid>)` returns null, the
    // town-build coroutine never completes, and the player loads forever with no
    // way out from the client side. These tests pin the invariant at the
    // serialization seam, the one place that covers BOTH freshly-created jobs and
    // already-poisoned stored ones.

    /// The exact rows on the affected character (`f0405dbc-…-e2434ae5b607`, report
    /// #34): `craftingTypeId` is an *item* uuid, not a CraftingType, and `results`
    /// is empty even though `completedAt` is in the past.
    fn poisoned_alchemy_job() -> CraftJob {
        CraftJob {
            id: Uuid::parse_str("de08437c-dbd6-425f-b013-160cdf94b55d").unwrap(),
            recipe_id: Uuid::parse_str("b5a2dbe9-d115-4bf2-99d9-558be1de3ef7").unwrap(),
            building_id: Uuid::parse_str("b782d584-01b7-4c2f-a019-81f56cf44993").unwrap(),
            crafting_type_id: Uuid::parse_str("b5a2dbe9-d115-4bf2-99d9-558be1de3ef7").unwrap(),
            completed_at_ms: 1_783_175_604_008,
            results: serde_json::json!({}),
        }
    }

    fn poisoned_smith_job() -> CraftJob {
        CraftJob {
            id: Uuid::parse_str("5e455253-dfa5-48f4-9c5a-4b699a40177f").unwrap(),
            recipe_id: Uuid::parse_str("fd13cfa0-0148-41c0-be70-b3d08852f673").unwrap(),
            building_id: Uuid::parse_str("cd83f12e-3a0d-4795-82ff-35c107ae07b3").unwrap(),
            crafting_type_id: Uuid::parse_str("fd13cfa0-0148-41c0-be70-b3d08852f673").unwrap(),
            completed_at_ms: 1_783_197_110_167,
            results: serde_json::json!({}),
        }
    }

    /// The third row on the same character — written AFTER the create-path fix. It is
    /// already well-formed and must survive the repair untouched.
    fn healthy_alchemy_job() -> CraftJob {
        CraftJob {
            id: Uuid::parse_str("4f732c4d-5601-4fbd-ba0e-a23c60bf43b5").unwrap(),
            recipe_id: Uuid::parse_str("b5a2dbe9-d115-4bf2-99d9-558be1de3ef7").unwrap(),
            building_id: Uuid::parse_str("b782d584-01b7-4c2f-a019-81f56cf44993").unwrap(),
            crafting_type_id: Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap(),
            completed_at_ms: 1_783_204_348_637,
            results: serde_json::json!({
                "stackableItems": { "b5a2dbe9-d115-4bf2-99d9-558be1de3ef7": 1 }
            }),
        }
    }

    /// A temper job as the (already fixed) create path writes it: universal temper
    /// CraftingType + the mutated item. Extends the temper regression coverage to the
    /// read path — the temper fix passing while alchemy/smith stayed broken is exactly
    /// what let report #34 through.
    fn healthy_temper_job() -> CraftJob {
        CraftJob {
            id: Uuid::from_u128(0x7E_9E_00),
            recipe_id: Uuid::from_u128(0xDEAD_BEEF),
            building_id: Uuid::from_u128(0xB1),
            crafting_type_id: item_mod_crafting_type(10),
            completed_at_ms: 1_783_204_348_637,
            results: serde_json::json!({"items":[{
                "id": "fad31819-b941-4446-a229-e22b3647b142",
                "itemTemplateId": "616b64ef-4184-4efb-af55-1a3f122431dc",
                "temperingLevel": 10, "durability": 675.0
            }]}),
        }
    }

    /// StaticData shaped like the deployed set: alchemy present in `recipes`, a
    /// smithing crafting type loaded. NEITHER report-#34 recipe id is in it (verified
    /// against the committed `recipes.json` / `smith_craftables.json`), so the repair
    /// must work from what it *does* have and never echo the recipe id.
    fn static_data_like_prod() -> blades_lib::static_data::StaticData {
        use blades_lib::static_data::{Recipe, SmithCraftables, StaticData};
        let mut sd = StaticData::default();
        sd.recipes.insert(
            Uuid::from_u128(0xA1),
            Recipe {
                crafting_type_id: Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap(),
                results: serde_json::json!({}),
                duration_ms: 0,
            },
        );
        sd.smith_craftables = SmithCraftables {
            smithing_crafting_type_id: Some(Uuid::parse_str(SMITHING_CRAFTING_TYPE_ID).unwrap()),
            forge_building_type_id: None,
            by_recipe: Default::default(),
            by_template: Default::default(),
        };
        sd
    }

    /// The committed APK durability ladder, as the server loads it. Real values matter
    /// here: a rebuilt forge item carries its template's own level-0 maximum, so a test
    /// running against an empty table would silently assert the fallback constant.
    fn repair_data_from_deploy() -> &'static RepairData {
        static CELL: std::sync::OnceLock<RepairData> = std::sync::OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
            let read = |name: &str| -> Value {
                std::fs::File::open(dir.join(name))
                    .ok()
                    .and_then(|f| serde_json::from_reader(std::io::BufReader::new(f)).ok())
                    .unwrap_or(Value::Null)
            };
            RepairData::from_json(&read("item_durability.json"), &read("repair_costs.json"))
        })
    }

    fn wire_of(job: &CraftJob, sd: &blades_lib::static_data::StaticData) -> Value {
        serde_json::to_value(CraftJobWire::from_job(
            job,
            Uuid::from_u128(0x11D),
            Uuid::from_u128(0xC1D),
            sd,
            repair_data_from_deploy(),
        ))
        .unwrap()
    }

    /// The forge recipes sitting in production `craftJobs` labelled Alchemy, with the
    /// stackable approximation as their results (owner character
    /// `5d1a3b4c-…-98e7e367beb7` holds twelve such rows across five of them; character
    /// `30581f3e-…` holds the sixth).
    const LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY: [(&str, &str); 6] = [
        ("Iron Hand Axe", "b949b05f-2e46-4a0c-80e4-171c4aecb9e5"),
        ("Iron Light Hammer", "38671302-f4f1-4357-aef9-5f57972c423d"),
        ("Iron Dagger", "a57591a0-9354-411b-862a-5449dfbd335b"),
        ("Iron Greatsword", "5fe0e868-957e-47c2-a094-9c1daad097d5"),
        ("Iron Warhammer", "7ad4e3a0-49b0-4c2f-9a94-6158acbb51d9"),
        ("Dragonbone Longsword", "668a077b-2a2e-477b-894d-cb0878fa7dd3"),
    ];

    /// The owner's forge row, verbatim: Alchemy in the `craftingTypeId` slot, the
    /// unknown-recipe stackable approximation in `results`, and the FORGE building
    /// (`105c24bf-…`, building type `26fdb92f-…`) it was actually crafted at.
    fn live_forge_row(recipe_id: Uuid, recipe_str: &str) -> CraftJob {
        CraftJob {
            id: Uuid::from_u128(0xF0),
            recipe_id,
            building_id: Uuid::parse_str("105c24bf-9e16-4cbb-bddd-514ad0b23e0e").unwrap(),
            crafting_type_id: Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap(),
            completed_at_ms: 1_785_999_240_168,
            results: serde_json::json!({"stackableItems": { recipe_str: 1 }}),
        }
    }

    /// Every distinct `(recipeId, craftingTypeId, results)` shape in production
    /// `craftJobs`, read off arena PG on 2026-08-20 (19 rows, 8 distinct shapes,
    /// 6 characters). This is the set the table has to survive contact with.
    fn live_craft_rows() -> Vec<(&'static str, CraftJob)> {
        let alchemy = Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap();
        let mut rows: Vec<(&'static str, CraftJob)> = LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY
            .iter()
            .map(|(name, id)| (*name, live_forge_row(Uuid::parse_str(id).unwrap(), id)))
            .collect();

        // N'wah: an alchemy recipe stored as Alchemy — the table agrees, nothing to do.
        let potion = "7a8600f2-24af-4a8e-8615-bc4d036825f3";
        rows.push((
            "Solution of Resist Fire (Alchemy, agrees)",
            CraftJob {
                id: Uuid::from_u128(0xF3),
                recipe_id: Uuid::parse_str(potion).unwrap(),
                building_id: Uuid::parse_str("faab4865-1657-484e-89e6-295fd0d0260f").unwrap(),
                crafting_type_id: alchemy,
                completed_at_ms: 1_785_038_215_000,
                results: serde_json::json!({"stackableItems": { potion: 1 }}),
            },
        ));
        // RonnieRaider (report #34) — the two unmappable rows and the healthy third.
        rows.push(("report #34 alchemy row", poisoned_alchemy_job()));
        rows.push(("report #34 forge row", poisoned_smith_job()));
        rows.push(("report #34 healthy row", healthy_alchemy_job()));
        // The Trickster: a stored Enchanting with the mutated item — must be untouched.
        rows.push((
            "stored enchant with instanced results",
            CraftJob {
                id: Uuid::from_u128(0xF4),
                recipe_id: Uuid::parse_str("a4dfdf4f-cf18-4be5-b706-91aadb0c1bea").unwrap(),
                building_id: Uuid::parse_str("26f35cbd-080c-4360-95d4-958797076ddc").unwrap(),
                crafting_type_id: item_mod_crafting_type(0),
                completed_at_ms: 1_782_775_391_000,
                results: serde_json::json!({"items":[{
                    "id": "066a599a-d6dd-40cf-b336-e4ea87e6e4ab",
                    "itemTemplateId": "dc2c3bd9-fb5a-4203-ad29-30c9d1724b75",
                    "temperingLevel": 0, "durability": 100.0
                }]}),
            },
        ));
        // WolfWalker: an UNMAPPABLE temper row that DOES carry its mutated item, so the
        // table's answer (Tempering) is both right and shapeable.
        rows.push((
            "unmappable temper with instanced results",
            CraftJob {
                id: Uuid::from_u128(0xF5),
                recipe_id: Uuid::parse_str("308f8752-518e-4e50-a5ce-12b20a4f871f").unwrap(),
                building_id: Uuid::parse_str("0e73f481-9efa-4dc8-a66b-46da95ff76ee").unwrap(),
                crafting_type_id: Uuid::parse_str("308f8752-518e-4e50-a5ce-12b20a4f871f").unwrap(),
                completed_at_ms: 1_782_511_435_000,
                results: serde_json::json!({"items":[{
                    "id": "a3891cfd-b131-48c1-a48a-4a6032b24f9d",
                    "itemTemplateId": "73c9bef2-2c2d-4a49-843c-a973fb7c3ee6",
                    "temperingLevel": 1, "durability": 100.0
                }]}),
            },
        ));
        rows
    }

    /// The invariant the pre-existing tests were missing: a serialized craft record's
    /// `craftingTypeId` and `results` must be a pair retail actually emitted. Checking
    /// that `craftingTypeId` is merely *mappable* is not enough — a mappable type
    /// bound to the wrong result shape stalls the town build just as dead.
    fn assert_shape_consistent(
        label: &str,
        wire: &Value,
        sd: &blades_lib::static_data::StaticData,
    ) {
        let ctid = Uuid::parse_str(wire["craftingTypeId"].as_str().unwrap()).unwrap();
        let wanted = sd
            .recipe_crafting_types
            .result_shape_of_type(&ctid)
            .unwrap_or_else(|| panic!("{label}: emitted craftingTypeId {ctid} is not a CraftingType"));
        let have = observed_result_shape(&wire["results"])
            .unwrap_or_else(|| panic!("{label}: emitted results have no recognisable shape"));
        assert_eq!(
            wanted, have,
            "{label}: retail never pairs {ctid} with {:?} results — this is the shape \
             that stalls loading pass 2 at town_level -1",
            have
        );
    }

    /// The Alchemy leg: a stored job whose `craftingTypeId` is an ITEM uuid must be
    /// repaired to a real CraftingType before it reaches the client.
    #[test]
    fn legacy_alchemy_job_is_repaired_on_read() {
        let sd = static_data_like_prod();
        let wire = wire_of(&poisoned_alchemy_job(), &sd);
        assert_ne!(
            wire["craftingTypeId"], wire["recipeId"],
            "craftingTypeId must never be the recipeId — GetCraftingStation() returns null \
             and the town-build coroutine never completes (report #34)"
        );
        assert_eq!(
            wire["craftingTypeId"].as_str().unwrap(),
            ALCHEMY_CRAFTING_TYPE_ID,
            "an unknown plain-craft recipe repairs to the Alchemy station"
        );
    }

    /// The Blacksmith leg: same defect, different building. Repaired the same way.
    #[test]
    fn legacy_smith_job_is_repaired_on_read() {
        use blades_lib::static_data::SmithCraftable;
        let mut sd = static_data_like_prod();
        // Make this recipe resolvable as a forge craftable, as a fuller data set would.
        let recipe = Uuid::parse_str("fd13cfa0-0148-41c0-be70-b3d08852f673").unwrap();
        sd.smith_craftables.by_recipe.insert(
            recipe,
            SmithCraftable {
                item_template_id: Uuid::from_u128(0xDBA7E),
                grade_index: 0,
                recipe_id: Some(recipe),
                duration_ms: 0,
                name: Some("Dragonbone War Axe".into()),
            },
        );
        let wire = wire_of(&poisoned_smith_job(), &sd);
        assert_ne!(wire["craftingTypeId"], wire["recipeId"], "must never echo the recipe id");
        assert_eq!(
            wire["craftingTypeId"].as_str().unwrap(),
            SMITHING_CRAFTING_TYPE_ID,
            "a forge craftable repairs to the Smithing station"
        );
    }

    /// A completed craft must never serialize `results: {}` — the client shows a
    /// collectable craft with nothing to collect and `finish` grants nothing.
    #[test]
    fn a_completed_craft_never_serializes_empty_results() {
        let sd = static_data_like_prod();
        for job in [poisoned_alchemy_job(), poisoned_smith_job()] {
            let wire = wire_of(&job, &sd);
            let results = &wire["results"];
            assert!(
                results.is_object() && !results.as_object().unwrap().is_empty(),
                "completed craft {} serialized empty results",
                job.id
            );
            // …and the repaired result must actually be grantable by `finish`.
            let reward = reward_from_results(results);
            assert!(
                !reward.items.is_empty() || !reward.stackable_items.is_empty(),
                "repaired results must yield a real grant on finish"
            );
        }
    }

    /// The blanket invariant over EVERY emitted craft record — poisoned and healthy,
    /// alchemy, smith and temper alike. This is the assertion whose absence let the
    /// temper-only fix ship while the other two paths stayed broken.
    #[test]
    fn every_emitted_craft_record_carries_a_station_and_a_grantable_result() {
        let sd = static_data_like_prod();
        let jobs = [
            ("legacy alchemy", poisoned_alchemy_job()),
            ("legacy smith", poisoned_smith_job()),
            ("healthy alchemy", healthy_alchemy_job()),
            ("healthy temper", healthy_temper_job()),
        ];
        for (label, job) in jobs {
            let wire = wire_of(&job, &sd);
            assert_ne!(
                wire["craftingTypeId"], wire["recipeId"],
                "{label}: craftingTypeId == recipeId"
            );
            assert_ne!(
                wire["craftingTypeId"].as_str().unwrap(),
                Uuid::nil().to_string(),
                "{label}: craftingTypeId is nil"
            );
            let results = &wire["results"];
            assert!(
                results.is_object() && !results.as_object().unwrap().is_empty(),
                "{label}: empty results"
            );
        }
    }

    /// Repair is surgical: an already-well-formed job passes through unchanged.
    #[test]
    fn a_well_formed_job_is_not_rewritten() {
        let sd = static_data_like_prod();
        for job in [healthy_alchemy_job(), healthy_temper_job()] {
            let wire = wire_of(&job, &sd);
            assert_eq!(
                wire["craftingTypeId"].as_str().unwrap(),
                job.crafting_type_id.to_string(),
                "healthy craftingTypeId must be preserved"
            );
            assert_eq!(wire["results"], job.results, "healthy results must be preserved");
        }
    }

    // ── The recipe -> CraftingType table (report #34, second half) ────────────
    //
    // Repairing a poisoned job to *a* real CraftingType unblocks the loading screen;
    // it does not make the job name the right bench. Report #34's forge craft is in
    // neither `recipes.json` nor `smith_craftables.json`, so before the APK table it
    // fell through to `derive_plain_craft_type` and the player's Dragonbone War Axe
    // was reported as an ALCHEMY craft. `recipe_crafting_types.json` is walked out of
    // the APK's own `RecipeData._recipeMap`, so every recipe the client ships now
    // names its real station.

    /// The committed `deploy/static` set, as the server actually loads it.
    fn static_data_from_deploy() -> blades_lib::static_data::StaticData {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    /// Report #34's acceptance test, re-baselined a second time — and this time against
    /// retail rather than against what we could manage.
    ///
    /// The Dragonbone War Axe craft (`fd13cfa0-…-b3d08852f673`) is a forge craft with
    /// EMPTY stored results, and neither `recipes.json` nor `smith_craftables.json` can
    /// mint the axe. Until the recipe -> output mapping existed the only result this
    /// repair could build was the stackable approximation, so the bench had to follow
    /// the results down to Alchemy: a wrong name on a loading game, which was the right
    /// way round while it was the only choice.
    ///
    /// It is no longer the only choice. `recipe_crafting_types.json` now carries the
    /// recipe's own output template, so the axe can be minted and the bench named
    /// honestly at the same time. The expected item id is not a guess either: retail
    /// itself served this exact recipe as Smithing with
    /// `results.items[0].itemTemplateId = efb6d38f-…` (capture_id 79683-era rows on
    /// `blades.bgs.services`), so the record this test now demands is byte-for-byte the
    /// kind of record retail emitted for this very recipe.
    #[test]
    fn report_34_forge_row_is_named_smithing_with_the_item_retail_itself_sent() {
        let sd = static_data_from_deploy();
        let job = poisoned_smith_job();

        // The premise still holds: neither table that mints a REAL item knows this
        // recipe. The repair no longer depends on them.
        assert!(sd.recipes.get(&job.recipe_id).is_none(), "not a captured recipe");
        assert!(
            sd.smith_craftables.resolve(&job.recipe_id).is_none(),
            "not a resolvable smith craftable"
        );
        assert_eq!(
            apk_crafting_type(&job.recipe_id, &sd).map(|u| u.to_string()).as_deref(),
            Some(SMITHING_CRAFTING_TYPE_ID),
            "the APK table says Smithing"
        );

        let wire = wire_of(&job, &sd);
        let ctid = wire["craftingTypeId"].as_str().unwrap();
        assert_ne!(ctid, wire["recipeId"].as_str().unwrap(), "must never echo the recipe id");
        assert_eq!(
            ctid, SMITHING_CRAFTING_TYPE_ID,
            "the Dragonbone War Axe is forged at the Smithy and its results can now be \
             built in the shape a Smithing station restores, so the bench is finally \
             named honestly"
        );
        assert_eq!(
            wire["results"]["items"][0]["itemTemplateId"].as_str(),
            Some("efb6d38f-ecc9-47bd-bde2-397cd35d888b"),
            "the item retail served for this recipe"
        );
        assert_shape_consistent("report #34 forge row", &wire, &sd);
    }

    /// Both report-#34 rows resolve from the table, each to its own bench — the alchemy
    /// row was right by luck (the fallback IS alchemy), the forge row was not.
    #[test]
    fn both_report_34_recipes_resolve_from_the_apk_table() {
        let sd = static_data_from_deploy();
        let alchemy_recipe = Uuid::parse_str("b5a2dbe9-d115-4bf2-99d9-558be1de3ef7").unwrap();
        let forge_recipe = Uuid::parse_str("fd13cfa0-0148-41c0-be70-b3d08852f673").unwrap();
        assert_eq!(
            apk_crafting_type(&alchemy_recipe, &sd).map(|u| u.to_string()).as_deref(),
            Some(ALCHEMY_CRAFTING_TYPE_ID),
            "Deadly Aversion to Frost is brewed at the Alchemist"
        );
        assert_eq!(
            apk_crafting_type(&forge_recipe, &sd).map(|u| u.to_string()).as_deref(),
            Some(SMITHING_CRAFTING_TYPE_ID),
            "Dragonbone War Axe is forged at the Smithy"
        );
    }

    /// An un-captured ENCHANTING recipe the capture never saw.
    fn an_uncaptured_enchanting_recipe(sd: &blades_lib::static_data::StaticData) -> (Uuid, Uuid) {
        let enchanting = sd
            .recipe_crafting_types
            .type_by_name("Enchanting")
            .expect("Enchanting crafting type in the table");
        let (recipe_id, _) = sd
            .recipe_crafting_types
            .recipes
            .iter()
            .find(|(id, r)| r.crafting_type_id == enchanting && !sd.recipes.contains_key(id))
            .expect("an un-captured enchanting recipe exists");
        (*recipe_id, enchanting)
    }

    /// The table is not smithing-only: an un-captured ENCHANTING recipe must report the
    /// Enchanter, where the alchemy fallback would have said Alchemist. The job carries
    /// the mutated item an enchant really stores (297/297 retail enchant rows carry
    /// `items`), so naming the Enchanter produces a record retail could have emitted.
    #[test]
    fn an_uncaptured_enchanting_recipe_reports_the_enchanter() {
        let sd = static_data_from_deploy();
        let (recipe_id, enchanting) = an_uncaptured_enchanting_recipe(&sd);
        let job = CraftJob {
            id: Uuid::from_u128(0xE0),
            recipe_id,
            building_id: Uuid::from_u128(0xB2),
            crafting_type_id: recipe_id, // poisoned exactly like report #34
            completed_at_ms: 1_783_204_348_637,
            results: serde_json::json!({"items":[{
                "id": "fad31819-b941-4446-a229-e22b3647b142",
                "itemTemplateId": "616b64ef-4184-4efb-af55-1a3f122431dc",
                "temperingLevel": 0, "durability": 100.0
            }]}),
        };
        let wire = wire_of(&job, &sd);
        assert_eq!(
            wire["craftingTypeId"].as_str().unwrap(),
            enchanting.to_string(),
            "an enchanting recipe belongs to the Enchanter, not the Alchemist"
        );
        assert_shape_consistent("uncaptured enchant", &wire, &sd);
    }

    /// …but the bench is only named when the record can be SHAPED like that bench's
    /// craft. An enchant whose mutated item was never stored cannot be rebuilt (the
    /// original item left the backpack), so an `items` result is unavailable and the
    /// Enchanter cannot honestly be named. The repair must not paper over that with an
    /// Enchanting job carrying `stackableItems` — the pair that stalls the town build.
    #[test]
    fn an_enchant_whose_results_cannot_be_rebuilt_does_not_claim_the_enchanter() {
        let sd = static_data_from_deploy();
        let (recipe_id, enchanting) = an_uncaptured_enchanting_recipe(&sd);
        let job = CraftJob {
            id: Uuid::from_u128(0xE1),
            recipe_id,
            building_id: Uuid::from_u128(0xB2),
            crafting_type_id: recipe_id,
            completed_at_ms: 1_783_204_348_637,
            results: serde_json::json!({}),
        };
        let wire = wire_of(&job, &sd);
        let ctid = wire["craftingTypeId"].as_str().unwrap();
        assert_ne!(ctid, wire["recipeId"].as_str().unwrap(), "must never echo the recipe id");
        assert_ne!(
            ctid,
            enchanting.to_string(),
            "an Enchanting job may not carry stackable results"
        );
        assert_shape_consistent("un-rebuildable enchant", &wire, &sd);
    }

    /// The committed table covers everything the server already knew about, and agrees
    /// with it — the guard that a re-extraction has not moved the mapping under us.
    #[test]
    fn the_apk_table_covers_and_agrees_with_every_captured_recipe() {
        let sd = static_data_from_deploy();
        assert_eq!(
            sd.recipe_crafting_types.crafting_types.len(),
            7,
            "the client ships exactly 7 CraftingTypes"
        );
        for (id, recipe) in &sd.recipes {
            assert_eq!(
                apk_crafting_type(id, &sd),
                Some(recipe.crafting_type_id),
                "captured recipe {id} missing from / disagreeing with the APK table"
            );
        }
        for id in sd.item_mod_recipes.keys() {
            assert!(apk_crafting_type(id, &sd).is_some(), "mod recipe {id} missing from the table");
        }
        for id in sd.salvage_recipes.keys() {
            assert!(
                apk_crafting_type(id, &sd).is_some(),
                "salvage recipe {id} missing from the table"
            );
        }
    }

    // ── The relabel that hung the town build ──────────────────────────────────
    //
    // Report #35 corrected a MAPPABLE but WRONG stored type from the APK table and
    // left `results` alone, on the reasoning that such rows "never hung the client —
    // they just name the wrong bench forever". The first half was true only for as
    // long as the rows stayed labelled Alchemy.
    //
    // `craftingTypeId` binds the job to a CraftingStation, and each station restores
    // its in-progress craft from `results` in the one shape it understands. In the
    // retail captures the pairing is total and exceptionless (482/482 craft records):
    // Smithing 26/26, Enchanting 297/297 and Tempering 51/51 carry `results.items`;
    // Alchemy 100/100 and DecorationCrafting 8/8 carry `results.stackableItems`.
    //
    // The live forge rows carry the unknown-recipe approximation
    // `{"stackableItems": {"<recipeId>": 1}}` — and the recipe id is not even an item
    // template (`items.json` has no `b949b05f-…`). Relabelled to Smithing they became
    // Smithing + stackableItems: a pair retail never emitted. As Alchemy they had been
    // inert, because their building is the Forge (type `26fdb92f-…`, whose stations
    // are Smithing / Tempering / Repair / Salvaging) and there is no Alchemy station
    // there to bind them to. The relabel is what ACTIVATED a malformed record that had
    // been harmlessly ignored, and loading pass 2 stalled with `town_level == -1`.

    /// The six recipe ids actually sitting in production `craftJobs` with
    /// `craftingTypeId = c9d3b3aa…` (Alchemy) and the stackable approximation as their
    /// results. All six are Smithing in the APK. This is the test the whole change
    /// exists for: each row is rebuilt into the `items` shape a Smithing station
    /// restores, and only then relabelled to the Smithy.
    ///
    /// Before the recipe -> output mapping this asserted the opposite — that the bench
    /// must stay Alchemy — because moving the name without the results emitted Smithing
    /// + `stackableItems`, a pair absent from all 482 retail craft records, and stalled
    /// loading pass 2 at `town_level == -1`. That constraint is satisfied here rather
    /// than dodged: the pair being emitted is Smithing + `items`, which is 26/26 of what
    /// retail sent for Smithing.
    #[test]
    fn the_live_forge_rows_are_rebuilt_as_items_and_named_smithing() {
        let sd = static_data_from_deploy();
        let smithing = Uuid::parse_str(SMITHING_CRAFTING_TYPE_ID).unwrap();
        for (name, id) in LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY {
            let recipe_id = Uuid::parse_str(id).unwrap();
            assert_eq!(
                apk_crafting_type(&recipe_id, &sd),
                Some(smithing),
                "{name} is forged at the Smithy"
            );
            // The premise of the old behaviour: still unmintable from the two tables
            // that hold captured results. The output mapping is what replaced them.
            assert!(
                sd.smith_craftables.resolve(&recipe_id).is_none(),
                "{name}: still not a resolvable smith craftable"
            );
            let template = sd
                .recipe_crafting_types
                .output_template_of(&recipe_id)
                .unwrap_or_else(|| panic!("{name}: the APK table must know its output"));

            let job = live_forge_row(recipe_id, id);
            let wire = wire_of(&job, &sd);

            assert_eq!(
                wire["craftingTypeId"].as_str().unwrap(),
                smithing.to_string(),
                "{name}: the bench is finally named honestly"
            );
            let items = wire["results"]["items"]
                .as_array()
                .unwrap_or_else(|| panic!("{name}: results must carry `items`, got {}", wire["results"]));
            assert_eq!(items.len(), 1, "{name}: a Recipe has exactly one output");
            assert_eq!(
                items[0]["itemTemplateId"].as_str(),
                Some(template.to_string().as_str()),
                "{name}: the real output item, not the recipe id"
            );
            assert!(
                wire["results"].get("stackableItems").is_none(),
                "{name}: the approximation must be gone, not merely accompanied"
            );
            // The recipe id must never survive into an item-template slot — that
            // category error is the original defect.
            assert_ne!(
                items[0]["itemTemplateId"].as_str(),
                Some(id),
                "{name}: recipe id leaked into itemTemplateId"
            );
            // Real durability from the APK ladder, not the 150.0 fallback constant.
            let want_durability = repair_data_from_deploy()
                .max_durability(template, 0)
                .unwrap_or_else(|| panic!("{name}: durability ladder must know {template}"));
            assert_eq!(
                items[0]["durability"].as_f64(),
                Some(want_durability),
                "{name}: durability must be this template's own level-0 maximum"
            );
            assert_eq!(items[0]["temperingLevel"].as_u64(), Some(0), "{name}: base level");
            assert_shape_consistent(name, &wire, &sd);
        }
    }

    /// The synthesized item id must be STABLE. `repaired_craft_fields` runs on every
    /// read, so a random id would hand the client a different `results.items[0].id` on
    /// each `GET /crafts` poll for the same job — and `finish` grants whatever id was
    /// last read. Two independent renders of the same job must agree; two different
    /// jobs must not collide.
    #[test]
    fn a_rebuilt_forge_item_keeps_the_same_id_across_reads() {
        let sd = static_data_from_deploy();
        let (_, id) = LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY[0];
        let recipe_id = Uuid::parse_str(id).unwrap();
        let job = live_forge_row(recipe_id, id);

        let first = wire_of(&job, &sd)["results"]["items"][0]["id"].clone();
        let second = wire_of(&job, &sd)["results"]["items"][0]["id"].clone();
        assert!(first.is_string(), "an instanced result carries an id");
        assert_eq!(first, second, "the same job must render the same item id every read");

        let other = CraftJob { id: Uuid::from_u128(0xF1), ..live_forge_row(recipe_id, id) };
        assert_ne!(
            wire_of(&other, &sd)["results"]["items"][0]["id"],
            first,
            "two different jobs must not share an item id"
        );
    }

    /// A GENUINE retail stackable result — keyed by a real item template rather than by
    /// the job's own recipe id — is never rewritten, even on a row whose bench is being
    /// corrected. The rebuild is gated on the results being provably our own
    /// approximation: across the captures, retail produced 0 self-keyed stackables out
    /// of 108, and our server produced 328 out of 328.
    #[test]
    fn a_real_retail_stackable_result_is_never_rewritten() {
        let sd = static_data_from_deploy();
        let (_, id) = LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY[0];
        let recipe_id = Uuid::parse_str(id).unwrap();
        // Same forge row, except the stackable is keyed by a real item template.
        let real_template = "cdbabba6-a6a2-46ed-a086-93d77acc274a";
        let job = CraftJob {
            results: serde_json::json!({"stackableItems": { real_template: 2 }}),
            ..live_forge_row(recipe_id, id)
        };
        assert!(
            !results_are_our_own_approximation(&job),
            "a template-keyed stackable is not our approximation"
        );
        let wire = wire_of(&job, &sd);
        assert_eq!(wire["results"], job.results, "retail data must be passed through untouched");
        assert_shape_consistent("real stackable", &wire, &sd);
    }

    /// The safety net is still there for a recipe the APK table has never heard of: no
    /// output template means no instanced result can be built, so the bench must once
    /// again follow the results rather than the recipe. This is the behaviour the
    /// rewritten tests above replaced, kept for the case that still needs it.
    #[test]
    fn a_recipe_absent_from_the_table_still_keeps_the_bench_that_loads() {
        let sd = static_data_from_deploy();
        let unknown = Uuid::from_u128(0xDEADBEEF);
        assert!(
            sd.recipe_crafting_types.output_template_of(&unknown).is_none(),
            "premise: the table cannot mint this recipe"
        );
        let job = live_forge_row(unknown, &unknown.to_string());
        let wire = wire_of(&job, &sd);
        assert_eq!(
            wire["craftingTypeId"].as_str().unwrap(),
            ALCHEMY_CRAFTING_TYPE_ID,
            "with no mintable output the stackable result keeps its matching bench"
        );
        assert_shape_consistent("untabled recipe", &wire, &sd);
    }

    /// The whole list, as `GET /crafts` builds it.
    fn wires_of(jobs: &[CraftJob], sd: &blades_lib::static_data::StaticData) -> Vec<Value> {
        craft_wires(
            jobs,
            Uuid::from_u128(0x11D),
            Uuid::from_u128(0xC1D),
            sd,
            repair_data_from_deploy(),
        )
        .into_iter()
        .map(|w| serde_json::to_value(w).unwrap())
        .collect()
    }

    /// All six live forge recipes as separate jobs at the SAME building — the owner's
    /// Forge, which is how production actually holds them.
    fn live_forge_pile() -> Vec<CraftJob> {
        LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY
            .iter()
            .enumerate()
            .map(|(n, (_, id))| CraftJob {
                id: Uuid::from_u128(0xF00 + n as u128),
                completed_at_ms: 1_785_999_240_168 + n as i64,
                ..live_forge_row(Uuid::parse_str(id).unwrap(), id)
            })
            .collect()
    }

    /// Retail never showed two craft jobs on one station: 238 of 238
    /// `(buildingId, craftingTypeId)` groups across the 135 captured retail
    /// `GET /crafts` snapshots hold exactly one job. Correcting each of the owner's six
    /// forge rows on its own merits would bind all six to the Forge's single Smithing
    /// station, so the correction is rationed to one and the rest stay inert.
    #[test]
    fn one_station_holds_one_job_even_when_six_rows_could_be_corrected() {
        let sd = static_data_from_deploy();
        let jobs = live_forge_pile();
        let wires = wires_of(&jobs, &sd);
        assert_eq!(wires.len(), jobs.len(), "no job may be dropped from the list");

        // The guarantee: a job we corrected is ALONE on the station we moved it to. The
        // rows we left alone stay as production serves them today (nominally on an
        // Alchemy station the Forge does not have, which is why they are inert) — thinning
        // that pile further would mean dropping the player's crafts.
        let mut per_station: std::collections::HashMap<(String, String), usize> = Default::default();
        for w in &wires {
            *per_station
                .entry((
                    w["buildingId"].as_str().unwrap().to_string(),
                    w["craftingTypeId"].as_str().unwrap().to_string(),
                ))
                .or_default() += 1;
        }
        for (job, w) in jobs.iter().zip(&wires) {
            let ctid = w["craftingTypeId"].as_str().unwrap();
            if ctid == job.crafting_type_id.to_string() {
                continue; // not corrected — it holds whatever station it was stored on
            }
            let building = w["buildingId"].as_str().unwrap().to_string();
            assert_eq!(
                per_station[&(building.clone(), ctid.to_string())], 1,
                "corrected job {} shares station {ctid} at building {building} with another; \
                 retail showed 238/238 singletons",
                w["id"]
            );
        }
        // And no group may be bigger than it was before the repair ran.
        let mut before: std::collections::HashMap<(Uuid, Uuid), usize> = Default::default();
        for job in &jobs {
            *before.entry((job.building_id, job.crafting_type_id)).or_default() += 1;
        }
        for ((building, ctid), n) in &per_station {
            let key = (Uuid::parse_str(building).unwrap(), Uuid::parse_str(ctid).unwrap());
            assert!(
                *n <= before.get(&key).copied().unwrap_or(0).max(1),
                "station {ctid} at building {building} went from {:?} to {n} jobs",
                before.get(&key)
            );
        }

        let smithing: Vec<&Value> = wires
            .iter()
            .filter(|w| w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID)
            .collect();
        assert_eq!(smithing.len(), 1, "exactly one row gets the honest bench");
        assert!(
            smithing[0]["results"]["items"].is_array(),
            "and it carries the instanced result that makes the bench legal"
        );

        // Every demoted row is back to exactly what it was stored as — both fields, so
        // the pair stays one retail emitted.
        for (job, w) in jobs.iter().zip(&wires) {
            if w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID {
                continue;
            }
            assert_eq!(
                w["craftingTypeId"].as_str().unwrap(),
                ALCHEMY_CRAFTING_TYPE_ID,
                "a demoted row keeps the bench that leaves it inert"
            );
            assert_eq!(w["results"], job.results, "and its stored results, untouched");
        }

        // The invariant that this whole change is downstream of, over the full list.
        for w in &wires {
            assert_shape_consistent("forge pile", w, &sd);
        }
    }

    /// The choice of which row is corrected must not depend on the order Postgres
    /// returned the rows in, or the same character would see the bench move between
    /// polls.
    #[test]
    fn which_row_gets_the_bench_does_not_depend_on_list_order() {
        let sd = static_data_from_deploy();
        let jobs = live_forge_pile();
        let winner = |js: &[CraftJob]| -> String {
            wires_of(js, &sd)
                .into_iter()
                .find(|w| w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID)
                .map(|w| w["id"].as_str().unwrap().to_string())
                .expect("one row is corrected")
        };
        let forward = winner(&jobs);
        let mut reversed = jobs.clone();
        reversed.reverse();
        assert_eq!(forward, winner(&reversed), "the same job must win either way");
    }

    /// A job already sitting on that station — one whose type we did not touch — outranks
    /// any repair. The repair must not shoulder a real job off its own bench.
    #[test]
    fn a_job_already_on_the_station_outranks_a_corrected_one() {
        let sd = static_data_from_deploy();
        let smithing = Uuid::parse_str(SMITHING_CRAFTING_TYPE_ID).unwrap();
        let mut jobs = live_forge_pile();
        // A well-formed Smithing job at the same building: correct type, instanced
        // result, nothing for the repair to do.
        let real = CraftJob {
            id: Uuid::from_u128(0xBEE),
            recipe_id: Uuid::parse_str("04730542-db74-46a8-89e9-c8c6bf951ee7").unwrap(),
            building_id: jobs[0].building_id,
            crafting_type_id: smithing,
            completed_at_ms: 1_786_000_000_000,
            results: serde_json::json!({"items": [{
                "id": "036d7be2-06ef-4a77-87c9-7d895327c708",
                "itemTemplateId": "bacb3089-2378-45d2-b717-2dd3f01e5939",
                "temperingLevel": 0,
                "durability": 162.5
            }]}),
        };
        jobs.push(real.clone());

        let wires = wires_of(&jobs, &sd);
        let smiths: Vec<&Value> = wires
            .iter()
            .filter(|w| w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID)
            .collect();
        assert_eq!(smiths.len(), 1, "the station still holds exactly one job");
        assert_eq!(
            smiths[0]["id"].as_str().unwrap(),
            real.id.to_string(),
            "and it is the real job, not a repaired one"
        );
        for w in &wires {
            assert_shape_consistent("forge pile + real smith job", w, &sd);
        }
    }

    /// Every distinct production craft row, as one list: no station may end up holding
    /// two jobs and no record may pair a type with results retail never paired.
    #[test]
    fn the_whole_production_row_set_keeps_both_retail_invariants() {
        let sd = static_data_from_deploy();
        let jobs: Vec<CraftJob> = live_craft_rows()
            .into_iter()
            .enumerate()
            // live_craft_rows reuses fixture ids; a real list has distinct ones.
            .map(|(n, (_, job))| CraftJob { id: Uuid::from_u128(0xA000 + n as u128), ..job })
            .collect();
        let wires = wires_of(&jobs, &sd);

        let mut occupancy: std::collections::HashMap<(String, String), usize> = Default::default();
        for w in &wires {
            assert_shape_consistent("production row set", w, &sd);
            *occupancy
                .entry((
                    w["buildingId"].as_str().unwrap().to_string(),
                    w["craftingTypeId"].as_str().unwrap().to_string(),
                ))
                .or_default() += 1;
        }
        // No station this repair MOVED a job onto may hold more than the one job retail
        // would have shown there — with one documented exception. A job whose STORED type
        // is unserveable has nowhere safe to be sent back to: reverting it would put
        // report #34's unmappable `craftingTypeId` back on the wire, which is a MEASURED
        // hang, whereas a shared station is an unproven one. Those keep their correction.
        for (job, w) in jobs.iter().zip(&wires) {
            let ctid = w["craftingTypeId"].as_str().unwrap().to_string();
            if ctid == job.crafting_type_id.to_string() {
                continue;
            }
            let building = w["buildingId"].as_str().unwrap().to_string();
            let n = occupancy[&(building.clone(), ctid.clone())];
            if stored_crafting_type_is_unserveable(job) {
                assert!(
                    n >= 1,
                    "a pinned row must still be present on the station it was moved to"
                );
                continue;
            }
            assert_eq!(
                n, 1,
                "repair put job {} onto station {ctid} at building {building} alongside \
                 another; retail showed 238/238 singletons",
                w["id"]
            );
        }
    }

    /// The forge half of production, verbatim: the owner's eleven broken forge rows as
    /// `characters.server_state->'craftJobs'` actually held them on 2026-08-20, read off
    /// arena PG. `(buildingId, recipeId)` in row order — note the two DIFFERENT buildings
    /// and the repeated recipes, neither of which the single-building fixture above
    /// reproduces. Three rows share `b949b05f` at one building, so a winner has to be
    /// picked among identical recipes as well as among different ones.
    const OWNER_FORGE_ROWS_2026_08_20: [(&str, &str); 11] = [
        ("0e73f481-9efa-4dc8-a66b-46da95ff76ee", "5fe0e868-957e-47c2-a094-9c1daad097d5"),
        ("0e73f481-9efa-4dc8-a66b-46da95ff76ee", "7ad4e3a0-49b0-4c2f-9a94-6158acbb51d9"),
        ("0e73f481-9efa-4dc8-a66b-46da95ff76ee", "a57591a0-9354-411b-862a-5449dfbd335b"),
        ("0e73f481-9efa-4dc8-a66b-46da95ff76ee", "b949b05f-2e46-4a0c-80e4-171c4aecb9e5"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "38671302-f4f1-4357-aef9-5f57972c423d"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "38671302-f4f1-4357-aef9-5f57972c423d"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "38671302-f4f1-4357-aef9-5f57972c423d"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "5fe0e868-957e-47c2-a094-9c1daad097d5"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "a57591a0-9354-411b-862a-5449dfbd335b"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "b949b05f-2e46-4a0c-80e4-171c4aecb9e5"),
        ("105c24bf-9e16-4cbb-bddd-514ad0b23e0e", "b949b05f-2e46-4a0c-80e4-171c4aecb9e5"),
    ];

    /// The owner's real forge pile, on the real two buildings, must come out with one
    /// honestly-named Smithing bench PER BUILDING and no crowded station anywhere. This
    /// is the character that got stuck on the startup screen, so it is the one layout the
    /// change has to be right about.
    #[test]
    fn the_owners_real_two_building_forge_pile_yields_one_bench_per_building() {
        let sd = static_data_from_deploy();
        let alchemy = Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap();
        let jobs: Vec<CraftJob> = OWNER_FORGE_ROWS_2026_08_20
            .iter()
            .enumerate()
            .map(|(n, (building, recipe))| CraftJob {
                id: Uuid::from_u128(0xB000 + n as u128),
                recipe_id: Uuid::parse_str(recipe).unwrap(),
                building_id: Uuid::parse_str(building).unwrap(),
                crafting_type_id: alchemy,
                completed_at_ms: 1_785_999_240_168 + n as i64,
                results: serde_json::json!({"stackableItems": { *recipe: 1 }}),
            })
            .collect();

        let wires = wires_of(&jobs, &sd);
        assert_eq!(wires.len(), 11, "no craft job may vanish from the list");

        let mut occupancy: std::collections::HashMap<(String, String), usize> = Default::default();
        for w in &wires {
            assert_shape_consistent("owner forge pile", w, &sd);
            *occupancy
                .entry((
                    w["buildingId"].as_str().unwrap().to_string(),
                    w["craftingTypeId"].as_str().unwrap().to_string(),
                ))
                .or_default() += 1;
        }

        let mut smith_buildings: Vec<String> = wires
            .iter()
            .filter(|w| w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID)
            .map(|w| w["buildingId"].as_str().unwrap().to_string())
            .collect();
        smith_buildings.sort();
        assert_eq!(
            smith_buildings,
            vec![
                "0e73f481-9efa-4dc8-a66b-46da95ff76ee".to_string(),
                "105c24bf-9e16-4cbb-bddd-514ad0b23e0e".to_string(),
            ],
            "each Forge gets exactly one honestly-named Smithing job"
        );
        for b in &smith_buildings {
            assert_eq!(
                occupancy[&(b.clone(), SMITHING_CRAFTING_TYPE_ID.to_string())], 1,
                "the Smithing station at {b} must hold one job, not a crowd"
            );
        }
        // Each promoted row carries a real instanced item the durability ladder knows.
        for w in wires.iter().filter(|w| w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID) {
            let tpl = w["results"]["items"][0]["itemTemplateId"].as_str().expect("an item");
            assert_ne!(tpl, w["recipeId"].as_str().unwrap(), "not the recipe id");
            assert!(
                repair_data_from_deploy()
                    .max_durability(Uuid::parse_str(tpl).unwrap(), 0)
                    .is_some(),
                "{tpl} must be a template the durability ladder knows"
            );
        }
        // The nine that stayed behind are byte-identical to what is stored, so this
        // change cannot have made them worse than the state that loads today.
        for (job, w) in jobs.iter().zip(&wires) {
            if w["craftingTypeId"] == SMITHING_CRAFTING_TYPE_ID {
                continue;
            }
            assert_eq!(w["craftingTypeId"].as_str().unwrap(), ALCHEMY_CRAFTING_TYPE_ID);
            assert_eq!(w["results"], job.results);
        }
    }

    /// The gate itself, directly: a retail-impossible `(craftingTypeId, results)` pair
    /// must be refused whatever the caller asks for. Smithing never carries a stackable
    /// result in retail (26/26 carry `items`), so handing `reconcile_type_with_results`
    /// that pair must not yield it back.
    #[test]
    fn reconcile_refuses_a_pairing_retail_never_emitted() {
        let sd = static_data_from_deploy();
        let smithing = Uuid::parse_str(SMITHING_CRAFTING_TYPE_ID).unwrap();
        let (_, id) = LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY[0];
        let job = live_forge_row(Uuid::parse_str(id).unwrap(), id);

        let stackable = Cow::Owned(serde_json::json!({"stackableItems": { id: 1 }}));
        let (ctid, results) = reconcile_type_with_results(&job, smithing, stackable, &sd, false);
        assert_ne!(
            ctid, smithing,
            "Smithing + stackableItems is the pair that stalled the town build"
        );
        assert!(
            results.get("stackableItems").is_some(),
            "the results are the evidence and must survive; only the bench name yields"
        );

        // And the mirror direction: an instanced result under Alchemy, which retail
        // also never sent (100/100 Alchemy rows carry `stackableItems`).
        let alchemy = Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap();
        let instanced = Cow::Owned(serde_json::json!({
            "items": [{ "id": Uuid::from_u128(7).to_string(), "itemTemplateId": id }]
        }));
        let (ctid, _) = reconcile_type_with_results(&job, alchemy, instanced, &sd, true);
        assert_ne!(ctid, alchemy, "Alchemy + items is equally absent from retail");
    }

    /// The relabel is not abandoned — it is conditioned. The same six recipes, stored
    /// as Alchemy but carrying the instanced result a forge craft really produces, are
    /// still corrected to the Smithy: that record is one retail could have emitted, so
    /// naming the right bench costs nothing.
    #[test]
    fn a_forge_craft_with_instanced_results_is_still_relabelled_to_the_smithy() {
        let sd = static_data_from_deploy();
        let alchemy = Uuid::parse_str(ALCHEMY_CRAFTING_TYPE_ID).unwrap();
        for (name, id) in LIVE_FORGE_RECIPES_STORED_AS_ALCHEMY {
            let job = CraftJob {
                id: Uuid::from_u128(0xF0),
                recipe_id: Uuid::parse_str(id).unwrap(),
                building_id: Uuid::from_u128(0xB4),
                crafting_type_id: alchemy,
                completed_at_ms: 1_783_204_348_637,
                results: serde_json::json!({"items":[{
                    "id": "6f2b8f66-2c0a-4a2f-9f9f-1d5d3b7a1f11",
                    "itemTemplateId": "606c8bf6-9dc7-4c5f-b44b-36eb02306c96",
                    "temperingLevel": 0, "durability": 150.0
                }]}),
            };
            let wire = wire_of(&job, &sd);
            assert_eq!(
                wire["craftingTypeId"].as_str().unwrap(),
                SMITHING_CRAFTING_TYPE_ID,
                "{name}: a forge craft that CAN be shaped like one names the Smithy"
            );
            assert_eq!(wire["results"], job.results, "a safe relabel still leaves results alone");
            assert_shape_consistent(name, &wire, &sd);
        }
    }

    /// The mod-craft exception: `temperingLevel` decides temper vs enchant and the
    /// recipe table cannot see it, so a stored Tempering/Enchanting type is left alone
    /// even when the table would say otherwise.
    #[test]
    fn a_stored_temper_or_enchant_type_is_never_second_guessed() {
        let sd = static_data_from_deploy();
        // A recipe the table calls Smithing, stored as a temper — the stored value wins.
        let recipe_id = Uuid::parse_str("b949b05f-2e46-4a0c-80e4-171c4aecb9e5").unwrap();
        for level in [0u64, 10] {
            let stored = item_mod_crafting_type(level);
            let job = CraftJob {
                id: Uuid::from_u128(0xF2),
                recipe_id,
                building_id: Uuid::from_u128(0xB5),
                crafting_type_id: stored,
                completed_at_ms: 1_783_204_348_637,
                results: serde_json::json!({"items":[{
                    "id": "fad31819-b941-4446-a229-e22b3647b142",
                    "itemTemplateId": "616b64ef-4184-4efb-af55-1a3f122431dc",
                    "temperingLevel": level, "durability": 675.0
                }]}),
            };
            let wire = wire_of(&job, &sd);
            assert_eq!(
                wire["craftingTypeId"].as_str().unwrap(),
                stored.to_string(),
                "a stored mod-craft type must survive (temperingLevel is not in the table)"
            );
        }
    }

    /// The regression net. Every distinct craft row in production, through the real
    /// loaded static data, asserting the whole emission contract at once: mappable
    /// CraftingType, non-empty grantable results, AND the two of them paired the way
    /// retail paired them.
    ///
    /// The last clause is the one that was missing. `#35` shipped with its own live-row
    /// test — the same six recipes, the same stackable results — and it passed, because
    /// it asserted only that the type had been rewritten to Smithing. It even pinned
    /// the defect as intended behaviour: `assert_eq!(wire["results"], job.results, "a
    /// mislabel must not touch the results")`. The oracle came from report #34's
    /// post-mortem ("an UNMAPPABLE craftingTypeId hangs") and was never widened to the
    /// real invariant, which is that the client must be handed a record it has seen
    /// the shape of before.
    #[test]
    fn no_emitted_craft_record_pairs_a_type_with_results_retail_never_paired() {
        let sd = static_data_from_deploy();
        for (label, job) in live_craft_rows() {
            let wire = wire_of(&job, &sd);
            assert_ne!(
                wire["craftingTypeId"], wire["recipeId"],
                "{label}: craftingTypeId == recipeId"
            );
            assert_ne!(
                wire["craftingTypeId"].as_str().unwrap(),
                Uuid::nil().to_string(),
                "{label}: craftingTypeId is nil"
            );
            let results = &wire["results"];
            assert!(
                results.is_object() && !results.as_object().unwrap().is_empty(),
                "{label}: empty results"
            );
            assert_shape_consistent(label, &wire, &sd);
        }
    }

    /// `finish` runs the same repair, so a row that cannot be relabelled safely must
    /// still grant something. Pins that the shape gate did not reintroduce the empty
    /// grant report #34's second half was about.
    #[test]
    fn every_live_row_still_grants_something_on_finish() {
        let sd = static_data_from_deploy();
        for (label, job) in live_craft_rows() {
            let (_, results) = repaired_craft_fields(&job, &sd, repair_data_from_deploy());
            let reward = reward_from_results(&results);
            assert!(
                !reward.items.is_empty() || !reward.stackable_items.is_empty(),
                "{label}: finish would grant nothing"
            );
        }
    }

    /// The classification behind the gate, pinned against what the captures show, so a
    /// re-extraction that renames or adds a CraftingType fails here rather than in
    /// someone's loading screen.
    #[test]
    fn every_crafting_type_classifies_as_the_captures_show_it() {
        let sd = static_data_from_deploy();
        let expected = [
            ("Smithing", CraftResultShape::Instanced),
            ("Tempering", CraftResultShape::Instanced),
            ("Enchanting", CraftResultShape::Instanced),
            ("Repairing", CraftResultShape::Instanced),
            ("Salvaging", CraftResultShape::Instanced),
            ("Alchemy", CraftResultShape::Stackable),
            ("DecorationCrafting", CraftResultShape::Stackable),
        ];
        assert_eq!(
            expected.len(),
            sd.recipe_crafting_types.crafting_types.len(),
            "a CraftingType was added or removed — classify it before shipping"
        );
        for (name, shape) in expected {
            let id = sd
                .recipe_crafting_types
                .type_by_name(name)
                .unwrap_or_else(|| panic!("{name} missing from the table"));
            assert_eq!(
                sd.recipe_crafting_types.result_shape_of_type(&id),
                Some(shape),
                "{name} classified against the captured result shape"
            );
        }
    }

    /// A recipe genuinely absent from the shipped client data keeps the old fallback —
    /// the table adds an answer, it does not remove the safety net.
    #[test]
    fn a_recipe_absent_from_the_table_still_falls_back() {
        let sd = static_data_from_deploy();
        let unknown = Uuid::from_u128(0xDEAD_BEEF_DEAD_BEEF);
        assert!(apk_crafting_type(&unknown, &sd).is_none(), "not in the shipped data");
        let job = CraftJob {
            id: Uuid::from_u128(0xF1),
            recipe_id: unknown,
            building_id: Uuid::from_u128(0xB3),
            crafting_type_id: unknown,
            completed_at_ms: 1_783_204_348_637,
            results: serde_json::json!({}),
        };
        let wire = wire_of(&job, &sd);
        let ctid = wire["craftingTypeId"].as_str().unwrap();
        assert_ne!(ctid, unknown.to_string(), "must never echo the recipe id");
        assert_eq!(ctid, ALCHEMY_CRAFTING_TYPE_ID, "falls back exactly as before");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Speed-up (tracker #88): `finish` with `speedUp: true` used to hand the
    // craft over for free. It is billed from the SAME global curve as town
    // construction — retail's `RecipeData._skipTimeData` and
    // `BuildingConstructionDataList._skipTimeData` point at one asset.
    // ─────────────────────────────────────────────────────────────────────────

    use blades_lib::economy::skip_time::SkipTimeCostTable;
    use blades_lib::economy::{GEMS, Price};

    fn shipped_table() -> SkipTimeCostTable {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../../deploy/static/building_upgrades.json"))
                .expect("valid JSON");
        SkipTimeCostTable::from_static(&v).expect("the shipped static carries the table")
    }

    fn wallet_with(gems: u64) -> CompleteWallet {
        let mut w = CompleteWallet::default();
        w.credit(GEMS, gems);
        w
    }

    /// The captured band-join price applies to crafts too: 47 994 s left = 152 gems.
    #[test]
    fn craft_speed_up_debits_the_measured_gem_price() {
        let now = 1_800_000_000_000i64;
        let mut w = wallet_with(1_000);
        let charged =
            charge_craft_speed_up(true, Some(&shipped_table()), now + 47_994_000, now, &mut w)
                .expect("affordable");
        assert_eq!(charged, vec![Price::new(GEMS, 152)]);
        assert_eq!(w.balance(GEMS), 848);
    }

    /// Collecting a finished craft is free, flag or no flag.
    #[test]
    fn craft_speed_up_without_the_flag_or_on_an_elapsed_job_is_free() {
        let now = 1_800_000_000_000i64;

        let mut w = wallet_with(1_000);
        assert!(
            charge_craft_speed_up(false, Some(&shipped_table()), now + 47_994_000, now, &mut w)
                .unwrap()
                .is_empty()
        );
        assert_eq!(w.balance(GEMS), 1_000, "speedUp:false must not debit");

        let mut w = wallet_with(1_000);
        assert!(
            charge_craft_speed_up(true, Some(&shipped_table()), now - 60_000, now, &mut w)
                .unwrap()
                .is_empty()
        );
        assert_eq!(w.balance(GEMS), 1_000, "an elapsed job is just a collect");
    }

    /// Not enough gems fails, and the wallet is untouched — the player keeps the
    /// job instead of collecting it early for nothing.
    #[test]
    fn craft_speed_up_without_enough_gems_fails_and_debits_nothing() {
        use actix_web::ResponseError;
        let now = 1_800_000_000_000i64;
        let mut w = wallet_with(151);
        let err = charge_craft_speed_up(true, Some(&shipped_table()), now + 47_994_000, now, &mut w)
            .expect_err("151 gems cannot buy a 152-gem skip");
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(w.balance(GEMS), 151);
    }

    /// No table (static not pushed to the box yet) → free, not an error.
    #[test]
    fn craft_speed_up_without_a_table_is_free_not_broken() {
        let now = 1_800_000_000_000i64;
        let mut w = wallet_with(1_000);
        assert!(
            charge_craft_speed_up(true, None, now + 47_994_000, now, &mut w)
                .expect("a missing table must not error")
                .is_empty()
        );
        assert_eq!(w.balance(GEMS), 1_000);
    }
}
