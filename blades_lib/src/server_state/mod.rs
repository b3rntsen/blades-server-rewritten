//! Per-character **server-managed** state that the captured character JSON does not
//! model: which gifts a player has claimed, when they last collected the daily
//! reward, active craft jobs, the current abyss run, generated challenge sets, etc.
//!
//! Persisted in the `characters.server_state` JSONB column (added by the
//! `add_server_state` migration) and never sent to the client — it backs the
//! server's own bookkeeping so flows stay economically coherent (e.g. the daily
//! reward can't be re-collected for infinite gold). Every field is `#[serde(default)]`
//! so an empty `{}` (or a row that predates a new field) deserializes cleanly.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::features::challenges::ChallengeState;
use crate::features::daily_reward::DailyRewardState;

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
}
