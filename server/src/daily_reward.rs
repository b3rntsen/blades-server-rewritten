//! Daily login reward — `POST /towns/current/rewards/current` (status) and
//! `.../rewards/current/collect`.
//!
//! A reward rotates each 24h period (pool is capture-derived); the player collects it
//! once per period (tracked in `server_state.daily_reward`). NOTE: `until` must be in
//! the future — a past value makes the client spin re-fetching and stall every other
//! request. `until_ms(period)` is the next period boundary, always ahead. Rotation/
//! period math is the pure [`blades_lib::features::daily_reward`] layer.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{RewardGrant, apply_reward};
use blades_lib::features::chests;
use blades_lib::features::daily_reward::{self, DailyRewardPayload};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
    daily_reward_status: CollectDailyRewardStatus,
    inventory: CompleteInventoryUpdate,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectDailyRewardStatus {
    reward_uid: Uuid,
    until: i64,
    daily_reward: RewardGrant,
    collected: bool,
}

fn merge_reward(into: &mut RewardGrant, extra: &RewardGrant) {
    for (currency, amount) in &extra.currencies {
        *into.currencies.entry(*currency).or_insert(0) += amount;
    }
    for (item, count) in &extra.stackable_items {
        *into.stackable_items.entry(*item).or_insert(0) += count;
    }
    into.items.extend(extra.items.iter().cloned());
    into.chests.extend(extra.chests.iter().cloned());
    into.character_xp += extra.character_xp;
    into.town_xp += extra.town_xp;
}

fn remint_reward_items(reward: &mut RewardGrant) {
    for item in &mut reward.items {
        item.id = Uuid::new_v4();
    }
}

fn collect_reward_for(
    payload: &DailyRewardPayload,
    chest_loots: &blades_lib::features::chests::ChestLootTables,
    reward_uid: Uuid,
) -> RewardGrant {
    let mut reward = RewardGrant {
        stackable_items: payload.stackable_items.clone(),
        ..Default::default()
    };

    // Retail advertises chest days as `dailyReward.chests` in the status endpoint,
    // but collect opens that chest immediately and returns the loot bundle. Nine
    // distinct captured chest-daily collect responses all omit `reward.chests` and
    // instead carry `items`, `stackableItems` and `currencies`.
    for (idx, chest) in payload.chests.iter().enumerate() {
        let key = format!("daily:{reward_uid}:{idx}:{}:{}", chest.tier, chest.level);
        if let Some(loot) = chests::pick_loot(chest_loots, chest.tier, chest.level, &key) {
            merge_reward(&mut reward, loot);
        }
    }
    remint_reward_items(&mut reward);
    reward
}

fn collected_status(
    period: i64,
    reward_uid: Uuid,
    daily_reward: RewardGrant,
) -> CollectDailyRewardStatus {
    CollectDailyRewardStatus {
        reward_uid,
        until: daily_reward::until_ms(period),
        daily_reward,
        collected: true,
    }
}

