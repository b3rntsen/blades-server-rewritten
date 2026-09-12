//! Challenges — `POST /challenges` (list/generate), `POST /challenges/{id}`
//! (progress), `POST /challenges/{id}/complete`, `POST /challenges/{id}/abandon`.
//!
//! Rotating per-character objectives that pay a currency reward. Templates are
//! capture-derived ([`crate::static_loader`] → `challenges.json`); the active set +
//! rotation cursor live in `server_state.challenges`. Progress is client-driven (an
//! absolute value); completing grants the reward, bumps the season points, and
//! rotates in a fresh challenge. Generation/progress/completion math is the pure
//! [`blades_lib::features::challenges`] layer.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::{GEMS, RewardGrant, apply_reward};
use blades_lib::features::challenges::{
    self, ACTIVE_CHALLENGE_COUNT, ChallengeInstance, ChallengeState, ChallengeStatus,
    ChallengeTemplate,
};
use blades_lib::user_data::{
    CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
    InventoryChangeTracker,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy,
    session::SessionLookedUpMaybe,
};

const CHALLENGE_SERVICE_ID: u64 = 9005;

// These are content-asset ids, not per-request ids. `ChallengeData` looks them up
// before it can dismiss the completion panel. Returning a random UUID made the
// reward commit successfully on the server and then left the client waiting on an
// asset that can never exist (tracker #127). Captured retail completions 146031–33
// use the gem category for a gem reward and the currency category for gold.
const GEM_CATEGORY_ID: Uuid = Uuid::from_u128(0xb6026a23_fd97_4df1_981d_57d68a0b7fdc);
const CURRENCY_CATEGORY_ID: Uuid = Uuid::from_u128(0x9fbd1d31_1000_4b4a_b7e4_0c29115c1e00);

