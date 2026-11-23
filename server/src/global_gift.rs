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
    chests, gift_capacity,
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
    arena::{arena_fight_gift, season_rewards, season_store},
    models::{CharacterDbEntryEconomy, CharacterDbEntryServerState},
    session::SessionLookedUpMaybe,
    util::get_only_single_character_and_check_permission,
};

/// Open the chests a season gift carries into `reward` itself, as retail did.
///
/// Report #335: season award rows build their chests with level 0, and this used
/// to roll them at that level — so a level-100 character's Gold chest paid what
/// retail paid for a level 1-3 Gold chest (1,716-4,655 gold and Iron gear instead
/// of 8,000-9,700 gold and Ebony). A level-0 chest takes the claimant's level.
fn open_season_chests(
    reward: &mut RewardGrant,
    tables: &chests::ChestLootTables,
    character_level: u64,
    owned: &std::collections::HashSet<Uuid>,
    key_prefix: &str,
) {
    let season_chests = std::mem::take(&mut reward.chests);
    let opener_level = i64::try_from(character_level).unwrap_or(i64::MAX);
    // From level 45, at the level-100 character's level (#398, #432).
    let character_level =
        blades_lib::features::loot_level::effective_loot_level_u64(character_level);
    for (index, chest) in season_chests.into_iter().enumerate() {
        let level = if chest.level == 0 {
            character_level
        } else {
            chest.level
        };
        let key = format!("{key_prefix}:{}:{index}", chest.tier);
        if let Some(mut loot) = chests::roll_loot(tables, chest.tier as i64, level, &key, owned) {
            // A Silver, Gold, Elder or Legendary chest's soul gems at 45-100
            // (owner-approved deviation from retail, #421, #429).
            blades_lib::features::chest_soul_gems::pay_chest_soul_gems(
                &mut loot.stackable_items,
                chest.tier as u64,
                opener_level,
                blades_lib::features::beginner_economy::key_seed(&key),
            );
            // Levels 45-100 (HauDrauf, #438).
            blades_lib::features::high_level_materials::pay_chest(
                &mut loot.stackable_items,
                chest.tier as u64,
                opener_level,
                blades_lib::features::beginner_economy::key_seed(&key) ^ 0x0438,
            );
            for item in &mut loot.items {
                item.id = Uuid::new_v4();
            }
            season_rewards::merge_reward(reward, loot);
        }
    }
}

/// The alt occupying a live character row (`None` for a row from before alts
/// were tracked, or no such row).
async fn live_alt(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
) -> Result<Option<Uuid>, BladeApiError> {
    use crate::schema::characters::dsl as c;
    Ok(c::characters
        .filter(c::id.eq(character_id))
        .select(c::source_alt_uuid)
        .first::<Option<Uuid>>(conn)
        .await
        .optional()?
        .flatten())
}

/// Out-of-band service id for gift error envelopes (not a real Blades id).
const GIFT_SERVICE_ID: u64 = 9003;

/// The arena weapon gift `gift_id` of this character, if it is one (owner-approved
/// #390/#391). A failed lookup — the table not yet migrated, say — reads as "not
/// one of these", so every other kind of gift keeps working.
async fn arena_weapon_gift(
    conn: &mut diesel_async::AsyncPgConnection,
    gift_id: Uuid,
    character_id: Uuid,
) -> Option<arena_fight_gift::FightGiftRow> {
    match arena_fight_gift::find(conn, gift_id, character_id, false).await {
        Ok(row) => row,
        Err(e) => {
            log::warn!("arena fight gift: lookup of {gift_id} failed; treating it as not one: {e}");
            None
        }
    }
}

fn gift_store_err(e: anyhow::Error) -> BladeApiError {
    log::error!("arena fight gift: {e}");
    BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, GIFT_SERVICE_ID, 4)
}

