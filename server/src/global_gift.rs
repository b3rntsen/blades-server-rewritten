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
use blades_lib::economy::{RewardGrant, apply_reward, grant_chest};
use blades_lib::features::{
    chests,
    gifts::{self, GiftError},
};
use blades_lib::static_data::GiftDef;
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, OptionalExtension, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    arena::{season_rewards, season_store},
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

    // A closed Arena season is its own per-character global gift, exactly as
    // in retail. The award rows were frozen at close and the unique index makes
    // this a bounded lookup of at most three rows; no character-table scan and
    // no bulk grant is involved.
    use crate::schema::arena_seasons::dsl as s;
    let season: Option<season_store::SeasonRow> = s::arena_seasons
        .filter(s::id.eq(gift_id))
        .filter(s::status.eq("ended"))
        .select(season_store::SeasonRow::as_select())
        .first(&mut conn)
        .await
        .optional()?;
    if let Some(season) = season {
        use crate::schema::arena_season_awards::dsl as a;
        let awards: Vec<season_store::AwardClaimRow> = a::arena_season_awards
            .filter(a::season_id.eq(gift_id))
            .filter(a::character_id.eq(character_id))
            .select(season_store::AwardClaimRow::as_select())
            .load(&mut conn)
            .await?;
        if awards.is_empty() {
            return Err(map_gift_err(GiftError::NotFound));
        }
        let claim_count = u64::from(awards.iter().all(|award| award.granted_at.is_some()));
        let def = season_rewards::gift_for_awards(
            gift_id,
            season.number,
            awards
                .iter()
                // A recovery batch may already have granted one row. Keep the
                // remaining gift honest instead of displaying that row twice.
                .filter(|award| award.granted_at.is_none())
                .map(|award| (award.kind.as_str(), award.tier.as_str(), &award.payload)),
        )
        .or_else(|| {
            // A claimed gift can still be queried directly by the client. It
            // expects the definition alongside claimCount=1 in that case.
            (claim_count == 1).then(|| {
                season_rewards::gift_for_awards(
                    gift_id,
                    season.number,
                    awards.iter().map(|award| {
                        (award.kind.as_str(), award.tier.as_str(), &award.payload)
                    }),
                )
            })?
        })
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
        return Ok(Json(GiftViewResponse {
            global_gift: GiftView {
                global_gift_id: gift_id,
                claim_count,
                global_gift_override: def,
            },
        }));
    }

    let def = app_state
        .static_data
        .gifts
        .get(&gift_id)
        .cloned()
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
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

    let now = now_secs();
    let mut conn = app_state.db_pool.get().await.unwrap();

    use crate::schema::arena_seasons::dsl as s;
    let season: Option<season_store::SeasonRow> = s::arena_seasons
        .filter(s::id.eq(gift_id))
        .filter(s::status.eq("ended"))
        .select(season_store::SeasonRow::as_select())
        .first(&mut conn)
        .await
        .optional()?;

    if let Some(season) = season {
        let globals = app_state.get_ref().clone();
        return conn
            .transaction(move |mut conn| {
                async move {
                    // Lock awards before the character, matching the admin fallback
                    // grant's lock order so the two claim paths cannot deadlock.
                    use crate::schema::arena_season_awards::dsl as a;
                    let awards: Vec<season_store::AwardClaimRow> = a::arena_season_awards
                        .filter(a::season_id.eq(gift_id))
                        .filter(a::character_id.eq(character_id))
                        .select(season_store::AwardClaimRow::as_select())
                        .for_update()
                        .load(&mut conn)
                        .await?;
                    if awards.is_empty() {
                        return Err(map_gift_err(GiftError::NotFound));
                    }
                    if awards.iter().all(|award| award.granted_at.is_some()) {
                        return Err(map_gift_err(GiftError::LimitReached));
                    }

                    // A season claim is atomic: a partially admin-granted season
                    // claims only its remaining rows, never duplicates an item.
                    let pending: Vec<_> = awards
                        .into_iter()
                        .filter(|award| award.granted_at.is_none())
                        .collect();
                    let mut reward = RewardGrant::default();
                    let mut recorded = Vec::with_capacity(pending.len());
                    for award in pending {
                        let mut individual = season_rewards::reward_for_award(
                            season.number,
                            &award.kind,
                            &award.tier,
                            &award.payload,
                            &globals.repair_data,
                        )
                        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
                        for item in &mut individual.items {
                            item.id = Uuid::new_v4();
                        }
                        season_rewards::merge_reward(&mut reward, individual.clone());
                        recorded.push((award, individual));
                    }

                    // Retail's season gift opens its Gold/Elder chest during claim;
                    // it does not leave a treasury chest. Use the same deterministic,
                    // capture-derived loot pool as the normal chest endpoint.
                    let season_chests = std::mem::take(&mut reward.chests);
                    for (index, chest) in season_chests.into_iter().enumerate() {
                        let key = format!(
                            "arena-season:{gift_id}:{character_id}:{}:{index}",
                            chest.tier
                        );
                        if let Some(loot) =
                            chests::pick_loot(&globals.static_data.chest_loots, &key)
                        {
                            let mut loot = loot.clone();
                            for item in &mut loot.items {
                                item.id = Uuid::new_v4();
                            }
                            season_rewards::merge_reward(&mut reward, loot);
                        }
                    }

                    let mut entry = {
                        use crate::schema::characters;
                        characters::table
                            .filter(characters::id.eq(character_id))
                            .filter(characters::user_id.eq(user_id))
                            .select(CharacterDbEntryEconomy::as_select())
                            .for_no_key_update()
                            .first(&mut conn)
                            .await
                            .map_err(|_| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
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
                    entry.server_state.0.gift_claims.insert(gift_id, 1);

                    let claimed_at = now_secs();
                    for (award, individual) in recorded {
                        let mut payload = award.payload;
                        payload["granted"] = serde_json::Value::Bool(true);
                        payload["claimedVia"] = serde_json::Value::String("globalGift".into());
                        payload["reward"] = serde_json::to_value(individual).map_err(|_| {
                            BladeApiError::new(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                GIFT_SERVICE_ID,
                                4,
                            )
                        })?;
                        let changed = diesel::update(
                            a::arena_season_awards
                                .filter(a::id.eq(award.id))
                                .filter(a::granted_at.is_null()),
                        )
                        .set((a::payload.eq(payload), a::granted_at.eq(Some(claimed_at))))
                        .execute(&mut conn)
                        .await?;
                        if changed != 1 {
                            return Err(BladeApiError::new(
                                StatusCode::CONFLICT,
                                GIFT_SERVICE_ID,
                                5,
                            ));
                        }
                    }

                    let inventory = entry.inventory.0.generate_client_update(&tracker);
                    let wallet = entry.wallet.0.clone();
                    {
                        use crate::schema::characters;
                        diesel::update(characters::table.filter(characters::id.eq(entry.id)))
                            .set(entry)
                            .execute(&mut conn)
                            .await?;
                    }

                    Ok::<_, BladeApiError>(Json(GiftClaimResponse {
                        reward,
                        global_gift: ClaimedGiftInfo {
                            global_gift_id: gift_id,
                            claim_count: 1,
                        },
                        inventory,
                        wallet,
                    }))
                }
                .scope_boxed()
            })
            .await;
    }

    let def = app_state
        .static_data
        .gifts
        .get(&gift_id)
        .cloned()
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;

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

            let mut reward = gifts::build_gift_reward(&def);
            for chest in &mut reward.chests {
                if chest.level == 0 {
                    chest.level = entry.character.0.level as u64;
                }
            }
            let mut tracker = InventoryChangeTracker::default();
            apply_reward(
                &reward,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut entry.character.0,
                &mut tracker,
            );
            for chest in &reward.chests {
                grant_chest(
                    &mut entry.inventory.0,
                    chest.tier,
                    chest.level,
                    &mut tracker,
                );
            }
            if !reward.stackable_items.is_empty() || !reward.items.is_empty() {
                entry.inventory.0.backpack_version += 1;
            }
            if !reward.chests.is_empty() {
                entry.inventory.0.treasury_version += 1;
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
