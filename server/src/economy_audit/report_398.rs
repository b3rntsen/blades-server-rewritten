//! Report #398 — "from level 49 there should be no difference to level 100".
//!
//! HauDrauf (2026-10-16): Gold chests at 45-48 paid low-value items; "from level 49,
//! is everything at the highest level? From then on there should be no difference
//! to level 100." His standing rule: Blades is done at 45, complete by 50, many
//! stopped at 49, so everything must be obtainable by 49.
//!
//! Every level-keyed loot and reward source now rolls a level 49+ character as a
//! level-100 one ([`blades_lib::features::loot_level`]). These tests pin both halves:
//!
//! * BELOW 49 NOTHING MOVED. [`levels_below_49_are_unchanged`] hashes every source
//!   at levels 1-48 through the production paths; the digests were taken on
//!   fork/main (6933e04c) before the change.
//! * FROM 49 EVERY SOURCE IS LEVEL 100'S. For levels 49, 60 and 80 against 100, with
//!   the same seeds: chests (treasury, daily, gift, store, Abyss rung, dungeon),
//!   enemy and container loot (story quests, events, jobs, the Abyss bonus), event
//!   rewards, job gold, daily rewards and the Abyss `/end` and rungs. Enemy levels
//!   and XP are asserted UNCHANGED alongside.
//!
//! `part_a` (ignored) prints the report's numbers: `cargo test -p server
//! report_398::part_a -- --ignored --nocapture`.

use std::collections::{BTreeMap, HashMap, HashSet};

use blades_lib::economy::RewardGrant;
use blades_lib::features::loot_level::{dungeon_loot_shift, effective_loot_level, effective_loot_level_u64};
use blades_lib::features::{abyss_rewards, chests, store_bundles};
use blades_lib::static_data::QuestLevelScaling;
use blades_lib::user_data::DungeonGeneratedData;
use serde_json::Value;
use uuid::Uuid;

use super::Ctx;
use crate::quest::jobs_gen;