fn map_gift_err(e: GiftError) -> BladeApiError {
    match e {
        GiftError::NotFound => BladeApiError::new(StatusCode::NOT_FOUND, GIFT_SERVICE_ID, 1),
        GiftError::NotActive => BladeApiError::new(StatusCode::BAD_REQUEST, GIFT_SERVICE_ID, 2),
        GiftError::LimitReached => BladeApiError::new(StatusCode::CONFLICT, GIFT_SERVICE_ID, 3),
    }
}

/// Hold back the gear of a claim that does not fit under the gift's backpack
/// rule, filing it on the character. A gift without a rule is untouched.
/// Returns how many pieces were held back.
fn cap_claimed_gear(
    reward: &mut RewardGrant,
    gift_id: Uuid,
    entry: &mut CharacterDbEntryEconomy,
) -> u64 {
    let Some(rule) = gift_capacity::rule_for(gift_id) else {
        return 0;
    };
    gift_capacity::cap_gift_gear(
        reward,
        rule,
        gift_id,
        &entry.character.0,
        &entry.inventory.0,
        &mut entry.server_state.0.deferred_gift_gear,
    )
}

/// Take the owed gear that is due and fits off the character's file, as a
/// reward with fresh instance ids. Returns it with the count still owed.
/// The caller applies the reward; an empty one means nothing changed.
fn pay_deferred_gear(
    entry: &mut CharacterDbEntryEconomy,
    now: i64,
    repair_data: &blades_lib::features::repair::RepairData,
    mut new_item_id: impl FnMut() -> Uuid,
) -> (RewardGrant, u64) {
    let mut reward = gift_capacity::pay_deferred(
        &mut entry.server_state.0.deferred_gift_gear,
        &entry.character.0,
        &entry.inventory.0,
        now,
        repair_data,
    );
    for item in &mut reward.items {
        item.id = new_item_id();
    }
    let still_owed = gift_capacity::owed(&entry.server_state.0.deferred_gift_gear);
    (reward, still_owed)
}

/// The deferred-gear payout as this character would see it now, with how many
/// pieces they are owed in all. `None` when nothing is due or nothing fits —
/// and for a character that is owed nothing, without reading its inventory.
pub(crate) async fn deferred_payout(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
    now: i64,
) -> Option<(GiftDef, u64)> {
    use crate::schema::characters;
    let entry: CharacterDbEntryEconomy = characters::table
        .filter(characters::id.eq(character_id))
        .select(CharacterDbEntryEconomy::as_select())
        .first(conn)
        .await
        .optional()
        .ok()
        .flatten()?;
    let owed = &entry.server_state.0.deferred_gift_gear;
    let def = gift_capacity::payout_gift_def(owed, &entry.character.0, &entry.inventory.0, now)?;
    Some((def, gift_capacity::owed(owed)))
}

