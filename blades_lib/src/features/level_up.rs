//! The level-up ladder: what a level costs, what it pays, and what the client is
//! told about it.
//!
//! MEASURED against 30 consecutive retail level-ups (levels 1→31) in Yumeko's
//! capture corpus, extracted by `script/extract_journey_fixtures.py` into
//! `deploy/retail-journey/levelups.json`. All 30 agree on both halves of the
//! contract below, and the tests at the bottom of this file replay every one of
//! them.
//!
//! **Experience is spent, not merely accumulated.** Retail deducts
//! `level_rewards[newLevel].xp_to_reach` from `character.experience` and leaves
//! the remainder. Level 1→2 with 57 xp banked answers with `experience: 7`,
//! because reaching level 2 costs 50. We used to only ever increment `level`, so
//! experience grew without bound and the client's xp bar — which reads the
//! remainder against the next threshold — drifted further from the truth with
//! every level, while nothing stopped a client asking for a hundred levels in a
//! row.
//!
//! **The wallet is a receipt, not a statement.** Retail answers `/levelup` with
//! a `wallet` key only when that level credited a currency, and then lists only
//! the currency it credited — gold at 2, 4, 9, 17 and 25, gems at 3, 7, 10, 13,
//! 18, 23 and 29, and no wallet key at all on the other 18. Sending the full
//! purse every time is the same class of mistake as the `shop` key `complete`
//! used to send: extra keys the client did not ask for, describing state it did
//! not change.

use serde::Deserialize;
use std::collections::HashMap;
use uuid::Uuid;

use crate::economy::{GEMS, GOLD, RewardGrant};
use crate::features::character_ops::{Attribute, MAX_ATTRIBUTE_POINT_LEVEL, apply_levelup};
use crate::user_data::CompleteCharacter;

#[derive(Deserialize, Clone, Debug)]
pub struct LevelItemReward {
    pub template_id: String,
    pub quantity: u32,
}

#[derive(Deserialize, Clone, Debug)]
pub struct LevelReward {
    pub xp_to_reach: u32,
    pub skill_points: u32,
    pub gold_reward: u32,
    pub gems_reward: u32,
    pub attribute_points: u32,
    pub health_bonus: u32,
    pub reset_cost: u32,
    #[serde(default)]
    pub items: Vec<LevelItemReward>,
}

#[derive(Default, Clone, Debug)]
pub struct LevelUpData {
    pub rewards: HashMap<u32, LevelReward>,
}

impl LevelUpData {
    pub fn from_json(value: &serde_json::Value) -> Self {
        match serde_json::from_value::<HashMap<u32, LevelReward>>(value.clone()) {
            Ok(rewards) => {
                LevelUpData { rewards }
            }
            Err(_e) => {
                LevelUpData::default()
            }
        }
    }

    pub fn get_reward(&self, level: u32) -> Option<&LevelReward> {
        self.rewards.get(&level)
    }
}


/// Why a level-up was refused.
///
/// Both arms are 400s at the wire: the client should not have offered the button.
#[derive(Debug, PartialEq, Eq)]
pub enum LevelUpRefusal {
    /// The character is at the top of the table (retail's cap is 100).
    AlreadyMaxLevel { level: u16 },
    /// Not enough banked experience for the next level's threshold.
    NotEnoughExperience { have: u64, need: u64 },
    /// The static table has no entry for the next level, so its price is unknown.
    ///
    /// Distinct from `AlreadyMaxLevel`: this is missing data, not a cap, and it
    /// is refused rather than waved through because charging nothing would be a
    /// silent free level.
    UnknownLevel { level: u16 },
}

/// What a granted level actually did, so the handler can build retail's response
/// without re-deriving any of it.
#[derive(Debug, Default)]
pub struct LevelUpOutcome {
    pub new_level: u16,
    pub experience_spent: u64,
    /// The currencies this level credited, in the order the wire lists them.
    /// Empty on the 18-of-30 levels that pay nothing — and an empty list is what
    /// tells the handler to omit the `wallet` key entirely.
    pub credited_currencies: Vec<Uuid>,
    pub reward: RewardGrant,
}

impl LevelUpData {
    /// The experience the next level costs, or `None` when the table is silent.
    pub fn cost_of_next_level(&self, current_level: u16) -> Option<u64> {
        self.get_reward(u32::from(current_level) + 1)
            .map(|r| u64::from(r.xp_to_reach))
    }

    /// The highest level the table describes — retail's 100.
    pub fn max_level(&self) -> u16 {
        self.rewards.keys().copied().max().unwrap_or(0).min(u32::from(u16::MAX)) as u16
    }
}

