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
            Ok(rewards) => LevelUpData { rewards },
            Err(_e) => LevelUpData::default(),
        }
    }

    pub fn get_reward(&self, level: u32) -> Option<&LevelReward> {
        self.rewards.get(&level)
    }
}

/// Retail's top level: the last row of `level_rewards.json` and the client's own
/// `PlayerStats.MAXIMUM_PLAYER_LEVEL`. A stock client's level table
/// (`PlayerStatsData._perLevelData` in `duplicates_sab`) ends here, and a
/// character above it hangs that client on load.
pub const RETAIL_MAX_LEVEL: u16 = 100;

/// The highest level the past-100 extension is defined for (#425): the level the
/// client data pack's table runs to. A server cap above it would hand a client a
/// level its table does not hold, which is the hang above.
pub const EXTENDED_MAX_LEVEL: u16 = 150;

/// How many retail levels the extension's rewards cycle through: level `n` past
/// 100 pays what level `n - 10` paid, so 101-110 repeat 91-100.
const REWARD_CYCLE: u32 = 10;

/// The server's level cap from `ARENA_MAX_PLAYER_LEVEL` (#425).
///
/// Unset, empty or unparseable means retail's 100, so the extension stays off
/// until the operator flips it — which must wait until the client pack that
/// extends the client's table is in production, or every player without it hangs
/// on load the moment one of their characters passes 100. Values are clamped to
/// `RETAIL_MAX_LEVEL..=EXTENDED_MAX_LEVEL`: lowering the cap below 100 would
/// strand existing level-100 characters above it, and raising it past 150 would
/// outrun the client table.
pub fn max_level_from_env(value: Option<&str>) -> u16 {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => RETAIL_MAX_LEVEL,
        Some(v) => match v.parse::<u16>() {
            Ok(n) => n.clamp(RETAIL_MAX_LEVEL, EXTENDED_MAX_LEVEL),
            Err(_) => RETAIL_MAX_LEVEL,
        },
    }
}

impl LevelUpData {
    /// The table with its top at `max_level` (#425, owner decision d27).
    ///
    /// At or below the table's own top this only drops rows above `max_level`.
    /// Above it, rows are added by continuing retail's ladder:
    ///
    /// - **Cost.** `xp_to_reach(n) = xp_to_reach(top) + step * (n - top)`, where
    ///   `step = xp_to_reach(top) - xp_to_reach(top - 1)` — retail's last
    ///   increment (44,316 at 99→100), held constant. Retail's own increments
    ///   shrink by exactly 75 per level from 51 to 100 (a cubic that peaks at
    ///   81); continuing THAT rule turns the increment negative near 120 and
    ///   levels would get cheaper, so the last increment is the continuation that
    ///   stays monotonic. 101 costs 2,638,269; 150 costs 4,809,753.
    /// - **Rewards.** Level `n` copies level `n - 10`'s row (gold, gems, items,
    ///   and the zero skill/attribute/health of every level past 50), so 101-110
    ///   pay what 91-100 paid. The client pack carries the same rewards, so the
    ///   level-up card shows what the server credits.
    ///
    /// An empty table stays empty: the extension continues retail's data and has
    /// nothing to continue without it.
    pub fn with_max_level(mut self, max_level: u16) -> Self {
        let max = u32::from(max_level);
        self.rewards.retain(|level, _| *level <= max);
        let top = self.rewards.keys().copied().max().unwrap_or(0);
        if top >= max || top <= REWARD_CYCLE {
            return self;
        }
        let (Some(last), Some(prev)) = (self.get_reward(top), self.get_reward(top - 1)) else {
            return self;
        };
        let base = last.xp_to_reach;
        let step = last.xp_to_reach.saturating_sub(prev.xp_to_reach);
        for n in (top + 1)..=max {
            let Some(mut row) = self.get_reward(n - REWARD_CYCLE).cloned() else {
                break;
            };
            row.xp_to_reach = base.saturating_add(step.saturating_mul(n - top));
            self.rewards.insert(n, row);
        }
        self
    }
}