async fn deferred_payout_def(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
    now: i64,
) -> Option<GiftDef> {
    deferred_payout(conn, character_id, now)
        .await
        .map(|(def, _)| def)
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
        let alt = live_alt(&mut conn, character_id).await?;
        let awards: Vec<season_store::AwardClaimRow> = a::arena_season_awards
            .filter(a::season_id.eq(gift_id))
            .filter(a::character_id.eq(character_id))
            .select(season_store::AwardClaimRow::as_select())
            .load(&mut conn)
            .await?
            .into_iter()
            // Only the awards of the alt playing now (owner decision 2026-10-10).
            .filter(|award| season_store::award_belongs_to_alt(award.source_alt_uuid, alt))
            .collect();
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
                    awards
                        .iter()
                        .map(|award| (award.kind.as_str(), award.tier.as_str(), &award.payload)),
                )
            })?
        })
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
        return Ok(Json(GiftViewResponse {
            global_gift: GiftView {
                global_gift_id: gift_id,
                claim_count,
                global_gift_override: def.for_client(),
            },
        }));
    }

    // An arena weapon gift (owner-approved #390/#391): one weapon, offered only
    // to the alt that fought the milestone fight, like a season award.
    if let Some(gift) = arena_weapon_gift(&mut conn, gift_id, character_id).await {
        let alt = live_alt(&mut conn, character_id).await?;
        if !season_store::award_belongs_to_alt(gift.source_alt_uuid, alt) {
            return Err(map_gift_err(GiftError::NotFound));
        }
        let def = gift
            .gift_def()
            .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
        return Ok(Json(GiftViewResponse {
            global_gift: GiftView {
                global_gift_id: gift_id,
                claim_count: gift.claim_count(),
                global_gift_override: def.for_client(),
            },
        }));
    }

    // The deferred-gear payout (tracker #499): one fixed id, its contents this
    // character's own — the owed pieces that are due and fit right now.
    if gift_id == gift_capacity::DEFERRED_GEAR_GIFT_ID {
        let def = deferred_payout_def(&mut conn, character_id, now_secs())
            .await
            .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
        return Ok(Json(GiftViewResponse {
            global_gift: GiftView {
                global_gift_id: gift_id,
                // Never counted as claimed: a payout that left pieces owed must
                // stay claimable for the next time there is room.
                claim_count: 0,
                global_gift_override: def.for_client(),
            },
        }));
    }

    // Database first, static catalogue second. Reading `static_data.gifts`
    // directly would serve the payload this process booted with, and would miss
    // every gift created at runtime — a Free for All opened today would not
    // exist at all until the next restart.
    let def = crate::free_for_all::effective_gift(&app_state, &mut conn, gift_id)
        .await
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
            global_gift_override: def.for_client(),
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
                    // The alt claiming. A user's alts share this character id,
                    // so without this the alt live at claim time took every
                    // alt's placing (#348). Read without a lock to keep the
                    // awards-then-character lock order; re-checked once the
                    // character row is locked below.
                    let alt = live_alt(&mut conn, character_id).await?;
                    let awards: Vec<season_store::AwardClaimRow> = a::arena_season_awards
                        .filter(a::season_id.eq(gift_id))
                        .filter(a::character_id.eq(character_id))
                        .select(season_store::AwardClaimRow::as_select())
                        .for_update()
                        .load(&mut conn)
                        .await?
                        .into_iter()
                        .filter(|award| {
                            season_store::award_belongs_to_alt(award.source_alt_uuid, alt)
                        })
                        .collect();
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
                    // An alt switch committed between the alt read above and this
                    // lock would pay one alt's placing to the other. Now that the
                    // row is locked the alt cannot change; refuse if it did.
                    if live_alt(&mut conn, character_id).await? != alt {
                        return Err(BladeApiError::new(StatusCode::CONFLICT, GIFT_SERVICE_ID, 6));
                    }

                    // Retail's season gift opens its Gold/Elder chest during claim;
                    // it does not leave a treasury chest. The chest is opened at the
                    // claiming character's level (#335) — season rows carry level 0.
                    open_season_chests(
                        &mut reward,
                        &globals.static_data.chest_loots,
                        entry.character.0.level as u64,
                        &entry.inventory.0.item_templates(),
                        // Per alt: two alts claiming one season must not roll the same chest.
                        &match alt {
                            Some(alt) => format!("arena-season:{gift_id}:{character_id}:{alt}"),
                            None => format!("arena-season:{gift_id}:{character_id}"),
                        },
                    );

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

    // An arena weapon gift (owner-approved #390/#391): one weapon instance, once,
    // to the alt that fought the milestone fight. The unlocked probe only routes;
    // the row is re-read under lock inside the transaction.
    if arena_weapon_gift(&mut conn, gift_id, character_id)
        .await
        .is_some()
    {
        let globals = app_state.get_ref().clone();
        return conn
            .transaction(move |mut conn| {
                async move {
                    // Gift row before the character, as season awards lock theirs.
                    let gift = arena_fight_gift::find(&mut conn, gift_id, character_id, true)
                        .await
                        .map_err(gift_store_err)?
                        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;
                    if gift.granted_at.is_some() {
                        return Err(map_gift_err(GiftError::LimitReached));
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
                    // Read with the character row locked, so it cannot change.
                    let alt = live_alt(&mut conn, character_id).await?;
                    if !season_store::award_belongs_to_alt(gift.source_alt_uuid, alt) {
                        return Err(map_gift_err(GiftError::NotFound));
                    }
                    let def = gift.gift_def().ok_or_else(|| map_gift_err(GiftError::NotFound))?;
                    let mut reward = gifts::build_gift_reward(
                        &def,
                        &globals.game_data.items_template,
                        &globals.repair_data,
                    );
                    for item in &mut reward.items {
                        item.id = Uuid::new_v4();
                    }
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
                    if !arena_fight_gift::mark_granted(&mut conn, gift_id, now_secs())
                        .await
                        .map_err(gift_store_err)?
                    {
                        return Err(BladeApiError::new(StatusCode::CONFLICT, GIFT_SERVICE_ID, 5));
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
                    log::info!(
                        "arena fight gift: {gift_id} (account {} fight #{}) claimed by character {}",
                        gift.user_id,
                        gift.milestone,
                        gift.character_id
                    );
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

    // The deferred-gear payout (tracker #499): hand over the owed pieces that
    // are due and fit, keep the rest on file.
    if gift_id == gift_capacity::DEFERRED_GEAR_GIFT_ID {
        let globals = app_state.get_ref().clone();
        return conn
            .transaction(move |mut conn| {
                async move {
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
                    let (reward, still_owed) =
                        pay_deferred_gear(&mut entry, now, &globals.repair_data, Uuid::new_v4);
                    if reward.items.is_empty() {
                        // Nothing due yet, or no room: nothing changes.
                        return Err(map_gift_err(GiftError::NotActive));
                    }
                    let mut tracker = InventoryChangeTracker::default();
                    apply_reward(
                        &reward,
                        &mut entry.wallet.0,
                        &mut entry.inventory.0,
                        &mut entry.character.0,
                        &mut tracker,
                    );
                    entry.inventory.0.backpack_version += 1;

                    let inventory = entry.inventory.0.generate_client_update(&tracker);
                    let wallet = entry.wallet.0.clone();
                    {
                        use crate::schema::characters;
                        diesel::update(characters::table.filter(characters::id.eq(entry.id)))
                            .set(entry)
                            .execute(&mut conn)
                            .await?;
                    }
                    log::info!(
                        "[gifts] deferred gear: paid {} piece(s) to character {character_id}, {still_owed} still owed",
                        reward.items.len()
                    );
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

    // Database first, static catalogue second. Reading `static_data.gifts`
    // directly would serve the payload this process booted with, and would miss
    // every gift created at runtime — a Free for All opened today would not
    // exist at all until the next restart.
    let def = crate::free_for_all::effective_gift(&app_state, &mut conn, gift_id)
        .await
        .ok_or_else(|| map_gift_err(GiftError::NotFound))?;

    // Cloned before the transaction closure takes ownership: the gear branch of
    // the reward needs the item table and the durability table.
    let globals = app_state.get_ref().clone();
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

            let mut reward = gifts::build_gift_reward(
                &def,
                &globals.game_data.items_template,
                &globals.repair_data,
            );
            // Gear arrives as an instance and every instance needs its own id —
            // two claims of the same gift must not share one.
            for item in &mut reward.items {
                item.id = Uuid::new_v4();
            }
            // A gift with a backpack rule grants only the gear that fits and
            // files the rest on the character (tracker #499).
            let held = cap_claimed_gear(&mut reward, gift_id, &mut entry);
            if held > 0 {
                log::info!(
                    "[gifts] {gift_id}: {held} gear piece(s) held back for character {character_id} (backpack limit)"
                );
            }
            for chest in &mut reward.chests {
                if chest.level == 0 {
                    // The claimant's paid-as level: 100 from level 45 (#398, #432).
                    chest.level = blades_lib::features::loot_level::effective_loot_level_u64(
                        entry.character.0.level as u64,
                    );
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

#[cfg(test)]
mod season_chest_tests {
    use super::*;
    use blades_lib::economy::{GOLD, RewardChest};
    use std::collections::HashSet;

    fn committed() -> chests::ChestLootTables {
        serde_json::from_str(include_str!("chest_loots.json")).expect("chest_loots.json parses")
    }

    /// Report #335, "gold chests only contain content from low levels". Season
    /// award rows carry their chest at level 0, and the claim used to open it at
    /// that level: a level-100 character's Gold chest paid a level 1-3 Gold
    /// chest's 1,716-4,655 gold. It opens at the claimant's level now, inside
    /// retail's level 81-100 Gold range (8,069-9,675).
    #[test]
    fn a_season_gold_chest_opens_at_the_claimants_level() {
        let tables = committed();
        let mut paid = HashSet::new();
        for i in 0..40 {
            let mut reward = RewardGrant {
                chests: vec![RewardChest {
                    id: None,
                    tier: 3,
                    level: 0,
                }],
                ..Default::default()
            };
            let prefix = format!("arena-season:{i}:489620db");
            open_season_chests(&mut reward, &tables, 100, &HashSet::new(), &prefix);
            assert!(
                reward.chests.is_empty(),
                "the season chest is opened, not granted"
            );
            assert_eq!(reward.items.len(), 2, "a Gold chest pays two items");
            let gold = reward.currencies[&GOLD];
            assert!(
                gold >= 8_069,
                "a level-100 season Gold chest paid {gold} gold"
            );
            let templates: Vec<Uuid> = reward
                .items
                .iter()
                .map(|i| i.item.item_template_id)
                .collect();
            paid.insert(format!("{gold}:{templates:?}"));
        }
        assert!(
            paid.len() > 30,
            "40 season chests paid only {} distinct rewards",
            paid.len()
        );

        // Control: what the level-0 roll paid.
        let low = chests::pick_loot(&tables, 3, 0, "arena-season:0:489620db:3:0").unwrap();
        assert!(
            low.currencies[&GOLD] <= 4_655,
            "control: level 0 is a level-1 Gold chest"
        );
    }

    /// A chest that already carries a level keeps it.
    #[test]
    fn a_season_chest_with_a_level_keeps_it() {
        let tables = committed();
        let mut reward = RewardGrant {
            chests: vec![RewardChest {
                id: None,
                tier: 3,
                level: 5,
            }],
            ..Default::default()
        };
        open_season_chests(&mut reward, &tables, 100, &HashSet::new(), "k");
        assert!(
            reward.currencies[&GOLD] < 8_069,
            "a level-5 chest pays level-5 gold"
        );
    }
}

#[cfg(test)]
mod deferred_gear_tests {
    use super::*;
    use crate::json_db::JsonDbWrapper;
    use blades_lib::features::gift_capacity::DeferredGiftGear;
    use blades_lib::features::repair::RepairData;
    use blades_lib::server_state::ServerState;
    use blades_lib::user_data::{
        Backpack, CompleteCharacter, CompleteInventory, Item, ItemPropertiesAll, Loadout, Treasury,
    };

    const ARMOR: Uuid = Uuid::from_u128(0x1063c3f9_1bbf_4962_b80d_fb9832075c1c);
    const DEC_25: i64 = 1_798_156_800;

    fn repair() -> RepairData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(dir.join(name)).expect(name)).expect(name)
        };
        RepairData::from_json(&read("item_durability.json"), &read("repair_costs.json"))
    }

    /// A character at backpack tier 8 (capacity 120, so 114 at 95 %) holding
    /// `gear` instanced items and owed `owed` armours from 25 December.
    fn entry(gear: u64, owed: u64) -> CharacterDbEntryEconomy {
        let mut inventory = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 7,
            treasury_version: 0,
        };
        for n in 0..gear {
            inventory.backpack.items.0.insert(
                Uuid::from_u128(0x5000 + u128::from(n)),
                Item {
                    item_template_id: ARMOR,
                    grade: None,
                    tempering_level: 0,
                    durability: 325.0,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            );
        }
        let mut state = ServerState::default();
        if owed > 0 {
            state.deferred_gift_gear.push(DeferredGiftGear {
                gift_id: Uuid::from_u128(6),
                item_template_id: ARMOR,
                quantity: owed,
                arcane_tier: None,
                enchanting: vec![],
                payable_from_secs: DEC_25,
            });
        }
        CharacterDbEntryEconomy {
            id: Uuid::from_u128(1),
            user_id: Uuid::from_u128(2),
            character: JsonDbWrapper(CompleteCharacter {
                inventory_level: 8,
                ..CompleteCharacter::default()
            }),
            wallet: JsonDbWrapper(Default::default()),
            inventory: JsonDbWrapper(inventory),
            server_state: JsonDbWrapper(state),
        }
    }

    fn counter() -> impl FnMut() -> Uuid {
        let mut next = 0x7000u128;
        move || {
            next += 1;
            Uuid::from_u128(next)
        }
    }

    /// A payout hands over what fits with an id of its own on every piece, at
    /// the committed durability table's value, and keeps the rest on file.
    #[test]
    fn a_payout_mints_distinct_instances_and_keeps_the_rest() {
        let mut e = entry(112, 5);
        let (reward, still_owed) = pay_deferred_gear(&mut e, DEC_25, &repair(), counter());
        assert_eq!(reward.items.len(), 2, "114 - 112");
        assert_eq!(still_owed, 3);
        assert_ne!(reward.items[0].id, reward.items[1].id);
        assert!(reward.items.iter().all(|p| !p.id.is_nil()));
        assert!(reward.items.iter().all(|p| p.item.durability == 325.0));
        assert!(reward.stackable_items.is_empty() && reward.currencies.is_empty());
    }

    /// Before the day, and with no room, a claim hands over nothing and the
    /// file is untouched — the handler answers "not active" on an empty reward.
    #[test]
    fn nothing_is_paid_early_or_into_a_full_backpack() {
        let mut early = entry(0, 5);
        let (reward, still_owed) = pay_deferred_gear(&mut early, DEC_25 - 1, &repair(), counter());
        assert!(reward.is_empty());
        assert_eq!(still_owed, 5);

        let mut full = entry(114, 5);
        let (reward, still_owed) = pay_deferred_gear(&mut full, DEC_25, &repair(), counter());
        assert!(reward.is_empty());
        assert_eq!(still_owed, 5);
    }

    /// CONTROL: a gift without a backpack rule — every gift there is today —
    /// is paid whole, into however full a backpack, exactly as before.
    #[test]
    fn a_gift_without_a_rule_is_never_capped() {
        let mut e = entry(400, 0);
        let mut reward = RewardGrant::default();
        for n in 0..3u128 {
            let mut piece = gifts::gear_instance(
                &blades_lib::static_data::GiftItem {
                    item_template_id: ARMOR,
                    quantity: 1,
                    arcane_tier: None,
                    enchanting: vec![],
                },
                &repair(),
            );
            piece.id = Uuid::from_u128(0x8000 + n);
            reward.items.push(piece);
        }
        let held = cap_claimed_gear(&mut reward, Uuid::from_u128(0x6f7c99c5), &mut e);
        assert_eq!(held, 0);
        assert_eq!(reward.items.len(), 3);
        assert!(e.server_state.0.deferred_gift_gear.is_empty());
    }
}
