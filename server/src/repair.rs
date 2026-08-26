//! Item repair — `POST /…/characters/{id}/repairs`.
//!
//! In Elder Scrolls: Blades gear loses durability and eventually breaks; the
//! player repairs it at the blacksmith. Captured request/response (retail):
//!
//! ```jsonc
//! // request
//! { "repairInfos": [ { "recipeId": "<uuid>", "itemId": "<uuid>" }, … ],
//!   "buildingId": "<smithy-uuid>", "gemsPayment": false }
//! // response
//! { "inventory": <CompleteInventoryUpdate>, "wallet": [ { currencyId, balance } ] }
//! ```
//!
//! A "Repair all" is ONE such POST carrying every damaged item (retail captures
//! show `repairInfos` lengths of 1..24), so this handler is exactly where
//! tracker #30's "Repair all does not repair all" lives.
//!
//! All the gameplay logic — max durability per `(itemTemplateId, temperingLevel)`
//! and the gold price — lives in [`blades_lib::features::repair`], derived from
//! the APK's own `ItemTemplate._temperProperties` and `RepairRecipe` tables. This
//! module is the thin transactional shell: load → repair → persist → serialize.
//!
//! ## Known limitation
//!
//! Retail could also hold gear in `inventory.treasury.items`, and a repair
//! response returned repaired treasury items there. Our `Treasury` models only
//! chests, so treasury-held gear is outside the repair path. No character on
//! prod has any (checked: 0 of 62), so this is a modelling gap to close with the
//! treasury model itself rather than here.

use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::features::repair::{self, RepairData};
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy, session::SessionLookedUpMaybe,
};

/// Out-of-band service id for repair error envelopes (not a real Blades service
/// id; the only failure path that fires in practice is "blacksmith busy", which
/// the emulator never hits today — see [`blacksmith_has_free_slot`]).
const REPAIR_SERVICE_ID: u64 = 9002;

/// The blacksmith has two work slots; repair is blocked only when both are busy
/// crafting/tempering.
const BLACKSMITH_SLOTS: usize = 2;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RepairInfo {
    /// The repair recipe. Retail's `recipeId` ↔ `itemTemplateId` mapping is
    /// exactly 1:1 (254 recipes / 254 templates over 426 captures), so it carries
    /// no information the item does not already have: we price and restore by
    /// `item_id` and the item's own template + tempering level.
    #[serde(default)]
    #[allow(dead_code)]
    recipe_id: Option<Uuid>,
    item_id: Uuid,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RepairRequest {
    repair_infos: Vec<RepairInfo>,
    #[serde(default)]
    #[allow(dead_code)]
    building_id: Option<Uuid>,
    /// Retail never sent `true` in any of the 426 captured repairs, so the gems
    /// price is unknown. Accepted and ignored; the gold price is charged.
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairResponse {
    inventory: CompleteInventoryUpdate,
    /// `CompleteWallet` (de)serializes as a bare ARRAY of `{currencyId, balance}`.
    wallet: CompleteWallet,
}

#[post("/api/game/v1/public/characters/{character_id}/repairs")]
pub async fn repair_items(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<RepairRequest>,
) -> Result<Json<RepairResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    // `globals` is moved into the transaction closure (for the repair tables), so
    // take `conn` from the `app_state` Data handle — not from `globals` — to
    // avoid borrowing the value we move.
    let globals: Arc<ServerGlobal> = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the requesting user's character (ownership enforced by the
            // user_id filter), locking the row for the read-modify-write.
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

            // Real-game rule: repair needs a free blacksmith slot (the smith can't
            // repair while crafting/tempering). The emulator persists no craft jobs,
            // so the smith is always free; gate kept here so it activates once
            // crafting is modeled.
            if !blacksmith_has_free_slot() {
                return Err(BladeApiError::new(StatusCode::CONFLICT, REPAIR_SERVICE_ID, 1));
            }

            let data: &RepairData = &globals.repair_data;
            let requested: Vec<Uuid> = body.repair_infos.iter().map(|r| r.item_id).collect();

            let mut tracker = InventoryChangeTracker::default();
            let outcome = repair::apply_repairs(
                data,
                &requested,
                &mut entry.inventory.0,
                &mut entry.wallet.0,
                &mut tracker,
            );

            if !outcome.unknown.is_empty() {
                log::info!(
                    "[repair] character {character_id}: {} unknown item id(s) in the request \
                     (stale client state), ignored",
                    outcome.unknown.len()
                );
            }
            if !outcome.unaffordable.is_empty() {
                log::info!(
                    "[repair] character {character_id}: {} item(s) left unrepaired — not enough gold",
                    outcome.unaffordable.len()
                );
            }

            // Captures show backpackVersion increments on a repair. Only bump it
            // when something actually changed, so a no-op repair does not force
            // the client to resync.
            if !outcome.repaired.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }

            // Build the response before writing back (mirrors dungeon_update).
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();

            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }

            Ok::<_, BladeApiError>(Json(RepairResponse { inventory, wallet }))
        }
        .scope_boxed()
    })
    .await
}

/// Number of blacksmith jobs currently occupying a work slot. The emulator does
/// not persist craft/temper jobs yet (`craft::get_crafts` is empty), so this is
/// always 0. When crafting lands, count active jobs (`completedAt` > now) at the
/// blacksmith building here.
fn active_blacksmith_jobs() -> usize {
    0
}

fn blacksmith_has_free_slot() -> bool {
    active_blacksmith_jobs() < BLACKSMITH_SLOTS
}
