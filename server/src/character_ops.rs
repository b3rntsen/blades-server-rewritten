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
use blades_lib::features::level_up;
use blades_lib::user_data::{
    CompleteCharacter, CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate,
    CompleteWallet, InventoryChangeTracker,
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

fn set_level_up_offer(
    character: &mut CompleteCharacter,
    offers: &HashMap<u16, Uuid>,
    now: u64,
) {
    character.global_shop_offers = match offers.get(&character.level) {
        Some(product_id) => serde_json::json!([{
            "globalShopProductId": product_id,
            "startTime": now,
        }]),
        // Do not carry a previous level's expired offer into the new level.
        None => serde_json::json!([]),
    };
}

/// `POST /levelup` — spend banked experience on a level, into STAMINA or MAGICKA.
///
/// The decision — is it affordable, what does it cost, what does it pay — lives in
/// [`blades_lib::features::level_up::apply_level_up`], measured against 30 captured
/// retail level-ups. This handler is the IO around it: load, apply, persist, and
/// answer in retail's shape.
///
/// Retail's shape is `{character, inventory}` plus a `wallet` **only when the level
/// credited a currency**, and then carrying only that currency (18 of the 30
/// captured level-ups send no wallet at all). We used to send the whole purse on
/// every call, which told the client a balance had changed when it had not.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/levelup")]
pub async fn levelup(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    body: Json<LevelupRequest>,
) -> Result<Json<LevelupResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let attribute = Attribute::parse(&body.attribute)
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, CHAR_OPS_SERVICE_ID, 1))?;
    let app_state_clone = app_state.into_inner().clone();
    let db_pool = app_state_clone.db_pool.clone();
    let mut conn = db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;

            // Refuse before anything is written: an unearned level must leave the
            // character exactly as it was, not half-applied.
            let outcome = level_up::apply_level_up(
                &mut entry.character.0,
                attribute,
                &app_state_clone.level_up_data,
            )
            .map_err(levelup_refusal)?;

            // The client renders a level-up offer from `character.globalShopOffers`.
            // Returning the old/null value still opens the card, but leaves its
            // price loading forever. Retail stores the product id plus the offer's
            // start time on the character; stamp the latest captured, grantable
            // product for this exact level. Levels with no fully-known grant are
            // deliberately left without an offer rather than advertising something
            // the purchase endpoint cannot fulfil.
            let start_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            set_level_up_offer(
                &mut entry.character.0,
                &app_state_clone.static_data.level_up_offers,
                start_time,
            );

            let mut tracker = InventoryChangeTracker::default();
            blades_lib::economy::apply_reward(
                &outcome.reward,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut entry.character.0,
                &mut tracker,
            );
            if !outcome.reward.stackable_items.is_empty() || !outcome.reward.items.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }

            let resp = LevelupResponse {
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: entry.character.0.clone(),
                },
                wallet: credited_wallet(&entry.wallet.0, &outcome.credited_currencies),
                inventory: entry.inventory.0.generate_client_update(&tracker),
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(resp))
        }
        .scope_boxed()
    })
    .await
}

/// `/levelup`'s response. `wallet` is absent unless the level paid something —
/// see the handler doc for the captured shape.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelupResponse {
    character: CompleteCharacterWithIdWithoutData,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
    inventory: CompleteInventoryUpdate,
}

/// The post-credit balances of just the currencies this level paid out, or `None`
/// when it paid none — which is what makes the `wallet` key disappear.
fn credited_wallet(wallet: &CompleteWallet, credited: &[Uuid]) -> Option<CompleteWallet> {
    if credited.is_empty() {
        return None;
    }
    let mut out = HashMap::new();
    for currency in credited {
        if let Some(entry) = wallet.0.get(currency) {
            out.insert(*currency, entry.clone());
        }
    }
    Some(CompleteWallet(out))
}

