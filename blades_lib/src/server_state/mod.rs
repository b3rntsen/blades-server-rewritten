//! Per-character **server-managed** state that the captured character JSON does not
//! model: which gifts a player has claimed, when they last collected the daily
//! reward, active craft jobs, the current abyss run, generated challenge sets, etc.
//!
//! Persisted in the `characters.server_state` JSONB column (added by the
//! `add_server_state` migration) and never sent to the client — it backs the
//! server's own bookkeeping so flows stay economically coherent (e.g. the daily
//! reward can't be re-collected for infinite gold). Every field is `#[serde(default)]`
//! so an empty `{}` (or a row that predates a new field) deserializes cleanly.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::features::challenges::ChallengeState;
use crate::features::daily_reward::DailyRewardState;
use crate::features::merchant::MerchantWindow;

/// One floor entry in an active abyss run. Mirrors the client wire shape exactly so
/// the server can reconstruct the full `abyss.slices` list on `/current` and `/start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssSliceEntry {
    pub dungeon_settings_id: Uuid,
    pub difficulty_level: u32,
    pub hardcore: bool,
    pub slice_index: u32,
    pub floor_index: u32,
    pub completed: bool,
    pub enemy_killed: bool,
}

/// Server-tracked state for an in-progress abyss run. Stored in
/// `server_state.abyss`; cleared on `/end`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssRun {
    /// The full pre-generated slice list (floors 1–N), faithfully matching prod.
    pub slices: Vec<AbyssSliceEntry>,
    /// Number of revives used so far.
    pub revive_count: u32,
    /// Player level at run start (used for difficulty/XP scaling).
    pub initial_player_level: u32,
    /// Pseudo-random seed for client-side generation.
    pub seed: i64,
    /// Cumulative score (enemy kills count toward future reward thresholds).
    pub score: f64,
    pub algorithm_version: u32,
    pub version: u32,
    /// Index of the current active floor (0-based into `slices`).
    pub current_floor_index: usize,
    /// Server-validated enemies killed during this run, keyed by floor and the
    /// three-part generated-enemy identity. Kept out of the client wire shape;
    /// this is only the replay/ownership proof for corpse loot.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub killed_enemies: BTreeSet<String>,
    /// Corpse-loot identities already paid during this run. Taking the entry
    /// before granting makes a retried update idempotent.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub collected_enemy_loot: BTreeSet<String>,
}

