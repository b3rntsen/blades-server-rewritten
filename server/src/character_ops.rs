//! Character & inventory management endpoints — all previously unhandled (404):
//! `POST /levelup`, `/abilities`, `/respec`, `/inventories/current/upgrade`,
//! `/inventories/current/destroy`, `/loadouts/profiles/{n}`, `/loadouts/current`.
//!
//! Thin IO over the pure [`blades_lib::features::character_ops`] mutations. See that
//! module for the (documented) leniency on level-up/respec/upgrade currency costs,
//! which captures don't reveal.

use std::{collections::HashMap, sync::Arc};

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::features::character_ops::{self, Attribute};
use blades_lib::user_data::{
    CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
    InventoryChangeTracker,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
};

const CHAR_OPS_SERVICE_ID: u64 = 9006;

async fn load_owned(
    conn: &mut AsyncPgConnection,
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
    conn: &mut AsyncPgConnection,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CharacterWalletInventory {
    character: CompleteCharacterWithIdWithoutData,
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CharacterWallet {
    character: CompleteCharacterWithIdWithoutData,
    wallet: CompleteWallet,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CharacterOnly {
    character: CompleteCharacterWithIdWithoutData,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InventoryOnly {
    inventory: CompleteInventoryUpdate,
}

#[derive(Deserialize)]
struct LevelupRequest {
    attribute: String,
}

/// `POST /levelup` — spend a level into STAMINA or MAGICKA.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/levelup")]
pub async fn levelup(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<LevelupRequest>,
) -> Result<Json<CharacterWalletInventory>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let attribute = Attribute::parse(&body.attribute)
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, CHAR_OPS_SERVICE_ID, 1))?;
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            character_ops::apply_levelup(&mut entry.character.0, attribute);
            let resp = CharacterWalletInventory {
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: entry.character.0.clone(),
                },
                wallet: entry.wallet.0.clone(),
                inventory: entry
                    .inventory
                    .0
                    .generate_client_update(&InventoryChangeTracker::default()),
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(resp))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
struct AbilitiesRequest {
    #[serde(default)]
    abilities: Value,
}

/// `POST /abilities` — learn/upgrade abilities (`{abilities:{id:level}}`).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/abilities")]
pub async fn learn_abilities(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<AbilitiesRequest>,
) -> Result<Json<CharacterOnly>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let updates = body.into_inner().abilities;
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            character_ops::merge_abilities(&mut entry.character.0, &updates);
            let character = CompleteCharacterWithIdWithoutData {
                id: character_id,
                character: entry.character.0.clone(),
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(CharacterOnly { character }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
struct RespecRequest {
    #[serde(default)]
    stamina: u32,
    #[serde(default)]
    magicka: u32,
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
}

/// `POST /respec` — reallocate attribute points.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/respec")]
pub async fn respec(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<RespecRequest>,
) -> Result<Json<CharacterWallet>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            character_ops::apply_respec(&mut entry.character.0, body.stamina, body.magicka);
            let resp = CharacterWallet {
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: entry.character.0.clone(),
                },
                wallet: entry.wallet.0.clone(),
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(resp))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
struct UpgradeRequest {
    #[serde(default)]
    #[allow(dead_code)]
    gems_payment: bool,
}

/// `POST /inventories/current/upgrade` — raise backpack capacity tier.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/inventories/current/upgrade")]
pub async fn upgrade_inventory(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    _body: Json<UpgradeRequest>,
) -> Result<Json<CharacterWallet>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            character_ops::upgrade_inventory(&mut entry.character.0);
            let resp = CharacterWallet {
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: entry.character.0.clone(),
                },
                wallet: entry.wallet.0.clone(),
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(resp))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
struct DestroyRequest {
    #[serde(default)]
    items: Vec<Uuid>,
}

/// `POST /inventories/current/destroy` — destroy instanced backpack items.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/inventories/current/destroy")]
pub async fn destroy_items(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<DestroyRequest>,
) -> Result<Json<InventoryOnly>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let items = body.into_inner().items;
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            character_ops::destroy_items(&mut entry.inventory.0, &items, &mut tracker);
            entry.inventory.0.backpack_version += 1;
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(InventoryOnly { inventory }))
        }
        .scope_boxed()
    })
    .await
}

/// `POST /loadouts/profiles/{n}` — save a named loadout profile (returns `null`).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/loadouts/profiles/{index}")]
pub async fn save_loadout_profile(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, u32)>,
    body: Json<Value>,
) -> Result<Json<Value>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, index) = path.into_inner();
    let profile = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            character_ops::set_loadout_profile(&mut entry.character.0, index as usize, profile);
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(Value::Null))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadoutCurrentRequest {
    #[serde(default)]
    equipment_updates: HashMap<Uuid, Option<Uuid>>,
    #[serde(default)]
    ability_updates: Value,
}

/// `POST /loadouts/current` — equip/unequip gear and/or set equipped-ability slots.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/loadouts/current")]
pub async fn update_loadout(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<LoadoutCurrentRequest>,
) -> Result<Json<CharacterWalletInventory>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let body = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            if !body.equipment_updates.is_empty() {
                character_ops::apply_equipment_updates(
                    &mut entry.inventory.0,
                    &body.equipment_updates,
                    &mut tracker,
                );
                entry.inventory.0.backpack_version += 1;
            }
            if body.ability_updates.is_object() {
                character_ops::set_equipped_abilities(&mut entry.character.0, &body.ability_updates);
            }
            let resp = CharacterWalletInventory {
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: entry.character.0.clone(),
                },
                wallet: entry.wallet.0.clone(),
                inventory: entry.inventory.0.generate_client_update(&tracker),
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(resp))
        }
        .scope_boxed()
    })
    .await
}
