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

/// The sizes each rule's stacks came in, per source; and per source, how many
/// stacks of each item were paid (`items`) out of how many stacks (`stacks`).
#[derive(Default, Debug)]
struct Seen(
    BTreeMap<(&'static str, Kind), Vec<u64>>,
    BTreeMap<(&'static str, Uuid), Vec<u64>>,
    BTreeMap<&'static str, usize>,
);

impl Seen {
    fn add(&mut self, source: &'static str, stacks: &HashMap<Uuid, u64>) {
        for (id, q) in stacks {
            if let Some(k) = kind(id) {
                self.0.entry((source, k)).or_default().push(*q);
            }
            self.1.entry((source, *id)).or_default().push(*q);
            *self.2.entry(source).or_default() += 1;
        }
    }

    /// The `id` stacks paid in `source`.
    fn of(&self, source: &str, id: Uuid) -> &[u64] {
        self.1
            .iter()
            .find(|((s, i), _)| *s == source && *i == id)
            .map_or(&[], |(_, v)| v.as_slice())
    }

    /// Every source seen.
    fn sources(&self) -> Vec<&'static str> {
        self.2.keys().copied().collect()
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

    /// [`Self::sizes`], for a source added under its own name.
    fn of_kind(&self, source: &str, k: Kind) -> Vec<u64> {
        self.1
            .iter()
            .filter(|((s, id), _)| *s == source && kind(id) == Some(k))
            .flat_map(|(_, v)| v.iter().copied())
            .collect()
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
    let rename = |source: &'static str| {
        if source == "corpses" {
            "abyss corpses"
        } else {
            "abyss containers"
        }
    };
    for ((source, k), v) in abyss.0 {
        seen.0.entry((rename(source), k)).or_default().extend(v);
    }
    for ((source, id), v) in abyss.1 {
        seen.1.entry((rename(source), id)).or_default().extend(v);
    }
    for (source, n) in abyss.2 {
        *seen.2.entry(rename(source)).or_default() += n;
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
            seen.add(TIER_NAMES[tier as usize], &r.stackable_items);
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
                seen.add("store chests", &r.stackable_items);
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
        // In chests only a Wooden chest's are 30 (HauDrauf, msg 3018: "in
        // Holztruhen ja"); Silver to Legendary and the store chests keep #388's.
        let (chests, _) = chest_sources(level);
        let v = chests.of_kind("wooden", Kind::Effect);
        assert!(
            !v.is_empty() && v.iter().all(|q| *q == EFFECT_INGREDIENT_STACK),
            "L{level} wooden: {v:?}"
        );
        // (Our Silver to Legendary and store chests pay no effect ingredient at
        // 45-100 at all, so this guards the rule; the unit test in
        // `high_level_materials` exercises it.)
        for source in ["silver", "gold", "elder", "legendary", "store chests"] {
            let v = chests.of_kind(source, Kind::Effect);
            assert!(
                v.iter().all(|q| MATERIAL_STACK_SIZES.contains(q)),
                "L{level} {source}: {v:?}"
            );
        }
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
    let sdp = |s: &Seen| {
        s.sources()
            .into_iter()
            .map(|src| {
                SEEDS_DYE_PELT
                    .iter()
                    .map(|id| s.of(src, *id).len())
                    .sum::<usize>()
            })
            .sum::<usize>()
    };
    assert!(
        sdp(&seen) > 0 && sdp(&chests) > 0,
        "L44: Seeds/Dye/Pelt still paid"
    );
    assert!(
        wooden.iter().any(|n| *n != 2),
        "L44 Wooden chests: {:?}",
        &wooden[..20]
    );
}

/// Potions (retail fix, #438): from 45 an enemy never drops a Minor, Light or Weak
/// potion, as retail's enemies never did for characters 61-100; at 44 they still do.
#[test]
fn rule_4_no_tier_1_or_2_potions_from_enemies_from_45() {
    use blades_lib::util::dungeon::LOW_TIER_POTIONS;
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let potions = |level: i64| {
        let (mut low, mut other) = (0usize, 0usize);
        for q in &quests {
            for seed in 0..3u64 {
                let Some(dungeon) = gd.quests[*q].dungeon_info.as_ref().map(|d| d.dungeon_uuid)
                else {
                    continue;
                };
                let Some(d) = blades_lib::util::dungeon::generate_for_dungeon_with_loot_shift(
                    &gd,
                    &dungeon,
                    Some(seed),
                    blades_lib::util::dungeon::EnemyLevels::Flat {
                        level: scaling.enemy_level(level),
                        given_xp: 0,
                    },
                    blades_lib::features::loot_level::dungeon_loot_shift(
                        &scaling,
                        level,
                        scaling.enemy_level(level),
                    ),
                ) else {
                    continue;
                };
                for e in d.enemy_generated_data.values().flatten().flatten() {
                    for l in e.loot_table_loot.values() {
                        for id in l.stackable_items.keys() {
                            if LOW_TIER_POTIONS.contains(id) {
                                low += 1;
                            } else if POTION_TABLE_POTIONS.contains(&id.to_string()) {
                                other += 1;
                            }
                        }
                    }
                }
            }
        }
        (low, other)
    };
    let (low_44, other_44) = potions(44);
    assert!(
        low_44 > 0 && other_44 > 0,
        "L44: {low_44} low, {other_44} other"
    );
    for level in [45i64, 100] {
        let (low, other) = potions(level);
        assert_eq!(low, 0, "L{level}: {low} tier-1/2 potions");
        assert!(other > 0, "L{level}: no potions at all");
    }
}

/// Every potion the two enemy potion tables (`dec6ae9d`, `6bda925f`) hold at tier
/// 3-5, from `enemy_loot.json`.
static POTION_TABLE_POTIONS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("../../../blades_lib/src/enemy_loot.json")).unwrap();
    let mut out = Vec::new();
    for t in [
        "dec6ae9d-8acd-407b-9da6-3083be8353a9",
        "6bda925f-81e9-4b4f-aaf6-628df05de8cd",
    ] {
        for r in corpus["tables"][t]["results"].as_array().unwrap() {
            for k in r["loot"]["stackableItems"]
                .as_object()
                .into_iter()
                .flatten()
            {
                let id: Uuid = k.0.parse().unwrap();
                if !blades_lib::util::dungeon::LOW_TIER_POTIONS.contains(&id) {
                    out.push(k.0.clone());
                }
            }
        }
    }
    out
});

