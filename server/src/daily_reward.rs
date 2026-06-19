//! Daily login reward — `POST /towns/current/rewards/current` (status) and
//! `.../rewards/current/collect`.
//!
//! A reward rotates each 24h period (pool is capture-derived); the player collects it
//! once per period (tracked in `server_state.daily_reward`). NOTE: `until` must be in
//! the future — a past value makes the client spin re-fetching and stall every other
//! request. `until_ms(period)` is the next period boundary, always ahead. Rotation/
//! period math is the pure [`blades_lib::features::daily_reward`] layer.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, apply_reward, grant_chest};
use blades_lib::features::daily_reward::{self, DailyRewardPayload};
use blades_lib::user_data::{CompleteInventoryUpdate, InventoryChangeTracker};
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyRewardStatus {
    reward_uid: Uuid,
    until: i64,
    daily_reward: DailyRewardPayload,
    collected: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    daily_reward_status: DailyRewardStatus,
}

/// Build the status block for `period`, given whether it has been collected.
fn status_for(
    app_state: &ServerGlobal,
    period: i64,
    collected: bool,
) -> DailyRewardStatus {
    let until = daily_reward::until_ms(period);
    match daily_reward::reward_for_period(&app_state.static_data.daily_rewards, period) {
        Some(def) => DailyRewardStatus {
            reward_uid: def.reward_uid,
            until,
            daily_reward: def.daily_reward.clone(),
            collected,
        },
        // Empty pool: a placeholder with a future `until` so the client doesn't stall.
        None => DailyRewardStatus {
            reward_uid: Uuid::nil(),
            until,
            daily_reward: DailyRewardPayload::default(),
            collected,
        },
    }
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current/rewards/current"
)]
pub async fn get_daily_reward(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<StatusResponse>, BladeApiError> {
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

    let period = daily_reward::current_period(now_secs());
    let collected = entry.server_state.0.daily_reward.collected_period == Some(period);
    Ok(Json(StatusResponse {
        daily_reward_status: status_for(&app_state, period, collected),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectResponse {
    reward: RewardGrant,
    daily_reward_status: DailyRewardStatus,
    inventory: CompleteInventoryUpdate,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current/rewards/current/collect"
)]
pub async fn collect_daily_reward(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<CollectResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let globals = app_state.get_ref().clone();
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

            let period = daily_reward::current_period(now_secs());
            let already = entry.server_state.0.daily_reward.collected_period == Some(period);

            let mut reward = RewardGrant::default();
            let mut tracker = InventoryChangeTracker::default();

            if !already {
                if let Some(def) = daily_reward::reward_for_period(
                    &globals.static_data.daily_rewards,
                    period,
                ) {
                    // Stackable part -> backpack via the reward; chests -> treasury.
                    reward.stackable_items = def.daily_reward.stackable_items.clone();
                    apply_reward(
                        &reward,
                        &mut entry.wallet.0,
                        &mut entry.inventory.0,
                        &mut entry.character.0,
                        &mut tracker,
                    );
                    if !reward.stackable_items.is_empty() {
                        entry.inventory.0.backpack_version += 1;
                    }
                    if !def.daily_reward.chests.is_empty() {
                        for chest in &def.daily_reward.chests {
                            grant_chest(&mut entry.inventory.0, chest.tier, chest.level, &mut tracker);
                        }
                        entry.inventory.0.treasury_version += 1;
                    }
                }
                entry.server_state.0.daily_reward.collected_period = Some(period);
            }

            let status = status_for(&globals, period, true);
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(CollectResponse {
                reward,
                daily_reward_status: status,
                inventory,
            }))
        }
        .scope_boxed()
    })
    .await
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
