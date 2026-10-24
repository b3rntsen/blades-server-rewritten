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
    ChallengeTemplate, gems_only_reward,
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

/// Re-derive every active challenge's reward from its template (owner-approved
/// deviation from retail, #388, 2026-10-16): one handed out before #388 still
/// carries its template's retail reward, gold included, and must pay (and show)
/// gems only, doubled. Always from the template, so it never doubles twice.
fn sync_rewards(state: &mut ChallengeState, pool: &[ChallengeTemplate]) {
    for c in &mut state.active {
        if let Some(t) = pool.iter().find(|t| t.template_id == c.template_id) {
            c.reward = gems_only_reward(&t.reward);
        }
    }
}

/// Drop terminal (completed/abandoned) challenges and refill the active set back up
/// to [`ACTIVE_CHALLENGE_COUNT`] from the templates offered at the character's
/// `level` (report #406: retail's `ChallengeSelector` level band), advancing the
/// cursor. The server mints the fresh instance/objective UUIDs (this crate has
/// no uuid-v4).
fn refill(state: &mut ChallengeState, pool: &[ChallengeTemplate], level: u16) {
    state
        .active
        .retain(|c| c.status == ChallengeStatus::Active);
    if pool.is_empty() {
        return;
    }
    sync_rewards(state, pool);
    let free = ACTIVE_CHALLENGE_COUNT.saturating_sub(state.active.len());
    let held: Vec<Uuid> = state.active.iter().map(|c| c.template_id).collect();
    let (picked, cursor) = challenges::pick_templates(pool, state.cursor % pool.len(), level, &held, free);
    state.cursor = cursor;
    let (secs, ms) = (now_secs(), now_ms());
    for idx in picked {
        state.active.push(ChallengeInstance::from_template(
            &pool[idx],
            Uuid::new_v4(),
            Uuid::new_v4(),
            secs,
            ms,
        ));
    }
}