/// Decide a level-up without touching the character: can it happen, and what does
/// it cost and pay?
///
/// Separated from [`apply_level_up`] so the refusal can be tested on its own and
/// so the handler can answer 400 before it has written anything.
pub fn plan_level_up(
    level: u16,
    experience: u64,
    data: &LevelUpData,
) -> Result<(u16, u64, RewardGrant, Vec<Uuid>), LevelUpRefusal> {
    let next = level.saturating_add(1);
    if level >= data.max_level() {
        return Err(LevelUpRefusal::AlreadyMaxLevel { level });
    }
    let reward = data
        .get_reward(u32::from(next))
        .ok_or(LevelUpRefusal::UnknownLevel { level: next })?;
    let need = u64::from(reward.xp_to_reach);
    if experience < need {
        return Err(LevelUpRefusal::NotEnoughExperience {
            have: experience,
            need,
        });
    }

    // Gold before gems: the two are never credited by the same level in the
    // corpus, so the order is cosmetic, but a stable one keeps the fixture
    // replay from depending on HashMap iteration.
    let mut credited = Vec::new();
    let mut grant = RewardGrant::default();
    if reward.gold_reward > 0 {
        grant.currencies.insert(GOLD, u64::from(reward.gold_reward));
        credited.push(GOLD);
    }
    if reward.gems_reward > 0 {
        grant.currencies.insert(GEMS, u64::from(reward.gems_reward));
        credited.push(GEMS);
    }
    for item in &reward.items {
        if let Ok(template) = Uuid::parse_str(&item.template_id) {
            grant
                .stackable_items
                .insert(template, u64::from(item.quantity));
        }
    }
    Ok((next, need, grant, credited))
}

