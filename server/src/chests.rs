//! Chests — `POST /chests/{id}/collect`.
//!
//! Open a treasury chest for loot. We draw a representative loot bundle from a
//! capture-derived pool (stable per granted chest — per-tier loot tables aren't
//! captured), re-mint the instanced item ids (capture ids would collide across
//! players), grant it, and remove the chest. See [`blades_lib::features::chests`].

use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json, Query},
};
use blades_lib::economy::{RewardGrant, apply_reward, remove_any_chest};
use blades_lib::features::chests;
use blades_lib::user_data::{CompleteInventoryUpdate, CompleteWallet, InventoryChangeTracker};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryEconomy, session::SessionLookedUpMaybe,
};

const CHEST_SERVICE_ID: u64 = 9007;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectResponse {
    reward: RewardGrant,
    wallet: CompleteWallet,
    inventory: CompleteInventoryUpdate,
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/chests/{chest_id}/collect"
)]
pub async fn collect_chest(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, String)>,
) -> Result<Json<CollectResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, chest_id) = path.into_inner();
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

            // The chest must exist in the treasury or, for a chest found while the
            // treasury was full, in the overflow treasury — the client opens that
            // one at once with `fastTrack=True` (capture 37529, report #451). Copy
            // its small identity tuple before mutating inventory below.
            let inventory_ref = &entry.inventory.0;
            let (chest_tier, chest_level) = inventory_ref
                .treasury
                .get_chest(&chest_id)
                .or_else(|| inventory_ref.overflow_treasury.get_chest(&chest_id))
                .map(|chest| (chest.tier, chest.level))
                .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, CHEST_SERVICE_ID, 1))?;

            // Representative loot for this chest; re-mint instanced item ids.
            // Numeric chest ids are deliberately compact and get reused after the
            // only chest is opened. Keying solely on `chest_id` therefore made every
            // such dungeon chest yield exactly the same bundle (#120). Treasury
            // version is persistent and advances when chests are granted/opened;
            // character/tier/level keep equal generations from collapsing together.
            // A later alt shares the live row's id, so it is folded in as well
            // (#387); the original's key is unchanged.
            let roll_alt = crate::alt_roll::for_character(&mut conn, character_id).await;
            let loot_key = crate::alt_roll::fold_key(
                format!(
                    "{character_id}:{chest_id}:{chest_tier}:{chest_level}:{}",
                    entry.inventory.0.treasury_version
                ),
                roll_alt,
            );
            // An artifact the character already holds is never handed out again;
            // retail re-rolled it as a Legendary piece instead (#310).
            let mut reward = chests::roll_loot(
                &globals.static_data.chest_loots,
                chest_tier,
                chest_level,
                &loot_key,
                &entry.inventory.0.item_templates(),
            )
            .unwrap_or_default();
            // The three best soul gems for a level 45-48 opener, whatever level the
            // chest was minted at (owner-approved deviation from retail, #388).
            blades_lib::features::beginner_economy::top_soul_gems(
                &mut reward.stackable_items,
                i64::from(entry.character.0.level),
                blades_lib::features::beginner_economy::key_seed(&loot_key),
            );
            // A Silver, Gold, Elder or Legendary chest's soul gems for a level
            // 45-100 opener (owner-approved deviation from retail, #421, #429).
            blades_lib::features::chest_soul_gems::pay_chest_soul_gems(
                &mut reward.stackable_items,
                chest_tier.max(0) as u64,
                i64::from(entry.character.0.level),
                blades_lib::features::beginner_economy::key_seed(&loot_key),
            );
            // Levels 45-100: tempering 1-3, effect ingredients 30 (two in a Wooden
            // chest), dragon 1-7 (HauDrauf, #438).
            blades_lib::features::high_level_materials::pay_chest(
                &mut reward.stackable_items,
                chest_tier.max(0) as u64,
                i64::from(entry.character.0.level),
                blades_lib::features::beginner_economy::key_seed(&loot_key) ^ 0x0438,
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
            remove_any_chest(&mut entry.inventory.0, &chest_id, &mut tracker);
            entry.inventory.0.treasury_version += 1;

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

            Ok::<_, BladeApiError>(Json(CollectResponse {
                reward,
                wallet,
                inventory,
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Deserialize)]
struct ResolveQuery {
    #[serde(default)]
    action: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveResponse {
    inventory: CompleteInventoryUpdate,
}

/// What `/treasuries/current/resolve` does to the overflow treasury.
#[derive(Debug, PartialEq, Eq)]
enum ResolveAction {
    /// Keep the overflow chests: move them into the treasury.
    Accept,
    /// Throw the overflow chests away.
    Discard,
}

impl ResolveAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_uppercase().as_str() {
            "ACCEPT" => Some(Self::Accept),
            "DISCARD" => Some(Self::Discard),
            _ => None,
        }
    }
}

/// Settle every overflow chest the way `action` says, recording the diff.
fn resolve_overflow(
    inventory: &mut blades_lib::user_data::CompleteInventory,
    action: &ResolveAction,
    tracker: &mut InventoryChangeTracker,
) {
    for chest in inventory.overflow_treasury.take_all() {
        tracker
            .modified_overflow_treasury
            .removed
            .push(chest.id.clone());
        if *action == ResolveAction::Accept {
            tracker.modified_treasury.added.push(chest.id.clone());
            inventory.treasury.push_chest(chest);
        }
    }
}

/// `POST /characters/{id}/treasuries/current/resolve?action=ACCEPT|DISCARD`.
///
/// The client's main menu calls this (`ResolveOverflowChestRequest`) when it finds a
/// chest still sitting in `overflowTreasury` — one found with the treasury full and
/// never opened, e.g. because the app was closed first. The route did not exist, so
/// such a chest could never be cleared (report #451).
///
/// Not in the capture corpus: the ACCEPT/DISCARD names come from the client
/// (`ResolveOverflowChestRequest` in `dump.cs`). ACCEPT keeps the chest by moving
/// it into the treasury (the client also calls this after a treasury expansion,
/// `ResolveOverflowFromTreasuryExpansion`); DISCARD drops it. The response type is
/// the bare `Response`, so only the inventory diff is returned.
#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/treasuries/current/resolve"
)]
pub async fn resolve_overflow_chest(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    query: Query<ResolveQuery>,
) -> Result<Json<ResolveResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let character_id = path.into_inner();
    let action = ResolveAction::parse(&query.action)
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, CHEST_SERVICE_ID, 2))?;
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

            let mut tracker = InventoryChangeTracker::default();
            resolve_overflow(&mut entry.inventory.0, &action, &mut tracker);
            if !tracker.modified_overflow_treasury.removed.is_empty() {
                entry.inventory.0.treasury_version += 1;
            }
            let inventory = entry.inventory.0.generate_client_update(&tracker);

            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(entry.id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }
            Ok::<_, BladeApiError>(Json(ResolveResponse { inventory }))
        }
        .scope_boxed()
    })
    .await
}

