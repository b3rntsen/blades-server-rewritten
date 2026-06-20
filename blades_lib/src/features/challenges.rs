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

use crate::economy::RewardGrant;

/// How many challenges are active at once (matches the captured 4-challenge set).
pub const ACTIVE_CHALLENGE_COUNT: usize = 4;

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
            reward: template.reward.clone(),
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
