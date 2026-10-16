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

// ---------------------------------------------------------------------------
// 3. Soul gems: the three best from 45
// ---------------------------------------------------------------------------

/// The soul gems in a dungeon's corpse and container loot.
fn dungeon_soul_gems(d: &DungeonGeneratedData) -> Vec<Uuid> {
    let soul_gems = blades_lib::features::beginner_economy::SOUL_GEMS;
    let mut out = Vec::new();
    let mut add = |s: &HashMap<Uuid, u64>| out.extend(s.keys().filter(|k| soul_gems.contains(k)).copied());
    for e in d.enemy_generated_data.values().flatten().flatten() {
        add(&e.spawn_group_loot.stackable_items);
        e.loot_table_loot.values().for_each(|l| add(&l.stackable_items));
    }
    for r in d.item_generated_data.values().flatten() {
        r.loot_table_loot.values().for_each(|l| add(&l.stackable_items));
    }
    out
}

/// At 45-48 every source pays Grand, Glorious or Transcendent soul gems only —
/// events Grand only, as at 49-60 — and each source is seen paying some.
#[test]
fn soul_gems_at_45_to_48_are_the_three_best_in_every_source() {
    use blades_lib::features::beginner_economy::{GRAND_SOUL_GEM, SOUL_GEMS};
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let pools = super::read::<serde_json::Value>("deploy/static/job_pools.json");
    let top: HashSet<Uuid> = SOUL_GEMS[7..].iter().copied().collect();
    let grand: HashSet<Uuid> = HashSet::from([GRAND_SOUL_GEM]);
    let elder = ctx.sd.chest_loots.provisional_tiers.get(&4).map(Vec::as_slice);
    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let mut events: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
    events.sort();
    for level in 45..=48i64 {
        let mut by_source: Vec<(&str, Vec<Uuid>, &HashSet<Uuid>)> = Vec::new();
        let gems_of = |s: &HashMap<Uuid, u64>| s.keys().filter(|k| SOUL_GEMS.contains(k)).copied().collect::<Vec<_>>();

        let mut v = Vec::new();
        for id in &events {
            let t = &ctx.sd.event_quests.templates[*id];
            t.rewards_for_level(level).iter().for_each(|r| v.extend(gems_of(&r.stackable_items)));
            t.final_reward_for_level(level).iter().for_each(|r| v.extend(gems_of(&r.stackable_items)));
        }
        by_source.push(("event rewards", v, &grand));

        let mut v = Vec::new();
        for id in &events {
            if let Some(d) = gd.quests.get(*id).and_then(|q| q.dungeon_info.as_ref()) {
                let data = blades_lib::util::quest::generate_for_event_dungeon_for_character(
                    &gd, &d.dungeon_uuid, Some(77), scaling.enemy_level(level), &scaling, level,
                );
                v.extend(data.iter().flat_map(dungeon_soul_gems));
            }
        }
        by_source.push(("event dungeons", v, &grand));

        let mut v = Vec::new();
        for q in &quests {
            if let Ok((_, Some(d))) = blades_lib::util::quest::generate_quest_data(&gd, **q, level, &scaling) {
                v.extend(dungeon_soul_gems(&d));
            }
        }
        by_source.push(("quest dungeons", v, &top));

        let mut v = Vec::new();
        for ch in 0..20u128 {
            let (jobs, _) = crate::quest::jobs_gen::generate(&pools, Uuid::from_u128(0x388_0000 + ch), level as u16, ch as i64, 1_792_022_400, 1_792_030_000);
            for j in &jobs {
                let d = crate::quest::jobs_gen::job_dungeon_for(&gd, j, &scaling, &pools, level as u16);
                v.extend(d.iter().flat_map(dungeon_soul_gems));
            }
        }
        by_source.push(("job dungeons", v, &top));

        let mut v = Vec::new();
        for seed in 0..400i64 {
            v.extend(gems_of(&blades_lib::features::abyss_rewards::end_package(200.0, level as u64, seed).stackable_items));
        }
        by_source.push(("abyss /end", v, &top));

        let mut v = Vec::new();
        for (p, _) in super::STORE_CHESTS {
            for n in 0..60u64 {
                let r = blades_lib::features::store_bundles::roll_bundle_with(&Uuid::from_u128(p), level as u64, n, &HashSet::new(), elder);
                v.extend(r.iter().flat_map(|r| gems_of(&r.stackable_items)));
            }
        }
        for tier in [4i64, 5] {
            for n in 0..60 {
                let r = chests::roll_loot(&ctx.sd.chest_loots, tier, level as u64, &format!("388:{tier}:{n}"), &HashSet::new());
                v.extend(r.iter().flat_map(|r| gems_of(&r.stackable_items)));
            }
        }
        by_source.push(("store and Elder/Legendary chests", v, &top));

        for (source, paid, allowed) in by_source {
            assert!(!paid.is_empty(), "level {level}: {source} paid no soul gem");
            let wrong: BTreeSet<Uuid> = paid.iter().filter(|g| !allowed.contains(g)).copied().collect();
            assert!(wrong.is_empty(), "level {level}: {source} paid {wrong:?}");
        }
    }
}

/// Opening a chest at 45-48 pays only the three best: Wooden, Silver and Gold
/// chests' soul gems go through the same rule as the opener's other sources.
#[test]
fn chest_soul_gems_opened_at_45_to_48_are_the_three_best() {
    use blades_lib::features::beginner_economy::{key_seed, top_soul_gems, SOUL_GEMS};
    let ctx = Ctx::load();
    let mut paid = 0;
    for tier in [1i64, 2, 3] {
        for chest_level in [30u64, 40, 45, 48] {
            for n in 0..200 {
                let key = format!("388:{tier}:{chest_level}:{n}");
                let mut r = chests::roll_loot(&ctx.sd.chest_loots, tier, chest_level, &key, &HashSet::new()).unwrap();
                top_soul_gems(&mut r.stackable_items, 46, key_seed(&key));
                for id in r.stackable_items.keys().filter(|k| SOUL_GEMS.contains(k)) {
                    assert!(SOUL_GEMS[7..].contains(id), "tier {tier} level {chest_level}: {id}");
                    paid += 1;
                }
            }
        }
    }
    assert!(paid > 0, "no treasury chest paid a soul gem");
}
