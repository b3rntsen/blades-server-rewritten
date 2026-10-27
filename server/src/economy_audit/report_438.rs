//! Report #438 — HauDrauf's material rules for levels 45-100 (owner-approved
//! deviation from retail under the standing rule for his reversible common-good
//! rules, 2026-10-27). Each rule through the production generators, and level 44
//! unchanged. The constants are in [`blades_lib::features::high_level_materials`].

use std::collections::{BTreeMap, HashMap, HashSet};

use blades_lib::features::beginner_economy::MATERIAL_STACK_SIZES;
use blades_lib::features::high_level_materials::{
    CHEST_TEMPERING_STACK, DRAGON_MATERIALS, DRAGON_STACK, EFFECT_INGREDIENT_STACK,
    EFFECT_INGREDIENTS, TEMPERING_MATERIALS, TEMPERING_STACK,
};
use blades_lib::user_data::DungeonGeneratedData;
use serde_json::json;
use uuid::Uuid;

use super::Ctx;

fn gd() -> blades_lib::game_data::GameData {
    super::read("deploy/static/parsed.json")
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Effect,
    Tempering,
    Dragon,
}

fn kind(id: &Uuid) -> Option<Kind> {
    if EFFECT_INGREDIENTS.contains(&id.as_u128()) {
        Some(Kind::Effect)
    } else if TEMPERING_MATERIALS.contains(id) {
        Some(Kind::Tempering)
    } else if DRAGON_MATERIALS.contains(id) {
        Some(Kind::Dragon)
    } else {
        None
    }
}

/// The sizes each rule's stacks came in, per source.
#[derive(Default, Debug)]
struct Seen(BTreeMap<(&'static str, Kind), Vec<u64>>);

impl Seen {
    fn add(&mut self, source: &'static str, stacks: &HashMap<Uuid, u64>) {
        for (id, q) in stacks {
            if let Some(k) = kind(id) {
                self.0.entry((source, k)).or_default().push(*q);
            }
        }
    }

    fn add_dungeon(&mut self, d: &DungeonGeneratedData) {
        for e in d.enemy_generated_data.values().flatten().flatten() {
            self.add("corpses", &e.spawn_group_loot.stackable_items);
            for l in e.loot_table_loot.values() {
                self.add("corpses", &l.stackable_items);
            }
        }
        for r in d.item_generated_data.values().flatten() {
            for l in r.loot_table_loot.values() {
                self.add("containers", &l.stackable_items);
            }
        }
    }

    fn sizes(&self, source: &str, k: Kind) -> &[u64] {
        self.0
            .iter()
            .find(|((s, kk), _)| *s == source && *kk == k)
            .map_or(&[], |(_, v)| v.as_slice())
    }
}

/// Every source but chests, for a character of `level`: quest, job-free story and
/// event dungeons (corpses, containers), Abyss floors (corpses incl. the #383 bonus,
/// containers), the Abyss `/end` package and rungs, story-quest completion rewards.
fn non_chest_sources(level: i64) -> Seen {
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut seen = Seen::default();

    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    for q in &quests {
        if let Ok((_, Some(d))) =
            blades_lib::util::quest::generate_quest_data(&gd, **q, level, &scaling)
        {
            seen.add_dungeon(&d);
        }
    }
    let mut events: Vec<Uuid> = ctx
        .sd
        .event_quests
        .templates
        .keys()
        .filter_map(|id| {
            gd.quests
                .get(id)
                .and_then(|q| q.dungeon_info.as_ref())
                .map(|d| d.dungeon_uuid)
        })
        .collect();
    events.sort();
    events.dedup();
    for d in &events {
        for seed in 0..4u64 {
            if let Some(data) = blades_lib::util::quest::generate_for_event_dungeon_for_character(
                &gd,
                d,
                Some(seed),
                scaling.enemy_level(level),
                &scaling,
                level,
            ) {
                seen.add_dungeon(&data);
            }
        }
    }
    let mut dungeons: Vec<&Uuid> = gd.dungeons.keys().collect();
    dungeons.sort();
    let mut abyss = Seen::default();
    for d in dungeons.iter().step_by(3) {
        for seed in 0..6u64 {
            if let Some(data) =
                blades_lib::util::dungeon::generate_for_abyss_floor(&gd, d, seed, 100, Some(level))
            {
                abyss.add_dungeon(&data);
            }
        }
    }
    for ((source, k), v) in abyss.0 {
        let source = if source == "corpses" {
            "abyss corpses"
        } else {
            "abyss containers"
        };
        seen.0.entry((source, k)).or_default().extend(v);
    }
    for seed in 0..3_000i64 {
        let p = blades_lib::features::abyss_rewards::end_package(400.0, level as u64, seed);
        seen.add("abyss end", &p.stackable_items);
    }
    for seed in 0..200i64 {
        for rung in (10..=1_500u32).step_by(10) {
            if let Some(g) = blades_lib::features::abyss_rewards::future_reward_for_rung(
                rung,
                level as u64,
                seed,
            ) {
                seen.add("abyss rungs", &g.stackable_items);
            }
        }
    }
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
                level,
                Uuid::from_u128(gld.as_u128() ^ n),
            );
            seen.add("quest rewards", &r.stackable_items);
        }
    }
    seen
}

