//! Daily login reward — `POST /towns/current/rewards/current` (status) and
//! `POST /towns/current/rewards/current/collect`.
//!
//! A reward rotates each 24h period; the player may collect it once per period. The
//! rotation pool is capture-derived (7 distinct rewards — some grant stackables, some
//! grant a treasury chest); the per-character last-collected period lives in
//! `server_state.daily_reward`.
//!
//! Captured status:
//! ```jsonc
//! { "dailyRewardStatus": { "rewardUid": "eefb9db4-…", "until": 1777784455168,
//!     "dailyReward": { "stackableItems": { "790a188b-…": 2 } }, "collected": false } }
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Length of a daily-reward period (24h).
pub const DAILY_PERIOD_SECS: i64 = 86_400;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChestDef {
    pub tier: u64,
    pub level: u64,
}

/// A daily reward's payload: either stackables or a chest (matches the captured
/// `dailyReward` object, which carries one or the other).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct DailyRewardPayload {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub stackable_items: HashMap<Uuid, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chests: Vec<ChestDef>,
}

/// One entry of the daily-reward rotation (capture-derived).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DailyRewardDef {
    pub reward_uid: Uuid,
    pub daily_reward: DailyRewardPayload,
}

/// Per-character daily-reward state (persisted in `server_state`).
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DailyRewardState {
    /// The last 24h period the reward was collected in (`None` = never).
    pub collected_period: Option<i64>,
}

/// The 24h period index for a unix timestamp.
pub fn current_period(now_secs: i64) -> i64 {
    now_secs.div_euclid(DAILY_PERIOD_SECS)
}

/// When the current period ends (next reset), in unix ms (the wire uses ms).
pub fn until_ms(period: i64) -> i64 {
    (period + 1) * DAILY_PERIOD_SECS * 1000
}

/// The reward offered in a given period (rotates through the pool).
pub fn reward_for_period(defs: &[DailyRewardDef], period: i64) -> Option<&DailyRewardDef> {
    if defs.is_empty() {
        None
    } else {
        Some(&defs[period.rem_euclid(defs.len() as i64) as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> Vec<DailyRewardDef> {
        vec![
            DailyRewardDef {
                reward_uid: Uuid::from_u128(1),
                daily_reward: DailyRewardPayload {
                    stackable_items: HashMap::from([(Uuid::from_u128(10), 2)]),
                    chests: vec![],
                },
            },
            DailyRewardDef {
                reward_uid: Uuid::from_u128(2),
                daily_reward: DailyRewardPayload {
                    stackable_items: HashMap::default(),
                    chests: vec![ChestDef { tier: 3, level: 1 }],
                },
            },
        ]
    }

    #[test]
    fn period_advances_daily_and_rotates() {
        let d0 = current_period(0);
        let d1 = current_period(DAILY_PERIOD_SECS);
        assert_eq!(d1, d0 + 1);
        // Rotation alternates between the two defs.
        assert_eq!(reward_for_period(&defs(), d0).unwrap().reward_uid, Uuid::from_u128(1));
        assert_eq!(reward_for_period(&defs(), d1).unwrap().reward_uid, Uuid::from_u128(2));
    }

    #[test]
    fn until_is_next_period_in_ms() {
        let p = current_period(1_000_000);
        assert_eq!(until_ms(p), (p + 1) * DAILY_PERIOD_SECS * 1000);
    }

    #[test]
    fn payload_serializes_one_branch_only() {
        let stack = serde_json::to_value(&defs()[0].daily_reward).unwrap();
        assert!(stack.get("stackableItems").is_some());
        assert!(stack.get("chests").is_none(), "empty chests omitted");
        let chest = serde_json::to_value(&defs()[1].daily_reward).unwrap();
        assert!(chest.get("chests").is_some());
        assert!(chest.get("stackableItems").is_none());
    }

    #[test]
    fn empty_pool_has_no_reward() {
        assert!(reward_for_period(&[], 5).is_none());
    }
}