/// Retire the active challenges a character of `level` can no longer be offered
/// and has not started (report #406), so [`refill`] replaces them. Only on the
/// list call, whose response is the client's whole set: a challenge retired
/// anywhere else would still be on the client's screen.
fn retire_out_of_band(state: &mut ChallengeState, pool: &[ChallengeTemplate], level: u16) {
    state
        .active
        .retain(|c| !challenges::retire_out_of_band(c, pool, level));
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
            let pool = &globals.static_data.challenge_templates;
            let level = entry.character.0.level;
            retire_out_of_band(&mut entry.server_state.0.challenges, pool, level);
            refill(&mut entry.server_state.0.challenges, pool, level);
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
            sync_rewards(&mut entry.server_state.0.challenges, &globals.static_data.challenge_templates);

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
            let level = entry.character.0.level;
            refill(&mut entry.server_state.0.challenges, &globals.static_data.challenge_templates, level);

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

    /// #388 (owner-approved deviation from retail, 2026-10-16): a challenge handed
    /// out before the change still carried retail's gold; it is re-derived from its
    /// template and pays gems only, and syncing twice never doubles twice.
    #[test]
    fn a_stored_gold_challenge_pays_doubled_gems_and_syncing_is_idempotent() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let pool = crate::static_loader::load(&dir).challenge_templates;
        let gold_template = pool
            .iter()
            .find(|t| t.reward.currencies.get(&blades_lib::economy::GOLD) == Some(&750))
            .expect("a 750-gold template");
        let mut stale = ChallengeInstance::from_template(gold_template, Uuid::from_u128(1), Uuid::from_u128(2), 0, 0);
        stale.reward = gold_template.reward.clone(); // as stored before #388
        let mut state = ChallengeState { active: vec![stale], cursor: 0, points: 0 };
        for _ in 0..2 {
            sync_rewards(&mut state, &pool);
            assert_eq!(state.active[0].reward.currencies, HashMap::from([(GEMS, 6)]), "750 gold = 3 gems, doubled");
            assert_eq!(category_id_for_reward(&state.active[0].reward), GEM_CATEGORY_ID);
        }
        refill(&mut state, &pool, 30);
        assert_eq!(state.active.len(), ACTIVE_CHALLENGE_COUNT);
        assert!(state.active.iter().all(|c| c.reward.currencies.keys().all(|k| *k == GEMS)));
    }

    /// The enemy family a `targetEnemyGroup` challenge asks for, as the job
    /// generator knows it. Every enemy-target template in the pool must be here.
    fn job_family_of_group(group: Uuid) -> Option<&'static str> {
        match group.to_string().as_str() {
            // EnemyGroup "Skeevers" -> the Skeever family.
            "fe6ca800-ea7e-4fd7-a0ef-d1547da6ec5b" => Some("31be99a6-8557-4e9b-81e6-5503f900b7d2"),
            _ => None,
        }
    }

    /// #406: for every level 1..=100, every enemy challenge the server can hand
    /// out names a family that spawns at that level (the job generator's own
    /// family table), and LLoyd (level 65) is never sent hunting Skeevers.
    #[test]
    fn report_406_offered_enemy_challenges_spawn_at_the_characters_level() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let pool = crate::static_loader::load(&dir).challenge_templates;
        let targets: Vec<&ChallengeTemplate> = pool.iter().filter(|t| t.target_enemy_group.is_some()).collect();
        assert!(!targets.is_empty(), "the pool has an enemy-target challenge to check");
        for level in 1..=100u16 {
            for cursor in 0..pool.len() {
                let mut state = ChallengeState { active: vec![], cursor, points: 0 };
                refill(&mut state, &pool, level);
                assert_eq!(state.active.len(), ACTIVE_CHALLENGE_COUNT, "level {level}");
                for c in &state.active {
                    let t = pool.iter().find(|t| t.template_id == c.template_id).unwrap();
                    let Some(group) = t.target_enemy_group else { continue };
                    let family = job_family_of_group(group).unwrap_or_else(|| panic!("no family for group {group}"));
                    let f = crate::quest::jobs_gen::JOB_FAMILIES.iter().find(|f| f.id == family).unwrap();
                    assert!(
                        (f.min_level..=f.max_level).contains(&(level as i64)),
                        "level {level}: {} asks for a family that spawns at {}..={}",
                        t.template_id,
                        f.min_level,
                        f.max_level
                    );
                }
                if level == 65 {
                    assert!(state.active.iter().all(|c| t_is_not_skeever(c)), "Skeevers offered at 65");
                }
            }
        }
    }

    fn t_is_not_skeever(c: &ChallengeInstance) -> bool {
        c.template_id != Uuid::from_u128(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e)
    }

    /// #406: LLoyd's stored set (prod, 2026-10-17) on the next list call. The
    /// four are all untouched and none is offered at 65 (the Skeever hunt is
    /// 15-19, the rest 1-15), so all four are replaced with ones that are.
    /// #426: Dwalin's stored set (prod, 2026-10-24, level 77). The tutorial
    /// "Craft a Decoration" (zero progress, uncancellable in the client) is
    /// retired and replaced on the next list call; the other three are offered at
    /// 77 and kept as they are.
    #[test]
    fn report_426_dwalins_tutorial_challenge_is_replaced_on_the_next_list() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let pool = crate::static_loader::load(&dir).challenge_templates;
        let stored = |tid: u128, id: u128| {
            let t = pool.iter().find(|t| t.template_id == Uuid::from_u128(tid)).unwrap();
            ChallengeInstance::from_template(t, Uuid::from_u128(id), Uuid::new_v4(), 0, 0)
        };
        let mut state = ChallengeState {
            active: vec![
                stored(0xbce21d12_8f3b_444b_ad1f_740478f1d0cf, 0xc95a9d24_4028_4549_bfb5_54a465f76eec), // TUTORIAL_CraftDecoration
                stored(0x78826178_9291_4255_a204_6ac312affda5, 0xd45e5b08_c16c_4f02_b17e_aa9bda1da692), // Salvage_Legendary
                stored(0xcb497c2f_9354_4545_a3d1_bfdab358579f, 0xef4301fd_7215_4579_ace4_021181b9b994), // Perform3HitCombos_15
                stored(0xba98f140_5dbb_4b58_b7f2_66c080651d8d, 0x606553ba_9aa0_4da6_b298_1e94daf0041f), // CollectChests_15
            ],
            cursor: 11,
            points: 36,
        };
        retire_out_of_band(&mut state, &pool, 77);
        refill(&mut state, &pool, 77);
        assert_eq!(state.active.len(), ACTIVE_CHALLENGE_COUNT);
        let ids: Vec<Uuid> = state.active.iter().map(|c| c.id).collect();
        assert!(!ids.contains(&Uuid::from_u128(0xc95a9d24_4028_4549_bfb5_54a465f76eec)), "tutorial retired");
        for kept in [0xd45e5b08_c16c_4f02_b17e_aa9bda1da692u128, 0xef4301fd_7215_4579_ace4_021181b9b994, 0x606553ba_9aa0_4da6_b298_1e94daf0041f] {
            assert!(ids.contains(&Uuid::from_u128(kept)), "{kept:x} kept");
        }
        assert!(state.active.iter().all(|c| c.template_id != Uuid::from_u128(0xbce21d12_8f3b_444b_ad1f_740478f1d0cf)));
    }

    #[test]
    fn report_406_lloyds_stored_set_is_rederived_on_the_next_list() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let pool = crate::static_loader::load(&dir).challenge_templates;
        let stored = |tid: u128| {
            let t = pool.iter().find(|t| t.template_id == Uuid::from_u128(tid)).unwrap();
            ChallengeInstance::from_template(t, Uuid::new_v4(), Uuid::new_v4(), 0, 0)
        };
        let mut state = ChallengeState {
            active: vec![
                stored(0x8ec2b1a1_62f2_476b_b323_53bade2ab7a5), // Perform4HitCombos_02, 11-15
                stored(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e), // DefeatSkeever_4, 15-19
                stored(0x8925740e_046b_4e7a_b418_a17bd83e035c), // PerformCriticalHits_01, 1-10
                stored(0xd5631e1c_2698_4bb1_8e15_52433da096e4), // DefeatEnemies_1, 1-5
            ],
            cursor: 6,
            points: 0,
        };
        retire_out_of_band(&mut state, &pool, 65);
        assert!(state.active.is_empty(), "all four are out of band at 65 and untouched");
        refill(&mut state, &pool, 65);
        assert_eq!(state.active.len(), ACTIVE_CHALLENGE_COUNT);
        for c in &state.active {
            let t = pool.iter().find(|t| t.template_id == c.template_id).unwrap();
            assert!(t.offered_at(65), "{}", c.template_id);
            assert!(c.reward.currencies.keys().all(|k| *k == GEMS), "#388 gems-only reward kept");
        }
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
