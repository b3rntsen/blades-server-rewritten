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
/// the objective shape (incl. its `_quota`), the reward it pays, and who retail
/// offered it to.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeTemplate {
    pub template_id: Uuid,
    pub objective: ChallengeObjective,
    pub reward: RewardGrant,
    /// Retail's `ChallengeSelector._requiredPlayerLevel`: the lowest level the
    /// challenge is offered at (0 = no lower bound). Report #406.
    #[serde(default)]
    pub required_player_level: u16,
    /// Retail's `ChallengeSelector._lockedAtPlayerLevel`: the level from which it
    /// is no longer offered (0 = no upper bound). Report #406.
    #[serde(default)]
    pub locked_at_player_level: u16,
    /// The enemy group a `DefeatChallengeData` asks the player to kill, if any
    /// (`Challenge_DefeatSkeever_4` → Skeevers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_enemy_group: Option<Uuid>,
}

impl ChallengeTemplate {
    /// Whether retail offered this challenge to a character of `level`: its
    /// selector's player-level band, `[required, lockedAt)` with 0 meaning no
    /// bound (report #406). `Challenge_DefeatSkeever_4` is
    /// `ChallengeSelector_PlayerLevel_15-19`, so a level-65 character is never
    /// sent hunting Skeevers, which only spawn in low-level content.
    pub fn offered_at(&self, level: u16) -> bool {
        level >= self.required_player_level
            && (self.locked_at_player_level == 0 || level < self.locked_at_player_level)
    }
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

/// The pool indices to hand a character of `level` for `free` empty slots, and
/// the cursor to resume from.
///
/// Walks the pool round-robin from `cursor` (the rotation the set always used)
/// and takes the next templates offered at `level` ([`ChallengeTemplate::offered_at`])
/// that are neither in `held` nor already taken. One lap at most: when fewer
/// than `free` templates qualify the set stays short rather than repeating one.
/// The cursor moves past the last template taken, so the rotation carries on.
pub fn pick_templates(
    pool: &[ChallengeTemplate],
    cursor: usize,
    level: u16,
    held: &[Uuid],
    free: usize,
) -> (Vec<usize>, usize) {
    let mut picked: Vec<usize> = Vec::new();
    let mut next = cursor;
    for step in 0..pool.len() {
        if picked.len() >= free {
            break;
        }
        let idx = (cursor + step) % pool.len();
        let t = &pool[idx];
        if t.offered_at(level)
            && !held.contains(&t.template_id)
            && !picked.iter().any(|&i| pool[i].template_id == t.template_id)
        {
            picked.push(idx);
            next = cursor.wrapping_add(step + 1);
        }
    }
    (picked, next)
}

/// Whether an active challenge should be retired for a character of `level`
/// (report #406): its template is no longer offered at that level and the
/// player has made no progress on it.
///
/// Retail offered a challenge only inside its level band but let one already
/// held run on until its category expired; the capture shows 23 of 196
/// held challenges a few levels past their band and none below it. This
/// server has no expiry, so an out-of-band challenge was held for good: Lloyd
/// (level 65) has carried `Challenge_DefeatSkeever_4` since 2026-09-25. One
/// with progress is kept, so nothing earned is taken away. A template no longer
/// in the pool is kept too: there is nothing to judge it by.
pub fn retire_out_of_band(c: &ChallengeInstance, pool: &[ChallengeTemplate], level: u16) -> bool {
    c.status == ChallengeStatus::Active
        && c.progress <= 0.0
        && pool
            .iter()
            .find(|t| t.template_id == c.template_id)
            .is_some_and(|t| !t.offered_at(level))
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
            required_player_level: 0,
            locked_at_player_level: 0,
            target_enemy_group: None,
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

    fn shipped_pool() -> Vec<ChallengeTemplate> {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/challenges.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    const SKEEVER_4: Uuid = Uuid::from_u128(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e);
    /// LLoyd's level on prod when he filed #406.
    const LLOYD_LEVEL: u16 = 65;

    /// #406: the shipped pool carries retail's `ChallengeSelector` bands. The
    /// Skeever hunt is `ChallengeSelector_PlayerLevel_15-19`.
    #[test]
    fn report_406_skeever_hunt_is_offered_only_at_levels_15_to_19() {
        let pool = shipped_pool();
        let skeever = pool.iter().find(|t| t.template_id == SKEEVER_4).expect("Challenge_DefeatSkeever_4");
        assert_eq!((skeever.required_player_level, skeever.locked_at_player_level), (15, 20));
        let offered: Vec<u16> = (1..=100).filter(|&l| skeever.offered_at(l)).collect();
        assert_eq!(offered, (15..=19).collect::<Vec<_>>());
        assert!(!skeever.offered_at(LLOYD_LEVEL));
        for t in &pool {
            assert!(
                t.locked_at_player_level == 0 || t.required_player_level < t.locked_at_player_level,
                "{} has an empty band",
                t.template_id
            );
        }
    }

    /// #406: at every level 1..=100 and every rotation cursor, a full set of
    /// distinct templates is handed out, each offered at that level, and the
    /// Skeever hunt never reaches Lloyd's level.
    #[test]
    fn report_406_every_level_gets_a_full_set_of_templates_offered_at_it() {
        let pool = shipped_pool();
        for level in 1..=100u16 {
            for cursor in 0..pool.len() {
                let (picked, next) = pick_templates(&pool, cursor, level, &[], ACTIVE_CHALLENGE_COUNT);
                assert_eq!(picked.len(), ACTIVE_CHALLENGE_COUNT, "level {level} cursor {cursor}");
                let mut ids: Vec<Uuid> = picked.iter().map(|&i| pool[i].template_id).collect();
                assert!(picked.iter().all(|&i| pool[i].offered_at(level)), "level {level}");
                ids.sort();
                ids.dedup();
                assert_eq!(ids.len(), ACTIVE_CHALLENGE_COUNT, "duplicates at level {level}");
                assert!(next > cursor, "the cursor advances");
                if level == LLOYD_LEVEL {
                    assert!(!ids.contains(&SKEEVER_4), "Skeever hunt offered at level {level}");
                }
            }
        }
    }

    #[test]
    fn pick_skips_held_templates_and_never_repeats_when_the_pool_is_short() {
        let mut a = template(1.0);
        a.template_id = Uuid::from_u128(1);
        let mut b = template(1.0);
        b.template_id = Uuid::from_u128(2);
        let mut c = template(1.0);
        c.template_id = Uuid::from_u128(3);
        c.required_player_level = 15;
        c.locked_at_player_level = 20;
        let pool = vec![a, b, c];
        let (picked, next) = pick_templates(&pool, 0, 30, &[Uuid::from_u128(1)], 4);
        assert_eq!(picked, vec![1], "a is held and c is out of band: the set stays short");
        assert_eq!(next, 2, "resume after the last template taken");
        let (picked, _) = pick_templates(&pool, 2, 17, &[], 4);
        assert_eq!(picked, vec![2, 0, 1]);
        assert_eq!(pick_templates(&[], 0, 17, &[], 4), (vec![], 0));
    }

    /// #406: LLoyd's held set on prod (2026-10-17). The zero-progress Skeever
    /// hunt and level-1-10 critical hits are retired at level 65; a challenge
    /// with progress, or with a template the pool no longer has, is kept.
    #[test]
    fn report_406_retires_zero_progress_challenges_outside_the_band() {
        let pool = shipped_pool();
        let held = |tid: u128, progress: f64| {
            let t = pool.iter().find(|t| t.template_id == Uuid::from_u128(tid)).unwrap();
            let mut c = ChallengeInstance::from_template(t, Uuid::from_u128(9), Uuid::from_u128(9), 0, 0);
            c.progress = progress;
            c
        };
        let skeever = held(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e, 0.0);
        assert!(retire_out_of_band(&skeever, &pool, LLOYD_LEVEL));
        assert!(!retire_out_of_band(&skeever, &pool, 17), "in band at 17");
        assert!(retire_out_of_band(&held(0x8925740e_046b_4e7a_b418_a17bd83e035c, 0.0), &pool, LLOYD_LEVEL));
        assert!(!retire_out_of_band(&held(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e, 3.0), &pool, LLOYD_LEVEL), "progress is kept");
        let mut unknown = skeever.clone();
        unknown.template_id = Uuid::from_u128(0xdead);
        assert!(!retire_out_of_band(&unknown, &pool, LLOYD_LEVEL), "no template, no judgement");
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