#[cfg(test)]
mod overflow_tests {
    use super::*;
    use blades_lib::economy::remove_any_chest;
    use blades_lib::user_data::{CompleteInventory, TREASURY_CAPACITY};

    fn inventory_with_chests(n: usize) -> CompleteInventory {
        let mut inv = CompleteInventory {
            backpack: Default::default(),
            loadout: Default::default(),
            treasury: Default::default(),
            overflow_treasury: Default::default(),
            backpack_version: 1,
            treasury_version: 1,
        };
        for _ in 0..n {
            inv.add_treasury_chest(2, 40);
        }
        inv
    }

    /// Retail capture 37528: treasury at 100, a dungeon chest → overflowTreasury.
    #[test]
    fn a_chest_found_with_a_full_treasury_goes_to_overflow() {
        let mut inv = inventory_with_chests(TREASURY_CAPACITY);
        let mut tracker = InventoryChangeTracker::default();
        let id = inv.add_found_chest(3, 33, &mut tracker);

        assert_eq!(inv.treasury.chests().len(), TREASURY_CAPACITY);
        assert_eq!(inv.overflow_treasury.chests().len(), 1);
        let wire = serde_json::to_value(inv.generate_client_update(&tracker)).unwrap();
        assert_eq!(wire["overflowTreasury"]["chests"][0]["id"], id);
        assert_eq!(wire["treasury"]["chests"], serde_json::json!([]));
        assert_eq!(id, (TREASURY_CAPACITY + 1).to_string(), "one id sequence");
    }

    #[test]
    fn a_chest_found_below_capacity_still_goes_to_the_treasury() {
        let mut inv = inventory_with_chests(TREASURY_CAPACITY - 1);
        let mut tracker = InventoryChangeTracker::default();
        inv.add_found_chest(2, 33, &mut tracker);
        assert_eq!(inv.treasury.chests().len(), TREASURY_CAPACITY);
        assert!(inv.overflow_treasury.chests().is_empty());
    }

    /// Retail capture 37529: collecting the overflow chest answers
    /// `overflowTreasury: {removedChests: ["118"]}`.
    #[test]
    fn collecting_an_overflow_chest_removes_it_from_overflow() {
        let mut inv = inventory_with_chests(TREASURY_CAPACITY);
        let mut t = InventoryChangeTracker::default();
        let id = inv.add_found_chest(3, 33, &mut t);

        let mut tracker = InventoryChangeTracker::default();
        assert!(remove_any_chest(&mut inv, &id, &mut tracker));
        assert!(inv.overflow_treasury.chests().is_empty());
        let wire = serde_json::to_value(inv.generate_client_update(&tracker)).unwrap();
        assert_eq!(
            wire["overflowTreasury"]["removedChests"],
            serde_json::json!([id])
        );
        assert!(wire["treasury"].get("removedChests").is_none());
    }

    #[test]
    fn resolve_accept_moves_and_discard_drops() {
        for (action, kept) in [(ResolveAction::Accept, 1), (ResolveAction::Discard, 0)] {
            let mut inv = inventory_with_chests(TREASURY_CAPACITY);
            let mut t = InventoryChangeTracker::default();
            inv.add_found_chest(3, 33, &mut t);
            let mut tracker = InventoryChangeTracker::default();
            resolve_overflow(&mut inv, &action, &mut tracker);
            assert!(inv.overflow_treasury.chests().is_empty());
            assert_eq!(inv.treasury.chests().len(), TREASURY_CAPACITY + kept);
        }
        assert_eq!(ResolveAction::parse("ACCEPT"), Some(ResolveAction::Accept));
        assert_eq!(
            ResolveAction::parse("DISCARD"),
            Some(ResolveAction::Discard)
        );
        assert_eq!(ResolveAction::parse("x"), None);
    }
}
