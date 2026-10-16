//! Report #398 — the level curve for levels 1-48
//! ([`blades_lib::features::level_curve`]).
//!
//! * FROM 49 NOTHING MOVED. [`levels_from_49_are_unchanged`] hashes every loot
//!   source the curve touches at character (and chest) levels 49-100 through the
//!   production paths; the pinned digests were printed by the same loops on
//!   fork/main 8a4220d9, before the curve existed.

use std::collections::HashSet;

use blades_lib::features::chests;
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
    jobs_gen::generate(pools, Uuid::from_u128(0x398c_0000 + ch), level, ch as i64, BOUNDARY, NOW).0
}

fn job_dungeon(gd: &blades_lib::game_data::GameData, pools: &Value, job: &Value, level: u16) -> Option<DungeonGeneratedData> {
    jobs_gen::generated_data_for_job(gd, job, &crate::quest::shipped_scaling(), jobs_gen::loot_shift_for(pools, job, level))
}

fn event_dungeon(gd: &blades_lib::game_data::GameData, scaling: &QuestLevelScaling, dungeon: &Uuid, seed: u64, level: i64) -> Option<DungeonGeneratedData> {
    blades_lib::util::quest::generate_for_event_dungeon_for_character(gd, dungeon, Some(seed), scaling.enemy_level(level), scaling, level)
}

/// Character levels the "nothing moved from 49" digests are taken at.
const FROM_49: [i64; 9] = [49, 50, 55, 60, 61, 75, 80, 99, 100];

/// Digests of every curve-touched source at levels 49-100. Pinned on fork/main
/// 8a4220d9 (before the curve) by these same loops.
#[test]
fn levels_from_49_are_unchanged() {
    let ctx = Ctx::load();
    let gd = gd();
    let scaling = crate::quest::shipped_scaling();
    let mut got: Vec<(&str, u64)> = Vec::new();

    let mut ids: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
    ids.sort();
    let mut h = FNV0;
    for l in FROM_49 {
        for id in &ids {
            let t = &ctx.sd.event_quests.templates[*id];
            digest_of(&mut h, &t.rewards_for_level(l));
            digest_of(&mut h, &t.final_reward_for_level(l));
        }
    }
    got.push(("events", h));

    let mut dungeons: Vec<&Uuid> = gd.dungeons.keys().collect();
    dungeons.sort();
    let mut h = FNV0;
    for l in FROM_49 {
        for d in dungeons.iter().step_by(17) {
            for floor_level in [3i64, 30, 60] {
                digest_of(&mut h, &blades_lib::util::dungeon::generate_for_abyss_floor(&gd, d, 4242, floor_level, Some(l)));
            }
        }
    }
    got.push(("abyss_floor", h));

    let mut quests: Vec<&Uuid> = gd.quests.keys().collect();
    quests.sort();
    let mut h = FNV0;
    for l in FROM_49 {
        for q in quests.iter().step_by(5) {
            let r = blades_lib::util::quest::generate_quest_data(&gd, **q, l, &scaling).ok();
            digest_of(&mut h, &r.map(|(q, d)| (serde_json::to_value(q).unwrap(), d)));
        }
    }
    got.push(("quests", h));

    let mut h = FNV0;
    for l in FROM_49 {
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
    for l in FROM_49 {
        for ch in 0..3u128 {
            let jobs = board(&pools, ch, l as u16);
            digest_of(&mut h, &jobs);
            for j in &jobs {
                digest_of(&mut h, &job_dungeon(&gd, &pools, j, l as u16));
            }
        }
    }
    got.push(("jobs", h));

    let mut h = FNV0;
    for l in 49..=100u64 {
        for tier in [-1i64, 1, 2, 3, 4, 5] {
            for n in 0..6u64 {
                let key = format!("c:{tier}:{l}:{n}");
                digest_of(&mut h, &chests::roll_loot(&ctx.sd.chest_loots, tier, l, &key, &HashSet::new()));
            }
        }
    }
    got.push(("treasury", h));

    let mut h = FNV0;
    for l in 49..=100i64 {
        for d in dungeons.iter().step_by(23) {
            for i in 0..8usize {
                digest_of(&mut h, &blades_lib::util::dungeon::abyss_bonus_material(d, 77, d, i, i % 3, l));
            }
        }
    }
    got.push(("abyss_bonus", h));

    let pinned: [(&str, u64); 7] = [
        ("events", 0xb7fb_ff73_8291_6c6d),
        ("abyss_floor", 0xa65b_4ed4_6ed7_3415),
        ("quests", 0x362a_1c64_af1a_ab00),
        ("event_dungeons", 0xc584_e641_48a9_129b),
        ("jobs", 0xa2cf_80e4_eb7c_0cbe),
        ("treasury", 0x55e1_ef7d_2a7e_692b),
        ("abyss_bonus", 0xcffa_3e56_ed08_f64d),
    ];
    assert_eq!(got, pinned.to_vec(), "a source moved for a character at level 49 or above");
}

// ---------------------------------------------------------------------------
// The numbers HauDrauf reviews: docs/level-curve.md
// ---------------------------------------------------------------------------

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use blades_lib::features::level_curve::{self, gear_tier_shares, is_curve_material};

/// The levels the review table is printed at.
const TABLE_LEVELS: [i64; 10] = [5, 10, 15, 20, 25, 30, 35, 40, 45, 48];

/// Gear tier names by unlock level (`store_bundles::required_level`).
fn tier_name(required: u64) -> &'static str {
    match required {
        1 => "Iron/Steel",
        8 => "Silver",
        13 => "Orcish",
        18 => "Dwarven",
        23 => "Elven",
        28 => "Glass",
        33 => "Ebony",
        39 => "Daedric",
        45 => "Dragon",
        _ => "?",
    }
}