/// Per-tier source names for the chest census.
const TIER_NAMES: [&str; 6] = ["", "wooden", "silver", "gold", "elder", "legendary"];

const MOON_SUGAR: Uuid = Uuid::from_u128(0x0de08119_d316_4933_a5d3_8ef702d4efdc);
const ECTOPLASM: Uuid = Uuid::from_u128(0xdba6adde_b1f3_4146_8155_91426652495a);
const SEEDS_DYE_PELT: [Uuid; 3] = [
    Uuid::from_u128(0xb94c2028_dba7_44bd_b6f9_6f85ab0195b6), // Seeds
    Uuid::from_u128(0x6bd960ca_9af3_4b33_9ec0_5c06d50ad43e), // Dye
    Uuid::from_u128(0x51f7612f_a797_4417_8a92_493b0aae7f45), // Pelt
];

/// Prints, per source at `ECONOMY_438_LEVEL` (default 100), how many of its stacks
/// are Moon Sugar, Ectoplasm and Seeds/Dye/Pelt (`--ignored --nocapture`).
#[test]
#[ignore]
fn print_moon_sugar_and_ectoplasm_rates() {
    let level: i64 = std::env::var("ECONOMY_438_LEVEL")
        .ok()
        .and_then(|l| l.parse().ok())
        .unwrap_or(100);
    // Wooden chests by the level they are minted at: a 45-100 character's dungeon
    // chests at the level-100 dungeon's (~78-84), daily and gift chests at 100.
    let ctx = Ctx::load();
    for chest_level in [78u64, 84, 90, 100] {
        let payload: blades_lib::features::daily_reward::DailyRewardPayload =
            serde_json::from_value(json!({ "chests": [{ "tier": 1, "level": chest_level }] }))
                .unwrap();
        let (mut moon, mut ecto) = (0, 0);
        for n in 0..10_000 {
            let r = crate::daily_reward::collect_reward_for(
                &payload,
                &ctx.sd.chest_loots,
                Uuid::nil(),
                &format!("438e:{n}"),
                &HashSet::new(),
                level,
            );
            moon += usize::from(r.stackable_items.contains_key(&MOON_SUGAR));
            ecto += usize::from(r.stackable_items.contains_key(&ECTOPLASM));
        }
        println!(
            "L{level} Wooden chest level {chest_level}: Moon Sugar in {moon}, Ectoplasm in {ecto} of 10000"
        );
    }
    let (chests, _) = chest_sources(level);
    for s in [non_chest_sources(level), chests] {
        for source in s.sources() {
            let n = s.2[source];
            let sdp: usize = SEEDS_DYE_PELT
                .iter()
                .map(|id| s.of(source, *id).len())
                .sum();
            println!(
                "L{level} {source}: {n} stacks, Moon Sugar {} {:?}, Ectoplasm {}, Seeds/Dye/Pelt {}",
                s.of(source, MOON_SUGAR).len(),
                {
                    let mut q = s.of(source, MOON_SUGAR).to_vec();
                    q.sort_unstable();
                    q.dedup();
                    q
                },
                s.of(source, ECTOPLASM).len(),
                sdp,
            );
        }
    }
}