/// Raise the character a level: spend the experience, bump the attribute, and
/// report what to credit.
///
/// The character is left untouched on refusal, so a caller that ignores the error
/// still cannot hand out a free level.
pub fn apply_level_up(
    ch: &mut CompleteCharacter,
    attribute: Attribute,
    data: &LevelUpData,
) -> Result<LevelUpOutcome, LevelUpRefusal> {
    let (new_level, spent, reward, credited) = plan_level_up(ch.level, ch.experience, data)?;
    // The level, the attribute point and its level-50 cap are one rule and live in
    // one place; this adds only the part that rule never had — the price.
    apply_levelup(ch, attribute);
    debug_assert_eq!(ch.level, new_level);
    ch.experience = ch.experience.saturating_sub(spent);
    Ok(LevelUpOutcome {
        new_level,
        experience_spent: spent,
        credited_currencies: credited,
        reward,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The shipped `level_rewards.json` — the same file the server loads at boot.
    fn table() -> LevelUpData {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/level_rewards.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let json: Value = serde_json::from_str(&raw).expect("valid level_rewards.json");
        let data = LevelUpData::from_json(&json);
        assert!(
            !data.rewards.is_empty(),
            "level_rewards.json parsed to nothing — from_json swallows shape drift, \
             so an empty table here means the file changed shape, not that it is empty"
        );
        data
    }

    /// Retail level-ups, verbatim. See `script/extract_journey_fixtures.py`.
    fn retail_levelups() -> Vec<Value> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/retail-journey/levelups.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let json: Value = serde_json::from_str(&raw).expect("valid levelups.json");
        let obs = json["observations"].as_array().cloned().unwrap_or_default();
        assert!(
            obs.len() >= 25,
            "only {} retail level-ups in the fixture — the corpus had 30; a shrunken \
             fixture would let these tests pass by having nothing to check",
            obs.len()
        );
        obs
    }

    fn at(level: u16, experience: u64) -> CompleteCharacter {
        let mut ch = CompleteCharacter::default();
        ch.level = level;
        ch.experience = experience;
        ch
    }

    /// THE measurement: every captured level-up, replayed.
    ///
    /// Each observation carries the character's experience on both sides of the
    /// call, so this asserts the exact remainder retail left — not merely that
    /// *something* was deducted. Before the fix this failed on all 30 with the
    /// experience untouched.
    #[test]
    fn every_retail_levelup_leaves_the_experience_retail_left() {
        let data = table();
        let mut checked = 0;
        for o in retail_levelups() {
            let before = o["levelBefore"].as_u64().unwrap() as u16;
            let after = o["levelAfter"].as_u64().unwrap() as u16;
            let xp_before = o["experienceBefore"].as_u64().unwrap();
            let xp_after = o["experienceAfter"].as_u64().unwrap();
            let attribute = match o["attribute"].as_str() {
                Some(a) => Attribute::parse(a).unwrap(),
                None => Attribute::Stamina,
            };

            let mut ch = at(before, xp_before);
            let outcome = apply_level_up(&mut ch, attribute, &data)
                .unwrap_or_else(|e| panic!("retail granted L{before}->{after}, we refused: {e:?}"));

            assert_eq!(ch.level, after, "level after L{before}->{after}");
            assert_eq!(
                ch.experience, xp_after,
                "experience after L{before}->{after}: retail left {xp_after}, \
                 we left {} (spent {})",
                ch.experience, outcome.experience_spent
            );
            checked += 1;
        }
        assert_eq!(checked, retail_levelups().len());
    }

    /// The wallet key is a receipt for what was credited, and retail omits it
    /// when nothing was. 18 of the 30 captured level-ups carry no wallet at all.
    #[test]
    fn the_credited_currencies_match_what_retail_put_in_the_wallet() {
        let data = table();
        let mut with_wallet = 0;
        let mut without = 0;
        for o in retail_levelups() {
            let before = o["levelBefore"].as_u64().unwrap() as u16;
            let xp_before = o["experienceBefore"].as_u64().unwrap();
            let retail: Vec<String> = o["walletCurrencyIds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();

            let mut ch = at(before, xp_before);
            let outcome = apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap();
            let ours: Vec<String> = outcome
                .credited_currencies
                .iter()
                .map(|u| u.to_string())
                .collect();

            assert_eq!(
                ours, retail,
                "level {} credited {retail:?} at retail, {ours:?} here",
                outcome.new_level
            );
            if retail.is_empty() {
                without += 1;
            } else {
                with_wallet += 1;
            }
        }
        assert!(
            with_wallet > 0 && without > 0,
            "the fixture must contain both paying and non-paying levels \
             ({with_wallet} paying, {without} silent) or this proves nothing"
        );
    }

    /// The gate. A client that asks for a level it has not earned is refused and
    /// the character is left exactly as it was.
    #[test]
    fn a_level_that_has_not_been_earned_is_refused_and_changes_nothing() {
        let data = table();
        let need = data.cost_of_next_level(1).expect("level 2 is in the table");
        let mut ch = at(1, need - 1);
        let before = (ch.level, ch.experience, ch.version);

        let err = apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap_err();
        assert_eq!(
            err,
            LevelUpRefusal::NotEnoughExperience {
                have: need - 1,
                need
            }
        );
        assert_eq!(
            (ch.level, ch.experience, ch.version),
            before,
            "a refused level-up must not touch the character"
        );

        // …and exactly one more point of experience makes it legal, which is what
        // proves the boundary is at `need` and not somewhere convenient.
        let mut ch = at(1, need);
        apply_level_up(&mut ch, Attribute::Stamina, &data).expect("earned");
        assert_eq!(ch.level, 2);
        assert_eq!(ch.experience, 0);
    }

    /// Retail's table stops at 100. Past it there is no threshold to charge, so a
    /// level-up must fail rather than be free.
    #[test]
    fn the_top_of_the_table_is_the_cap() {
        let data = table();
        let max = data.max_level();
        assert_eq!(max, 100, "retail's MAXIMUM_PLAYER_LEVEL");

        let mut ch = at(max, u64::MAX / 2);
        assert_eq!(
            apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap_err(),
            LevelUpRefusal::AlreadyMaxLevel { level: max }
        );
        assert_eq!(ch.level, max, "capped, not incremented");
    }

    /// Levelling from 1 to the cap must consume exactly the table's cumulative
    /// cost — a walk that would expose an off-by-one in which level's threshold
    /// is charged (charging `xp_to_reach[level]` instead of `[level + 1]` is the
    /// natural mistake and passes a single-step test).
    #[test]
    fn the_whole_ladder_costs_the_tables_cumulative_total() {
        let data = table();
        let max = data.max_level();
        let expected: u64 = (2..=max)
            .map(|l| u64::from(data.get_reward(u32::from(l)).unwrap().xp_to_reach))
            .sum();

        let mut ch = at(1, expected);
        let mut steps = 0;
        while apply_level_up(&mut ch, Attribute::Stamina, &data).is_ok() {
            steps += 1;
        }
        assert_eq!(steps, usize::from(max - 1), "1 → {max}");
        assert_eq!(ch.level, max);
        assert_eq!(ch.experience, 0, "exact change, no rounding slack");
    }

    /// Attribute points still stop at 50 while levels keep going — the pre-existing
    /// rule, re-checked here because `apply_level_up` now owns that code path.
    #[test]
    fn attribute_points_still_stop_at_fifty() {
        let data = table();
        let mut ch = at(MAX_ATTRIBUTE_POINT_LEVEL, u64::MAX / 2);
        ch.experience = data
            .cost_of_next_level(MAX_ATTRIBUTE_POINT_LEVEL)
            .unwrap();
        let points = ch.stamina_attribute_points;
        apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap();
        assert_eq!(ch.level, MAX_ATTRIBUTE_POINT_LEVEL + 1);
        assert_eq!(
            ch.stamina_attribute_points, points,
            "no attribute point past level {MAX_ATTRIBUTE_POINT_LEVEL}"
        );
    }
}
