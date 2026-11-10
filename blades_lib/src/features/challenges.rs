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

use crate::economy::{GEMS, GOLD, RewardGrant};

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
        gems += ((gold + CHALLENGE_GOLD_PER_GEM / 2) / CHALLENGE_GOLD_PER_GEM)
            .max(CHALLENGE_MIN_CONVERTED_GEMS);
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
    /// The APK asset name (`Challenge_DealDamage_Cleaving_15_Common`), for
    /// reviewers and tests; never sent to the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Retail's `ChallengeSelector._requiredAbilities`: the abilities a character
    /// must hold to be offered the challenge (report #466). Every
    /// `Challenge_PerformAbility_*` names its own ability here
    /// (`ChallengeSelector_RequiredAbilities_11-20_Skullcrusher`), so "use
    /// Skullcrusher 10 times" went only to a character who had learned it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_abilities: Vec<Uuid>,
    /// Retail's `BaseChallengeData._challengePoolCategory`: [`COMMON_CATEGORY`] for
    /// the gold challenges, one of four gem categories (uncommon, rare, epic,
    /// legendary) for the rest (report #466). Absent = common.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<Uuid>,
    /// Retail's `BaseChallengeData._weight`, the template's draw weight among
    /// those offered (report #466). Absent = 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

/// `_challengePoolCategory` of the gold ("common") challenges. Every captured
/// retail set (214 of 224 list responses in the June 2026 snapshot) held three
/// of these and at most one from a gem category.
pub const COMMON_CATEGORY: Uuid = Uuid::from_u128(0x9fbd1d31_1000_4b4a_b7e4_0c29115c1e00);

/// The challenges the APK marks `BaseChallengeData._oneTimeOnly` — all fifteen
/// `Challenge_TUTORIAL_*`, and nothing else (every `Challenge_*` asset in
/// `reference/apk/blades.apk` read, 2026-10-24).
///
/// The client hides the Cancel button for these (`QuestChallengeEntry.Refresh`
/// sets it from `!ChallengeData._oneTimeOnly`), so one handed out that never
/// progresses holds a slot for good. Retail handed a character each of them
/// once, as part of the tutorial; this server has no tutorial (FTUE is patched
/// out) and used to rotate `Challenge_TUTORIAL_CraftDecoration` in at every
/// level, again and again (tracker #426). They are never offered
/// ([`ChallengeTemplate::offered_at`]).
pub const ONE_TIME_ONLY_TEMPLATES: [Uuid; 15] = [
    Uuid::from_u128(0xbce21d12_8f3b_444b_ad1f_740478f1d0cf), // CraftDecoration
    Uuid::from_u128(0x1eb73b9f_b8c2_4359_8734_8acaf2a781b8), // CastFireball
    Uuid::from_u128(0xa0f7207a_a530_4413_aae3_ddd2f80bae90), // BrewHealthPotion
    Uuid::from_u128(0x2d782025_9287_412e_9865_8f992dd7260c), // PerformCriticalHit
    Uuid::from_u128(0x42b56ab2_a172_4096_9876_f18abc8338a0), // TryTheAbyss
    Uuid::from_u128(0x4b2b8652_0274_48b5_a578_b00d8421361a), // RestoreMagicka
    Uuid::from_u128(0x26c45c2f_b632_48f8_ab88_ade13c052c14), // Repair
    Uuid::from_u128(0x16d0a4b3_8a35_443a_94ef_3732e08eee69), // StunEnemy
    Uuid::from_u128(0x06c8d0d6_2d33_4899_8064_c112249e9b45), // BuildHouse
    Uuid::from_u128(0x7f4ebb4a_c244_4933_856e_f85c92fbea3a), // Smith
    Uuid::from_u128(0x59f38e6b_39df_410d_9756_6ef87a1d95c3), // Salvage
    Uuid::from_u128(0x5960c6e0_9638_4983_a5f2_43230e93cd45), // DealSlashingDamage
    Uuid::from_u128(0xd690be0e_0efe_4e0c_a194_33a1a9161864), // Dodge
    Uuid::from_u128(0xb91563d0_87fb_49a9_8e48_6ee40aa146de), // DefeatEnemiesInJobs
    Uuid::from_u128(0x93a363f5_5eea_44b5_8d92_2f321658b3b8), // RestoreStamina
];

