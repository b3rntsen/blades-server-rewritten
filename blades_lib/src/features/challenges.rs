//! Challenges — `POST /challenges` (list/generate), `POST /challenges/{id}`
//! (progress), `POST /challenges/{id}/complete`, `POST /challenges/{id}/abandon`.
//!
//! Rotating per-character objectives ("salvage 9 items", "kill 10 enemies", …) that
//! pay a currency reward. Templates (objective + reward) are capture-derived; the
//! active set + a rotation cursor live in `server_state`. Progress is client-driven
//! (the client reports an absolute value); completing grants the reward, bumps the
//! season point total, and rotates in a fresh challenge.
//!
//! Captured challenge object:
//! ```jsonc
//! { "id": "…", "templateId": "…", "status": "ACTIVE",
//!   "objective": { "type": "Backend.DefaultObjective", "_uid": {"_id":"…"},
//!                  "_location": 1, "_quota": 9.0 },
//!   "progress": 0.0, "reward": { "currencies": { "f8d27767-…": 300 } },
//!   "generatedCategoryTimestamp": 1777808334, "createdTimestamp": 1778672126533 }
//! ```

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::economy::{RewardGrant, GEMS, GOLD};

/// How many challenges are active at once (matches the captured 4-challenge set).
pub const ACTIVE_CHALLENGE_COUNT: usize = 4;

/// Gold a challenge's gold reward is converted at, per gem.
///
/// Owner-approved deviation from retail, #388, 2026-10-16: challenges pay no gold,
/// only gems (HauDrauf's beginner-economy package). The pool has no tiers to take
/// a same-tier gem reward from, so the ratio is the pool's own: its 35 gold
/// templates pay 41,450 gold (mean 1,184) and its 10 gem templates 46 gems (mean
/// 4.6), about 257 gold a gem, rounded to 250. 750 gold (the median) is 3 gems,
/// 5,000 (the most) is 20.
pub const CHALLENGE_GOLD_PER_GEM: u64 = 250;

/// The fewest gems a converted gold reward pays, before doubling: the pool's
/// smallest gem reward (2).
///
/// Owner-approved deviation from retail, #388, 2026-10-16.
pub const CHALLENGE_MIN_CONVERTED_GEMS: u64 = 2;

/// Every challenge's gem reward is multiplied by this, converted gold included.
///
/// Owner-approved deviation from retail, #388, 2026-10-16: "double the gem
/// amounts".
pub const CHALLENGE_GEM_MULTIPLIER: u64 = 2;

/// The reward a challenge pays: `retail`'s with its gold replaced by gems
/// ([`CHALLENGE_GOLD_PER_GEM`], at least [`CHALLENGE_MIN_CONVERTED_GEMS`]) and
/// every gem doubled ([`CHALLENGE_GEM_MULTIPLIER`]). Anything else is kept.
///
/// Owner-approved deviation from retail, #388, 2026-10-16. Always applied to the
/// TEMPLATE's reward, never to an instance's, so it cannot double twice.
pub fn gems_only_reward(retail: &RewardGrant) -> RewardGrant {
    let mut out = retail.clone();
    let mut gems = out.currencies.remove(&GEMS).unwrap_or(0);
    if let Some(gold) = out.currencies.remove(&GOLD).filter(|g| *g > 0) {
        gems += ((gold + CHALLENGE_GOLD_PER_GEM / 2) / CHALLENGE_GOLD_PER_GEM).max(CHALLENGE_MIN_CONVERTED_GEMS);
    }
    if gems > 0 {
        out.currencies.insert(GEMS, gems * CHALLENGE_GEM_MULTIPLIER);
    }
    out
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ChallengeStatus {
    #[serde(rename = "ACTIVE")]
    Active,
    #[serde(rename = "COMPLETED")]
    Completed,
    #[serde(rename = "ABANDONED")]
    Abandoned,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ObjectiveUid {
    #[serde(rename = "_id")]
    pub id: Uuid,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChallengeObjective {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "_uid")]
    pub uid: ObjectiveUid,
    #[serde(rename = "_location")]
    pub location: i64,
    #[serde(rename = "_quota")]
    pub quota: f64,
}

/// A challenge template (the capture-derived pool the active set is generated from):
/// the objective shape (incl. its `_quota`) and the reward it pays.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeTemplate {
    pub template_id: Uuid,
    pub objective: ChallengeObjective,
    pub reward: RewardGrant,
}

/// A live per-character challenge instance — persisted in `server_state` and returned
/// on the wire verbatim.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeInstance {
    pub id: Uuid,
    pub template_id: Uuid,
    pub status: ChallengeStatus,
    pub objective: ChallengeObjective,
    pub progress: f64,
    pub reward: RewardGrant,
    pub generated_category_timestamp: i64,
    pub created_timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completed_timestamp: Option<i64>,
}

impl ChallengeInstance {
    /// Instantiate a template into a live challenge. `id`/`uid_id` are freshly minted
    /// by the caller (the server has uuid-v4; this crate does not), `now_secs`/`now_ms`
    /// stamp the generation/creation times.
    pub fn from_template(
        template: &ChallengeTemplate,
        id: Uuid,
        uid_id: Uuid,
        now_secs: i64,
        now_ms: i64,
    ) -> Self {
        ChallengeInstance {
            id,
            template_id: template.template_id,
            status: ChallengeStatus::Active,
            objective: ChallengeObjective {
                kind: template.objective.kind.clone(),
                uid: ObjectiveUid { id: uid_id },
                location: template.objective.location,
                quota: template.objective.quota,
            },
            progress: 0.0,
            reward: gems_only_reward(&template.reward),
            generated_category_timestamp: now_secs,
            created_timestamp: now_ms,
            completed_timestamp: None,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.progress >= self.objective.quota
    }
}

/// Per-character challenge state (persisted in `server_state`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChallengeState {
    pub active: Vec<ChallengeInstance>,
    /// Rotation cursor into the template pool (advances as challenges are replaced).
    pub cursor: usize,
    /// Season points accrued from completed challenges.
    pub points: i64,
}