fn category_id_for_reward(reward: &RewardGrant) -> Uuid {
    if reward.currencies.contains_key(&GEMS) {
        GEM_CATEGORY_ID
    } else {
        CURRENCY_CATEGORY_ID
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Drop terminal (completed/abandoned) challenges and refill the active set back up
/// to [`ACTIVE_CHALLENGE_COUNT`] from the template pool (advancing the cursor). The
/// server mints the fresh instance/objective UUIDs (this crate has no uuid-v4).
fn refill(state: &mut ChallengeState, pool: &[ChallengeTemplate]) {
    state
        .active
        .retain(|c| c.status == ChallengeStatus::Active);
    if pool.is_empty() {
        return;
    }
    let (secs, ms) = (now_secs(), now_ms());
    while state.active.len() < ACTIVE_CHALLENGE_COUNT {
        let idx = state.cursor % pool.len();
        state.cursor = state.cursor.wrapping_add(1);
        state.active.push(ChallengeInstance::from_template(
            &pool[idx],
            Uuid::new_v4(),
            Uuid::new_v4(),
            secs,
            ms,
        ));
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChallengeStatusBlock {
    active: Vec<ChallengeInstance>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetChallengesResult {
    character: CompleteCharacterWithIdWithoutData,
    challenge_status: ChallengeStatusBlock,
}

/// List the active challenges, generating a fresh set on first call.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/challenges")]
pub async fn get_challenges(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetChallengesResult>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            refill(&mut entry.server_state.0.challenges, &globals.static_data.challenge_templates);
            let active = entry.server_state.0.challenges.active.clone();
            let character = entry.character.0.clone();
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(GetChallengesResult {
                character: CompleteCharacterWithIdWithoutData { id: character_id, character },
                challenge_status: ChallengeStatusBlock { active },
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
struct ProgressRequest {
    #[serde(default)]
    progress: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressResponse {
    challenge: ChallengeInstance,
}

/// Update a challenge's progress (client reports an absolute value).
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/challenges/{challenge_id}")]
pub async fn update_challenge(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<ProgressRequest>,
) -> Result<Json<ProgressResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, challenge_id) = path.into_inner();
    let new_progress = body.into_inner().progress;
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;
            let challenge = {
                let c = entry
                    .server_state
                    .0
                    .challenges
                    .active
                    .iter_mut()
                    .find(|c| c.id == challenge_id)
                    .ok_or_else(|| {
                        BladeApiError::new(StatusCode::NOT_FOUND, CHALLENGE_SERVICE_ID, 1)
                    })?;
                c.progress = challenges::clamp_progress(new_progress, c.objective.quota);
                c.clone()
            };
            write_back(&mut conn, entry).await?;
            Ok::<_, BladeApiError>(Json(ProgressResponse { challenge }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextCategory {
    category_id: Uuid,
    generated_time: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveResponse {
    character: CompleteCharacterWithIdWithoutData,
    challenge: ChallengeInstance,
    next_challenge_categories: Vec<NextCategory>,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
}

/// Complete a challenge: grant the reward, bump season points, rotate in a fresh one.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/challenges/{challenge_id}/complete")]
pub async fn complete_challenge(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<ResolveResponse>, BladeApiError> {
    resolve(session, app_state, path, true).await
}

/// Abandon a challenge: no reward, just rotate in a fresh one.
#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/challenges/{challenge_id}/abandon")]
pub async fn abandon_challenge(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<ResolveResponse>, BladeApiError> {
    resolve(session, app_state, path, false).await
}

async fn resolve(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    complete: bool,
) -> Result<Json<ResolveResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, challenge_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            let mut entry = load_owned(&mut conn, character_id, user_id).await?;

            // Resolve the targeted challenge in place.
            let mut resolved = {
                let c = entry
                    .server_state
                    .0
                    .challenges
                    .active
                    .iter_mut()
                    .find(|c| c.id == challenge_id)
                    .ok_or_else(|| {
                        BladeApiError::new(StatusCode::NOT_FOUND, CHALLENGE_SERVICE_ID, 1)
                    })?;
                c.status = if complete {
                    ChallengeStatus::Completed
                } else {
                    ChallengeStatus::Abandoned
                };
                c.completed_timestamp = Some(now_secs());
                c.clone()
            };

            let mut tracker = InventoryChangeTracker::default();
            if complete {
                resolved.progress = resolved.objective.quota;
                apply_reward(
                    &resolved.reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                if !resolved.reward.stackable_items.is_empty() || !resolved.reward.items.is_empty() {
                    entry.inventory.0.backpack_version += 1;
                }
                entry.character.0.challenge_season.points += 1;
                entry.server_state.0.challenges.points += 1;
            }

            // Drop the resolved challenge and refill the active set.
            refill(&mut entry.server_state.0.challenges, &globals.static_data.challenge_templates);

            let character = entry.character.0.clone();
            let inventory = entry.inventory.0.generate_client_update(&tracker);
            let wallet = entry.wallet.0.clone();
            let category_id = category_id_for_reward(&resolved.reward);
            write_back(&mut conn, entry).await?;

            Ok::<_, BladeApiError>(Json(ResolveResponse {
                character: CompleteCharacterWithIdWithoutData { id: character_id, character },
                challenge: resolved,
                next_challenge_categories: vec![NextCategory {
                    category_id,
                    generated_time: now_secs(),
                }],
                inventory,
                wallet,
            }))
        }
        .scope_boxed()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn completion_categories_are_client_known_asset_ids() {
        let gems = RewardGrant::currencies(HashMap::from([(GEMS, 1)]));
        let gold = RewardGrant::currencies(HashMap::from([(blades_lib::economy::GOLD, 300)]));

        assert_eq!(category_id_for_reward(&gems), GEM_CATEGORY_ID);
        assert_eq!(category_id_for_reward(&gold), CURRENCY_CATEGORY_ID);
        assert_ne!(GEM_CATEGORY_ID, CURRENCY_CATEGORY_ID);
    }
}

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
