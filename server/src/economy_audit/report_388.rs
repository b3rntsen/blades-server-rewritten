//! Report #388 — the beginner economy (HauDrauf, #388/#389/#390; owner-approved
//! deviation from retail, 2026-10-16). Sampled checks of each change through the
//! production generators, and the Abyss runs the owner asked for.
//!
//! The constants are in [`blades_lib::features::beginner_economy`].

use std::collections::{BTreeSet, HashMap, HashSet};

use blades_lib::economy::GEMS;
use blades_lib::features::beginner_economy::{
    SILVER_CHEST_GEMS, WOODEN_CHEST_GEMS, with_gem_bonus,
};
use blades_lib::features::chests;
use blades_lib::user_data::DungeonGeneratedData;
use serde_json::json;
use uuid::Uuid;

use super::Ctx;

fn gd() -> blades_lib::game_data::GameData {
    super::read("deploy/static/parsed.json")
}

const SAMPLE_LEVELS: [u64; 10] = [1, 7, 20, 30, 44, 45, 48, 49, 60, 100];

// ---------------------------------------------------------------------------
// 2. Gems
// ---------------------------------------------------------------------------

/// Every Wooden chest pays 3-5 gems and every Silver chest 6-8, at every level,
/// each value reached; Gold chests still pay none.
#[test]
fn wooden_and_silver_chests_always_pay_their_gems() {
    let ctx = Ctx::load();
    for (tier, want) in [
        (1i64, Some(WOODEN_CHEST_GEMS)),
        (2, Some(SILVER_CHEST_GEMS)),
        (3, None),
    ] {
        for level in SAMPLE_LEVELS {
            let mut seen = BTreeSet::new();
            for n in 0..60 {
                let r = chests::roll_loot(
                    &ctx.sd.chest_loots,
                    tier,
                    level,
                    &format!("388:{tier}:{level}:{n}"),
                    &HashSet::new(),
                )
                .expect("a chest");
                seen.insert(r.currencies.get(&GEMS).copied().unwrap_or(0));
            }
            match &want {
                Some(range) => {
                    assert_eq!(seen, range.clone().collect(), "tier {tier} level {level}")
                }
                None => assert_eq!(seen, BTreeSet::from([0]), "tier {tier} level {level}"),
            }
        }
    }
}

/// Story quests pay their `quest_rewards.json` gems plus 20%, rounded up.
#[test]
fn story_quest_gems_pay_20_percent_more() {
    let ctx = Ctx::load();
    let mut checked = 0;
    for (gld, retail) in &ctx.sd.quest_rewards {
        let Some(gems) = retail.currencies.get(&GEMS) else {
            continue;
        };
        let quest: blades_lib::user_data::Quest = serde_json::from_value(json!({
            "version": 0, "type": "NORMAL", "objectiveStatuses": {}, "difficultyLevel": 0,
            "seed": 0, "gldQuestId": gld, "completed": false,
        }))
        .unwrap();
        let paid = crate::quest::resolve_completion_reward(&ctx.sd, *gld, &quest);
        assert_eq!(paid.currencies[&GEMS], with_gem_bonus(*gems), "{gld}");
        checked += 1;
    }
    assert!(checked >= 50, "{checked} gem-paying quests");
}