/// Every chest source as the server pays it to an opener of `level`: treasury chests
/// through the daily-reward collect path (the same roll and passes as
/// `/chests/open`), and the three store chests through `roll_store_chest`.
fn chest_sources(level: i64) -> (Seen, Vec<usize>) {
    let ctx = Ctx::load();
    let mut seen = Seen::default();
    let mut wooden_effect_stacks = Vec::new();
    let uid: Uuid = "2c4ba1ff-517d-4a7f-b353-054d635ecd9e".parse().unwrap();
    for tier in 1..=5u64 {
        let payload: blades_lib::features::daily_reward::DailyRewardPayload =
            serde_json::from_value(
                json!({ "chests": [{ "tier": tier, "level": level.min(100) }] }),
            )
            .unwrap();
        for n in 0..300 {
            let r = crate::daily_reward::collect_reward_for(
                &payload,
                &ctx.sd.chest_loots,
                uid,
                &format!("438:{tier}:{n}"),
                &HashSet::new(),
                level,
            );
            seen.add("chests", &r.stackable_items);
            if tier == 1 {
                wooden_effect_stacks.push(
                    r.stackable_items
                        .keys()
                        .filter(|id| kind(id) == Some(Kind::Effect))
                        .count(),
                );
            }
        }
    }
    for (p, _) in super::STORE_CHESTS {
        for n in 0..200u64 {
            if let Some(r) = blades_lib::features::store_bundles::roll_store_chest(
                &Uuid::from_u128(p),
                level as u64,
                n,
                &HashSet::new(),
                Some(&ctx.sd.chest_loots),
            ) {
                seen.add("chests", &r.stackable_items);
            }
        }
    }
    (seen, wooden_effect_stacks)
}

/// Rule 1: from 45 every effect-ingredient stack is 30 in events, quests, the Abyss
/// and containers (and corpses), and in chests.
#[test]
fn rule_1_effect_ingredient_stacks_are_30_from_45() {
    for level in [45i64, 100] {
        let seen = non_chest_sources(level);
        // Every source pays them, and each one pays 30 (the rungs and quest
        // rewards only where their retail pool holds one).
        for source in ["corpses", "containers", "abyss corpses", "abyss end"] {
            assert!(
                !seen.sizes(source, Kind::Effect).is_empty(),
                "L{level} {source}: no effect ingredient seen"
            );
        }
        for ((source, k), v) in &seen.0 {
            if *k == Kind::Effect {
                assert!(
                    v.iter().all(|q| *q == EFFECT_INGREDIENT_STACK),
                    "L{level} {source}: {v:?}"
                );
            }
        }
        let (chests, _) = chest_sources(level);
        let v = chests.sizes("chests", Kind::Effect);
        assert!(
            !v.is_empty() && v.iter().all(|q| *q == EFFECT_INGREDIENT_STACK),
            "L{level} chests: {v:?}"
        );
    }
}

/// Rule 1b: from 45 a Wooden chest always holds exactly two effect-ingredient stacks.
#[test]
fn rule_1_wooden_chests_hold_two_effect_ingredient_stacks_from_45() {
    for level in [45i64, 70, 100] {
        let (_, wooden) = chest_sources(level);
        assert!(
            wooden.iter().all(|n| *n == 2),
            "L{level}: {:?}",
            &wooden[..20]
        );
    }
}

/// Rule 2: from 45 a tempering material is 1 outside chests and 1-3 in a chest,
/// each of 1, 2 and 3 occurring.
#[test]
fn rule_2_tempering_materials_are_1_or_1_to_3_in_chests_from_45() {
    for level in [45i64, 100] {
        let seen = non_chest_sources(level);
        let mut n = 0;
        for ((source, k), v) in &seen.0 {
            if *k == Kind::Tempering {
                assert!(
                    v.iter().all(|q| *q == TEMPERING_STACK),
                    "L{level} {source}: {v:?}"
                );
                n += v.len();
            }
        }
        assert!(n > 0, "L{level}: no tempering material outside chests");
        let (chests, _) = chest_sources(level);
        let v = chests.sizes("chests", Kind::Tempering);
        assert!(
            v.iter().all(|q| CHEST_TEMPERING_STACK.contains(q)),
            "L{level}: {v:?}"
        );
        for q in CHEST_TEMPERING_STACK {
            assert!(v.contains(&q), "L{level}: no chest tempering stack of {q}");
        }
    }
}

/// Rule 3: from 45 Dragon Bone and Dragon Scale come in 1-7 everywhere; other
/// ingots keep #388's 5, 10 or 20.
#[test]
fn rule_3_dragon_bone_and_scale_are_1_to_7_from_45() {
    for level in [45i64, 100] {
        let (chests, _) = chest_sources(level);
        let seen = non_chest_sources(level);
        let mut all = Vec::new();
        for s in [&chests, &seen] {
            for ((_, k), v) in &s.0 {
                if *k == Kind::Dragon {
                    all.extend(v.iter().copied());
                }
            }
        }
        assert!(
            all.iter().all(|q| DRAGON_STACK.contains(q)),
            "L{level}: {all:?}"
        );
        for q in DRAGON_STACK {
            assert!(
                all.contains(&q),
                "L{level}: no dragon stack of {q} in {} stacks",
                all.len()
            );
        }
    }
}

/// Level 44 is unchanged: every stack the rules name is still one of #388's
/// 5 / 10 / 20 in every source, and a Wooden chest's effect stacks are as rolled
/// (often none).
#[test]
fn level_44_is_unchanged() {
    let seen = non_chest_sources(44);
    let (chests, wooden) = chest_sources(44);
    for s in [&seen, &chests] {
        for ((source, k), v) in &s.0 {
            if *source == "abyss rungs" {
                continue; // retail's own rung amounts, never sized
            }
            assert!(
                v.iter().all(|q| MATERIAL_STACK_SIZES.contains(q)),
                "L44 {source} {k:?}: {v:?}"
            );
        }
    }
    assert!(
        wooden.iter().any(|n| *n != 2),
        "L44 Wooden chests: {:?}",
        &wooden[..20]
    );
}