/// Seeds, Dye and Pelt make way for Ectoplasm (HauDrauf, msg 3018): from 45 none of
/// the three is paid anywhere, and Ectoplasm comes up in Wooden chests as often as
/// Moon Sugar does there, at Moon Sugar's sizes; no other source gains it.
#[test]
fn rule_5_ectoplasm_replaces_seeds_dye_and_pelt_from_45() {
    for level in [45i64, 100] {
        let seen = non_chest_sources(level);
        let (chests, _) = chest_sources(level);
        for s in [&seen, &chests] {
            for source in s.sources() {
                for id in SEEDS_DYE_PELT {
                    assert!(s.of(source, id).is_empty(), "L{level} {source}: {id} paid");
                }
                let gained = s.of(source, ECTOPLASM).len();
                match source {
                    "wooden" | "chests" => {}
                    // Two story quests pay Ectoplasm as retail did.
                    "quest rewards" => assert_eq!(gained, 20, "L{level}"),
                    _ => assert_eq!(gained, 0, "L{level} {source}: Ectoplasm added"),
                }
            }
        }
        // Wooden chests at the levels a 45-100 character's are minted at.
        let ctx = Ctx::load();
        for chest_level in [84u64, 100] {
            let payload: blades_lib::features::daily_reward::DailyRewardPayload =
                serde_json::from_value(json!({ "chests": [{ "tier": 1, "level": chest_level }] }))
                    .unwrap();
            let (mut moon, mut ecto) = (0usize, 0usize);
            for n in 0..4_000 {
                let r = crate::daily_reward::collect_reward_for(
                    &payload,
                    &ctx.sd.chest_loots,
                    Uuid::from_u128(0x438),
                    &format!("438r5:{n}"),
                    &HashSet::new(),
                    level,
                );
                moon += usize::from(r.stackable_items.contains_key(&MOON_SUGAR));
                if let Some(q) = r.stackable_items.get(&ECTOPLASM) {
                    assert!(MATERIAL_STACK_SIZES.contains(q), "Ectoplasm x{q}");
                    ecto += 1;
                }
            }
            assert!(
                moon.abs_diff(ecto) <= 120,
                "L{level} chest level {chest_level}: Moon Sugar {moon}, Ectoplasm {ecto} of 4000"
            );
        }
    }
}