fn material_names() -> HashMap<Uuid, String> {
    let raw: Value = super::read("blades_lib/src/material_tiers.json");
    raw["materials"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(id, m)| {
            // The APK's own name for Clay (`ingredient_values.json`, TOWN_MATERIALS).
            let name = match m["name"].as_str().unwrap() {
                "Copper" => "Clay",
                n => n,
            };
            (id.parse().unwrap(), name.to_string())
        })
        .collect()
}

/// Curve-material stack counts of one source.
#[derive(Default)]
struct Mats(BTreeMap<Uuid, f64>);

impl Mats {
    fn add(&mut self, stacks: &HashMap<Uuid, u64>) {
        for id in stacks.keys().filter(|id| is_curve_material(id)) {
            *self.0.entry(*id).or_default() += 1.0;
        }
    }
    fn add_weighted(&mut self, id: Uuid, w: f64) {
        if is_curve_material(&id) {
            *self.0.entry(id).or_default() += w;
        }
    }
    /// The top `k` materials by share, and how many kinds in all.
    fn top(&self, names: &HashMap<Uuid, String>, k: usize) -> String {
        let total: f64 = self.0.values().sum();
        if total <= 0.0 {
            return "-".into();
        }
        let mut v: Vec<(&Uuid, &f64)> = self.0.iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap().then(a.0.cmp(b.0)));
        let mut out: Vec<String> = v
            .iter()
            .take(k)
            .map(|(id, n)| format!("{} {:.0}%", names.get(id).map_or("?", String::as_str), **n * 100.0 / total))
            .collect();
        if v.len() > k {
            out.push(format!("(+{} more)", v.len() - k));
        }
        out.join(", ")
    }
}

/// Gear tier counts of one source.
#[derive(Default)]
struct Gear(BTreeMap<u64, f64>);

impl Gear {
    fn add(&mut self, item: &blades_lib::user_data::Item) {
        if let Some(r) = gear_required_level(&item.item_template_id) {
            *self.0.entry(r).or_default() += 1.0;
        }
    }
    fn shares(&self) -> String {
        let total: f64 = self.0.values().sum();
        if total <= 0.0 {
            return "-".into();
        }
        self.0
            .iter()
            .map(|(r, n)| format!("{} {:.0}%", tier_name(*r), n * 100.0 / total))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The unlock level of a non-jewellery loot-pool template.
fn gear_required_level(template: &Uuid) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct T {
        #[serde(rename = "requiredLevel")]
        required_level: u64,
        category: String,
    }
    #[derive(serde::Deserialize)]
    struct G {
        templates: HashMap<Uuid, T>,
    }
    static POOL: std::sync::OnceLock<HashMap<Uuid, u64>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let g: G = super::read("blades_lib/src/legendary_gear.json");
        g.templates
            .into_iter()
            .filter(|(_, t)| t.category != "ring" && t.category != "necklace")
            .map(|(id, t)| (id, t.required_level))
            .collect()
    })
    .get(template)
    .copied()
}

