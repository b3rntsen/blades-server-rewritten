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
use uuid::Uuid;

use crate::features::challenges::ChallengeState;

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
}