/// An in-progress craft job, persisted in `server_state.craft_jobs`. Created by
/// `POST /crafts`, consumed (and results granted) by `POST /crafts/{id}/finish`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CraftJob {
    pub id: Uuid,
    pub recipe_id: Uuid,
    pub building_id: Uuid,
    pub crafting_type_id: Uuid,
    /// Unix milliseconds when the job completes (now + durationMs at creation time).
    pub completed_at_ms: i64,
    /// Verbatim `results` from the recipe (items or stackableItems) — re-expanded on finish.
    pub results: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerState {
    /// How many times each global gift has been claimed (`globalGiftId` -> count).
    pub gift_claims: HashMap<Uuid, u64>,
    /// How many times each global-shop product has been bought
    /// (`globalShopProductId` -> count), surfaced by `GET /globalshops/current`.
    pub global_shop_purchases: HashMap<Uuid, u64>,
    /// How many times each global-shop offer has been bought **in its current
    /// window**, keyed by the offer's full `purchaseTrackingId`.
    ///
    /// Separate from [`Self::global_shop_purchases`] because the two count
    /// different things and cannot share a key. That one is a LIFETIME total per
    /// product and backs `maxPurchases`; this one backs the `maxPurchaseLimits`
    /// entries whose tracking id ends in a window start
    /// (`<product>::override::<override>::<startTimeSecs>`).
    ///
    /// Keying by the whole tracking id is what makes the reset free: the rotation
    /// shifts that embedded timestamp, so a new window is a new key and the count
    /// starts at zero without any expiry logic. Enforcing a per-window cap against
    /// the lifetime counter instead would bar the offer for ever after its first
    /// window — the bug that locked a player out of an event permanently.
    #[serde(default)]
    pub global_shop_window_purchases: HashMap<String, u64>,
    /// Active challenge set + rotation cursor + season points.
    pub challenges: ChallengeState,
    /// Last 24h period the daily login reward was collected.
    pub daily_reward: DailyRewardState,
    /// Active craft jobs (smithy/alchemy). Created by `POST /crafts`, finished by
    /// `POST /crafts/{id}/finish`. `#[serde(default)]` ensures old rows without this
    /// field deserialize cleanly as an empty list.
    #[serde(default)]
    pub craft_jobs: Vec<CraftJob>,
    /// Active abyss run, if any. `None` means no run in progress. Set by `/start`,
    /// updated by `/update`, cleared by `/end`.
    #[serde(default)]
    pub abyss: Option<AbyssRun>,
    /// Town-merchant state per shop (building instance) id: the rolled 10-hour
    /// catalog, the merchant's gold budget for that window, what the player has
    /// bought from it, and live buyback slots. Without this a merchant had no money
    /// at all and its stock re-rolled off the wall clock instead of from the
    /// player's first visit (tracker #30). `#[serde(default)]` so rows written
    /// before this field deserialize as an empty map.
    ///
    /// Keyed by shop id on the VISITING character, so browsing another player's
    /// vendor tracks a window here rather than on its owner — adequate for the
    /// buy/sell endpoints we serve, and a noted divergence from retail, which held
    /// the catalog against the shop itself.
    #[serde(default)]
    pub shops: HashMap<Uuid, MerchantWindow>,
    /// How many times each event-quest INSTANCE has been completed, keyed by the
    /// per-character instance quest id.
    ///
    /// RETIRED — no longer read or written. It was a second event-completion counter
    /// beside the server's `event_completions` table, advanced by `/complete` while
    /// the dungeon exit advanced the table, so a first completion paid twice. The
    /// table is the one counter now. Kept only so stored rows that carry it still
    /// deserialize (`#[serde(default)]` covers rows written before it existed).
    #[serde(default)]
    pub event_quest_completions: HashMap<Uuid, u32>,
    /// Arena trophy thresholds whose fixed promotion-loot entries have been
    /// granted. This is server-only idempotency state: the client-visible high-
    /// water mark says a rung was crossed, but cannot prove its loot persisted.
    /// Existing rows deserialize to an empty set.
    #[serde(default)]
    pub arena_promotion_loot_grants: BTreeSet<i64>,
    /// Individually repaired promotion items, keyed by
    /// `seasonUuid:trophyThreshold:itemTemplateUuid`. Normal match persistence
    /// grants a whole rung atomically and records the threshold above; this finer
    /// key lets an operator repair every member of a multi-item bundle without
    /// the first item suppressing the rest.
    #[serde(default)]
    pub arena_promotion_item_repairs: BTreeSet<String>,
    /// Number of chests already emitted by the Arena's eight-round-win meter.
    /// This is deliberately season-independent: the shipped PvP chest cycle keeps
    /// its position when a season rolls over. Existing rows default to the start of
    /// the cycle and are backfilled from the bounded `arena_match_results` audit.
    #[serde(default)]
    pub arena_chests_earned: u64,
    /// Last time each exact `AdditionalChestRule` from the shipped PvP cycle fired.
    /// The two Elder rules have distinct UIDs and therefore distinct one-day repeat
    /// limits; merging them would incorrectly suppress the second Elder in a fast
    /// cycle. The Legendary rule repeats after one week.
    #[serde(default)]
    pub arena_last_elder_one_chest_at_secs: i64,
    #[serde(default)]
    pub arena_last_elder_two_chest_at_secs: i64,
    #[serde(default)]
    pub arena_last_legendary_chest_at_secs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_in_flight_abyss_run_defaults_the_new_loot_ledger() {
        let run: AbyssRun = serde_json::from_value(serde_json::json!({
            "slices": [],
            "reviveCount": 0,
            "initialPlayerLevel": 1,
            "seed": 1,
            "score": 0.0,
            "algorithmVersion": 1,
            "version": 1,
            "currentFloorIndex": 0
        }))
        .expect("runs persisted before the loot ledger must still load");
        assert!(run.killed_enemies.is_empty());
        assert!(run.collected_enemy_loot.is_empty());

        let value = serde_json::to_value(run).unwrap();
        assert!(value.get("killedEnemies").is_none());
        assert!(value.get("collectedEnemyLoot").is_none());
    }

    #[test]
    fn old_server_state_defaults_the_promotion_ledger() {
        let state: ServerState = serde_json::from_str("{}").unwrap();
        assert!(state.arena_promotion_loot_grants.is_empty());
        assert_eq!(state.arena_chests_earned, 0);
        assert_eq!(state.arena_last_elder_one_chest_at_secs, 0);
        assert_eq!(state.arena_last_elder_two_chest_at_secs, 0);
        assert_eq!(state.arena_last_legendary_chest_at_secs, 0);
    }

    #[test]
    fn promotion_ledger_round_trips_as_server_only_json() {
        let mut state = ServerState::default();
        state.arena_promotion_loot_grants.extend([100, 200]);
        state
            .arena_promotion_item_repairs
            .insert("season:100:item".into());
        state.arena_chests_earned = 41;
        state.arena_last_elder_one_chest_at_secs = 1_725_000_000;
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["arenaPromotionLootGrants"], serde_json::json!([100, 200]));
        assert_eq!(
            value["arenaPromotionItemRepairs"],
            serde_json::json!(["season:100:item"])
        );
        assert_eq!(
            serde_json::from_value::<ServerState>(value.clone())
                .unwrap()
                .arena_promotion_loot_grants,
            state.arena_promotion_loot_grants
        );
        assert_eq!(value["arenaChestsEarned"], serde_json::json!(41));
        assert_eq!(
            value["arenaLastElderOneChestAtSecs"],
            serde_json::json!(1_725_000_000i64)
        );
    }
}