/// The next `count` template indices from the pool, starting at `cursor` (wrapping).
pub fn rotate_indices(pool_len: usize, cursor: usize, count: usize) -> Vec<usize> {
    if pool_len == 0 {
        return Vec::new();
    }
    (0..count).map(|i| (cursor + i) % pool_len).collect()
}

/// Clamp a client-reported absolute progress to `[0, quota]`.
pub fn clamp_progress(progress: f64, quota: f64) -> f64 {
    progress.clamp(0.0, quota)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::GOLD;
    use std::collections::HashMap;

    fn template(quota: f64) -> ChallengeTemplate {
        ChallengeTemplate {
            template_id: Uuid::from_u128(0xabc),
            objective: ChallengeObjective {
                kind: "Backend.DefaultObjective".to_string(),
                uid: ObjectiveUid {
                    id: Uuid::from_u128(0xdef),
                },
                location: 1,
                quota,
            },
            reward: RewardGrant {
                currencies: HashMap::from([(GOLD, 300)]),
                ..Default::default()
            },
        }
    }

    #[test]
    fn rotate_wraps_around_the_pool() {
        assert_eq!(rotate_indices(3, 0, 4), vec![0, 1, 2, 0]);
        assert_eq!(rotate_indices(3, 2, 2), vec![2, 0]);
        assert!(rotate_indices(0, 0, 4).is_empty());
    }

    #[test]
    fn instance_starts_active_at_zero_progress() {
        let inst = ChallengeInstance::from_template(
            &template(9.0),
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            1000,
            1000_000,
        );
        assert_eq!(inst.status, ChallengeStatus::Active);
        assert_eq!(inst.progress, 0.0);
        assert_eq!(inst.objective.quota, 9.0);
        assert!(!inst.is_complete());
        assert!(inst.completed_timestamp.is_none());
    }

    #[test]
    fn progress_clamps_and_completion_tracks_quota() {
        assert_eq!(clamp_progress(5.0, 9.0), 5.0);
        assert_eq!(clamp_progress(20.0, 9.0), 9.0, "over-quota clamps");
        assert_eq!(clamp_progress(-1.0, 9.0), 0.0, "negative clamps");
        let mut inst = ChallengeInstance::from_template(
            &template(9.0),
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            1000,
            1000_000,
        );
        inst.progress = clamp_progress(9.0, inst.objective.quota);
        assert!(inst.is_complete());
    }

    /// #388 (owner-approved deviation from retail, 2026-10-16): no gold, gems
    /// doubled, converted at 250 gold a gem with a floor of 2.
    #[test]
    fn challenges_pay_doubled_gems_and_no_gold() {
        let gold = |n| RewardGrant { currencies: HashMap::from([(GOLD, n)]), ..Default::default() };
        let gems = |n| RewardGrant { currencies: HashMap::from([(GEMS, n)]), ..Default::default() };
        for (retail, want) in [
            (gold(200), 4),
            (gold(300), 4),
            (gold(750), 6),
            (gold(1250), 10),
            (gold(2000), 16),
            (gold(4250), 34),
            (gold(5000), 40),
            (gems(2), 4),
            (gems(10), 20),
        ] {
            assert_eq!(gems_only_reward(&retail), gems(want), "{:?}", retail.currencies);
        }
        let inst = ChallengeInstance::from_template(&template(9.0), Uuid::from_u128(1), Uuid::from_u128(2), 0, 0);
        assert_eq!(inst.reward.currencies, HashMap::from([(GEMS, 4)]), "300 gold is 2 gems, doubled");
    }

    /// Every shipped challenge pays gems only.
    #[test]
    fn every_shipped_challenge_pays_gems_only() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/challenges.json")).unwrap();
        let pool: Vec<ChallengeTemplate> = serde_json::from_str(&raw).unwrap();
        assert_eq!(pool.len(), 45);
        let (mut before, mut after) = (0u64, 0u64);
        for t in &pool {
            let paid = gems_only_reward(&t.reward);
            assert_eq!(paid.currencies.keys().collect::<Vec<_>>(), vec![&GEMS], "{}", t.template_id);
            before += t.reward.currencies.get(&GEMS).copied().unwrap_or(0);
            after += paid.currencies[&GEMS];
        }
        assert_eq!((before, after), (46, 92 + 2 * 167), "pool gems: 46 retail; 92 doubled + 167 from gold, doubled");
    }

    #[test]
    fn instance_round_trips_camelcase_wire() {
        let inst = ChallengeInstance::from_template(
            &template(9.0),
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            1777808334,
            1778672126533,
        );
        let v = serde_json::to_value(&inst).unwrap();
        assert_eq!(v["status"], "ACTIVE");
        assert_eq!(v["objective"]["_quota"], 9.0);
        assert_eq!(v["objective"]["type"], "Backend.DefaultObjective");
        assert!(v.get("completedTimestamp").is_none());
    }
}