fn credited_wallet(wallet: &CompleteWallet, reward: &RewardGrant) -> Option<CompleteWallet> {
    if reward.currencies.is_empty() {
        return None;
    }
    let mut out = HashMap::new();
    for currency in reward.currencies.keys() {
        if let Some(entry) = wallet.0.get(currency) {
            out.insert(*currency, entry.clone());
        }
    }
    Some(CompleteWallet(out))
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
            let mut reward_uid = Uuid::nil();
            let mut status_reward = RewardGrant::default();
            let mut tracker = InventoryChangeTracker::default();

            if let Some(def) =
                daily_reward::reward_for_period(&globals.static_data.daily_rewards, period)
            {
                reward_uid = def.reward_uid;
                let collect_reward = collect_reward_for(
                    &def.daily_reward,
                    &globals.static_data.chest_loots,
                    def.reward_uid,
                );
                status_reward = collect_reward.clone();
                if !already {
                    reward = collect_reward;
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
                }
            }
            if !already {
                entry.server_state.0.daily_reward.collected_period = Some(period);
            }

            let wallet = credited_wallet(&entry.wallet.0, &reward);
            let status = collected_status(period, reward_uid, status_reward);
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(CollectResponse {
                reward,
                wallet,
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

#[cfg(test)]
mod collect_response_tests {
    use super::*;
    use blades_lib::{
        economy::{GOLD, RewardGrant},
        features::chests::{ChestLootSample, ChestLootTables},
    };
    use std::collections::{BTreeMap, HashMap};

    /// Build the `reward` exactly as `collect_daily_reward` does, for one day's
    /// definition. Kept beside the handler so the two cannot drift silently.
    fn reward_for(payload: &blades_lib::features::daily_reward::DailyRewardPayload) -> RewardGrant {
        collect_reward_for(payload, &loot_tables(), Uuid::from_u128(0xDA11A))
    }

    fn loot_tables() -> ChestLootTables {
        ChestLootTables {
            tiers: BTreeMap::from([(
                2,
                vec![ChestLootSample {
                    chest_level: 86,
                    reward: RewardGrant {
                        currencies: HashMap::from([(GOLD, 2218)]),
                        stackable_items: HashMap::from([(
                            "42d91529-c88b-4c5b-815b-b55508b4e7ef".parse().unwrap(),
                            7,
                        )]),
                        ..Default::default()
                    },
                }],
            )]),
            ..Default::default()
        }
    }

    /// Retail's daily-reward status advertises chest days as `dailyReward.chests`,
    /// but all nine distinct captured chest-day collect responses omit chests and
    /// return opened chest loot instead.
    #[test]
    fn a_chest_only_day_collects_as_opened_loot_not_a_treasury_chest() {
        let payload: blades_lib::features::daily_reward::DailyRewardPayload =
            serde_json::from_value(serde_json::json!({
                "chests": [{ "tier": 2, "level": 86 }]
            }))
            .expect("a chest-only day must deserialize");
        let reward = reward_for(&payload);

        assert!(!reward.is_empty(), "the reward must not be empty");
        assert!(reward.chests.is_empty(), "collect opens the chest immediately");
        assert_eq!(reward.currencies.get(&GOLD), Some(&2218));
        assert_eq!(reward.stackable_items.values().copied().sum::<u64>(), 7);

        let json = serde_json::to_value(&reward).expect("serializes");
        assert_ne!(
            json,
            serde_json::json!({}),
            "`reward: {{}}` is the bug — the client reads this to present the reward"
        );
        assert!(
            json.get("chests").is_none(),
            "daily collect should not hand over an unopened chest"
        );
        assert!(json.get("currencies").is_some(), "opened loot reaches the wire");
    }

    /// The control: a stackables-only day must be unchanged. Opening chest rewards
    /// must not disturb the path that already worked.
    #[test]
    fn a_stackable_only_day_is_unchanged() {
        let payload: blades_lib::features::daily_reward::DailyRewardPayload =
            serde_json::from_value(serde_json::json!({
                "stackableItems": { "e7193116-d761-479b-8a20-5633737977f5": 25 }
            }))
            .expect("a stackable-only day must deserialize");
        let reward = reward_for(&payload);

        assert!(reward.chests.is_empty());
        assert_eq!(reward.stackable_items.len(), 1);
        let json = serde_json::to_value(&reward).expect("serializes");
        assert!(json.get("stackableItems").is_some());
        assert!(
            json.get("chests").is_none(),
            "an empty chest list must stay off the wire, as every other reward does"
        );
    }

    /// A mixed day keeps its authored stackables and adds opened chest loot.
    #[test]
    fn a_day_with_both_reports_stackables_and_opened_chest_loot() {
        let payload: blades_lib::features::daily_reward::DailyRewardPayload =
            serde_json::from_value(serde_json::json!({
                "stackableItems": { "e7193116-d761-479b-8a20-5633737977f5": 25 },
                "chests": [{ "tier": 2, "level": 86 }]
            }))
            .expect("a mixed day must deserialize");
        let reward = reward_for(&payload);
        assert!(reward.chests.is_empty());
        assert_eq!(reward.currencies.get(&GOLD), Some(&2218));
        assert_eq!(reward.stackable_items.values().copied().sum::<u64>(), 32);
    }

    /// An empty day stays empty — `reward: {}` is correct when there is genuinely
    /// nothing, and the fix must not start inventing rewards.
    #[test]
    fn an_empty_day_stays_empty() {
        let payload: blades_lib::features::daily_reward::DailyRewardPayload =
            serde_json::from_value(serde_json::json!({})).expect("an empty day deserializes");
        assert!(reward_for(&payload).is_empty());
    }
}