/// Why a level-up was refused.
///
/// Both arms are 400s at the wire: the client should not have offered the button.
#[derive(Debug, PartialEq, Eq)]
pub enum LevelUpRefusal {
    /// The character is at the top of the table (retail's cap is 100; the
    /// server's is `ARENA_MAX_PLAYER_LEVEL`, see [`max_level_from_env`]).
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
        self.rewards
            .keys()
            .copied()
            .max()
            .unwrap_or(0)
            .min(u32::from(u16::MAX)) as u16
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
    // No table, no gate. `LevelUpData::from_json` returns an empty table on any
    // shape drift, and `static_loader` treats every file as optional, so this is
    // reachable on a server started without static data — where `max_level()` is
    // 0 and a naive `level >= max` would refuse EVERY level-up, at every level,
    // permanently. A progression system that fails closed on a missing file turns
    // one absent table into an unplayable game; the old ungated behaviour is the
    // right thing to degrade to.
    if data.rewards.is_empty() {
        return Ok((next, 0, RewardGrant::default(), Vec::new()));
    }
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

/// Why [`set_level`] refused.
#[derive(Debug, PartialEq, Eq)]
pub enum SetLevelRefusal {
    /// Outside `1..=max_level()`. Also what an empty table answers: with no
    /// ladder there is no level that can be set consistently.
    OutOfRange { level: u16, max: u16 },
}

/// What [`set_level`] did, for the caller's audit record.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LevelSet {
    pub from_level: u16,
    pub to_level: u16,
    pub from_experience: u64,
    pub stamina_attribute_points: u32,
    pub magicka_attribute_points: u32,
    /// Learned and equipped abilities were cleared because the new level holds
    /// fewer skill points than were available at the old one.
    pub abilities_reset: bool,
}

impl LevelUpData {
    /// Skill points earned on the way to `level`: the sum of every level's
    /// `skill_points` from 2 up to it (level 1 is where a character starts, not a
    /// level it earns).
    pub fn skill_points_through(&self, level: u16) -> u64 {
        (2..=u32::from(level))
            .filter_map(|l| self.get_reward(l))
            .map(|r| u64::from(r.skill_points))
            .sum()
    }
}