fn dungeon_sample(gd: &blades_lib::game_data::GameData, level: i64, corpse: &mut Mats, container: &mut Mats, gear: &mut Gear) {
    let mut dungeons: Vec<&Uuid> = gd.dungeons.keys().collect();
    dungeons.sort();
    for d in dungeons.iter().step_by(7) {
        for seed in 0..3u64 {
            let Some(data) = blades_lib::util::dungeon::generate_for_dungeon_with_seed(gd, d, seed, level, 0) else { continue };
            for e in data.enemy_generated_data.values().flatten().flatten() {
                for loot in e.loot_table_loot.values() {
                    corpse.add(&loot.stackable_items);
                    for item in loot.item.0.values() {
                        gear.add(item);
                    }
                }
            }
            for i in data.item_generated_data.values().flatten() {
                for loot in i.loot_table_loot.values() {
                    container.add(&loot.stackable_items);
                }
            }
        }
    }
}

/// The retail tables' own material shares at `level`: the enemy table 871c2e9b's
/// band, and the level-keyed containers' bands (the curve's input).
fn retail_dungeon_shares(level: i64) -> (Mats, Mats) {
    let enemy: Value = super::read("blades_lib/src/enemy_loot.json");
    let floor: Value = super::read("blades_lib/src/interactable_loot_by_level.json");
    let mut corpse = Mats::default();
    for b in enemy["tables"]["871c2e9b-7e7a-4564-a022-e435dfb8a436"]["byLevel"].as_array().unwrap() {
        if b["minEnemyLevel"].as_i64().unwrap() <= level && level <= b["maxEnemyLevel"].as_i64().unwrap() {
            for (id, _, n) in level_curve::pool_from_results(b["results"].as_array().unwrap()) {
                corpse.add_weighted(id, n);
            }
        }
    }
    let mut container = Mats::default();
    for (_, t) in floor["tables"].as_object().unwrap() {
        for b in t["byLevel"].as_array().into_iter().flatten() {
            if (b["minEnemyLevel"].as_i64().unwrap() - level).abs() <= 2 {
                for (id, _, n) in level_curve::pool_from_results(b["results"].as_array().unwrap()) {
                    container.add_weighted(id, n);
                }
            }
        }
    }
    (corpse, container)
}

