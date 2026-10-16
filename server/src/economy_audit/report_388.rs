//! Report #388 — the beginner economy (HauDrauf, #388/#389/#390; owner-approved
//! deviation from retail, 2026-10-16). Sampled checks of each change through the
//! production generators, and the Abyss runs the owner asked for.
//!
//! The constants are in [`blades_lib::features::beginner_economy`].

use std::collections::{BTreeSet, HashMap, HashSet};

use blades_lib::economy::GEMS;
use blades_lib::features::beginner_economy::{with_gem_bonus, SILVER_CHEST_GEMS, WOODEN_CHEST_GEMS};
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
    for (tier, want) in [(1i64, Some(WOODEN_CHEST_GEMS)), (2, Some(SILVER_CHEST_GEMS)), (3, None)] {
        for level in SAMPLE_LEVELS {
            let mut seen = BTreeSet::new();
            for n in 0..60 {
                let r = chests::roll_loot(&ctx.sd.chest_loots, tier, level, &format!("388:{tier}:{level}:{n}"), &HashSet::new())
                    .expect("a chest");
                seen.insert(r.currencies.get(&GEMS).copied().unwrap_or(0));
            }
            match &want {
                Some(range) => assert_eq!(seen, range.clone().collect(), "tier {tier} level {level}"),
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
        let Some(gems) = retail.currencies.get(&GEMS) else { continue };
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
    let gems = |r: &blades_lib::economy::RewardGrant| r.stackable_items.get(&GEMS).or(r.currencies.get(&GEMS)).copied();
    let mut checked = 0;
    for tmpl in ctx.sd.event_quests.templates.values() {
        for level in SAMPLE_LEVELS.map(|l| l as i64) {
            let band = tmpl.band_for_level(level);
            let raw_tiers = band.map_or(&tmpl.rewards, |b| &b.rewards);
            for (raw, paid) in raw_tiers.iter().zip(tmpl.rewards_for_level(level)) {
                if let Some(g) = gems(raw) {
                    assert_eq!(gems(&paid), Some(with_gem_bonus(g)), "{} at {level}", tmpl.gld_quest_id);
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
            if let Ok((_, Some(d))) = blades_lib::util::quest::generate_quest_data(&gd, **q, level, &scaling) {
                let (n, t) = dungeon_gems(&d);
                drops += n;
                total += t;
            }
        }
    }
    assert!(drops > 500, "{drops} gem drops");
    let mean = total as f64 / drops as f64;
    assert!((1.15..=1.25).contains(&mean), "{total} gems in {drops} drops: {mean}");
}
