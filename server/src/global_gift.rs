//! Global gifts — `GET /…/globalgifts`, `GET /…/globalgifts/{id}`,
//! `POST /…/globalgifts/{id}` (claim).
//!
//! Bethesda hands out time-windowed gifts (e.g. the captured "Sunset Gift" =
//! 50000 Gems + 1000 Sigil, claim limit 1). The gift catalogue is capture-derived
//! ([`crate::static_loader`] → `gifts.json`); per-character claim counts live in
//! `server_state.gift_claims`. The reward/window/limit logic is the pure
//! [`blades_lib::features::gifts`] layer; this handler only does IO.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, apply_reward};
use blades_lib::features::gifts::{self, GiftError};
use blades_lib::static_data::GiftDef;
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    models::{CharacterDbEntryEconomy, CharacterDbEntryServerState},
    session::SessionLookedUpMaybe,
    util::get_only_single_character_and_check_permission,
};

/// Out-of-band service id for gift error envelopes (not a real Blades id).
const GIFT_SERVICE_ID: u64 = 9003;

fn map_gift_err(e: GiftError) -> BladeApiError {
    match e {
        GiftError::NotFound => BladeApiError::new(StatusCode::NOT_FOUND, GIFT_SERVICE_ID, 1),
        GiftError::NotActive => BladeApiError::new(StatusCode::BAD_REQUEST, GIFT_SERVICE_ID, 2),
        GiftError::LimitReached => BladeApiError::new(StatusCode::CONFLICT, GIFT_SERVICE_ID, 3),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimedGift {
    global_gift_id: Uuid,
    claim_count: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimedGiftsResponse {
    claimed_global_gifts: Vec<ClaimedGift>,
}

/// List the gifts this character has already claimed (and how many times).
#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/globalgifts")]
pub async fn get_global_gifts(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<ClaimedGiftsResponse>, BladeApiError> {
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

    let claimed_global_gifts = entry
        .server_state
        .0
        .gift_claims
        .iter()
        .map(|(global_gift_id, claim_count)| ClaimedGift {
            global_gift_id: *global_gift_id,
            claim_count: *claim_count,
        })
        .collect();
    Ok(Json(ClaimedGiftsResponse {
        claimed_global_gifts,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GiftView {
    global_gift_id: Uuid,
    claim_count: u64,
    global_gift_override: GiftDef,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GiftViewResponse {
    global_gift: GiftView,
}

/// View a single gift definition + this character's claim count.
#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/globalgifts/{gift_id}")]
pub async fn get_global_gift(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<GiftViewResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_id, gift_id) = path.into_inner();

    let def = app_state
        .static_data
        .gifts
        .get(&gift_id)
        .cloned()
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;

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
    let claim_count = entry
        .server_state
        .0
        .gift_claims
        .get(&gift_id)
        .copied()
        .unwrap_or(0);

    Ok(Json(GiftViewResponse {
        global_gift: GiftView {
            global_gift_id: gift_id,
            claim_count,
            global_gift_override: def,
        },
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimedGiftInfo {
    global_gift_id: Uuid,
    claim_count: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GiftClaimResponse {
    reward: RewardGrant,
    global_gift: ClaimedGiftInfo,
    inventory: CompleteInventoryUpdate,
    /// `CompleteWallet` serializes as the bare `wallet` array of `{currencyId, balance}`.
    wallet: CompleteWallet,
}

/// Claim a gift: validate the window + per-character claim limit, grant the reward
/// (currencies credit the wallet, other templates grant stackables), bump the claim
/// count, and return the uniform `{reward, globalGift, inventory, wallet}` shape.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/globalgifts/{gift_id}")]
pub async fn claim_global_gift(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    _body: Json<Option<serde_json::Value>>,
) -> Result<Json<GiftClaimResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, gift_id) = path.into_inner();

    let def = app_state
        .static_data
        .gifts
        .get(&gift_id)
        .cloned()
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
    let now = now_secs();
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

            let current = entry
                .server_state
                .0
                .gift_claims
                .get(&gift_id)
                .copied()
                .unwrap_or(0);
            gifts::can_claim(&def, current, now).map_err(map_gift_err)?;

            let reward = gifts::build_gift_reward(&def);
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
            let new_count = current + 1;
            entry.server_state.0.gift_claims.insert(gift_id, new_count);

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

            Ok::<_, BladeApiError>(Json(GiftClaimResponse {
                reward,
                global_gift: ClaimedGiftInfo {
                    global_gift_id: gift_id,
                    claim_count: new_count,
                },
                inventory,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}