/// Map a refusal onto the wire. All three are the client asking for a level it
/// cannot have, so all three are 400s rather than 500s.
fn levelup_refusal(err: level_up::LevelUpRefusal) -> BladeApiError {
    use level_up::LevelUpRefusal::*;
    let code = match err {
        NotEnoughExperience { .. } => 2,
        AlreadyMaxLevel { .. } => 3,
        UnknownLevel { .. } => 4,
    };
    log::info!("levelup refused: {err:?}");
    BladeApiError::new(StatusCode::BAD_REQUEST, CHAR_OPS_SERVICE_ID, code)
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

/// The client writes an EMPTY SLOT two different ways in `equipmentUpdates`:
/// `null`, and the empty string `""`. Both mean "nothing is in this slot".
///
/// `Option<Uuid>` accepts the first and rejects the second, so a single `""`
/// failed the whole request with
/// `Json deserialize error: UUID parsing failed: invalid length: expected
/// length 32 for simple format, found 0`, HTTP 400 — and because the client
/// retries a failed loadout switch, the player got a loading loop they had to
/// force-quit out of (tracker #22).
///
/// Reported by Swanne, whose saved "Vs Warrior" profile has 3 of 9 slots
/// filled. The request carried the 6 empty ones as four `""` and two `null` —
/// the same emptiness spelled two ways, one of which we refused.
///
/// Mapping `""` to `None` is safe rather than merely convenient: `None`
/// already means unequip in `apply_equipment_updates`, which returns whatever
/// occupies the slot to the backpack and puts nothing in it. So both spellings
/// now do the one thing the client means by them.
///
/// A non-empty value that is not a uuid is still an error. The bug was that we
/// rejected a legitimate encoding of "empty", not that we were too strict.
fn deserialize_equipment_updates<'de, D>(de: D) -> Result<HashMap<Uuid, Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let raw: HashMap<Uuid, Option<String>> = HashMap::deserialize(de)?;
    raw.into_iter()
        .map(|(slot, target)| {
            let item = match target.as_deref().map(str::trim) {
                None | Some("") => None,
                Some(s) => Some(Uuid::parse_str(s).map_err(D::Error::custom)?),
            };
            Ok((slot, item))
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadoutCurrentRequest {
    #[serde(default, deserialize_with = "deserialize_equipment_updates")]
    equipment_updates: HashMap<Uuid, Option<Uuid>>,
    #[serde(default)]
    ability_updates: Value,
    /// Equipped consumables (potions), by stackable TEMPLATE id — the client's separate
    /// `equippedConsumables` field on this endpoint (il2cpp `PARAMETER_EQUIPPED_CONSUMABLES`).
    /// Absent for a gear/ability-only update; when present it is the FULL equipped-
    /// consumable list. Consumables are stackable (not instanced gear), so equipping a
    /// potion arrives here — the old handler ignored the field and the equip silently
    /// vanished, which the client surfaced as "Unable to connect".
    #[serde(default)]
    equipped_consumables: Option<Vec<Uuid>>,
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
    // Cloned before the closure so the slot check can read game data inside it
    // without borrowing `app_state` across the `conn` borrow (E0505).
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let mut tracker = InventoryChangeTracker::default();
            let mut inventory_changed = false;
            if !body.equipment_updates.is_empty() {
                character_ops::apply_equipment_updates(
                    &mut entry.inventory.0,
                    &body.equipment_updates,
                    &mut tracker,
                    Some(&globals.game_data),
                );
                inventory_changed = true;
            }
            // A potion equip arrives as the separate `equippedConsumables` list (a full
            // replacement). Apply it faithfully so the equipped consumable lands + is
            // echoed in the loadout diff — the old handler ignored this, so the equip
            // never took and the client showed "Unable to connect".
            if let Some(consumables) = &body.equipped_consumables {
                let changed = character_ops::set_equipped_consumables(
                    &mut entry.inventory.0,
                    consumables,
                    &mut tracker,
                );
                inventory_changed |= changed;
            }
            if inventory_changed {
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

///////////////////////////////////////////////////////////////////////////////////////////////////////
/// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use blades_lib::economy::{GEMS, GOLD};
    use super::*;

    #[test]
    fn level_up_offer_gets_a_current_start_time() {
        let product = Uuid::from_u128(7);
        let offers = HashMap::from([(14, product)]);
        let mut character = CompleteCharacter {
            level: 14,
            global_shop_offers: serde_json::json!([{
                "globalShopProductId": "expired",
                "startTime": 1,
            }]),
            ..CompleteCharacter::default()
        };

        set_level_up_offer(&mut character, &offers, 1_789_261_777);
        assert_eq!(
            character.global_shop_offers,
            serde_json::json!([{
                "globalShopProductId": product,
                "startTime": 1_789_261_777u64,
            }])
        );

        character.level = 15;
        set_level_up_offer(&mut character, &offers, 1_789_261_778);
        assert_eq!(character.global_shop_offers, serde_json::json!([]));
    }

    /// Swanne's actual request body, tracker #22 (capture 261220). Nine slots:
    /// three real items, four `""` and two `null`. Before the custom
    /// deserializer this whole body failed with HTTP 400 on the first `""`,
    /// and the client's retry turned that into a loading loop.
    const SWANNE_VS_WARRIOR: &str = r#"{
      "equipmentUpdates": {
        "897a600c-91d6-4449-af09-173da88a907e": "933a1308-96c7-408d-a7e0-e6e0286f50a3",
        "e273a4d7-fb87-4f7e-8f1e-398be59afbcb": "94d056cb-8462-47d1-8574-58956141cce6",
        "58b6d121-2e23-4fa4-b892-c92ae2e2c4c5": "",
        "48021ab1-a1a6-487b-80a4-ca472a4d0c77": "573dbf5f-ecd3-434c-bdf6-0e47dcff8c69",
        "417e79de-c810-42f8-8273-f9759df6ae25": null,
        "862605de-c67f-4bce-b527-4e5fb6f25162": null,
        "36d141e4-7783-466c-9565-6f90f09de428": "",
        "0d8f2023-4701-41e8-8bd5-92381d787456": "",
        "959c1931-bf85-4587-92ec-8ecaa58b06d5": ""
      }
    }"#;

    #[test]
    fn a_loadout_with_empty_slots_is_accepted() {
        let req: LoadoutCurrentRequest = serde_json::from_str(SWANNE_VS_WARRIOR)
            .expect("the real request body must deserialize");
        assert_eq!(req.equipment_updates.len(), 9, "all nine slots survive");
        let filled = req.equipment_updates.values().filter(|v| v.is_some()).count();
        assert_eq!(filled, 3, "exactly the three equipped items are Some");
        assert_eq!(
            req.equipment_updates.values().filter(|v| v.is_none()).count(),
            6,
            "the six empty slots are None, however they were spelled"
        );
    }

    #[test]
    fn empty_string_and_null_mean_the_same_thing() {
        let slot_a = "58b6d121-2e23-4fa4-b892-c92ae2e2c4c5".parse::<Uuid>().unwrap();
        let slot_b = "417e79de-c810-42f8-8273-f9759df6ae25".parse::<Uuid>().unwrap();
        let req: LoadoutCurrentRequest = serde_json::from_str(SWANNE_VS_WARRIOR).unwrap();
        assert_eq!(req.equipment_updates[&slot_a], None, "\"\" is an empty slot");
        assert_eq!(req.equipment_updates[&slot_b], None, "null is an empty slot");
    }

    #[test]
    fn whitespace_only_is_also_empty() {
        let body = r#"{"equipmentUpdates":{"897a600c-91d6-4449-af09-173da88a907e":"   "}}"#;
        let req: LoadoutCurrentRequest = serde_json::from_str(body).unwrap();
        assert!(req.equipment_updates.values().all(Option::is_none));
    }

    /// The fix must not turn the endpoint into one that accepts anything. A
    /// non-empty value that is not a uuid is still a client bug and still an
    /// error — otherwise this test could not tell a working deserializer from
    /// one that simply dropped every value on the floor.
    #[test]
    fn a_malformed_uuid_is_still_rejected() {
        let body = r#"{"equipmentUpdates":{"897a600c-91d6-4449-af09-173da88a907e":"not-a-uuid"}}"#;
        assert!(serde_json::from_str::<LoadoutCurrentRequest>(body).is_err());
    }

    #[test]
    fn an_absent_field_is_still_an_empty_map() {
        let req: LoadoutCurrentRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert!(req.equipment_updates.is_empty());
    }

    // ── /levelup's response shape ────────────────────────────────────────────
    //
    // The serialized body is what the client parses, so these assert on the JSON
    // rather than on the struct: a `skip_serializing_if` that stops working is
    // invisible at the type level.

    fn wallet_of(pairs: &[(uuid::Uuid, u64)]) -> CompleteWallet {
        CompleteWallet(
            pairs
                .iter()
                .map(|(c, b)| (*c, blades_lib::user_data::WalletEntry { balance: *b }))
                .collect(),
        )
    }

    fn body(wallet: Option<CompleteWallet>) -> serde_json::Value {
        let resp = LevelupResponse {
            character: CompleteCharacterWithIdWithoutData {
                id: uuid::Uuid::nil(),
                character: Default::default(),
            },
            wallet,
            inventory: CompleteInventoryUpdate {
                backpack: Default::default(),
                loadout: Default::default(),
                treasury: Default::default(),
                overflow_treasury: Default::default(),
                backpack_version: 1,
                treasury_version: 1,
            },
        };
        serde_json::to_value(resp).unwrap()
    }

    /// 18 of the 30 captured retail level-ups pay nothing and send no `wallet`
    /// key. Sending an empty array instead is not the same thing: it reads as
    /// "your balances are now these", which for an empty array means zero.
    #[test]
    fn a_level_that_pays_nothing_sends_no_wallet_key() {
        let json = body(credited_wallet(&wallet_of(&[(GOLD, 900), (GEMS, 12)]), &[]));
        assert!(
            json.get("wallet").is_none(),
            "expected no wallet key, got {json}"
        );
        assert!(json.get("character").is_some() && json.get("inventory").is_some());
    }

    /// …and a level that pays sends only what it paid, at its post-credit balance.
    #[test]
    fn a_paying_level_sends_only_the_currency_it_credited() {
        let purse = wallet_of(&[(GOLD, 900), (GEMS, 12)]);
        let json = body(credited_wallet(&purse, &[GEMS]));
        let entries = json["wallet"].as_array().expect("wallet array");
        assert_eq!(entries.len(), 1, "one currency, not the whole purse: {json}");
        assert_eq!(entries[0]["currencyId"], GEMS.to_string());
        assert_eq!(entries[0]["balance"], 12);
    }

    /// Every refusal is a 400 — the client asked for a level it cannot have, which
    /// is a bad request, not a server fault. A 500 here would make the client
    /// retry forever.
    #[test]
    fn every_levelup_refusal_is_a_client_error() {
        use blades_lib::features::level_up::LevelUpRefusal::*;
        for err in [
            NotEnoughExperience { have: 1, need: 50 },
            AlreadyMaxLevel { level: 100 },
            UnknownLevel { level: 101 },
        ] {
            let api = levelup_refusal(err);
            assert_eq!(
                actix_web::ResponseError::status_code(&api),
                StatusCode::BAD_REQUEST,
                "refusals must not look like server faults"
            );
        }
    }
}