/// Prints (and with `LEVEL_CURVE_WRITE=1` writes into `docs/level-curve.md`) the
/// review tables: `cargo test -p server level_curve_table -- --ignored --nocapture`.
#[test]
#[ignore]
fn level_curve_table() {
    let ctx = Ctx::load();
    let gd = gd();
    let names = material_names();
    let mut md = String::new();

    // Introduction levels, by APK tier, with what the fade makes of them.
    writeln!(md, "### Crafting materials: introduction level and weight by level\n").unwrap();
    writeln!(md, "`I` is the material's APK tier's unlock level. The weight is the factor on what a source pays it at: full from `I - 2` to `I`, then down to 0 at `I + 20`. Town materials are tier 1. Soul gems and the seven enchanting ingredients are never faded.\n").unwrap();
    let raw: Value = super::read("blades_lib/src/material_tiers.json");
    let mut by_tier: BTreeMap<u64, Vec<(Uuid, String)>> = BTreeMap::new();
    for (id, m) in raw["materials"].as_object().unwrap() {
        let id: Uuid = id.parse().unwrap();
        if is_curve_material(&id) {
            by_tier.entry(m["tier"].as_u64().unwrap()).or_default().push((id, names[&id].clone()));
        }
    }
    write!(md, "| APK tier | I | Materials |").unwrap();
    for l in TABLE_LEVELS {
        write!(md, " L{l} |").unwrap();
    }
    write!(md, "\n|---|---|---|").unwrap();
    for _ in TABLE_LEVELS {
        write!(md, "---|").unwrap();
    }
    writeln!(md).unwrap();
    for (tier, mats) in &mut by_tier {
        mats.sort_by(|a, b| a.1.cmp(&b.1));
        let intro = level_curve::introduction_level(&mats[0].0).unwrap();
        let list = mats.iter().map(|m| m.1.as_str()).collect::<Vec<_>>().join(", ");
        write!(md, "| {tier} | {intro} | {list} |").unwrap();
        for l in TABLE_LEVELS {
            let w = level_curve::material_weight(&mats[0].0, l);
            if w == 0.0 {
                write!(md, " - |").unwrap();
            } else {
                write!(md, " {w:.2} |").unwrap();
            }
        }
        writeln!(md).unwrap();
    }
    writeln!(md).unwrap();

    // Gear: the rule's shares, and what the chests and corpses paid.
    writeln!(md, "### Gear: material tier shares (weapons, armour, shields)\n").unwrap();
    writeln!(md, "The rule (`gear_tier_shares`), and the shares measured over 1,500 chests of every tier (300 each, Wooden to Legendary) and the dungeon corpses at that level. Rings, necklaces and artifacts are not moved.\n").unwrap();
    writeln!(md, "| Level | Rule | Measured |").unwrap();
    writeln!(md, "|---|---|---|").unwrap();
    for l in TABLE_LEVELS {
        let rule = gear_tier_shares(l)
            .iter()
            .map(|(r, s)| format!("{} {:.0}%", tier_name(*r), s * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        let mut gear = Gear::default();
        for tier in 1..=5i64 {
            for n in 0..300u64 {
                let g = chests::roll_loot(&ctx.sd.chest_loots, tier, l as u64, &format!("t:{tier}:{l}:{n}"), &HashSet::new()).unwrap();
                for i in &g.items {
                    gear.add(&i.item);
                }
            }
        }
        let (mut c, mut f) = (Mats::default(), Mats::default());
        dungeon_sample(&gd, l, &mut c, &mut f, &mut gear);
        writeln!(md, "| {l} | {rule} | {} |", gear.shares()).unwrap();
    }

    // Materials, before (retail's tables) and after (the curve), per source.
    let mut dungeon_rows = String::new();
    let mut chest_rows = String::new();
    let mut other_rows = String::new();
    for l in TABLE_LEVELS {
        let (rc, rf) = retail_dungeon_shares(l);
        let (mut c, mut f, mut g) = (Mats::default(), Mats::default(), Gear::default());
        dungeon_sample(&gd, l, &mut c, &mut f, &mut g);
        writeln!(dungeon_rows, "| {l} | corpses | {} | {} |", rc.top(&names, 5), c.top(&names, 5)).unwrap();
        writeln!(dungeon_rows, "| {l} | containers | {} | {} |", rf.top(&names, 5), f.top(&names, 5)).unwrap();
        for (tier, label) in [(1i64, "Wooden"), (2, "Silver"), (3, "Gold"), (4, "Elder"), (5, "Legendary")] {
            let (mut before, mut after) = (Mats::default(), Mats::default());
            for n in 0..400u64 {
                let key = format!("m:{tier}:{l}:{n}");
                before.add(&chests::roll_retail_loot(&ctx.sd.chest_loots, tier, l as u64, &key, &HashSet::new()).unwrap().stackable_items);
                after.add(&chests::roll_loot(&ctx.sd.chest_loots, tier, l as u64, &key, &HashSet::new()).unwrap().stackable_items);
            }
            writeln!(chest_rows, "| {l} | {label} | {} | {} |", before.top(&names, 5), after.top(&names, 5)).unwrap();
        }
        let (mut before, mut after) = (Mats::default(), Mats::default());
        let mut story: Vec<(&Uuid, &blades_lib::economy::RewardGrant)> = ctx.sd.quest_rewards.iter().collect();
        story.sort_by_key(|(id, _)| **id);
        for (id, r) in story {
            before.add(&r.stackable_items);
            for ch in 0..10u128 {
                after.add(&crate::quest::story_reward_at_level(r.clone(), *id, Uuid::from_u128(ch), l).stackable_items);
            }
        }
        writeln!(other_rows, "| {l} | story-quest rewards | {} | {} |", before.top(&names, 5), after.top(&names, 5)).unwrap();
        let mut bonus = Mats::default();
        let mut dungeons: Vec<&Uuid> = gd.dungeons.keys().collect();
        dungeons.sort();
        for d in dungeons.iter().step_by(5) {
            for i in 0..20usize {
                if let Some((id, _)) = blades_lib::util::dungeon::abyss_bonus_material(d, 3, d, i, i % 3, l) {
                    bonus.add_weighted(id, 1.0);
                }
            }
        }
        writeln!(other_rows, "| {l} | Abyss bonus material | | {} |", bonus.top(&names, 5)).unwrap();
    }
    writeln!(md, "\n### Crafting materials: top shares by source\n").unwrap();
    writeln!(md, "Share of the stacks of curve materials (soul gems and the seven enchanting ingredients are left out: they are not faded and their shares do not move). Before = retail's tables at that level; after = the curve.\n").unwrap();
    writeln!(md, "| Level | Source | Before | After |\n|---|---|---|---|\n{dungeon_rows}").unwrap();
    writeln!(md, "| Level | Chest | Before | After |\n|---|---|---|---|\n{chest_rows}").unwrap();
    writeln!(md, "| Level | Source | Before | After |\n|---|---|---|---|\n{other_rows}").unwrap();

    println!("{md}");
    if std::env::var("LEVEL_CURVE_WRITE").is_ok() {
        let path = super::repo().join("docs/level-curve.md");
        let doc = std::fs::read_to_string(&path).unwrap_or_default();
        let (start, end) = ("<!-- level-curve-table:start -->", "<!-- level-curve-table:end -->");
        let (Some(a), Some(b)) = (doc.find(start), doc.find(end)) else {
            panic!("docs/level-curve.md lacks the {start} / {end} markers");
        };
        let out = format!("{}{start}\n\n{md}\n{}", &doc[..a], &doc[b..]);
        std::fs::write(&path, out).unwrap();
    }
}

/// Story-quest rewards and event rewards below 49: no crafting material the curve
/// has faded out (an event's own ladder materials step by #362, and it pays town
/// materials at every level as retail did; both are left as they are), gold, XP and chests as before, every weapon, armour piece and shield
/// in the level's window; from 49 the reward is exactly the table's.
#[test]
fn story_and_event_rewards_follow_the_curve() {
    use blades_lib::features::event_materials::ladders;
    use blades_lib::features::level_curve::material_weight;
    let ctx = Ctx::load();
    // An event's ladder materials and its town materials are retail's own level
    // rules and are left as they are (`EventQuestTemplate::apply_level_curve`).
    let in_ladder = |id: &Uuid| {
        ladders().iter().any(|l| l.contains(id)) || blades_lib::util::dungeon::TOWN_MATERIALS.contains(&id.as_u128())
    };
    let mut story: Vec<(&Uuid, &blades_lib::economy::RewardGrant)> = ctx.sd.quest_rewards.iter().collect();
    story.sort_by_key(|(id, _)| **id);
    let (mut stacks, mut moved) = (0usize, 0usize);
    for level in 1..=60i64 {
        let window: Vec<u64> = gear_tier_shares(level).into_iter().map(|(r, _)| r).collect();
        for (id, table) in &story {
            for ch in 0..3u128 {
                let paid = crate::quest::story_reward_at_level((*table).clone(), **id, Uuid::from_u128(ch), level);
                assert_eq!(paid, crate::quest::story_reward_at_level((*table).clone(), **id, Uuid::from_u128(ch), level), "deterministic");
                if level >= 49 {
                    assert_eq!(&paid, *table, "{id} at {level}");
                    continue;
                }
                assert_eq!(paid.currencies, table.currencies);
                assert_eq!((paid.character_xp, paid.town_xp, paid.chests.len(), paid.items.len()), (table.character_xp, table.town_xp, table.chests.len(), table.items.len()));
                moved += usize::from(&paid != *table);
                for item in paid.stackable_items.keys().filter(|i| is_curve_material(i)) {
                    stacks += 1;
                    assert!(material_weight(item, level) > 0.0, "{id} at {level}: faded {item}");
                }
                for r in &paid.items {
                    if let Some(req) = gear_required_level(&r.item.item_template_id) {
                        assert!(window.contains(&req), "{id} at {level}: gear tier {req}");
                    }
                }
            }
        }
        let mut ids: Vec<&Uuid> = ctx.sd.event_quests.templates.keys().collect();
        ids.sort();
        for id in ids {
            let t = &ctx.sd.event_quests.templates[id];
            if level >= 49 {
                continue;
            }
            for r in t.rewards_for_level(level).iter().chain(t.final_reward_for_level(level).iter()) {
                for item in r.stackable_items.keys().filter(|i| is_curve_material(i) && !in_ladder(i)) {
                    stacks += 1;
                    assert!(material_weight(item, level) > 0.0, "event {id} at {level}: faded {item}");
                }
            }
        }
    }
    assert!(stacks > 1_000 && moved > 100, "{stacks} stacks, {moved} rewards moved");
}