impl ChallengeTemplate {
    /// Whether the APK marks this a one-time (tutorial) challenge
    /// ([`ONE_TIME_ONLY_TEMPLATES`]).
    pub fn one_time_only(&self) -> bool {
        ONE_TIME_ONLY_TEMPLATES.contains(&self.template_id)
    }

    /// Whether retail offered this challenge to a character of `level`: its
    /// selector's player-level band, `[required, lockedAt)` with 0 meaning no
    /// bound (report #406). `Challenge_DefeatSkeever_4` is
    /// `ChallengeSelector_PlayerLevel_15-19`, so a level-65 character is never
    /// sent hunting Skeevers, which only spawn in low-level content.
    ///
    /// A one-time tutorial challenge is offered at no level (#426), so
    /// [`retire_out_of_band`] also retires one already held at zero progress.
    pub fn offered_at(&self, level: u16) -> bool {
        !self.one_time_only()
            && level >= self.required_player_level
            && (self.locked_at_player_level == 0 || level < self.locked_at_player_level)
    }

    /// Whether this is a gem-category ("premium") challenge, of which retail held
    /// at most one at a time ([`COMMON_CATEGORY`]).
    pub fn premium(&self) -> bool {
        self.category.is_some_and(|c| c != COMMON_CATEGORY)
    }

    /// The draw weight ([`Self::weight`], 1 when absent).
    pub fn draw_weight(&self) -> f64 {
        self.weight.unwrap_or(1.0).max(0.0)
    }

    /// Whether retail offered this challenge to a character of `level` holding
    /// `abilities` (the character's `abilities` map, ability id -> rank): its
    /// level band ([`Self::offered_at`]) and every one of its
    /// [`Self::required_abilities`] learned (report #466: HauDrauf, without
    /// Skullcrusher, was asked to use it).
    pub fn offered_to(&self, level: u16, abilities: &serde_json::Value) -> bool {
        self.offered_at(level)
            && self
                .required_abilities
                .iter()
                .all(|id| holds_ability(abilities, id))
    }
}