/// Event milestones and final rewards pay their band's gems plus 20%, rounded up,
/// at every level.
#[test]
fn event_gems_pay_20_percent_more() {
    let ctx = Ctx::load();
    let gems = |r: &blades_lib::economy::RewardGrant| {
        r.stackable_items
            .get(&GEMS)
            .or(r.currencies.get(&GEMS))
            .copied()
    };
    let mut checked = 0;
    for tmpl in ctx.sd.event_quests.templates.values() {
        for level in SAMPLE_LEVELS.map(|l| l as i64) {
            let band = tmpl.band_for_level(level);
            let raw_tiers = band.map_or(&tmpl.rewards, |b| &b.rewards);
            for (raw, paid) in raw_tiers.iter().zip(tmpl.rewards_for_level(level)) {
                if let Some(g) = gems(raw) {
                    assert_eq!(
                        gems(&paid),
                        Some(with_gem_bonus(g)),
                        "{} at {level}",
                        tmpl.gld_quest_id
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 100, "{checked} gem tiers");
}

/// Every gem a dungeon corpse or container pays, summed.
fn dungeon_gems(d: &DungeonGeneratedData) -> (u64, u64) {
    let mut drops = 0;
    let mut total = 0;
    let mut add = |c: &HashMap<Uuid, u64>| {
        if let Some(g) = c.get(&GEMS) {
            drops += 1;
            total += g;
        }
    };
    for e in d.enemy_generated_data.values().flatten().flatten() {
        add(&e.spawn_group_loot.currencies);
        e.loot_table_loot.values().for_each(|l| add(&l.currencies));
    }
    for r in d.item_generated_data.values().flatten() {
        r.loot_table_loot.values().for_each(|l| add(&l.currencies));
    }
    (drops, total)
}

/// Dungeon gem drops (retail: always 1) pay 1.2 on average: 2 one time in five.
#[test]
fn dungeon_gem_drops_pay_20_percent_more_on_average() {
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let (mut drops, mut total) = (0, 0);
    for level in [5i64, 25, 45] {
        for q in &quests {
            if let Ok((_, Some(d))) =
                blades_lib::util::quest::generate_quest_data(&gd, **q, level, &scaling)
            {
                let (n, t) = dungeon_gems(&d);
                drops += n;
                total += t;
            }
        }
    }
    assert!(drops > 500, "{drops} gem drops");
    let mean = total as f64 / drops as f64;
    assert!(
        (1.15..=1.25).contains(&mean),
        "{total} gems in {drops} drops: {mean}"
    );
}

// ---------------------------------------------------------------------------
// 3. Soul gems: the three best from 45 (since #432 only an event's fixed rewards)
// ---------------------------------------------------------------------------

/// #388 paid 45-48 the three best soul gems in every source, events Grand only.
/// Since #432 (HauDrauf, 2026-10-26: "No more differences between levels 45 and
/// 100, except in events with fixed rewards") every source but an event's fixed
/// rewards pays 45-48 as level 100 (`report_398` pins each against level 100), so
/// the bridge no longer applies there. An event's milestones and final reward stay
/// their own level's, and still pay Grand only, and some.
#[test]
fn event_fixed_rewards_at_45_to_48_still_pay_grand_only() {
    use blades_lib::features::beginner_economy::{GRAND_SOUL_GEM, SOUL_GEMS};
    let ctx = Ctx::load();
    let mut events: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
    events.sort();
    for level in 45..=48i64 {
        let mut paid = Vec::new();
        for id in &events {
            let t = &ctx.sd.event_quests.templates[*id];
            let rewards = t
                .rewards_for_level(level)
                .into_iter()
                .chain(t.final_reward_for_level(level));
            for r in rewards {
                paid.extend(
                    r.stackable_items
                        .keys()
                        .filter(|k| SOUL_GEMS.contains(k))
                        .copied(),
                );
            }
        }
        assert!(!paid.is_empty(), "level {level}: no event paid a soul gem");
        assert!(
            paid.iter().all(|g| *g == GRAND_SOUL_GEM),
            "level {level}: {paid:?}"
        );
    }
}

/// Opening a chest at 45-48 pays its soul gems exactly as opening it at level 100
/// does: #388's three best no longer apply on top (#432), only the owner's #421/#429
/// rule, which already covers 45-100. Nothing applies twice.
#[test]
fn chest_soul_gems_opened_at_45_to_48_are_paid_as_at_100() {
    use blades_lib::features::beginner_economy::{key_seed, top_soul_gems};
    use blades_lib::features::chest_soul_gems::pay_chest_soul_gems;
    let ctx = Ctx::load();
    let opened = |tier: i64, chest_level: u64, key: &str, opener: i64| {
        let mut r = chests::roll_loot(&ctx.sd.chest_loots, tier, chest_level, key, &HashSet::new())
            .unwrap();
        // The order `/chests/open` pays them in.
        top_soul_gems(&mut r.stackable_items, opener, key_seed(key));
        pay_chest_soul_gems(&mut r.stackable_items, tier as u64, opener, key_seed(key));
        r
    };
    for tier in [1i64, 2, 3, 4, 5] {
        for chest_level in [30u64, 45, 100] {
            for n in 0..100 {
                let key = format!("432:{tier}:{chest_level}:{n}");
                let at_100 = opened(tier, chest_level, &key, 100);
                for opener in [45i64, 46, 48] {
                    assert_eq!(
                        opened(tier, chest_level, &key, opener),
                        at_100,
                        "tier {tier} chest level {chest_level} opener {opener}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Crafting-material stacks: 5, 10 or 20
// ---------------------------------------------------------------------------

/// How many crafting-material stacks of 5, 10 and 20, and of anything else.
#[derive(Default, Debug)]
struct Sizes([u64; 3], Vec<u64>);

impl Sizes {
    fn add(&mut self, stacks: &HashMap<Uuid, u64>) {
        use blades_lib::features::beginner_economy::{MATERIAL_STACK_SIZES, is_crafting_material};
        for (_, q) in stacks.iter().filter(|(id, _)| is_crafting_material(id)) {
            match MATERIAL_STACK_SIZES.iter().position(|s| s == q) {
                Some(i) => self.0[i] += 1,
                None => self.1.push(*q),
            }
        }
    }

    /// Only 5, 10 and 20, each a third (within `slack`) of at least `min` stacks.
    fn assert_thirds(&self, what: &str, min: u64, slack: f64) {
        assert!(
            self.1.is_empty(),
            "{what}: other sizes {:?}",
            &self.1[..self.1.len().min(10)]
        );
        let n: u64 = self.0.iter().sum();
        assert!(n >= min, "{what}: only {n} material stacks");
        for c in self.0 {
            let share = c as f64 / n as f64;
            assert!((share - 1.0 / 3.0).abs() <= slack, "{what}: {:?}", self.0);
        }
    }
}

/// Chests (Wooden to Legendary, and the store chests), corpses and containers:
/// every crafting-material stack is 5, 10 or 20, in equal thirds.
#[test]
fn material_stacks_are_5_10_or_20_in_chests_corpses_and_containers() {
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let treasury = Some(&ctx.sd.chest_loots);

    let mut chest_sizes = Sizes::default();
    for tier in [-1i64, 1, 2, 3, 4, 5] {
        for level in SAMPLE_LEVELS {
            for n in 0..40 {
                let r = chests::roll_loot(
                    &ctx.sd.chest_loots,
                    tier,
                    level,
                    &format!("388m:{tier}:{level}:{n}"),
                    &HashSet::new(),
                );
                r.iter().for_each(|r| chest_sizes.add(&r.stackable_items));
            }
        }
    }
    for (p, _) in super::STORE_CHESTS {
        for level in SAMPLE_LEVELS {
            for n in 0..20u64 {
                let r = blades_lib::features::store_bundles::roll_bundle_with(
                    &Uuid::from_u128(p),
                    level,
                    n,
                    &HashSet::new(),
                    treasury,
                );
                r.iter().for_each(|r| chest_sizes.add(&r.stackable_items));
            }
        }
    }
    chest_sizes.assert_thirds("chests", 1_000, 0.04);

    let (mut corpses, mut containers) = (Sizes::default(), Sizes::default());
    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    for level in [3i64, 20, 45, 49, 100] {
        for q in &quests {
            let Ok((_, Some(d))) =
                blades_lib::util::quest::generate_quest_data(&gd, **q, level, &scaling)
            else {
                continue;
            };
            for e in d.enemy_generated_data.values().flatten().flatten() {
                e.loot_table_loot
                    .values()
                    .for_each(|l| corpses.add(&l.stackable_items));
            }
            for r in d.item_generated_data.values().flatten() {
                r.loot_table_loot
                    .values()
                    .for_each(|l| containers.add(&l.stackable_items));
            }
        }
    }
    // An unseeded quest dungeon draws its corpses' and containers' sizes the same
    // at every level, so the five levels are one sample, not five: hence 0.05.
    corpses.assert_thirds("corpses", 1_000, 0.05);
    containers.assert_thirds("containers", 1_000, 0.05);
}

/// Story-quest completion rewards, and event milestones and final rewards when
/// `EVENT_LADDER_STACKS_FIXED` is on: every crafting-material stack is 5, 10 or 20.
#[test]
fn material_stacks_are_5_10_or_20_in_events_and_quest_rewards() {
    let ctx = Ctx::load();
    let mut events = Sizes::default();
    for tmpl in ctx.sd.event_quests.templates.values() {
        for level in SAMPLE_LEVELS.map(|l| l as i64) {
            tmpl.rewards_for_level(level)
                .iter()
                .for_each(|r| events.add(&r.stackable_items));
            tmpl.final_reward_for_level(level)
                .iter()
                .for_each(|r| events.add(&r.stackable_items));
        }
    }
    if blades_lib::features::beginner_economy::EVENT_LADDER_STACKS_FIXED {
        events.assert_thirds("events", 300, 0.10);
    }

    let mut quests = Sizes::default();
    for (gld, _) in &ctx.sd.quest_rewards {
        let quest: blades_lib::user_data::Quest = serde_json::from_value(json!({
            "version": 0, "type": "NORMAL", "objectiveStatuses": {}, "difficultyLevel": 0,
            "seed": 0, "gldQuestId": gld, "completed": false,
        }))
        .unwrap();
        for n in 0..10u128 {
            let mut r = crate::quest::resolve_completion_reward(&ctx.sd, *gld, &quest);
            crate::quest::beginner_economy_completion(
                &mut r,
                30,
                Uuid::from_u128(gld.as_u128() ^ n),
            );
            quests.add(&r.stackable_items);
        }
    }
    quests.assert_thirds("story quest rewards", 100, 0.10);
}

/// With `EVENT_LADDER_STACKS_FIXED` off (the default, pending HauDrauf), event
/// ladders keep retail's material amounts at every level: the minted ladder's
/// materials are the band's, quantity for quantity. Event-dungeon corpses and
/// containers are 5, 10 or 20 regardless.
#[test]
fn event_ladders_keep_retail_material_amounts_while_the_switch_is_off() {
    use blades_lib::features::beginner_economy::{EVENT_LADDER_STACKS_FIXED, is_crafting_material};
    if EVENT_LADDER_STACKS_FIXED {
        return;
    }
    let ctx = Ctx::load();
    let amounts = |r: &blades_lib::economy::RewardGrant| {
        let mut v: Vec<u64> = r
            .stackable_items
            .iter()
            .filter(|(id, _)| is_crafting_material(id))
            .map(|(_, q)| *q)
            .collect();
        v.sort_unstable();
        v
    };
    let mut big = 0;
    for tmpl in ctx.sd.event_quests.templates.values() {
        for level in SAMPLE_LEVELS.map(|l| l as i64) {
            let Some(band) = tmpl.band_for_level(level) else {
                continue;
            };
            for (raw, paid) in band.rewards.iter().zip(tmpl.rewards_for_level(level)) {
                assert_eq!(
                    amounts(&paid),
                    amounts(raw),
                    "{} at {level}",
                    tmpl.gld_quest_id
                );
                big += amounts(&paid).iter().filter(|q| **q > 20).count();
            }
        }
    }
    assert!(big > 50, "{big} ladder stacks above 20 kept");

    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut dungeon = Sizes::default();
    let mut ids: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
    ids.sort();
    for id in ids {
        let Some(d) = gd.quests.get(id).and_then(|q| q.dungeon_info.as_ref()) else {
            continue;
        };
        for level in [10i64, 30, 45] {
            let Some(data) = blades_lib::util::quest::generate_for_event_dungeon_for_character(
                &gd,
                &d.dungeon_uuid,
                Some(7),
                scaling.enemy_level(level),
                &scaling,
                level,
            ) else {
                continue;
            };
            for e in data.enemy_generated_data.values().flatten().flatten() {
                e.loot_table_loot
                    .values()
                    .for_each(|l| dungeon.add(&l.stackable_items));
            }
            for r in data.item_generated_data.values().flatten() {
                r.loot_table_loot
                    .values()
                    .for_each(|l| dungeon.add(&l.stackable_items));
            }
        }
    }
    dungeon.assert_thirds("event dungeons", 300, 0.06);
}