fn fnv(h: &mut u64, s: &str) {
    for b in s.as_bytes() {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

/// `to_value` sorts every map, so equal content hashes equal.
fn digest_of<T: serde::Serialize>(h: &mut u64, t: &T) {
    fnv(h, &serde_json::to_value(t).unwrap().to_string());
}

const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

fn gd() -> blades_lib::game_data::GameData {
    super::read("deploy/static/parsed.json")
}

fn job_pools() -> Value {
    super::read("deploy/static/job_pools.json")
}

const BOUNDARY: u64 = 1_792_022_400;
const NOW: u64 = 1_792_030_000;

fn board(pools: &Value, ch: u128, level: u16) -> Vec<Value> {
    jobs_gen::generate(pools, Uuid::from_u128(0x398_0000 + ch), level, ch as i64, BOUNDARY, NOW).0
}

/// A job's dungeon, as `/quests` stores and serves it for a character of `level`.
fn job_dungeon(gd: &blades_lib::game_data::GameData, pools: &Value, job: &Value, level: u16) -> Option<DungeonGeneratedData> {
    jobs_gen::generated_data_for_job(gd, job, &crate::quest::shipped_scaling(), jobs_gen::loot_shift_for(pools, job, level))
}

/// An event dungeon run, as `/enter` generates it for a character of `level`.
fn event_dungeon(gd: &blades_lib::game_data::GameData, scaling: &QuestLevelScaling, dungeon: &Uuid, seed: u64, level: i64) -> Option<DungeonGeneratedData> {
    let e = scaling.enemy_level(level);
    blades_lib::util::quest::generate_for_event_dungeon_with_loot_shift(
        gd,
        dungeon,
        Some(seed),
        e,
        scaling,
        dungeon_loot_shift(scaling, level, e),
    )
}

fn event_dungeons(ctx: &Ctx, gd: &blades_lib::game_data::GameData) -> Vec<Uuid> {
    let mut ids: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
    ids.sort();
    ids.into_iter()
        .filter_map(|id| gd.quests.get(id).and_then(|q| q.dungeon_info.as_ref()).map(|d| d.dungeon_uuid))
        .collect()
}

/// Everything a dungeon PAYS — corpse, container and chest — without the enemies'
/// levels and XP, which must not move.
fn loot_of(d: &DungeonGeneratedData) -> Value {
    let mut v = serde_json::to_value(d).unwrap();
    for spawners in v["enemyGeneratedData"].as_object_mut().into_iter().flat_map(|m| m.values_mut()) {
        for enemies in spawners.as_array_mut().into_iter().flatten() {
            for e in enemies.as_array_mut().into_iter().flatten() {
                let e = e.as_object_mut().unwrap();
                e.remove("enemyLevel");
                e.remove("givenXP");
            }
        }
    }
    v
}

/// Every enemy's (level, XP), in a canonical order.
fn enemy_levels(d: &DungeonGeneratedData) -> Vec<(String, i64, u64)> {
    let mut out: Vec<(String, i64, u64)> = d
        .enemy_generated_data
        .iter()
        .flat_map(|(g, s)| {
            s.iter().enumerate().flat_map(move |(i, es)| {
                es.iter().enumerate().map(move |(j, e)| (format!("{g}:{i}:{j}"), e.enemy_level, e.given_xp))
            })
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Below 49: unchanged
// ---------------------------------------------------------------------------

/// Digests of every source at levels 1-48 through today's production paths. The
/// pinned values were printed by the same loops on fork/main 6933e04c, before
/// #398, through that commit's API (the job dungeon then took no loot shift; the
/// event run was `generate_for_event_dungeon_with_seed`; the store chest was rolled
/// at the raw level).
#[test]
fn levels_below_49_are_unchanged() {
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let levels = 1..=48i64;
    let mut got: Vec<(&str, u64)> = Vec::new();

    let mut h = FNV0;
    let mut ids: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
    ids.sort();
    for l in levels.clone() {
        for id in &ids {
            let t = &ctx.sd.event_quests.templates[*id];
            digest_of(&mut h, &t.rewards_for_level(l));
            digest_of(&mut h, &t.final_reward_for_level(l));
        }
    }
    got.push(("events", h));

    let mut h = FNV0;
    for l in levels.clone() {
        for d in &ctx.sd.daily_rewards {
            digest_of(&mut h, &d.daily_reward_for_level(l as u16));
        }
    }
    got.push(("daily", h));

    let mut h = FNV0;
    for l in levels.clone() {
        for seed in 0..6i64 {
            for score in [20.0, 40.0, 80.0, 200.0, 700.0] {
                digest_of(&mut h, &abyss_rewards::end_package(score, l as u64, seed * 7919));
                digest_of(&mut h, &abyss_rewards::next_future_reward(score, l as u64, seed * 7919));
            }
            for rung in abyss_rewards::ABYSS_LADDER {
                digest_of(&mut h, &abyss_rewards::future_reward_for_rung(rung, l as u64, seed * 7919));
            }
        }
    }
    got.push(("abyss_rewards", h));

    let mut dungeons: Vec<&Uuid> = gd.dungeons.keys().collect();
    dungeons.sort();
    let mut h = FNV0;
    for l in levels.clone() {
        for d in dungeons.iter().step_by(17) {
            digest_of(&mut h, &blades_lib::util::dungeon::generate_for_abyss_floor(&gd, d, 4242, 60, Some(l)));
        }
    }
    got.push(("abyss_floor", h));

    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let mut h = FNV0;
    for l in levels.clone() {
        for q in quests.iter().step_by(5) {
            let r = blades_lib::util::quest::generate_quest_data(&gd, **q, l, &scaling).ok();
            digest_of(&mut h, &r.map(|(q, d)| (serde_json::to_value(q).unwrap(), d)));
        }
    }
    got.push(("quests", h));

    let mut h = FNV0;
    for l in levels.clone() {
        for id in ids.iter().step_by(3) {
            let Some(dungeon) = gd.quests.get(*id).and_then(|q| q.dungeon_info.as_ref()).map(|d| d.dungeon_uuid) else {
                continue;
            };
            digest_of(&mut h, &event_dungeon(&gd, &scaling, &dungeon, 99, l));
        }
    }
    got.push(("event_dungeons", h));

    let pools = job_pools();
    let mut h = FNV0;
    for l in levels.clone() {
        for ch in 0..3u128 {
            let jobs = board(&pools, ch, l as u16);
            digest_of(&mut h, &jobs);
            for j in &jobs {
                digest_of(&mut h, &job_dungeon(&gd, &pools, j, l as u16));
            }
        }
    }
    got.push(("jobs", h));

    let elder = ctx.sd.chest_loots.provisional_tiers.get(&4).map(Vec::as_slice);
    let mut h = FNV0;
    for l in levels.clone() {
        for (p, _) in super::STORE_CHESTS {
            for n in 0..20u64 {
                digest_of(
                    &mut h,
                    &store_bundles::roll_bundle_with(&Uuid::from_u128(p), effective_loot_level_u64(l as u64), n, &HashSet::new(), elder),
                );
            }
        }
    }
    got.push(("store", h));

    let mut h = FNV0;
    for l in levels {
        for tier in [-1i64, 1, 2, 3, 4, 5] {
            for n in 0..10u64 {
                let key = format!("k:{tier}:{l}:{n}");
                digest_of(&mut h, &chests::roll_loot(&ctx.sd.chest_loots, tier, l as u64, &key, &HashSet::new()));
            }
        }
    }
    got.push(("treasury", h));

    let pinned: [(&str, u64); 9] = [
        ("events", 0x7445_3735_abb6_35b1),
        ("daily", 0x40d7_bed5_0a28_cc9a),
        ("abyss_rewards", 0xcbb0_1935_28c0_3bba),
        ("abyss_floor", 0x57ca_1f7e_2bfe_f747),
        ("quests", 0x1e88_fcc6_608a_b8c1),
        ("event_dungeons", 0x7b34_bfa1_3352_e75a),
        ("jobs", 0xaaa8_1f0b_d8d0_e39d),
        ("store", 0xb3b8_adde_182b_a5ca),
        ("treasury", 0xfe7b_85af_c723_b9e0),
    ];
    assert_eq!(got, pinned.to_vec(), "a source moved for a character below level 49");
}

// ---------------------------------------------------------------------------
// From 49: level 100's
// ---------------------------------------------------------------------------

const PAID_AS_100: [i64; 3] = [49, 60, 80];

#[test]
fn event_rewards_from_49_are_level_100s() {
    let ctx = Ctx::load();
    for tmpl in ctx.sd.event_quests.templates.values() {
        for l in PAID_AS_100 {
            assert_eq!(
                serde_json::to_value(tmpl.rewards_for_level(l)).unwrap(),
                serde_json::to_value(tmpl.rewards_for_level(100)).unwrap(),
                "{} milestones at {l}",
                tmpl.gld_quest_id
            );
            assert_eq!(
                serde_json::to_value(tmpl.final_reward_for_level(l)).unwrap(),
                serde_json::to_value(tmpl.final_reward_for_level(100)).unwrap(),
                "{} final at {l}",
                tmpl.gld_quest_id
            );
        }
    }
}

#[test]
fn daily_rewards_from_49_are_level_100s() {
    let ctx = Ctx::load();
    for d in &ctx.sd.daily_rewards {
        for l in PAID_AS_100 {
            assert_eq!(
                serde_json::to_value(d.daily_reward_for_level(l as u16)).unwrap(),
                serde_json::to_value(d.daily_reward_for_level(100)).unwrap(),
            );
        }
        for chest in d.daily_reward_for_level(49).chests {
            assert_eq!(chest.level, 100, "a level-49 daily chest is a level-100 chest");
        }
    }
}

/// No daily or event reward is worth less at 49-99 than at the character's own
/// level: where level 100's pays less of the same thing, the larger count is kept.
#[test]
fn no_daily_or_event_reward_is_worth_less_from_49() {
    let ctx = Ctx::load();
    let p = prices();
    let value = |r: &HashMap<Uuid, u64>| -> u64 { r.iter().map(|(id, n)| p.stackable_price(*id, *n)).sum() };
    let unmapped = |d: &blades_lib::features::daily_reward::DailyRewardDef, l: u16| -> u64 {
        // The tier a character of `l` was paid before #398.
        let tier = d.level_tiers.iter().filter(|t| t.min_level <= l).max_by_key(|t| t.min_level);
        value(&tier.map_or(&d.daily_reward, |t| &t.daily_reward).stackable_items)
    };
    for d in &ctx.sd.daily_rewards {
        for l in 49..=99u16 {
            let paid = value(&d.daily_reward_for_level(l).stackable_items);
            assert!(paid >= unmapped(d, l), "daily {} at {l}: {paid} < {}", d.reward_uid, unmapped(d, l));
        }
    }
    // Event milestones: the level-100 ladder is worth at least each level's own.
    for tmpl in ctx.sd.event_quests.templates.values() {
        let at_100: u64 = tmpl.rewards_for_level(100).iter().map(|r| value(&r.stackable_items)).sum();
        for l in [45i64, 46, 47, 48] {
            let own: u64 = tmpl.rewards_for_level(l).iter().map(|r| value(&r.stackable_items)).sum();
            assert!(at_100 >= own, "event {} level {l}: 100 pays {at_100} < {own}", tmpl.gld_quest_id);
        }
    }
}

/// `/end` and every rung but 190 and 490, which keep #371's extra Grand gem at 49-59
/// (see `rung_reward`): the same seed, the same reward.
#[test]
fn abyss_rewards_from_49_are_level_100s() {
    for seed in 0..200i64 {
        let seed = seed * 104_729;
        for l in PAID_AS_100 {
            let l = l as u64;
            for score in [20.0, 40.0, 80.0, 200.0, 700.0] {
                assert_eq!(abyss_rewards::end_package(score, l, seed), abyss_rewards::end_package(score, 100, seed));
            }
            for rung in abyss_rewards::ABYSS_LADDER {
                let kept_grand = (190 == rung || 490 == rung) && l <= 59;
                if !kept_grand {
                    assert_eq!(
                        abyss_rewards::future_reward_for_rung(rung, l, seed),
                        abyss_rewards::future_reward_for_rung(rung, 100, seed),
                        "rung {rung} level {l}"
                    );
                }
            }
        }
    }
    // The rung chests are level-100 chests.
    for rung in [50u32, 360] {
        let g = abyss_rewards::future_reward_for_rung(rung, 49, 7).unwrap();
        assert_eq!(g.chests[0].level, 100, "rung {rung}");
    }
}

/// The Abyss bonus material (#383) is banded to the level the character is paid as.
#[test]
fn abyss_corpse_bonus_from_49_is_level_100s() {
    let gd = gd();
    let mut dungeons: Vec<&Uuid> = gd.dungeons.keys().collect();
    dungeons.sort();
    for d in dungeons.iter().step_by(9) {
        for seed in [1u64, 4242] {
            let at_100 = blades_lib::util::dungeon::generate_for_abyss_floor(&gd, d, seed, 100, Some(100));
            for l in PAID_AS_100 {
                let at = blades_lib::util::dungeon::generate_for_abyss_floor(&gd, d, seed, 100, Some(l));
                assert_eq!(serde_json::to_value(&at).unwrap(), serde_json::to_value(&at_100).unwrap());
            }
        }
    }
}

/// Story quests: the same corpse, container and chest loot as level 100's; the
/// enemies stay at the character's own enemy level and XP.
#[test]
fn story_quest_loot_from_49_is_level_100s_and_enemies_stay() {
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let mut compared = 0;
    for q in quests {
        let Ok((_, Some(at_100))) = blades_lib::util::quest::generate_quest_data(&gd, *q, 100, &scaling) else {
            continue;
        };
        for l in PAID_AS_100 {
            let (quest, Some(at)) = blades_lib::util::quest::generate_quest_data(&gd, *q, l, &scaling).unwrap() else {
                panic!("{q} lost its dungeon at {l}");
            };
            assert_eq!(loot_of(&at), loot_of(&at_100), "quest {q} level {l}");
            let e = scaling.enemy_level(l);
            assert_eq!(quest.difficulty_level, e);
            assert!(at.enemy_generated_data.values().flatten().flatten().all(|x| x.enemy_level == e && x.given_xp == scaling.given_xp(e)));
            compared += 1;
        }
    }
    assert!(compared > 100, "only {compared} quest dungeons compared");
}

/// Events: loot as level 100's run of the same seed; enemies at the row's level
/// plus their group delta, as before.
#[test]
fn event_dungeon_loot_from_49_is_level_100s_and_enemies_stay() {
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    for dungeon in event_dungeons(&ctx, &gd) {
        for seed in [0u64, 99] {
            let Some(at_100) = event_dungeon(&gd, &scaling, &dungeon, seed, 100) else { continue };
            for l in PAID_AS_100 {
                let at = event_dungeon(&gd, &scaling, &dungeon, seed, l).unwrap();
                assert_eq!(loot_of(&at), loot_of(&at_100), "event {dungeon} level {l}");
                let unshifted = blades_lib::util::quest::generate_for_event_dungeon_with_seed(&gd, &dungeon, seed, scaling.enemy_level(l), &scaling).unwrap();
                assert_eq!(enemy_levels(&at), enemy_levels(&unshifted), "event {dungeon} enemies moved at {l}");
            }
        }
    }
}

/// The job difficulty rows are one set of bands translated, shared by every job
/// type — what makes [`jobs_gen::loot_shift`] land a draw exactly where the
/// level-100 board's same draw lands.
#[test]
fn job_difficulty_rows_are_translations_shared_by_every_type() {
    let pools = job_pools();
    let types = pools["perTypeDifficulty"].as_array().unwrap();
    let rows = |t: &Value| t["difficultyByLevel"].clone();
    for t in types {
        assert_eq!(rows(t), rows(&types[0]), "job type {} has its own rows", t["jobType"]);
    }
    let shape = |r: &Value| -> Vec<i64> {
        let lo = r["veryEasyMin"].as_i64().unwrap();
        ["easyMin", "normalMin", "hardMin", "veryHardMin", "veryHardMax"].iter().map(|k| r[*k].as_i64().unwrap() - lo).collect()
    };
    let all = types[0]["difficultyByLevel"].as_array().unwrap();
    for r in all {
        assert_eq!(shape(r), shape(&all[0]), "row {} is not a translation", r["level"]);
    }
}

/// Jobs: the same gold and the same corpse and container loot as the level-100
/// board's job of the same draw; the job's own difficulty, enemy levels and XP stay.
#[test]
fn job_rewards_from_49_are_level_100s_and_enemies_stay() {
    let gd = gd();
    let pools = job_pools();
    for ch in 0..12u128 {
        let at_100 = board(&pools, ch, 100);
        for l in PAID_AS_100 {
            let at = board(&pools, ch, l as u16);
            assert_eq!(at.len(), at_100.len());
            for (j, j100) in at.iter().zip(&at_100) {
                assert_eq!(j["jobSetup"]["rewardItemCount"], j100["jobSetup"]["rewardItemCount"], "gold, char {ch} level {l}");
                assert_eq!(j["jobSetup"]["rewardGemCount"], j100["jobSetup"]["rewardGemCount"]);
                let d = job_dungeon(&gd, &pools, j, l as u16).unwrap();
                let d100 = job_dungeon(&gd, &pools, j100, 100).unwrap();
                assert_eq!(loot_of(&d), loot_of(&d100), "job loot, char {ch} level {l}");
                // The job's own difficulty and XP are untouched.
                let own = jobs_gen::generated_data_for_job(&gd, j, &crate::quest::shipped_scaling(), 0).unwrap();
                assert_eq!(enemy_levels(&d), enemy_levels(&own));
                assert_eq!(j["jobSetup"]["rewardXp"], serde_json::json!(jobs_gen::job_reward_xp(j["difficultyLevel"].as_i64().unwrap())));
            }
        }
    }
}

/// Dungeon chests: a level 49+ character's chest is minted at the level a level-100
/// character's chest from the same dungeon is (story quest, event, job).
#[test]
fn dungeon_chests_from_49_are_minted_at_level_100s_level() {
    let scaling = crate::quest::shipped_scaling();
    let pools = job_pools();
    let paid = |l: i64, is_job: bool, level: u64| {
        crate::dungeon_update::paid_chest_level(&scaling, &pools, l as u16, is_job, level)
    };
    let e100 = scaling.enemy_level(100) as u64;
    assert_eq!(paid(100, false, e100), e100, "level 100 is unchanged");
    for l in PAID_AS_100 {
        let e = scaling.enemy_level(l) as u64;
        assert_eq!(paid(l, false, e), e100, "story/event chest at level {l}");
    }
    // A job: the level-100 board's same draw.
    for ch in 0..6u128 {
        let at_100 = board(&pools, ch, 100);
        for l in PAID_AS_100 {
            for (j, j100) in board(&pools, ch, l as u16).iter().zip(&at_100) {
                let own = j["difficultyLevel"].as_u64().unwrap();
                assert_eq!(paid(l, true, own), j100["difficultyLevel"].as_u64().unwrap(), "job chest at {l}");
            }
        }
    }
    for l in 1..49 {
        let e = scaling.enemy_level(l) as u64;
        assert_eq!(paid(l, false, e), e, "level {l} moved");
        assert_eq!(paid(l, true, e), e, "level {l} job moved");
    }
}

/// Part C (#398): a story quest's dungeon is entered at the level its enemies were
/// generated at, not at 1, so its chests are no longer level-1 chests.
#[test]
fn story_dungeon_chests_are_minted_at_the_dungeons_enemy_level() {
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let data = quests
        .into_iter()
        .filter_map(|q| blades_lib::util::quest::generate_quest_data(&gd, *q, 45, &scaling).ok()?.1)
        .find(|d| d.enemy_generated_data.values().flatten().flatten().next().is_some())
        .expect("a story dungeon with enemies");
    let level = crate::dungeon::quest_dungeon_level(blades_lib::user_data::STORY_QUEST_DIFFICULTY_LEVEL, Some(&data));
    assert_eq!(level, scaling.enemy_level(45) as u64, "the story dungeon's level");
    // A dungeon_state written before the fix (level 1) still mints at that level.
    assert_eq!(crate::dungeon_update::dungeon_chest_level(1, &data), level);
    assert_eq!(crate::dungeon_update::dungeon_chest_level(30, &data), 30, "a job or event keeps its own");
    // And that is what the report was about: a level-45 Gold chest from a story
    // quest paid ~3,000 gold and Iron/Steel gear at level 1.
    let gold = |chest_level: u64| -> u64 {
        (0..200u64)
            .filter_map(|n| chests::roll_loot(&ctx.sd.chest_loots, 3, chest_level, &format!("s:{n}"), &HashSet::new()))
            .map(|r| r.currencies.get(&blades_lib::economy::GOLD).copied().unwrap_or(0))
            .sum::<u64>()
            / 200
    };
    let (before, after) = (gold(1), gold(level));
    assert!(before < 4_000 && after > 7_500, "Gold chest gold: level 1 {before}, level {level} {after}");
}

/// Store chests bought with gems, and season/gift chests: rolled at the level the
/// buyer or claimant is paid as.
#[test]
fn store_and_gift_chests_from_49_are_level_100s() {
    let ctx = Ctx::load();
    let elder = ctx.sd.chest_loots.provisional_tiers.get(&4).map(Vec::as_slice);
    for (p, _) in super::STORE_CHESTS {
        for n in 0..40u64 {
            let at_100 = store_bundles::roll_bundle_with(&Uuid::from_u128(p), 100, n, &HashSet::new(), elder);
            for l in PAID_AS_100 {
                let at = store_bundles::roll_bundle_with(&Uuid::from_u128(p), effective_loot_level_u64(l as u64), n, &HashSet::new(), elder);
                assert_eq!(serde_json::to_value(&at).unwrap(), serde_json::to_value(&at_100).unwrap());
            }
        }
    }
    for l in PAID_AS_100 {
        assert_eq!(effective_loot_level(l), 100);
    }
}

// ---------------------------------------------------------------------------
// Part A report (ignored)
// ---------------------------------------------------------------------------

const GEMS: [&str; 9] =
    ["Pearl", "Topaz", "Garnet", "Amethyst", "Ruby", "Sapphire", "Emerald", "Diamond", "FirebreathOpal"];
const MATERIALS: [&str; 16] = [
    "Iron", "Steel", "Silver", "Orcish", "Dwarven", "Elven", "Glass", "Ebony", "Daedric", "Dragon",
    "Stalhrim", "Orichalcum", "Imperial", "Nordic", "Chitin", "Leather",
];

fn material_of(name: &str) -> String {
    MATERIALS.iter().find(|m| name.starts_with(*m)).map(|m| m.to_string()).unwrap_or_else(|| "other".into())
}

#[derive(Default)]
struct Tally {
    n: u64,
    gold: u64,
    sell: u64,
    items: u64,
    weapons: BTreeMap<String, u64>,
    gems: BTreeMap<String, u64>,
    stacks: BTreeMap<String, u64>,
}

type Prices = blades_lib::features::merchant::SellPrices;

fn prices() -> Prices {
    Prices::from_json(&super::read("deploy/static/item_sell_values.json"), &super::read("deploy/static/enchant_values.json"))
}

fn add_stacks(ctx: &Ctx, t: &mut Tally, p: &Prices, s: &HashMap<Uuid, u64>) {
    for (id, q) in s {
        let name = ctx.name(id);
        t.sell += p.stackable_price(*id, *q);
        if GEMS.contains(&name.as_str()) {
            *t.gems.entry(name.clone()).or_default() += q;
        }
        *t.stacks.entry(name).or_default() += q;
    }
}

fn add_item(ctx: &Ctx, t: &mut Tally, p: &Prices, i: &blades_lib::user_data::Item) {
    t.items += 1;
    t.sell += p.item_price(i);
    if ctx.item_type(&i.item_template_id) == Some(2) {
        *t.weapons.entry(material_of(&ctx.name(&i.item_template_id))).or_default() += 1;
    }
}

fn add_reward(ctx: &Ctx, t: &mut Tally, p: &Prices, r: &RewardGrant) {
    t.n += 1;
    let g = r.currencies.get(&blades_lib::economy::GOLD).copied().unwrap_or(0);
    t.gold += g;
    t.sell += g;
    add_stacks(ctx, t, p, &r.stackable_items);
    for i in &r.items {
        add_item(ctx, t, p, &i.item);
    }
}

fn add_loot(ctx: &Ctx, t: &mut Tally, p: &Prices, r: &blades_lib::user_data::LootTableResult) {
    let g = r.currencies.get(&blades_lib::economy::GOLD).copied().unwrap_or(0);
    t.gold += g;
    t.sell += g;
    add_stacks(ctx, t, p, &r.stackable_items);
    for i in r.item.0.values() {
        add_item(ctx, t, p, i);
    }
}

fn add_enemies(ctx: &Ctx, t: &mut Tally, p: &Prices, d: &DungeonGeneratedData) {
    for e in d.enemy_generated_data.values().flatten().flatten() {
        t.n += 1;
        for r in e.loot_table_loot.values() {
            add_loot(ctx, t, p, r);
        }
    }
}

fn share(m: &BTreeMap<String, u64>) -> String {
    let total: u64 = m.values().sum();
    if total == 0 {
        return "-".into();
    }
    let mut v: Vec<(&String, &u64)> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    v.iter().map(|(k, n)| format!("{k} {:.0}%", **n as f64 * 100.0 / total as f64)).collect::<Vec<_>>().join(", ")
}

fn print_tally(label: &str, t: &Tally) {
    let n = t.n.max(1) as f64;
    println!(
        "{label}: n={} gold/ea={:.0} sell/ea={:.0} items/ea={:.2} | weapons: {} | gems/ea={:.3}: {}",
        t.n,
        t.gold as f64 / n,
        t.sell as f64 / n,
        t.items as f64 / n,
        share(&t.weapons),
        t.gems.values().sum::<u64>() as f64 / n,
        share(&t.gems),
    );
}

#[test]
#[ignore]
fn part_a() {
    let ctx = Ctx::load();
    let gd = gd();
    let p = prices();
    let scaling = crate::quest::shipped_scaling();
    let pools = job_pools();
    let levels = [45i64, 46, 47, 48, 49, 100];

    for c in levels {
        // Gold chests at the level each source mints them for this character.
        let e = scaling.enemy_level(c) as u64;
        for (src, lvl) in [
            ("daily/gift/abyss", effective_loot_level_u64(c as u64)),
            ("story/event dungeon", crate::dungeon_update::paid_chest_level(&scaling, &pools, c as u16, false, e)),
        ] {
            let mut t = Tally::default();
            for n in 0..400u64 {
                if let Some(r) = chests::roll_loot(&ctx.sd.chest_loots, 3, lvl, &format!("a398:{c}:{n}"), &HashSet::new()) {
                    add_reward(&ctx, &mut t, &p, &r);
                }
            }
            print_tally(&format!("GOLD CHEST c={c} {src} (chest level {lvl})"), &t);
        }
        let mut story = Tally::default();
        let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
        quests.sort();
        for q in quests {
            if let Ok((_, Some(d))) = blades_lib::util::quest::generate_quest_data(&gd, *q, c, &scaling) {
                add_enemies(&ctx, &mut story, &p, &d);
            }
        }
        print_tally(&format!("STORY ENEMIES c={c}"), &story);
        let mut ev = Tally::default();
        for dungeon in event_dungeons(&ctx, &gd) {
            for run in 0..4u64 {
                if let Some(d) = event_dungeon(&gd, &scaling, &dungeon, run, c) {
                    add_enemies(&ctx, &mut ev, &p, &d);
                }
            }
        }
        print_tally(&format!("EVENT ENEMIES c={c}"), &ev);
        let mut jobs = Tally::default();
        let (mut gold, mut n) = (0u64, 0u64);
        for ch in 0..40u128 {
            for j in board(&pools, ch, c as u16) {
                gold += j["jobSetup"]["rewardItemCount"].as_u64().unwrap_or(0);
                n += 1;
                if let Some(d) = job_dungeon(&gd, &pools, &j, c as u16) {
                    add_enemies(&ctx, &mut jobs, &p, &d);
                }
            }
        }
        print_tally(&format!("JOB ENEMIES c={c} (gold per job {:.0})", gold as f64 / n.max(1) as f64), &jobs);
        let mut evr = Tally::default();
        for tmpl in ctx.sd.event_quests.templates.values() {
            for r in tmpl.rewards_for_level(c).iter().chain(tmpl.final_reward_for_level(c).as_ref()) {
                add_reward(&ctx, &mut evr, &p, r);
            }
        }
        let soul: Vec<_> = evr.stacks.iter().filter(|(k, _)| k.starts_with("SoulGem")).collect();
        println!("EVENT REWARDS c={c}: sell/milestone={:.0} soul gems {soul:?}", evr.sell as f64 / evr.n.max(1) as f64);
    }
}