/// Whether the character's `abilities` map (ability id -> rank) holds `id` at
/// rank 1 or more.
pub fn holds_ability(abilities: &serde_json::Value, id: &Uuid) -> bool {
    abilities
        .get(id.to_string())
        .is_some_and(|rank| rank.as_f64().is_none_or(|r| r > 0.0))
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

/// One step of SplitMix64: the next state and its output. The challenge draw's
/// RNG (this crate has no `rand`): the server seeds it afresh per refill, tests
/// with a fixed seed so a draw is reproducible.
fn splitmix64(state: u64) -> (u64, u64) {
    let next = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = next;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (next, z ^ (z >> 31))
}

/// The pool indices to hand a character of `level` holding `abilities` for
/// `free` empty slots, and the RNG state to resume from.
///
/// Retail's draw (report #466): each slot is a weighted draw
/// ([`ChallengeTemplate::draw_weight`]) among the templates offered to the
/// character ([`ChallengeTemplate::offered_to`]) that are neither in `held` nor
/// already drawn. A set holds at most one gem-category challenge
/// ([`ChallengeTemplate::premium`]): when none is held, the first free slot draws
/// from those, the rest from the common ones. When too few qualify the set stays
/// short rather than repeating one. `seed` is the RNG state.
///
/// This replaced a round-robin through a 45-template captured pool, which at
/// level 100 could only ever offer the same dozen challenges.
pub fn pick_templates(
    pool: &[ChallengeTemplate],
    seed: usize,
    level: u16,
    abilities: &serde_json::Value,
    held: &[Uuid],
    free: usize,
) -> (Vec<usize>, usize) {
    let mut state = seed as u64;
    let mut picked: Vec<usize> = Vec::new();
    let mut premium_held = held
        .iter()
        .any(|id| pool.iter().any(|t| t.template_id == *id && t.premium()));
    for _ in 0..free {
        let candidates = |premium: bool| -> Vec<usize> {
            (0..pool.len())
                .filter(|&i| {
                    let t = &pool[i];
                    t.premium() == premium
                        && t.offered_to(level, abilities)
                        && !held.contains(&t.template_id)
                        && !picked.iter().any(|&p| pool[p].template_id == t.template_id)
                })
                .collect()
        };
        let mut options = if premium_held {
            Vec::new()
        } else {
            candidates(true)
        };
        let premium = !options.is_empty();
        if !premium {
            options = candidates(false);
        }
        if options.is_empty() {
            break;
        }
        let total: f64 = options.iter().map(|&i| pool[i].draw_weight()).sum();
        let (next, out) = splitmix64(state);
        state = next;
        let choice = if total > 0.0 {
            let mut x = (out >> 11) as f64 / (1u64 << 53) as f64 * total;
            *options
                .iter()
                .find(|&&i| {
                    x -= pool[i].draw_weight();
                    x < 0.0
                })
                .unwrap_or(options.last().unwrap())
        } else {
            options[(out % options.len() as u64) as usize]
        };
        premium_held |= premium;
        picked.push(choice);
    }
    (picked, state as usize)
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
pub fn retire_out_of_band(
    c: &ChallengeInstance,
    pool: &[ChallengeTemplate],
    level: u16,
    abilities: &serde_json::Value,
) -> bool {
    c.status == ChallengeStatus::Active
        && c.progress <= 0.0
        && pool
            .iter()
            .find(|t| t.template_id == c.template_id)
            .is_some_and(|t| !t.offered_to(level, abilities))
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
            name: None,
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
            required_abilities: Vec::new(),
            category: None,
            weight: None,
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
        let gold = |n| RewardGrant {
            currencies: HashMap::from([(GOLD, n)]),
            ..Default::default()
        };
        let gems = |n| RewardGrant {
            currencies: HashMap::from([(GEMS, n)]),
            ..Default::default()
        };
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
            assert_eq!(
                gems_only_reward(&retail),
                gems(want),
                "{:?}",
                retail.currencies
            );
        }
        let inst = ChallengeInstance::from_template(
            &template(9.0),
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            0,
            0,
        );
        assert_eq!(
            inst.reward.currencies,
            HashMap::from([(GEMS, 4)]),
            "300 gold is 2 gems, doubled"
        );
    }

    /// Every shipped challenge pays gems only.
    #[test]
    fn every_shipped_challenge_pays_gems_only() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/challenges.json"
        ))
        .unwrap();
        let pool: Vec<ChallengeTemplate> = serde_json::from_str(&raw).unwrap();
        // The APK library minus one-time, weight-0, town/Abyss-gated and
        // unspawnable-enemy
        // templates (#466; it was the 45 captured ones before).
        assert_eq!(pool.len(), 1584);
        for t in &pool {
            let paid = gems_only_reward(&t.reward);
            assert_eq!(
                paid.currencies.keys().collect::<Vec<_>>(),
                vec![&GEMS],
                "{}",
                t.template_id
            );
            assert!(
                paid.currencies[&GEMS] >= CHALLENGE_GEM_MULTIPLIER * CHALLENGE_MIN_CONVERTED_GEMS
            );
        }
    }

    fn shipped_pool() -> Vec<ChallengeTemplate> {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/challenges.json"
        ))
        .unwrap();
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
        let skeever = pool
            .iter()
            .find(|t| t.template_id == SKEEVER_4)
            .expect("Challenge_DefeatSkeever_4");
        assert_eq!(
            (
                skeever.required_player_level,
                skeever.locked_at_player_level
            ),
            (15, 20)
        );
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

    /// #406/#466: at every level 1..=100 and many seeds, a full set of distinct
    /// templates is handed out, each offered at that level, at most one of them
    /// from a gem category, and the Skeever hunt never reaches Lloyd's level.
    #[test]
    fn report_406_every_level_gets_a_full_set_of_templates_offered_at_it() {
        let pool = shipped_pool();
        for level in 1..=100u16 {
            for seed in 0..40usize {
                let (picked, _) = pick_templates(
                    &pool,
                    seed,
                    level,
                    &serde_json::Value::Null,
                    &[],
                    ACTIVE_CHALLENGE_COUNT,
                );
                assert_eq!(
                    picked.len(),
                    ACTIVE_CHALLENGE_COUNT,
                    "level {level} seed {seed}"
                );
                let mut ids: Vec<Uuid> = picked.iter().map(|&i| pool[i].template_id).collect();
                assert!(
                    picked.iter().all(|&i| pool[i].offered_at(level)),
                    "level {level}"
                );
                assert!(
                    picked.iter().filter(|&&i| pool[i].premium()).count() <= 1,
                    "two gem challenges at level {level}"
                );
                ids.sort();
                ids.dedup();
                assert_eq!(
                    ids.len(),
                    ACTIVE_CHALLENGE_COUNT,
                    "duplicates at level {level}"
                );
                if level == LLOYD_LEVEL {
                    assert!(
                        !ids.contains(&SKEEVER_4),
                        "Skeever hunt offered at level {level}"
                    );
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
        let (picked, _) = pick_templates(
            &pool,
            0,
            30,
            &serde_json::Value::Null,
            &[Uuid::from_u128(1)],
            4,
        );
        assert_eq!(
            picked,
            vec![1],
            "a is held and c is out of band: the set stays short"
        );
        let (mut picked, _) = pick_templates(&pool, 2, 17, &serde_json::Value::Null, &[], 4);
        picked.sort();
        assert_eq!(picked, vec![0, 1, 2]);
        assert_eq!(
            pick_templates(&[], 7, 17, &serde_json::Value::Null, &[], 4),
            (vec![], 7)
        );
    }

    /// #466: a gem-category challenge goes in the first free slot when none is
    /// held, never a second one; a weight-0 template is never drawn while others
    /// can be; the draw follows the weights.
    #[test]
    fn report_466_one_gem_challenge_per_set_and_draws_follow_the_weights() {
        let gem = Uuid::from_u128(0x3e59327f_2552_487a_a070_64580f3737b1);
        let mk = |id: u128, category: Uuid, weight: f64| {
            let mut t = template(1.0);
            t.template_id = Uuid::from_u128(id);
            t.category = Some(category);
            t.weight = Some(weight);
            t
        };
        let pool = vec![
            mk(1, COMMON_CATEGORY, 1.0),
            mk(2, COMMON_CATEGORY, 3.0),
            mk(3, COMMON_CATEGORY, 0.0),
            mk(4, COMMON_CATEGORY, 1.0),
            mk(5, gem, 1.0),
            mk(6, gem, 1.0),
        ];
        let none = serde_json::Value::Null;
        let mut first_of_two = [0usize; 2];
        for seed in 0..2000usize {
            let (picked, _) = pick_templates(&pool, seed, 50, &none, &[], 4);
            assert_eq!(picked.len(), 4);
            assert!(pool[picked[0]].premium(), "slot 1 is the gem challenge");
            assert_eq!(picked.iter().filter(|&&i| pool[i].premium()).count(), 1);
            assert!(!picked.contains(&2), "weight 0 drawn while others could be");
            let (picked, _) = pick_templates(&pool, seed, 50, &none, &[Uuid::from_u128(5)], 1);
            assert!(
                !pool[picked[0]].premium(),
                "a gem challenge is already held"
            );
            let (picked, _) = pick_templates(&pool, seed, 50, &none, &[Uuid::from_u128(6)], 1);
            if picked[0] == 0 {
                first_of_two[0] += 1;
            } else if picked[0] == 1 {
                first_of_two[1] += 1;
            }
        }
        // Weights 1 : 3 among templates 1 and 2 (template 4 also weight 1).
        let ratio = first_of_two[1] as f64 / first_of_two[0] as f64;
        assert!((2.4..3.6).contains(&ratio), "weight ratio {ratio}");
    }

    const CLEAVING_15_COMMON: Uuid = Uuid::from_u128(0xbf88a825_8c46_4825_91e1_b2f1774180bc);

    /// #466 (HauDrauf, level 100): "always blunt damage, never cleave or slash,
    /// and retail had many different ability challenges". The captured pool held
    /// one damage challenge for level 76+ (Bashing) and four ability ones;
    /// retail's library has every damage type and an ability challenge for most
    /// abilities. Over many draws a level-100 character must see all of them.
    #[test]
    fn report_466_level_100_draws_every_damage_type_and_many_abilities() {
        let pool = shipped_pool();
        let every_ability: serde_json::Map<String, serde_json::Value> = pool
            .iter()
            .flat_map(|t| t.required_abilities.iter())
            .map(|id| (id.to_string(), serde_json::json!(1)))
            .collect();
        let every_ability = serde_json::Value::Object(every_ability);
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        for seed in 0..3000usize {
            let (picked, _) = pick_templates(&pool, seed, 100, &every_ability, &[], 4);
            for i in picked {
                let name = pool[i].name.clone().unwrap_or_default();
                if let Some(rest) = name.strip_prefix("Challenge_DealDamage_") {
                    seen.insert(format!("damage:{}", rest.split('_').next().unwrap()));
                }
                if let Some(rest) = name.strip_prefix("Challenge_PerformAbility_") {
                    seen.insert(format!("ability:{}", rest.split('_').next().unwrap()));
                }
            }
        }
        for kind in [
            "Slashing",
            "Slashing+",
            "Cleaving",
            "Cleaving+",
            "Bashing",
            "Bashing+",
            "Fire",
            "Frost",
            "Shock",
            "Poison",
        ] {
            assert!(
                seen.contains(&format!("damage:{kind}")),
                "no {kind} damage challenge: {seen:?}"
            );
        }
        let abilities = seen.iter().filter(|s| s.starts_with("ability:")).count();
        assert!(
            abilities >= 10,
            "only {abilities} ability challenges: {seen:?}"
        );

        // A template new to the pool carries the APK's own values: quota the
        // middle of 22,500..28,125 rounded to 500, reward the top-weight 4,000 gold.
        let cleave = pool
            .iter()
            .find(|t| t.template_id == CLEAVING_15_COMMON)
            .expect("Challenge_DealDamage_Cleaving_15_Common");
        assert_eq!(cleave.objective.quota, 25500.0);
        assert_eq!(cleave.reward.currencies.get(&GOLD), Some(&4000));
        assert_eq!(
            (cleave.required_player_level, cleave.locked_at_player_level),
            (76, 0)
        );
        assert!(!cleave.premium());
    }

    /// #406: LLoyd's held set on prod (2026-10-17). The zero-progress Skeever
    /// hunt and level-1-10 critical hits are retired at level 65; a challenge
    /// with progress, or with a template the pool no longer has, is kept.
    #[test]
    fn report_406_retires_zero_progress_challenges_outside_the_band() {
        let pool = shipped_pool();
        let held = |tid: u128, progress: f64| {
            let t = pool
                .iter()
                .find(|t| t.template_id == Uuid::from_u128(tid))
                .unwrap();
            let mut c =
                ChallengeInstance::from_template(t, Uuid::from_u128(9), Uuid::from_u128(9), 0, 0);
            c.progress = progress;
            c
        };
        let skeever = held(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e, 0.0);
        assert!(retire_out_of_band(
            &skeever,
            &pool,
            LLOYD_LEVEL,
            &serde_json::Value::Null
        ));
        assert!(
            !retire_out_of_band(&skeever, &pool, 17, &serde_json::Value::Null),
            "in band at 17"
        );
        assert!(retire_out_of_band(
            &held(0x8925740e_046b_4e7a_b418_a17bd83e035c, 0.0),
            &pool,
            LLOYD_LEVEL,
            &serde_json::Value::Null
        ));
        assert!(
            !retire_out_of_band(
                &held(0x48efabe0_2138_42f6_9aa7_1d53a4751a1e, 3.0),
                &pool,
                LLOYD_LEVEL,
                &serde_json::Value::Null
            ),
            "progress is kept"
        );
        let mut unknown = skeever.clone();
        unknown.template_id = Uuid::from_u128(0xdead);
        assert!(
            !retire_out_of_band(&unknown, &pool, LLOYD_LEVEL, &serde_json::Value::Null),
            "no template, no judgement"
        );
    }

    const TUTORIAL_CRAFT_DECORATION: Uuid = Uuid::from_u128(0xbce21d12_8f3b_444b_ad1f_740478f1d0cf);

    /// #426: every template the shipped pool names `Challenge_TUTORIAL_*` is in
    /// [`ONE_TIME_ONLY_TEMPLATES`] — the APK sets `_oneTimeOnly` on exactly those 15.
    #[test]
    fn report_426_every_tutorial_template_in_the_pool_is_one_time_only() {
        let raw: Vec<serde_json::Value> = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../deploy/static/challenges.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let tutorials: Vec<Uuid> = raw
            .iter()
            .filter(|t| {
                t["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("Challenge_TUTORIAL_"))
            })
            .map(|t| t["templateId"].as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(
            tutorials,
            vec![TUTORIAL_CRAFT_DECORATION],
            "the pool's one tutorial challenge"
        );
        assert_eq!(ONE_TIME_ONLY_TEMPLATES.len(), 15);
        for id in &tutorials {
            assert!(ONE_TIME_ONLY_TEMPLATES.contains(id), "{id}");
        }
        let pool = shipped_pool();
        let one_time = pool.iter().filter(|t| t.one_time_only()).count();
        assert_eq!(one_time, 1, "CONTROL: nothing else in the pool is one-time");
    }

    /// #426: `Challenge_TUTORIAL_CraftDecoration` (required/locked level 0/0) was
    /// offered at every level, every rotation, so a level-77 character could be
    /// handed it again and again. It is a one-time tutorial challenge the client
    /// cannot cancel; it is never offered.
    #[test]
    fn report_426_the_tutorial_decoration_challenge_is_never_offered() {
        let pool = shipped_pool();
        let tutorial = pool
            .iter()
            .find(|t| t.template_id == TUTORIAL_CRAFT_DECORATION)
            .expect("in the pool");
        assert_eq!(
            (
                tutorial.required_player_level,
                tutorial.locked_at_player_level
            ),
            (0, 0)
        );
        for level in 0..=100u16 {
            assert!(!tutorial.offered_at(level), "offered at {level}");
            for seed in 0..20usize {
                let (picked, _) = pick_templates(
                    &pool,
                    seed,
                    level,
                    &serde_json::Value::Null,
                    &[],
                    ACTIVE_CHALLENGE_COUNT,
                );
                assert!(
                    picked
                        .iter()
                        .all(|&i| pool[i].template_id != TUTORIAL_CRAFT_DECORATION),
                    "picked at level {level} seed {seed}"
                );
            }
        }
    }

    /// #426: a held tutorial challenge the player has made no progress on is
    /// retired on the next list call (the client cannot cancel it); one with
    /// progress is kept, so nothing earned is taken away.
    #[test]
    fn report_426_a_held_tutorial_challenge_is_retired_unless_it_has_progress() {
        let pool = shipped_pool();
        let t = pool
            .iter()
            .find(|t| t.template_id == TUTORIAL_CRAFT_DECORATION)
            .unwrap();
        let mut held =
            ChallengeInstance::from_template(t, Uuid::from_u128(9), Uuid::from_u128(9), 0, 0);
        assert!(
            retire_out_of_band(&held, &pool, 77, &serde_json::Value::Null),
            "Dwalin, level 77"
        );
        assert!(
            retire_out_of_band(&held, &pool, 1, &serde_json::Value::Null),
            "and at level 1"
        );
        held.progress = 1.0;
        assert!(
            !retire_out_of_band(&held, &pool, 77, &serde_json::Value::Null),
            "progress is kept"
        );
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