/// Put a character at `target` as if it had earned every level up to it — for
/// operator TEST characters only (the server's `testchar` module). Never a
/// player path: it bypasses the experience gate on purpose.
///
/// "As if earned" means the state [`apply_level_up`] leaves behind, level by
/// level, without the rewards:
///
/// - `level` = `target`, `experience` = 0: the start of the level, which is what
///   a level-up leaves when the banked experience exactly paid for it.
/// - Attribute points total `min(target, 50) - 1` — the retail identity
///   [`apply_levelup`] reproduces — split in the character's current
///   stamina:magicka proportion (all stamina when it had none).
/// - Abilities are kept when the level holds at least as many skill points as
///   before. When it holds fewer (a lower level), the learned and equipped
///   abilities are cleared exactly as a respec clears them, so the client never
///   sees more points spent than the level has; the player re-learns them.
/// - `globalShopOffers` is emptied: a level-up offer belongs to a level-up, and
///   there was none.
///
/// NOT paid: the gold, gems and items each level credits. A test character's
/// purse should not grow by setting its level, and the loot being tuned on it
/// must not be confused with level-up payouts.
pub fn set_level(
    ch: &mut CompleteCharacter,
    target: u16,
    data: &LevelUpData,
) -> Result<LevelSet, SetLevelRefusal> {
    let max = data.max_level();
    if target == 0 || target > max {
        return Err(SetLevelRefusal::OutOfRange { level: target, max });
    }
    let from_level = ch.level;
    let from_experience = ch.experience;

    let total = u32::from(target.min(MAX_ATTRIBUTE_POINT_LEVEL)).saturating_sub(1);
    let (stamina, magicka) = (ch.stamina_attribute_points, ch.magicka_attribute_points);
    let current = u64::from(stamina) + u64::from(magicka);
    let magicka = if current == 0 {
        0
    } else {
        (u64::from(total) * u64::from(magicka) / current) as u32
    };
    ch.magicka_attribute_points = magicka;
    ch.stamina_attribute_points = total - magicka;

    let abilities_reset = data.skill_points_through(target) < data.skill_points_through(from_level);
    if abilities_reset {
        ch.abilities = serde_json::Value::Null;
        ch.equipped_abilities = serde_json::Value::Null;
    }

    ch.level = target;
    ch.experience = 0;
    ch.global_shop_offers = serde_json::json!([]);
    ch.version += 1;

    Ok(LevelSet {
        from_level,
        to_level: target,
        from_experience,
        stamina_attribute_points: ch.stamina_attribute_points,
        magicka_attribute_points: ch.magicka_attribute_points,
        abilities_reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::character_ops::MAX_ATTRIBUTE_POINT_LEVEL;
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
        ch.experience = data.cost_of_next_level(MAX_ATTRIBUTE_POINT_LEVEL).unwrap();
        let points = ch.stamina_attribute_points;
        apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap();
        assert_eq!(ch.level, MAX_ATTRIBUTE_POINT_LEVEL + 1);
        assert_eq!(
            ch.stamina_attribute_points, points,
            "no attribute point past level {MAX_ATTRIBUTE_POINT_LEVEL}"
        );
    }

    /// A server started without static data must still let players level.
    ///
    /// `static_loader` treats every file as optional and `LevelUpData::from_json`
    /// swallows shape drift into an empty table, so this is a real state — and a
    /// gate that reads `level >= max_level()` against an empty table refuses every
    /// level-up at every level, forever. One missing file would have made the game
    /// unplayable rather than merely unpriced.
    #[test]
    fn without_the_table_levelling_is_unpriced_rather_than_impossible() {
        let none = LevelUpData::default();
        let mut ch = at(7, 0);
        let outcome = apply_level_up(&mut ch, Attribute::Stamina, &none)
            .expect("no table must mean no gate, not a permanent refusal");
        assert_eq!(ch.level, 8, "the level still advances");
        assert_eq!(ch.experience, 0, "nothing to charge, so nothing is charged");
        assert!(
            outcome.credited_currencies.is_empty(),
            "and nothing is invented to pay out"
        );

        // …and it keeps working, rather than blocking at some arbitrary ceiling.
        for _ in 0..50 {
            apply_level_up(&mut ch, Attribute::Stamina, &none).expect("still ungated");
        }
        assert_eq!(ch.level, 58);
    }
    // ── levels past 100 (#425) ──────────────────────────────────────────────

    /// THE GATE: without `ARENA_MAX_PLAYER_LEVEL` the server is exactly retail —
    /// 100 is the cap and the table is byte-for-byte the shipped one. A client
    /// without the pack hangs at 101, so nothing may change until it is flipped.
    #[test]
    fn unset_the_cap_stays_retails_100_and_the_table_is_untouched() {
        for unset in [None, Some(""), Some("  "), Some("banana"), Some("-5")] {
            assert_eq!(max_level_from_env(unset), RETAIL_MAX_LEVEL, "{unset:?}");
        }
        assert_eq!(max_level_from_env(Some("100")), 100);
        let stock = table();
        let gated = table().with_max_level(max_level_from_env(None));
        assert_eq!(gated.max_level(), 100);
        assert_eq!(gated.rewards.len(), stock.rewards.len());
        for (level, row) in &stock.rewards {
            let g = &gated.rewards[level];
            assert_eq!(
                (g.xp_to_reach, g.gold_reward, g.gems_reward, g.items.len()),
                (
                    row.xp_to_reach,
                    row.gold_reward,
                    row.gems_reward,
                    row.items.len()
                ),
                "level {level} changed with the extension off"
            );
        }
        let mut ch = at(100, 76_163_908); // the most XP a prod level-100 holds
        assert_eq!(
            apply_level_up(&mut ch, Attribute::Stamina, &gated).unwrap_err(),
            LevelUpRefusal::AlreadyMaxLevel { level: 100 }
        );
    }

    /// The cap clamps to the range the client pack covers: below 100 would strand
    /// level-100 characters, above 150 outruns the client's table.
    #[test]
    fn the_cap_is_clamped_to_the_packs_range() {
        assert_eq!(max_level_from_env(Some("150")), 150);
        assert_eq!(max_level_from_env(Some("120")), 120);
        assert_eq!(max_level_from_env(Some("50")), RETAIL_MAX_LEVEL);
        assert_eq!(max_level_from_env(Some("200")), EXTENDED_MAX_LEVEL);
        assert_eq!(table().with_max_level(120).max_level(), 120);
    }

    /// The pinned numbers. The client pack's rows are generated by the same rule
    /// (blades-capture `tools/bundle-reskin/patch_player_levels.py`), and these
    /// values are what it writes — if either side changes, the level-up bar and
    /// the server's charge disagree.
    #[test]
    fn the_extension_continues_retails_last_increment() {
        let data = table().with_max_level(EXTENDED_MAX_LEVEL);
        let xp = |l: u32| data.get_reward(l).unwrap().xp_to_reach;
        assert_eq!(xp(99), 2_549_637, "retail");
        assert_eq!(xp(100), 2_593_953, "retail");
        assert_eq!(xp(101), 2_638_269);
        assert_eq!(xp(110), 3_037_113);
        assert_eq!(xp(150), 4_809_753);
        assert_eq!(data.max_level(), 150);
        assert!(data.get_reward(151).is_none());
        // Inside the client's float and its MAXIMUM_EXPERIENCE (9,999,999) clamp.
        assert!(xp(150) < 9_999_999);
    }

    #[test]
    fn thresholds_are_monotonic_all_the_way_to_the_cap() {
        let data = table().with_max_level(EXTENDED_MAX_LEVEL);
        let mut prev = 0;
        for l in 2..=u32::from(EXTENDED_MAX_LEVEL) {
            let x = data
                .get_reward(l)
                .unwrap_or_else(|| panic!("no row {l}"))
                .xp_to_reach;
            assert!(x > prev, "level {l} costs {x}, not more than {prev}");
            prev = x;
        }
    }

    /// Rewards past 100 repeat 91-100, and no level past 50 grants skill,
    /// attribute or health points — retail's own rule for 51-100.
    #[test]
    fn rewards_past_100_cycle_the_last_ten_retail_levels() {
        let data = table().with_max_level(EXTENDED_MAX_LEVEL);
        for n in 101..=u32::from(EXTENDED_MAX_LEVEL) {
            let row = data.get_reward(n).unwrap();
            let src = data.get_reward(91 + (n - 101) % 10).unwrap();
            assert_eq!(
                (row.gold_reward, row.gems_reward, row.reset_cost),
                (src.gold_reward, src.gems_reward, src.reset_cost),
                "level {n}"
            );
            let ids = |r: &LevelReward| {
                r.items
                    .iter()
                    .map(|i| (i.template_id.clone(), i.quantity))
                    .collect::<Vec<_>>()
            };
            assert_eq!(ids(row), ids(src), "items at level {n}");
            assert_eq!(
                (row.skill_points, row.attribute_points, row.health_bonus),
                (0, 0, 0),
                "level {n}"
            );
        }
    }

    /// Level-up past 100 works with the gate open, charges the extended threshold
    /// and pays the cycled reward; the new top refuses.
    #[test]
    fn levelling_past_100_works_once_the_cap_is_raised() {
        let data = table().with_max_level(EXTENDED_MAX_LEVEL);
        let need = data.cost_of_next_level(100).unwrap();
        let mut ch = at(100, need + 7);
        let points = ch.stamina_attribute_points;
        let out = apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap();
        assert_eq!((ch.level, ch.experience), (101, 7));
        assert_eq!(out.experience_spent, 2_638_269);
        assert_eq!(
            ch.stamina_attribute_points, points,
            "no attribute point past 50"
        );
        // 101 pays what 91 paid (items, no currency).
        assert!(out.credited_currencies.is_empty());
        assert_eq!(
            out.reward.stackable_items.len(),
            data.get_reward(91).unwrap().items.len()
        );

        // One short of the threshold is refused; the walk stops exactly at 150.
        let mut ch = at(120, data.cost_of_next_level(120).unwrap() - 1);
        assert!(matches!(
            apply_level_up(&mut ch, Attribute::Stamina, &data),
            Err(LevelUpRefusal::NotEnoughExperience { .. })
        ));
        let total: u64 = (101..=150u32)
            .map(|l| u64::from(data.get_reward(l).unwrap().xp_to_reach))
            .sum();
        let mut ch = at(100, total);
        while apply_level_up(&mut ch, Attribute::Stamina, &data).is_ok() {}
        assert_eq!((ch.level, ch.experience), (150, 0));
        assert_eq!(
            apply_level_up(&mut ch, Attribute::Stamina, &data).unwrap_err(),
            LevelUpRefusal::AlreadyMaxLevel { level: 150 }
        );
    }

    /// Operator test characters can be set past 100 only with the gate open —
    /// how a level-101 character is made for the client test.
    #[test]
    fn set_level_follows_the_cap() {
        let mut ch = at(100, 0);
        assert!(set_level(&mut ch, 101, &table()).is_err());
        let open = table().with_max_level(EXTENDED_MAX_LEVEL);
        set_level(&mut ch, 101, &open).unwrap();
        assert_eq!((ch.level, ch.stamina_attribute_points), (101, 49));
    }

    #[test]
    fn an_empty_table_is_not_extended() {
        assert!(
            LevelUpData::default()
                .with_max_level(150)
                .rewards
                .is_empty()
        );
    }

    // ── set_level (operator test characters) ────────────────────────────────

    /// THE CONTROL: levelling one step at a time through `apply_level_up` and
    /// setting the level directly must land on the same attribute total, level and
    /// (for a just-paid level) zero experience. If they disagree, `set_level` is
    /// inventing a character the real path could never produce.
    #[test]
    fn set_level_lands_where_levelling_up_would() {
        let data = table();
        for target in [2u16, 10, 48, 50, 51, 77, 100] {
            let mut walked = at(1, 0);
            while walked.level < target {
                walked.experience = data.cost_of_next_level(walked.level).unwrap();
                apply_level_up(&mut walked, Attribute::Stamina, &data).unwrap();
            }
            let mut set = at(1, 0);
            set_level(&mut set, target, &data).unwrap();
            assert_eq!(set.level, walked.level, "level at {target}");
            assert_eq!(set.experience, walked.experience, "experience at {target}");
            assert_eq!(
                set.stamina_attribute_points + set.magicka_attribute_points,
                walked.stamina_attribute_points + walked.magicka_attribute_points,
                "attribute total at {target}"
            );
        }
    }

    #[test]
    fn set_level_keeps_the_attribute_split_and_abilities_when_raising() {
        let data = table();
        let mut ch = at(48, 1234);
        ch.stamina_attribute_points = 30;
        ch.magicka_attribute_points = 17;
        ch.abilities = serde_json::json!({"a": 1});
        ch.equipped_abilities = serde_json::json!({"0": "a"});
        let before_version = ch.version;
        let done = set_level(&mut ch, 80, &data).unwrap();
        assert_eq!((ch.level, ch.experience), (80, 0));
        assert_eq!(
            ch.stamina_attribute_points + ch.magicka_attribute_points,
            49
        );
        assert!(
            ch.magicka_attribute_points >= 17,
            "the split follows the old proportion"
        );
        assert!(!done.abilities_reset);
        assert_eq!(
            ch.abilities,
            serde_json::json!({"a": 1}),
            "abilities survive a raise"
        );
        assert_eq!(ch.global_shop_offers, serde_json::json!([]));
        assert!(
            ch.version > before_version,
            "the client must see a new version"
        );
        assert_eq!((done.from_level, done.from_experience), (48, 1234));
    }

    #[test]
    fn set_level_clears_abilities_only_when_the_level_holds_fewer_skill_points() {
        let data = table();
        let mut ch = at(48, 0);
        ch.stamina_attribute_points = 47;
        ch.abilities = serde_json::json!({"a": 3});
        ch.equipped_abilities = serde_json::json!({"0": "a"});
        let done = set_level(&mut ch, 20, &data).unwrap();
        assert!(done.abilities_reset);
        assert!(ch.abilities.is_null() && ch.equipped_abilities.is_null());
        assert_eq!(ch.stamina_attribute_points, 19);

        // 60 -> 55: both past the last skill point (level 50), nothing to take back.
        let mut ch = at(60, 0);
        ch.abilities = serde_json::json!({"a": 3});
        assert!(!set_level(&mut ch, 55, &data).unwrap().abilities_reset);
        assert_eq!(ch.abilities, serde_json::json!({"a": 3}));
    }

    #[test]
    fn set_level_refuses_outside_the_ladder_and_changes_nothing() {
        let data = table();
        for bad in [0u16, data.max_level() + 1] {
            let mut ch = at(48, 99);
            assert_eq!(
                set_level(&mut ch, bad, &data),
                Err(SetLevelRefusal::OutOfRange {
                    level: bad,
                    max: data.max_level()
                })
            );
            assert_eq!((ch.level, ch.experience), (48, 99));
        }
        let mut ch = at(48, 99);
        assert!(
            set_level(&mut ch, 10, &LevelUpData::default()).is_err(),
            "no table, no consistent level to set"
        );
    }
}
