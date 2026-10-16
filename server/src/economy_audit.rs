//! Economy logic audit (#392): every shop product and chest, checked against the
//! game's own rules rather than against the few retail data points we hold.
//!
//! THE REPORT. HauDrauf, after a run of loot bugs our data-driven tests missed —
//! frozen captured items on every purchase (#386), arcane rings drawn from 32
//! recorded outcomes so four spells were unreachable (#392), Wooden chests that
//! were 79% Quicksilver (#381), the same jewellery again and again: "Build a
//! system that monitors all shop items and chests until everything runs right.
//! Check everything for logic, not on the few data points."
//!
//! THE PATTERN those bugs share: a generator copied or sampled a THIN retail
//! corpus, and the tests compared against that same corpus. The APK says what is
//! POSSIBLE (which GRADING a ring can carry, which enchantments a template takes,
//! which offer a level can see); this audit checks generated output against that.
//!
//! WHAT IT DOES. For every sellable global-shop product (Sigil shop, store
//! chests, the rest of the storefront), every chest kind (tutorial, Wooden,
//! Silver, Gold, Elder, Legendary, the daily chests and the store chests) and the
//! four town merchants, it calls the REAL generator the handler calls, many times,
//! with varied nonces and character ids, at character levels 1-100, and checks:
//!
//! * **Validity** (asserted): every item obeys the rules the prod jewellery audit
//!   uses — slot-valid GRADING, `grade == sum(tiers)`, no repeated property,
//!   ENCHANTING count 0 or arcane tier + 1, enchantments from the APK tables at an
//!   APK tier, required level within reach of the character.
//! * **Variety** (asserted where the APK definition is not fixed): no rolled
//!   product or chest hands out one item instance (template + every property) or
//!   one whole reward in more than [`VARIETY_MAX_SHARE`] of rolls.
//! * **Coverage** (reported): every GRADING property, enchantment and template the
//!   APK allows for that slot / tier is reachable within the rolls; every potion a
//!   merchant sells at the tiers a building level stocks is in that level's pool
//!   (#397). A merchant level whose pool is no larger than its catalog is a frozen
//!   catalog (Variety).
//! * **Level fit** (reported): no single metal or enchanting ingredient above
//!   [`SINGLE_KIND_MAX_SHARE`] of a chest's stacks where other kinds are paid at
//!   that level.
//! * **Shop availability** (reported): which offers the storefront shows a
//!   character at each level over a whole replay cycle, after the APK level gate
//!   the client applies; offers that are listed but cannot be bought; caps that
//!   never reset.
//!
//! Known failures live in [`KNOWN_FAILURES`] with a reason, so CI stays green and
//! the list shrinks as fixes land.
//!
//! The report: `ECONOMY_AUDIT_WRITE=1 cargo test -p server economy_audit -- --nocapture`
//! rewrites the generated part of `docs/economy-audit.md`. Without the variable
//! the test only asserts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use blades_lib::economy::RewardGrant;
use blades_lib::features::{chests, store_bundles};
use blades_lib::game_data::GameDataItem;
use blades_lib::static_data::StaticData;
use blades_lib::user_data::Item;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::jewelry_grade::{self, NECKLACE, RING};

/// The character levels every level-dependent generator is sampled at.
const LEVELS: [u64; 10] = [1, 10, 20, 30, 40, 45, 49, 60, 80, 100];

/// Rolls per (product or chest, level). `ECONOMY_AUDIT_ROLLS` overrides it.
const DEFAULT_ROLLS: u64 = 500;

/// No rolled item instance (or whole reward) may come out more often than this.
const VARIETY_MAX_SHARE: f64 = 0.25;

/// Level fit: one metal / enchanting ingredient above this share of a chest's
/// stacks of that family, where other kinds are paid at the level, is flagged.
const SINGLE_KIND_MAX_SHARE: f64 = 0.60;

/// Level fit: more than this share of a chest's gear above the character's
/// level plus [`REQUIRED_LEVEL_LOOKAHEAD`] is flagged.
const ABOVE_LEVEL_MAX_SHARE: f64 = 0.10;

/// The fewest stacks of a family a level-fit share is computed over.
const LEVEL_FIT_MIN_STACKS: usize = 20;

/// How far above a character's level retail itself paid gear: the Legendary
/// chest's top slot shows the next tier up to two levels early
/// (`store_bundles::LOOKAHEAD_LEVELS`).
const REQUIRED_LEVEL_LOOKAHEAD: u64 = 2;

/// Five fixed buyers, so the rolls cover several characters' sequences.
const BUYERS: [u128; 5] = [
    0xf7817aa6_5892_4794_871e_9b51a475a606, // Sephoris
    0x0b0e0000_0000_4000_8000_000000000001,
    0x0b0e0000_0000_4000_8000_000000000002,
    0x0b0e0000_0000_4000_8000_000000000003,
    0x0b0e0000_0000_4000_8000_000000000004,
];

/// The enchanting ingredients (the non-soul-gem inputs of `enchanting.json`).
const ENCHANTING_INGREDIENTS: [u128; 7] = [
    0x05b4dd6b_796f_4088_a772_0e33ea3db976, // Frost Salts
    0xf1c5c03c_5297_48fb_a9d2_5f32bffb467c, // Fire Salts
    0x10c54e43_a2a1_4833_9491_4c19149f43b5, // Void Salts
    0x9687d83c_aa7b_4cf3_a69f_0bba8204fa61, // Glow Dust
    0x07f380dd_f123_4ef7_9b12_8b9fdb03c3e1, // Imp Stool
    0x7116a2a8_ac2d_4cd9_8b7c_b80c397d3f50, // Honeycomb
    0x8b7d0044_3a38_4ee0_af45_d2892b0f508d, // Beehive Husk
];

/// The three store chests at full price; their 1-gem promo twins share a corpus.
const STORE_CHESTS: [(u128, &str); 3] = [
    (0x0e224ca0_1506_490f_884a_8871ffe6399b, "Golden (Rare) store chest"),
    (0x7bf00a9c_6a08_4b55_a60d_53915baa38a3, "Epic (Elder) store chest"),
    (0x1275d959_bbe5_460d_8f6a_1c31106a8eb2, "Legendary store chest"),
];

/// One cycle of the replayed shop calendar starts here: 2026-10-15 00:00 UTC.
/// Fixed so the report is reproducible; the calendar repeats every 42 days.
const AVAILABILITY_T0: i64 = 1_792_022_400;
const AVAILABILITY_DAYS: i64 = 42;

// ---------------------------------------------------------------------------
// Known failures
// ---------------------------------------------------------------------------

/// A hard-asserted finding that is known and tracked, matched on a subject-key
/// prefix, the check and a substring of the finding. Remove the entry when the fix
/// lands: a stale entry is listed as such in the report's allowlist section.
struct Known {
    subject: &'static str,
    check: Check,
    contains: &'static str,
    reason: &'static str,
}

const KNOWN_FAILURES: &[Known] = &[
    Known {
        subject: "chest:",
        check: Check::Validity,
        contains: "but no GRADING properties",
        reason: "Silver/Gold treasury (and daily) chest rings and necklaces carry a grade but no GRADING: \
                 `script/extract_chest_loot.py` keeps only ENCHANTING when an item has any, so 77 of the \
                 100 retail chest jewels (every enchanted one) lost their bonuses. Fix: \
                 fix/audit-chest-jewelry-grading (#525).",
    },
    Known {
        subject: "chest:",
        check: Check::Variety,
        contains: "frozen item",
        reason: "Treasury tiers 1-3 copy one retail piece per item slot from the nearest openings; where few \
                 of them paid an item (Wooden: 44 of 154) one piece is every item. Fix: \
                 fix/audit-treasury-gear-generated (#527; generated around the retail piece, as Legendary chests).",
    },
    Known {
        subject: "chest:",
        check: Check::Variety,
        contains: "repeating jewellery",
        reason: "Same cause as the frozen items: one retail ring/necklace instance per slot, verbatim. Fix: \
                 fix/audit-treasury-gear-generated.",
    },
    Known {
        subject: "merchant:",
        check: Check::Variety,
        contains: "frozen catalog",
        reason: "Building levels with ONE retail catalog (Forge 5-6, Workshop 2, Alchemist 1 and 6) were \
                 measured as exactly that catalog, so every window lists it again. One catalog says what \
                 the level CAN sell, not how often; widening it needs a model of the neighbouring levels \
                 (follow-up to #397, which fixed the level-9 Alchemist's rare potions).",
    },
    Known {
        subject: "chest:treasury:wooden",
        check: Check::Validity,
        contains: "requires level",
        reason: "The Wooden table is thin (44 openings with an item), so a level-10 chest's neighbourhood \
                 reaches openings at 20+ and pays Elven gear (+13; retail's own Wooden max is +10). Pinning \
                 items to the level would NARROW what players get today: open question in the report.",
    },
];

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApkFactsRaw {
    item_required_level: HashMap<Uuid, u64>,
    /// String-keyed: the APK catalogue holds one malformed id (`…e23969f7p`).
    offer_level_gates: HashMap<String, Gate>,
}

struct ApkFacts {
    item_required_level: HashMap<Uuid, u64>,
    offer_level_gates: HashMap<Uuid, Gate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApkNames {
    offer_level_gates: HashMap<String, OfferName>,
}

#[derive(Deserialize, Clone, Copy, Debug)]
struct Gate {
    min: u64,
    max: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfferName {
    #[serde(default)]
    editor_name: Option<String>,
}

impl Gate {
    fn admits(self, level: u64) -> bool {
        (self.min == 0 && self.max == 0) || (self.min <= level && level <= self.max)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegendaryGear {
    templates: HashMap<Uuid, LegendaryTemplate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegendaryTemplate {
    required_level: u64,
}

struct Ctx {
    sd: StaticData,
    items: HashMap<Uuid, GameDataItem>,
    repair: blades_lib::features::repair::RepairData,
    apk: ApkFacts,
    offer_names: HashMap<Uuid, String>,
    /// APK loot pool templates per material tier (as its unlock level).
    tier_pool: BTreeMap<u64, BTreeSet<Uuid>>,
    shop_stock: crate::shop_gen::ShopStockConfig,
    /// Every property an APK enchant recipe can put first, with its tiers.
    recipe_tiers: HashMap<Uuid, BTreeSet<u64>>,
    enchant_tier_range: (u64, u64),
    rolls: u64,
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read<T: for<'de> Deserialize<'de>>(rel: &str) -> T {
    let p = repo().join(rel);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

impl Ctx {
    fn load() -> Ctx {
        let sd = crate::static_loader::load(&repo().join("deploy/static"));
        let gd: blades_lib::game_data::GameData = read("deploy/static/parsed.json");
        let durability: Value = read("deploy/static/item_durability.json");
        let repair =
            blades_lib::features::repair::RepairData::from_json(&durability, &serde_json::json!({}));
        let raw: ApkFactsRaw = read("server/data/economy_audit_apk.json");
        let apk = ApkFacts {
            item_required_level: raw.item_required_level,
            offer_level_gates: raw
                .offer_level_gates
                .into_iter()
                .filter_map(|(k, g)| Some((Uuid::parse_str(&k).ok()?, g)))
                .collect(),
        };
        let names: ApkNames = read("server/data/economy_audit_apk.json");
        let offer_names = names
            .offer_level_gates
            .into_iter()
            .filter_map(|(id, n)| Some((Uuid::parse_str(&id).ok()?, n.editor_name?)))
            .collect();
        let gear: LegendaryGear = read("blades_lib/src/legendary_gear.json");
        let mut tier_pool: BTreeMap<u64, BTreeSet<Uuid>> = BTreeMap::new();
        for (id, t) in &gear.templates {
            tier_pool.entry(t.required_level).or_default().insert(*id);
        }
        let shop_stock: crate::shop_gen::ShopStockConfig = read("deploy/static/shop_stock.json");
        let mut recipe_tiers: HashMap<Uuid, BTreeSet<u64>> = HashMap::new();
        for r in sd.enchanting.recipes.values() {
            recipe_tiers.entry(r.property).or_default().insert(r.tier);
        }
        let tiers: Vec<u64> = recipe_tiers.values().flatten().copied().collect();
        let enchant_tier_range = (
            tiers.iter().copied().min().unwrap_or(1),
            tiers.iter().copied().max().unwrap_or(10),
        );
        let rolls = std::env::var("ECONOMY_AUDIT_ROLLS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_ROLLS);
        Ctx {
            sd,
            items: gd.items_template,
            repair,
            apk,
            offer_names,
            tier_pool,
            shop_stock,
            recipe_tiers,
            enchant_tier_range,
            rolls,
        }
    }

    fn name(&self, id: &Uuid) -> String {
        self.items
            .get(id)
            .map(|i| i.name.trim_start_matches("Items.Name.").to_string())
            .unwrap_or_else(|| short(id))
    }

    fn item_type(&self, id: &Uuid) -> Option<u64> {
        self.items.get(id).map(|i| i.r#type)
    }

    fn is_jewelry(&self, id: &Uuid) -> bool {
        matches!(self.item_type(id), Some(RING | NECKLACE))
    }
}

fn short(id: &Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn buyer(i: u64) -> Uuid {
    Uuid::from_u128(BUYERS[(i % BUYERS.len() as u64) as usize])
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
enum Check {
    Validity,
    Variety,
    Coverage,
    LevelFit,
    Availability,
}

impl Check {
    fn label(self) -> &'static str {
        match self {
            Check::Validity => "Validity",
            Check::Variety => "Variety",
            Check::Coverage => "Coverage",
            Check::LevelFit => "Level fit",
            Check::Availability => "Availability",
        }
    }
    /// Hard-asserted checks fail the test; the rest are report warnings.
    fn asserted(self) -> bool {
        matches!(self, Check::Validity | Check::Variety)
    }
}

/// One audited subject: a product, a chest kind, a merchant.
struct Subject {
    section: &'static str,
    key: String,
    name: String,
    how: String,
    levels: String,
    rolls: u64,
    findings: Vec<(Check, String)>,
    notes: Vec<String>,
}

impl Subject {
    fn new(section: &'static str, key: String, name: String, how: String) -> Subject {
        Subject {
            section,
            key,
            name,
            how,
            levels: String::new(),
            rolls: 0,
            findings: Vec::new(),
            notes: Vec::new(),
        }
    }
    fn flag(&mut self, check: Check, detail: String) {
        if !self.findings.iter().any(|(c, d)| *c == check && *d == detail) {
            self.findings.push((check, detail));
        }
    }
    fn known(&self, check: Check, detail: &str) -> Option<&'static Known> {
        KNOWN_FAILURES.iter().find(|k| {
            k.check == check && self.key.starts_with(k.subject) && detail.contains(k.contains)
        })
    }
    fn status(&self) -> &'static str {
        if self.findings.iter().any(|(c, _)| c.asserted()) {
            "FLAG"
        } else if self.findings.is_empty() {
            "PASS"
        } else {
            "WARN"
        }
    }
}

// ---------------------------------------------------------------------------
// Item rules
// ---------------------------------------------------------------------------

/// An item instance's identity for variety: template and every property, but not
/// its instance id or its (temper-derived) durability.
fn item_key(i: &Item) -> String {
    let mut e: Vec<(Uuid, u64)> = i.properties.enchanting.iter().map(|p| (p.id, p.tier)).collect();
    e.sort();
    let mut g: Vec<(Uuid, u64)> = i.properties.grading.iter().map(|p| (p.id, p.tier)).collect();
    g.sort();
    format!(
        "{}|g{:?}|t{}|a{:?}|e{:?}|r{:?}",
        i.item_template_id, i.grade, i.tempering_level, i.arcane_tier, e, g
    )
}

fn reward_key(r: &RewardGrant) -> String {
    let mut c: Vec<_> = r.currencies.iter().collect();
    c.sort();
    let mut s: Vec<_> = r.stackable_items.iter().collect();
    s.sort();
    let mut i: Vec<String> = r.items.iter().map(|x| item_key(&x.item)).collect();
    i.sort();
    format!("{c:?}|{s:?}|{i:?}|{:?}", r.chests.len())
}

/// How far above the character's level an item may require, and why.
#[derive(Clone, Copy)]
enum LevelRule {
    /// A storefront offer: the APK level gate decides who sees it.
    Gated,
    /// A chest or bundle at `level`. `slack` is how far above it retail ITSELF
    /// paid gear from this source (the measured maximum, at least
    /// [`REQUIRED_LEVEL_LOOKAHEAD`]); beyond that an item is invalid.
    Character { level: u64, slack: u64 },
}

/// Every rule the prod jewellery audit holds an item to, plus the enchantment and
/// level rules. Returns the violations, each a short phrase.
fn validate(ctx: &Ctx, item: &Item, rule: LevelRule) -> Vec<String> {
    let mut out = Vec::new();
    let t = item.item_template_id;
    let name = ctx.name(&t);
    let Some(kind) = ctx.item_type(&t) else {
        out.push(format!("unknown template {}", short(&t)));
        return out;
    };
    let fixed = crate::fixed_templates::has_mandatory_properties(t);
    let grading = &item.properties.grading;
    let enchanting = &item.properties.enchanting;

    // GRADING.
    if matches!(kind, RING | NECKLACE) && !fixed {
        match (item.grade, grading.is_empty()) {
            (None, true) => out.push(format!("{name}: ungraded (APK _alwaysGradedItemTypes [10, 11])")),
            (Some(g), true) => out.push(format!(
                "{name}: grade {g} but no GRADING properties (its ability bonuses are missing)"
            )),
            _ => {}
        }
        for p in grading {
            if !jewelry_grade::slot_allows(kind, p.id) {
                out.push(format!("{name}: GRADING {} not on the slot's APK list", short(&p.id)));
            }
            if !(1..=2).contains(&p.tier) {
                out.push(format!("{name}: GRADING tier {} outside 1..=2", p.tier));
            }
        }
        let sum: u64 = grading.iter().map(|p| p.tier).sum();
        if !grading.is_empty() && item.grade != Some(sum) {
            out.push(format!("{name}: grade {:?} != sum of tiers {sum}", item.grade));
        }
        if item.arcane_tier.unwrap_or(0) == 0 && !grading.is_empty() {
            let mut tiers: Vec<u64> = grading.iter().map(|p| p.tier).collect();
            tiers.sort_unstable_by(|a, b| b.cmp(a));
            let authored = jewelry_grade::JEWELRY_GRADES
                .iter()
                .any(|(g, _, bonuses)| Some(*g) == item.grade && *bonuses == tiers.as_slice());
            if !authored {
                out.push(format!(
                    "{name}: grade {:?} tiers {tiers:?} not a JewelryGradeData row",
                    item.grade
                ));
            }
        }
        if !grading.is_empty() && (item.tempering_level != 0 || item.durability != 0.0) {
            out.push(format!("{name}: graded jewellery carries tempering/durability"));
        }
    } else if !matches!(kind, RING | NECKLACE) {
        if item.grade.is_some() || !grading.is_empty() {
            out.push(format!("{name}: non-jewellery carries a grade/GRADING"));
        }
        if !blades_lib::economy::template_skips_durability(t, &ctx.items)
            && ctx.repair.max_durability(t, item.tempering_level).is_some()
            && item.durability <= 0.0
        {
            out.push(format!("{name}: wearable gear with durability {}", item.durability));
        }
    }
    let mut seen = HashSet::new();
    for p in grading {
        if !seen.insert(p.id) {
            out.push(format!("{name}: GRADING {} twice", short(&p.id)));
        }
    }

    // ENCHANTING.
    let mut seen = HashSet::new();
    let table: HashSet<Uuid> = ctx
        .sd
        .enchanting
        .table_for(&t)
        .map(|tb| tb.properties.iter().map(|p| p.id).collect())
        .unwrap_or_default();
    let (lo, hi) = ctx.enchant_tier_range;
    for p in enchanting {
        if !seen.insert(p.id) {
            out.push(format!("{name}: ENCHANTING {} twice", short(&p.id)));
        }
        if !(lo..=hi).contains(&p.tier) {
            out.push(format!("{name}: enchant tier {} outside APK {lo}..={hi}", p.tier));
        }
        if !fixed && !table.contains(&p.id) && !ctx.recipe_tiers.contains_key(&p.id) {
            out.push(format!(
                "{name}: ENCHANTING {} is neither an APK recipe nor in the template's table",
                short(&p.id)
            ));
        }
    }
    if let Some(a) = item.arcane_tier.filter(|a| *a > 0) {
        let n = enchanting.len() as u64;
        if !fixed && n != 0 && n != a + 1 {
            out.push(format!("{name}: {n} enchantments on arcane tier {a} (0 or {})", a + 1));
        }
    } else if !fixed && enchanting.len() > 3 {
        out.push(format!("{name}: {} enchantments (APK max primary + 2)", enchanting.len()));
    }

    // Required level.
    if let LevelRule::Character { level, slack } = rule {
        let required = ctx.apk.item_required_level.get(&t).copied().unwrap_or(0);
        if !store_bundles::is_artifact(&t) && required > level + slack {
            out.push(format!(
                "{name}: requires level {required} at character level {level} (retail paid at most +{slack})"
            ));
        }
    }
    out
}

/// The share of the commonest value among `keys`, and that value.
fn modal_share<'a>(keys: impl IntoIterator<Item = &'a String>) -> (f64, usize, String) {
    let mut counts: HashMap<&String, usize> = HashMap::new();
    let mut n = 0usize;
    for k in keys {
        *counts.entry(k).or_insert(0) += 1;
        n += 1;
    }
    let Some((k, c)) = counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0))) else {
        return (0.0, 0, String::new());
    };
    (c as f64 / n.max(1) as f64, c, k.clone())
}

fn describe_key(ctx: &Ctx, key: &str) -> String {
    let template = key.split('|').next().and_then(|t| Uuid::parse_str(t).ok());
    match template {
        Some(t) => {
            let grade = key.split("|g").nth(1).and_then(|s| s.split('|').next()).unwrap_or("");
            format!("{} (grade {grade})", ctx.name(&t))
        }
        None => "the same whole reward".to_string(),
    }
}

/// Coverage of the GRADING lists by the jewellery in `items`, per slot.
fn grading_coverage(ctx: &Ctx, items: &[&Item]) -> Vec<String> {
    let mut out = Vec::new();
    for (slot, label) in [(RING, "ring"), (NECKLACE, "necklace")] {
        let mut seen = HashSet::new();
        let mut any = false;
        for i in items {
            if ctx.item_type(&i.item_template_id) != Some(slot)
                || crate::fixed_templates::has_mandatory_properties(i.item_template_id)
            {
                continue;
            }
            any = true;
            seen.extend(i.properties.grading.iter().map(|p| p.id));
        }
        if !any {
            continue;
        }
        let all: Vec<Uuid> = crate::arena::combat::gamedata::GRADE_PROPERTIES
            .iter()
            .filter(|g| g.slot == if slot == RING { "Ring" } else { "Necklace" })
            .filter_map(|g| Uuid::parse_str(g.uuid).ok())
            .collect();
        let missing: Vec<&Uuid> = all.iter().filter(|p| !seen.contains(p)).collect();
        if !missing.is_empty() {
            out.push(format!(
                "{label} GRADING: {}/{} APK properties reached, missing {}",
                all.len() - missing.len(),
                all.len(),
                missing.iter().map(|p| short(p)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    out
}

/// Coverage of the enchant table by rolled jewellery with authored enchantments.
fn enchant_coverage(ctx: &Ctx, items: &[&Item]) -> Vec<String> {
    let mut by_template: BTreeMap<Uuid, HashSet<Uuid>> = BTreeMap::new();
    for i in items {
        if i.properties.enchanting.is_empty()
            || crate::fixed_templates::has_mandatory_properties(i.item_template_id)
        {
            continue;
        }
        by_template
            .entry(i.item_template_id)
            .or_default()
            .extend(i.properties.enchanting.iter().map(|p| p.id));
    }
    let mut out = Vec::new();
    for (t, seen) in by_template {
        let Some(table) = ctx.sd.enchanting.table_for(&t) else {
            continue;
        };
        let allowed: Vec<Uuid> = table.properties.iter().filter(|p| p.weight > 0.0).map(|p| p.id).collect();
        let missing = allowed.iter().filter(|p| !seen.contains(p)).count();
        if missing > 0 {
            out.push(format!(
                "{}: {}/{} weighted APK enchantments reached",
                ctx.name(&t),
                allowed.len() - missing,
                allowed.len()
            ));
        }
    }
    out
}

/// Coverage of the APK loot pool's templates at each material tier paid.
fn template_coverage(ctx: &Ctx, items: &[&Item]) -> Vec<String> {
    let mut by_tier: BTreeMap<u64, HashSet<Uuid>> = BTreeMap::new();
    for i in items {
        let t = i.item_template_id;
        if store_bundles::is_artifact(&t) {
            continue;
        }
        if let Some((tier, _)) = ctx.tier_pool.iter().find(|(_, pool)| pool.contains(&t)) {
            by_tier.entry(*tier).or_default().insert(t);
        }
    }
    let mut out = Vec::new();
    for (tier, seen) in by_tier {
        let pool = &ctx.tier_pool[&tier];
        if seen.len() < pool.len() {
            out.push(format!(
                "tier {tier} (req. level) gear: {}/{} APK loot-pool templates reached",
                seen.len(),
                pool.len()
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Sampling a generator
// ---------------------------------------------------------------------------

/// Everything one (subject, level) sample produced.
#[derive(Default)]
struct Sample {
    rewards: Vec<RewardGrant>,
}

impl Sample {
    fn items(&self) -> Vec<&Item> {
        self.rewards.iter().flat_map(|r| r.items.iter().map(|i| &i.item)).collect()
    }
}

/// Validity + variety over one sample. `rolled_items` says whether the APK
/// definition leaves the items to a roll (variety is asserted only then);
/// `rolled_reward` the same for the whole reward.
fn judge(
    ctx: &Ctx,
    s: &mut Subject,
    sample: &Sample,
    level: Option<u64>,
    rule: LevelRule,
    rolled_items: bool,
    rolled_reward: bool,
) {
    let at = level.map(|l| format!(" at L{l}")).unwrap_or_default();
    let mut bad: BTreeMap<String, usize> = BTreeMap::new();
    for item in sample.items() {
        for v in validate(ctx, item, rule) {
            *bad.entry(v).or_insert(0) += 1;
        }
    }
    let n_items = sample.items().len();
    for (v, n) in bad {
        s.flag(Check::Validity, format!("{v} ({n}/{n_items} items{at})"));
    }
    if let LevelRule::Character { level, .. } = rule {
        let above = sample
            .items()
            .iter()
            .filter(|i| !store_bundles::is_artifact(&i.item_template_id))
            .filter(|i| {
                ctx.apk.item_required_level.get(&i.item_template_id).copied().unwrap_or(0)
                    > level + REQUIRED_LEVEL_LOOKAHEAD
            })
            .count();
        if n_items >= 20 && above as f64 / n_items as f64 > ABOVE_LEVEL_MAX_SHARE {
            s.flag(
                Check::LevelFit,
                format!(
                    "{above}/{n_items} items ({:.0}%) require more than level {} at L{level}",
                    above as f64 * 100.0 / n_items as f64,
                    level + REQUIRED_LEVEL_LOOKAHEAD
                ),
            );
        }
    }
    if rolled_reward && sample.rewards.len() > 1 {
        let keys: Vec<String> = sample.rewards.iter().map(reward_key).collect();
        let (share, c, _) = modal_share(&keys);
        if share > VARIETY_MAX_SHARE {
            s.flag(
                Check::Variety,
                format!(
                    "one whole reward in {c}/{} rolls ({:.0}%){at}",
                    keys.len(),
                    share * 100.0
                ),
            );
        }
    }
    if rolled_items {
        let keys: Vec<String> = sample.items().iter().map(|i| item_key(i)).collect();
        if keys.len() >= 20 {
            let (share, c, k) = modal_share(&keys);
            if share > VARIETY_MAX_SHARE {
                s.flag(
                    Check::Variety,
                    format!(
                        "frozen item: {} is {c}/{} items ({:.0}%){at}",
                        describe_key(ctx, &k),
                        keys.len(),
                        share * 100.0
                    ),
                );
            }
        }
        // Jewellery specifically: the "same jewellery again and again" complaint.
        let jewels: Vec<String> = sample
            .items()
            .iter()
            .filter(|i| ctx.is_jewelry(&i.item_template_id))
            .filter(|i| !crate::fixed_templates::has_mandatory_properties(i.item_template_id))
            .map(|i| item_key(i))
            .collect();
        if jewels.len() >= 20 {
            let (share, c, k) = modal_share(&jewels);
            if share > VARIETY_MAX_SHARE {
                s.flag(
                    Check::Variety,
                    format!(
                        "repeating jewellery: {} is {c}/{} rings/necklaces ({:.0}%){at}",
                        describe_key(ctx, &k),
                        jewels.len(),
                        share * 100.0
                    ),
                );
            }
        }
    }
}

/// Level fit over the stackables of one sample: metals and enchanting
/// ingredients, against the kinds paid anywhere at `level`.
fn level_fit(ctx: &Ctx, s: &mut Subject, sample: &Sample, level: u64, available: &LevelKinds) {
    let metals: HashSet<Uuid> = chests::METALS.iter().map(|m| Uuid::from_u128(*m)).collect();
    let salts: HashSet<Uuid> = ENCHANTING_INGREDIENTS.iter().map(|m| Uuid::from_u128(*m)).collect();
    for (family, set, avail) in [
        ("metal", &metals, available.metals.get(&level)),
        ("enchanting ingredient", &salts, available.salts.get(&level)),
    ] {
        let mut counts: BTreeMap<Uuid, usize> = BTreeMap::new();
        for r in &sample.rewards {
            for id in r.stackable_items.keys().filter(|id| set.contains(id)) {
                *counts.entry(*id).or_insert(0) += 1;
            }
        }
        let total: usize = counts.values().sum();
        let kinds = avail.map(|a| a.len()).unwrap_or(0).max(counts.len());
        if total < LEVEL_FIT_MIN_STACKS || kinds < 2 {
            continue;
        }
        if let Some((id, n)) = counts.iter().max_by_key(|(_, n)| **n) {
            let share = *n as f64 / total as f64;
            if share > SINGLE_KIND_MAX_SHARE {
                s.flag(
                    Check::LevelFit,
                    format!(
                        "{} is {:.0}% of {family} stacks at L{level} ({n}/{total}; {kinds} kinds paid at this level)",
                        ctx.name(id),
                        share * 100.0
                    ),
                );
            }
        }
    }
}

/// The metal / ingredient kinds any treasury tier pays at a level, for level fit.
#[derive(Default)]
struct LevelKinds {
    metals: HashMap<u64, BTreeSet<Uuid>>,
    salts: HashMap<u64, BTreeSet<Uuid>>,
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn chest_key(i: u64, tier: i64, level: u64) -> String {
    format!("{}:{}:{tier}:{level}:{}", buyer(i), i % 7, i)
}

/// How far above its chest level retail's own treasury openings of `tier` paid
/// gear: the "unless retail did otherwise" of the required-level rule.
fn treasury_slack(ctx: &Ctx, tier: i64) -> u64 {
    let pool = u64::try_from(tier)
        .ok()
        .and_then(|t| ctx.sd.chest_loots.tiers.get(&t).or(ctx.sd.chest_loots.provisional_tiers.get(&t)));
    pool.into_iter()
        .flatten()
        .flat_map(|s| {
            s.reward.items.iter().map(move |i| {
                ctx.apk
                    .item_required_level
                    .get(&i.item.item_template_id)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(s.chest_level)
            })
        })
        .max()
        .unwrap_or(0)
        .max(REQUIRED_LEVEL_LOOKAHEAD)
}

/// As [`treasury_slack`] for a store chest corpus (`store_bundle_loot.json`):
/// how far above the bottom of its buyer band retail paid gear.
fn store_slack(ctx: &Ctx, product: &Uuid) -> u64 {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Corpus {
        products: Vec<P>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct P {
        product_id: Uuid,
        by_level: Vec<B>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct B {
        min_buyer_level: u64,
        results: Vec<R>,
    }
    #[derive(Deserialize)]
    struct R {
        reward: Paid,
    }
    // The corpus strips instance ids, so not a `RewardGrant`.
    #[derive(Deserialize)]
    struct Paid {
        #[serde(default)]
        items: Vec<Item>,
    }
    // The base chest ids are mined under their 1-gem promo twins.
    let mined_as: HashMap<Uuid, Uuid> = [
        (0x0e224ca0_1506_490f_884a_8871ffe6399b, 0x11b322d2_1b5e_4e90_bb41_a8d90ca9548b),
        (0x7bf00a9c_6a08_4b55_a60d_53915baa38a3, 0x9e4dc391_422e_4b25_ba90_faab39e0769f),
        (0x1275d959_bbe5_460d_8f6a_1c31106a8eb2, 0x11102495_fde7_4e77_b6c4_d13b9303f1f5),
    ]
    .into_iter()
    .map(|(a, b)| (Uuid::from_u128(a), Uuid::from_u128(b)))
    .collect();
    let id = mined_as.get(product).unwrap_or(product);
    let corpus: Corpus = read("blades_lib/src/store_bundle_loot.json");
    corpus
        .products
        .iter()
        .filter(|p| &p.product_id == id)
        .flat_map(|p| &p.by_level)
        .flat_map(|b| {
            b.results.iter().flat_map(move |r| {
                r.reward.items.iter().map(move |i| {
                    ctx.apk
                        .item_required_level
                        .get(&i.item_template_id)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(b.min_buyer_level)
                })
            })
        })
        .max()
        .unwrap_or(0)
        .max(REQUIRED_LEVEL_LOOKAHEAD)
}

fn sample_treasury(ctx: &Ctx, tier: i64, level: u64, daily: bool) -> Sample {
    let mut out = Sample::default();
    for i in 0..ctx.rolls {
        let key = if daily {
            // `daily_reward::collect_reward_for`'s key shape.
            format!("daily:{}:{}:0:{tier}:{level}", buyer(i), 20_000 + i)
        } else {
            chest_key(i, tier, level)
        };
        if let Some(r) = chests::roll_loot(&ctx.sd.chest_loots, tier, level, &key, &HashSet::new()) {
            out.rewards.push(r);
        }
    }
    out
}

fn sample_store_chest(ctx: &Ctx, product: Uuid, level: u64) -> Sample {
    let elder = ctx.sd.chest_loots.provisional_tiers.get(&4).map(Vec::as_slice);
    let mut out = Sample::default();
    for i in 0..ctx.rolls {
        let nonce = crate::global_shop::purchase_roll_nonce(i, buyer(i), None);
        if let Some(r) =
            store_bundles::roll_bundle_with(&product, level, nonce, &HashSet::new(), elder)
        {
            out.rewards.push(r);
        }
    }
    out
}

fn audit_chests(ctx: &Ctx) -> Vec<Subject> {
    let kinds: [(i64, &str, &str); 6] = [
        (chests::TUTORIAL_FIRST_CHEST, "tutorial", "Tutorial first chest (tier -1)"),
        (1, "wooden", "Wooden chest (tier 1)"),
        (2, "silver", "Silver chest (tier 2)"),
        (3, "gold", "Gold chest (tier 3)"),
        (4, "elder", "Elder chest (tier 4)"),
        (5, "legendary", "Legendary chest (tier 5)"),
    ];
    let mut samples: Vec<(String, String, String, bool, Vec<(u64, Sample)>)> = Vec::new();
    let mut slack: HashMap<String, u64> = HashMap::new();
    for (tier, key, name) in kinds {
        let levels: Vec<u64> = if tier < 1 { vec![1] } else { LEVELS.to_vec() };
        let per: Vec<(u64, Sample)> =
            levels.iter().map(|&l| (l, sample_treasury(ctx, tier, l, false))).collect();
        slack.insert(format!("chest:treasury:{key}"), treasury_slack(ctx, tier));
        samples.push((
            format!("chest:treasury:{key}"),
            name.to_string(),
            "chests::roll_loot (POST /chests/{id}/collect)".into(),
            tier >= 1,
            per,
        ));
    }
    for (tier, key, name) in [(2i64, "silver", "Daily Silver chest"), (3, "gold", "Daily Gold chest")] {
        let per: Vec<(u64, Sample)> =
            LEVELS.iter().map(|&l| (l, sample_treasury(ctx, tier, l, true))).collect();
        slack.insert(format!("chest:daily:{key}"), treasury_slack(ctx, tier));
        samples.push((
            format!("chest:daily:{key}"),
            name.to_string(),
            "daily_reward::collect_reward_for -> chests::roll_loot".into(),
            true,
            per,
        ));
    }
    for (id, name) in STORE_CHESTS {
        let product = Uuid::from_u128(id);
        let per: Vec<(u64, Sample)> =
            LEVELS.iter().map(|&l| (l, sample_store_chest(ctx, product, l))).collect();
        slack.insert(format!("chest:store:{}", short(&product)), store_slack(ctx, &product));
        samples.push((
            format!("chest:store:{}", short(&product)),
            name.to_string(),
            "store_bundles::roll_bundle_with (global-shop purchase)".into(),
            true,
            per,
        ));
    }

    // The kinds every chest pays at each level, for level fit.
    let mut available = LevelKinds::default();
    let metals: HashSet<Uuid> = chests::METALS.iter().map(|m| Uuid::from_u128(*m)).collect();
    let salts: HashSet<Uuid> = ENCHANTING_INGREDIENTS.iter().map(|m| Uuid::from_u128(*m)).collect();
    for (_, _, _, _, per) in &samples {
        for (l, s) in per {
            for r in &s.rewards {
                for id in r.stackable_items.keys() {
                    if metals.contains(id) {
                        available.metals.entry(*l).or_default().insert(*id);
                    }
                    if salts.contains(id) {
                        available.salts.entry(*l).or_default().insert(*id);
                    }
                }
            }
        }
    }
    for l in LEVELS {
        available
            .metals
            .entry(l)
            .or_default()
            .extend(chests::treasury_metals_near(&ctx.sd.chest_loots, l));
    }

    let mut out = Vec::new();
    for (key, name, how, rolled, per) in samples {
        let mut s = Subject::new("chest", key, name, how);
        s.levels = per.iter().map(|(l, _)| l.to_string()).collect::<Vec<_>>().join(",");
        s.rolls = ctx.rolls;
        let mut all_items: Vec<Item> = Vec::new();
        let mut distinct_rewards = 0usize;
        let mut total_rewards = 0usize;
        for (l, sample) in &per {
            let rule = LevelRule::Character {
                level: *l,
                slack: slack.get(&s.key).copied().unwrap_or(REQUIRED_LEVEL_LOOKAHEAD),
            };
            judge(ctx, &mut s, sample, Some(*l), rule, rolled, rolled);
            if rolled {
                level_fit(ctx, &mut s, sample, *l, &available);
            }
            all_items.extend(sample.items().into_iter().cloned());
            distinct_rewards += sample.rewards.iter().map(reward_key).collect::<HashSet<_>>().len();
            total_rewards += sample.rewards.len();
        }
        let refs: Vec<&Item> = all_items.iter().collect();
        if rolled {
            for c in grading_coverage(ctx, &refs) {
                s.flag(Check::Coverage, c);
            }
            for c in template_coverage(ctx, &refs) {
                s.flag(Check::Coverage, c);
            }
        } else {
            s.notes.push("scripted retail bundle (3 recorded openings): fixed by definition".into());
        }
        let distinct_items = refs.iter().map(|i| item_key(i)).collect::<HashSet<_>>().len();
        s.notes.push(format!(
            "{distinct_rewards}/{total_rewards} distinct rewards, {distinct_items} distinct item instances in {} items",
            refs.len()
        ));
        out.push(s);
    }
    out
}

/// The products the storefront can list: every calendar offer and any authored one.
fn served_products(ctx: &Ctx) -> BTreeSet<Uuid> {
    let mut ids = BTreeSet::new();
    for v in [&ctx.sd.global_shop_windows, &ctx.sd.global_shop_authored] {
        let map = v
            .get("globalShopWindows")
            .or_else(|| v.get("globalShopOverrides"))
            .and_then(Value::as_object);
        for k in map.into_iter().flat_map(|m| m.keys()) {
            if let Ok(id) = Uuid::parse_str(k) {
                ids.insert(id);
            }
        }
    }
    ids
}

fn product_name(ctx: &Ctx, id: &Uuid) -> String {
    ctx.offer_names
        .get(id)
        .filter(|n| !n.is_empty())
        .cloned()
        .unwrap_or_else(|| short(id))
}

fn audit_products(ctx: &Ctx) -> Vec<Subject> {
    let mut out = Vec::new();
    for id in served_products(ctx) {
        let name = product_name(ctx, &id);
        let key = format!("product:{id}");
        if store_bundles::is_randomised_bundle(&id) {
            // Sampled once per corpus under the store chests (chest section).
            let mut s = Subject::new("product", key, name, "randomised store chest".into());
            s.notes.push("rolled by store_bundles::roll_bundle_with; audited in the chest section".into());
            out.push(s);
            continue;
        }
        let rolled = crate::global_shop::rolls_jewelry_per_purchase(&ctx.sd, &ctx.items, id);
        let how = if rolled {
            "global_shop::purchase_reward, jewellery rolled per purchase"
        } else if ctx.sd.global_shop_grants.contains_key(&id) {
            "global_shop::purchase_reward, captured retail grant (fixed)"
        } else {
            "global_shop::purchase_reward, APK offer contents"
        };
        let mut s = Subject::new("product", key, name, how.into());
        s.levels = "any (level-independent)".into();
        s.rolls = ctx.rolls;
        let mut sample = Sample::default();
        for i in 0..ctx.rolls {
            let nonce = crate::global_shop::purchase_roll_nonce(i, buyer(i), None);
            if let Some(r) =
                crate::global_shop::purchase_reward(&ctx.sd, &ctx.repair, &ctx.items, id, nonce)
            {
                sample.rewards.push(r);
            }
        }
        if sample.rewards.is_empty() {
            s.flag(
                Check::Availability,
                "no deliverable reward: the purchase is refused (#167 shape)".into(),
            );
            out.push(s);
            continue;
        }
        // Does the APK definition leave anything to a roll? Jewellery that the
        // server rolls, or an arcane ring/necklace whose authored GRADING is empty.
        let apk_rolls = rolled
            || ctx.sd.global_shop_offer_contents.get(&id).is_some_and(|c| {
                c.contents.iter().any(|e| {
                    ctx.is_jewelry(&e.item_template_id)
                        && !crate::fixed_templates::has_mandatory_properties(e.item_template_id)
                        && e.grading.is_empty()
                })
            });
        judge(ctx, &mut s, &sample, None, LevelRule::Gated, apk_rolls, false);
        let distinct = sample.items().iter().map(|i| item_key(i)).collect::<HashSet<_>>().len();
        if apk_rolls {
            let items = sample.items();
            for c in grading_coverage(ctx, &items).into_iter().chain(enchant_coverage(ctx, &items)) {
                s.flag(Check::Coverage, c);
            }
            // A product whose definition rolls but whose output never varies.
            if distinct == 1 && !items.is_empty() {
                s.flag(Check::Variety, "frozen: every purchase is the same item".into());
            }
        } else if !sample.items().is_empty() {
            s.notes.push("fixed by its APK definition".into());
        }
        // An item the offer's own gate lets a character buy before they can use it.
        if let Some(gate) = ctx.apk.offer_level_gates.get(&id).filter(|g| g.max > 0) {
            for item in sample.items() {
                let req = ctx.apk.item_required_level.get(&item.item_template_id).copied().unwrap_or(0);
                if req > gate.min.max(1) + REQUIRED_LEVEL_LOOKAHEAD {
                    s.notes.push(format!(
                        "APK gate {}-{} sells {} (requires {req}); authored that way",
                        gate.min,
                        gate.max,
                        ctx.name(&item.item_template_id)
                    ));
                    break;
                }
            }
        }
        if !sample.items().is_empty() {
            s.notes.push(format!("{distinct} distinct item instances"));
        }
        out.push(s);
    }
    out
}

fn audit_merchants(ctx: &Ctx) -> Vec<Subject> {
    let names: [(Uuid, &str); 4] = [
        (crate::shop::VENDOR_BUILDING_TYPES[0], "Blacksmith (Forge)"),
        (crate::shop::VENDOR_BUILDING_TYPES[1], "Enchanter"),
        (crate::shop::VENDOR_BUILDING_TYPES[2], "Workshop"),
        (crate::shop::VENDOR_BUILDING_TYPES[3], "Alchemist"),
    ];
    let windows = (ctx.rolls / 5).max(20);
    let mut out = Vec::new();
    for (type_id, name) in names {
        let mut s = Subject::new(
            "merchant",
            format!("merchant:{}", short(&type_id)),
            name.to_string(),
            "shop_gen::generate_catalog + jewelry_roll::roll_generated_jewelry".into(),
        );
        s.levels = "building 0-9".into();
        s.rolls = windows;
        let Some(building) = ctx.shop_stock.generation.get(&type_id) else {
            s.flag(Check::Availability, "no shop_stock generation config".into());
            out.push(s);
            continue;
        };
        let mut stocked: HashSet<Uuid> = HashSet::new();
        let mut jewels: Vec<Item> = Vec::new();
        let mut per_bundle: HashMap<Uuid, Vec<String>> = HashMap::new();
        for level in 0..=9u64 {
            let mut sample = Sample::default();
            for w in 0..windows {
                let shop = buyer(w);
                let start_ms = 1_792_022_400_000i64 + (w as i64) * 36_000_000;
                let bundles = crate::shop_gen::generate_catalog(
                    &ctx.shop_stock,
                    &type_id,
                    level,
                    &shop,
                    crate::shop_gen::window_key(start_ms),
                );
                for (idx, b) in bundles.iter().enumerate() {
                    stocked.insert(b.id);
                    let Some(def) = ctx.sd.shop_bundles.get(&b.id) else {
                        s.flag(Check::Availability, format!("stocks unknown bundle {}", short(&b.id)));
                        continue;
                    };
                    let mut grant = def.grant.clone();
                    // As `shop::roll_generated_jewelry_for_window`, then `mint_bundle`.
                    let mut rng = crate::jewelry_roll::seeded(
                        &[shop.as_bytes(), b.id.as_bytes()],
                        start_ms as u64 ^ idx as u64,
                    );
                    for item in &mut grant.items {
                        crate::jewelry_roll::roll_generated_jewelry(&mut item.item, &ctx.items, &mut rng);
                        item.item.properties.enchanting.clear();
                        jewelry_grade::grade_if_bare(&mut item.item, &ctx.items, &mut rng);
                        if ctx.is_jewelry(&item.item.item_template_id) {
                            jewels.push(item.item.clone());
                            per_bundle.entry(b.id).or_default().push(item_key(&item.item));
                        }
                    }
                    sample.rewards.push(grant);
                }
            }
            judge(ctx, &mut s, &sample, None, LevelRule::Gated, false, false);
        }
        for (bundle, keys) in &per_bundle {
            if keys.len() >= 20 {
                let (share, c, k) = modal_share(keys);
                if share > VARIETY_MAX_SHARE {
                    s.flag(
                        Check::Variety,
                        format!(
                            "bundle {}: {} is {c}/{} rolls ({:.0}%)",
                            short(bundle),
                            describe_key(ctx, &k),
                            keys.len(),
                            share * 100.0
                        ),
                    );
                }
            }
        }
        let refs: Vec<&Item> = jewels.iter().collect();
        for c in grading_coverage(ctx, &refs) {
            s.flag(Check::Coverage, c);
        }
        for level in 0..=9u64 {
            if let Some(f) = frozen_catalog(building, level) {
                s.flag(Check::Variety, f);
            }
            if let Some(c) = merchant_potion_reach(ctx, building, level) {
                s.flag(Check::Coverage, c);
            }
        }
        // The capture-measured level pools replace the authored tier pool at
        // every level they cover (`shop_gen::generate_catalog`), so they are
        // what a merchant can stock.
        let measured: HashSet<Uuid> =
            building.level_pools.values().flatten().map(|e| e.bundle_id).collect();
        let pool: HashSet<Uuid> = if measured.is_empty() {
            building.item_pool.iter().map(|e| e.bundle_id).collect()
        } else {
            measured
        };
        let never = pool.difference(&stocked).count();
        if never > 0 {
            s.flag(
                Check::Coverage,
                format!("{never}/{} stockable bundles never stocked at building levels 0-9", pool.len()),
            );
        }
        s.notes.push(format!("{} bundles stocked, {} jewellery rolls", stocked.len(), jewels.len()));
        out.push(s);
    }
    out
}

/// The pool `shop_gen::generate_catalog` draws from at a building level: the
/// measured level pool where there is one, else the authored pool up to the
/// level's tier cap.
fn merchant_pool(
    building: &crate::shop_gen::BuildingGeneration,
    level: u64,
) -> Vec<&crate::shop_gen::PoolEntry> {
    let cap = building.levels.get(&level.to_string()).map(|p| p.tier_cap).unwrap_or(0);
    match building.level_pools.get(&level.to_string()).filter(|p| !p.is_empty()) {
        Some(pool) => pool.iter().collect(),
        None => building.item_pool.iter().filter(|e| e.tier <= cap).collect(),
    }
}

/// Variety for a merchant: a level whose pool is no larger than the catalog it
/// rolls lists the same bundles in every window.
fn frozen_catalog(building: &crate::shop_gen::BuildingGeneration, level: u64) -> Option<String> {
    let k = building.levels.get(&level.to_string())?.max_items as usize;
    let pool = merchant_pool(building, level).len();
    (pool > 0 && pool <= k).then(|| {
        format!(
            "frozen catalog: building L{level} lists the same {pool} bundles in every window \
             (pool {pool} <= maxItems {k})"
        )
    })
}

/// Coverage for a merchant's potions (report #397), against the APK rather than
/// against the captured catalogs the pool was built from: every potion or poison
/// the merchant sells (a one-stack shop bundle in its authored pool) at a tier
/// inside the tier window the level's pool already spans must be in that pool.
///
/// The older coverage line compares what was stocked with the pool itself, so a
/// pool built from 108 retail catalogs that never happened to list 12 rare
/// potions passed it: those 12 were in neither.
fn merchant_potion_reach(
    ctx: &Ctx,
    building: &crate::shop_gen::BuildingGeneration,
    level: u64,
) -> Option<String> {
    let own: HashSet<Uuid> = building.item_pool.iter().map(|e| e.bundle_id).collect();
    let potions: HashMap<Uuid, u64> =
        crate::shop_gen::potion_bundles(&ctx.sd.shop_bundles, &ctx.items)
            .into_iter()
            .filter(|(b, _)| own.contains(b))
            .collect();
    let held: HashSet<Uuid> = merchant_pool(building, level).iter().map(|e| e.bundle_id).collect();
    let tiers: Vec<u64> = held.iter().filter_map(|b| potions.get(b).copied()).collect();
    let (lo, hi) = (*tiers.iter().min()?, *tiers.iter().max()?);
    let allowed: Vec<Uuid> = potions
        .iter()
        .filter(|(_, t)| (lo..=hi).contains(*t))
        .map(|(b, _)| *b)
        .collect();
    let mut missing: Vec<String> = allowed
        .iter()
        .filter(|b| !held.contains(b))
        .map(|b| {
            let template = ctx.sd.shop_bundles[b].grant.stackable_items.keys().next().copied();
            template.map(|t| ctx.name(&t)).unwrap_or_else(|| short(b))
        })
        .collect();
    if missing.is_empty() {
        return None;
    }
    missing.sort();
    let n = missing.len();
    missing.truncate(4);
    Some(format!(
        "L{level}: {n}/{} potions the APK sells at tiers {lo}-{hi} can never be stocked ({}{})",
        allowed.len(),
        missing.join(", "),
        if n > 4 { ", ..." } else { "" }
    ))
}

// ---------------------------------------------------------------------------
// Shop availability
// ---------------------------------------------------------------------------

struct Availability {
    /// level -> (min, median, max) offers the client lists over the cycle.
    per_level: Vec<(u64, usize, usize, usize, usize)>,
    subjects: Vec<Subject>,
    live_any: usize,
}

const AVAIL_LEVELS: [u64; 16] = [1, 10, 20, 30, 35, 40, 44, 45, 46, 47, 48, 49, 50, 60, 80, 100];

fn live_offers(ctx: &Ctx, now: i64) -> BTreeMap<Uuid, Value> {
    crate::global_shop::current_catalog(&ctx.sd, now)
        .get("globalShopOverrides")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(k, e)| {
            let id = Uuid::parse_str(k).ok()?;
            let s = e.get("activeStartDate")?.as_i64()?;
            let en = e.get("activeEndDate")?.as_i64()?;
            let active = e.get("isActive").and_then(Value::as_bool).unwrap_or(false);
            (active && s <= now && now <= en).then(|| (id, e.clone()))
        })
        .collect()
}

fn audit_availability(ctx: &Ctx) -> Availability {
    let gate = |id: &Uuid| ctx.apk.offer_level_gates.get(id).copied().unwrap_or(Gate { min: 0, max: 0 });
    let mut counts: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    let mut ever_live: HashSet<Uuid> = HashSet::new();
    let mut live_mid: HashSet<Uuid> = HashSet::new();
    let mut ungrantable: BTreeSet<Uuid> = BTreeSet::new();
    let mut unpriced: BTreeSet<Uuid> = BTreeSet::new();
    let mut windowed_keys: HashMap<Uuid, HashSet<String>> = HashMap::new();
    for d in 0..AVAILABILITY_DAYS * 2 {
        // 17:30 UTC: after the 16:00 daily turnover and the 17:00 block.
        let now = AVAILABILITY_T0 + d * 86_400 + 63_000;
        let live = live_offers(ctx, now);
        for (id, entry) in &live {
            // `global_shop::window_purchase_cap` and `authoritative_prices` read
            // the same entry; recomputing the catalogue per offer is too slow here.
            let limits = entry.get("maxPurchaseLimits").and_then(Value::as_array);
            for lim in limits.into_iter().flatten() {
                let tid = lim.get("purchaseTrackingId").and_then(Value::as_str).unwrap_or("");
                let windowed = tid
                    .rsplit("::")
                    .next()
                    .is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()));
                if windowed && lim.get("limit").and_then(Value::as_u64).unwrap_or(0) > 0 {
                    windowed_keys.entry(*id).or_default().insert(tid.to_string());
                }
            }
            if d >= AVAILABILITY_DAYS {
                continue;
            }
            ever_live.insert(*id);
            if (40..=50).any(|l| gate(id).admits(l)) {
                live_mid.insert(*id);
            }
            let priced = entry
                .get("prices")
                .and_then(Value::as_array)
                .is_some_and(|p| !p.is_empty())
                || ctx.sd.global_shop_prices.contains_key(id)
                || ctx.sd.global_shop_free.contains(id);
            if !priced {
                unpriced.insert(*id);
            }
        }
        if d >= AVAILABILITY_DAYS {
            continue;
        }
        for l in AVAIL_LEVELS {
            counts
                .entry(l)
                .or_default()
                .push(live.keys().filter(|id| gate(id).admits(l)).count());
        }
    }
    for id in &ever_live {
        if store_bundles::is_randomised_bundle(id) {
            continue;
        }
        let nonce = crate::global_shop::purchase_roll_nonce(0, buyer(0), None);
        if crate::global_shop::purchase_reward(&ctx.sd, &ctx.repair, &ctx.items, *id, nonce).is_none() {
            ungrantable.insert(*id);
        }
    }

    let per_level = counts
        .into_iter()
        .map(|(l, mut v)| {
            v.sort_unstable();
            (l, v[0], v[v.len() / 2], v[v.len() - 1], v.iter().sum::<usize>() / v.len().max(1))
        })
        .collect();

    let mut subjects = Vec::new();
    let mut s = Subject::new(
        "availability",
        "availability:storefront".into(),
        "Storefront over one 42-day replay cycle".into(),
        "global_shop::current_catalog + APK level gate (client filter)".into(),
    );
    s.levels = AVAIL_LEVELS.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    s.rolls = AVAILABILITY_DAYS as u64;
    let served = served_products(ctx);
    // Retail scheduled a few offers with `isActive: false` in every window: they
    // were never shown in retail either.
    let inactive: BTreeSet<Uuid> = ctx
        .sd
        .global_shop_windows
        .get("globalShopWindows")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, list)| {
            list.as_array().into_iter().flatten().all(|e| {
                !e.get("isActive").and_then(Value::as_bool).unwrap_or(false)
            })
        })
        .filter_map(|(k, _)| Uuid::parse_str(k).ok())
        .collect();
    if !inactive.is_empty() {
        s.notes.push(format!(
            "{} offers inactive in every retail window (never shown by retail either): {}",
            inactive.len(),
            inactive.iter().map(|id| product_name(ctx, id)).collect::<Vec<_>>().join(", ")
        ));
    }
    let served: BTreeSet<Uuid> = served.difference(&inactive).copied().collect();
    let never: Vec<&Uuid> = served.iter().filter(|id| !ever_live.contains(id)).collect();
    if !never.is_empty() {
        s.flag(
            Check::Availability,
            format!(
                "{} offers never live in a whole cycle: {}",
                never.len(),
                never.iter().map(|id| product_name(ctx, id)).collect::<Vec<_>>().join(", ")
            ),
        );
    }
    let mid_never: Vec<&Uuid> = served
        .iter()
        .filter(|id| (40..=50).any(|l| gate(id).admits(l)) && !live_mid.contains(id))
        .collect();
    if !mid_never.is_empty() {
        s.flag(
            Check::Availability,
            format!(
                "{} offers gated for levels 40-50 but never live: {}",
                mid_never.len(),
                mid_never.iter().map(|id| product_name(ctx, id)).collect::<Vec<_>>().join(", ")
            ),
        );
    }
    if !ungrantable.is_empty() {
        s.flag(
            Check::Availability,
            format!(
                "{} live offers cannot be bought (no deliverable reward): {}",
                ungrantable.len(),
                ungrantable.iter().map(|id| product_name(ctx, id)).collect::<Vec<_>>().join(", ")
            ),
        );
    }
    if !unpriced.is_empty() {
        s.flag(
            Check::Availability,
            format!(
                "{} live offers have no authoritative price (purchase refused): {}",
                unpriced.len(),
                unpriced.iter().map(|id| product_name(ctx, id)).collect::<Vec<_>>().join(", ")
            ),
        );
    }
    let stuck: Vec<&Uuid> = windowed_keys
        .iter()
        .filter(|(_, keys)| keys.len() < 2)
        .map(|(id, _)| id)
        .collect();
    if !stuck.is_empty() {
        let mut names: Vec<String> = stuck.iter().map(|id| product_name(ctx, id)).collect();
        names.sort();
        s.notes.push(format!(
            "{} window-capped offers kept one cap key over two cycles (live once per cycle or a cap that never resets): {}",
            names.len(),
            names.join(", ")
        ));
    }
    // Permanent hide: a non-timestamped override counter with a limit on an
    // offer that has no lifetime cap would hide it for ever after one purchase.
    let mut permanent = BTreeSet::new();
    if let Some(cal) = ctx.sd.global_shop_windows.get("globalShopWindows").and_then(Value::as_object) {
        for (k, list) in cal {
            for e in list.as_array().into_iter().flatten() {
                let lifetime = e.get("maxPurchases").and_then(Value::as_u64).unwrap_or(0);
                for lim in e.get("maxPurchaseLimits").and_then(Value::as_array).into_iter().flatten() {
                    let tid = lim.get("purchaseTrackingId").and_then(Value::as_str).unwrap_or("");
                    let n = lim.get("limit").and_then(Value::as_u64).unwrap_or(0);
                    let windowed = tid.rsplit("::").next().is_some_and(|t| t.chars().all(|c| c.is_ascii_digit()));
                    if n > 0 && tid.contains("::override::") && !windowed && lifetime == 0 {
                        permanent.insert(k.clone());
                    }
                }
            }
        }
    }
    if !permanent.is_empty() {
        s.flag(
            Check::Availability,
            format!("{} offers carry a never-resetting override cap without a lifetime cap", permanent.len()),
        );
    }
    let gated = served.iter().filter(|id| ctx.apk.offer_level_gates.contains_key(id)).count();
    s.notes.push(format!(
        "{gated}/{} served offers have an APK entry (the rest are ungated)",
        served.len()
    ));
    subjects.push(s);
    Availability { per_level, subjects, live_any: ever_live.len() }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

const BEGIN: &str = "<!-- BEGIN GENERATED: cargo test -p server economy_audit (ECONOMY_AUDIT_WRITE=1) -->";
const END: &str = "<!-- END GENERATED -->";

fn esc(s: &str) -> String {
    s.replace('|', "\\|")
}

fn render(ctx: &Ctx, subjects: &[Subject], avail: &Availability) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{BEGIN}\n");
    let _ = writeln!(
        o,
        "Rolls per subject and level: **{}**. Levels: {}. Variety limit: {:.0}% of rolls; level-fit limit: {:.0}% of a family's stacks.\n",
        ctx.rolls,
        LEVELS.iter().map(u64::to_string).collect::<Vec<_>>().join(", "),
        VARIETY_MAX_SHARE * 100.0,
        SINGLE_KIND_MAX_SHARE * 100.0
    );

    // Summary.
    let mut by_check: BTreeMap<Check, usize> = BTreeMap::new();
    for s in subjects {
        for c in s.findings.iter().map(|(c, _)| *c).collect::<BTreeSet<_>>() {
            *by_check.entry(c).or_insert(0) += 1;
        }
    }
    let _ = writeln!(o, "### Summary\n\n| Check | Subjects flagged | Asserted |\n|---|---:|---|");
    for c in [Check::Validity, Check::Variety, Check::Coverage, Check::LevelFit, Check::Availability] {
        let _ = writeln!(
            o,
            "| {} | {} | {} |",
            c.label(),
            by_check.get(&c).copied().unwrap_or(0),
            if c.asserted() { "yes (allowlist below)" } else { "no, warning" }
        );
    }
    let total = subjects.len();
    let pass = subjects.iter().filter(|s| s.status() == "PASS").count();
    let _ = writeln!(o, "\n{pass} of {total} subjects pass every check.\n");

    for (section, title) in [
        ("chest", "Chests"),
        ("merchant", "Town merchants"),
        ("availability", "Storefront availability"),
        ("product", "Global-shop products (Sigil shop and storefront)"),
    ] {
        let rows: Vec<&Subject> = subjects.iter().filter(|s| s.section == section).collect();
        let _ = writeln!(o, "### {title}\n");
        if section == "availability" {
            let _ = writeln!(
                o,
                "Offers the client lists at each character level (live in the replayed calendar AND inside the APK `_min/_maxPlayerLevelAllowedForPurchase` gate), one sample a day over a whole 42-day cycle from 2026-10-15. {} distinct offers are live at some point.\n",
                avail.live_any
            );
            let _ = writeln!(o, "| Level | min | median | max | mean |\n|---:|---:|---:|---:|---:|");
            for (l, mn, md, mx, mean) in &avail.per_level {
                let _ = writeln!(o, "| {l} | {mn} | {md} | {mx} | {mean} |");
            }
            let _ = writeln!(o);
        }
        let flagged: Vec<&&Subject> = rows.iter().filter(|s| s.status() != "PASS").collect();
        let passed: Vec<&&Subject> = rows.iter().filter(|s| s.status() == "PASS").collect();
        if !flagged.is_empty() || section != "product" {
            let _ = writeln!(o, "| Subject | Generator | Levels | Rolls | Status | Findings | Notes |\n|---|---|---|---:|---|---|---|");
            for s in rows.iter().filter(|s| section != "product" || s.status() != "PASS") {
                let findings: Vec<String> = s
                    .findings
                    .iter()
                    .map(|(c, d)| {
                        let known = if c.asserted() && s.known(*c, d).is_some() { " (known)" } else { "" };
                        format!("**{}**{known}: {}", c.label(), esc(d))
                    })
                    .collect();
                let _ = writeln!(
                    o,
                    "| {} | {} | {} | {} | {} | {} | {} |",
                    esc(&s.name),
                    esc(&s.how),
                    esc(&s.levels),
                    s.rolls,
                    s.status(),
                    if findings.is_empty() { "-".into() } else { findings.join("<br>") },
                    esc(&s.notes.join("; "))
                );
            }
            let _ = writeln!(o);
        }
        if section == "product" && !passed.is_empty() {
            let _ = writeln!(
                o,
                "<details><summary>{} products PASS every check</summary>\n\n| Product | Generator | Notes |\n|---|---|---|",
                passed.len()
            );
            for s in passed {
                let _ = writeln!(o, "| {} | {} | {} |", esc(&s.name), esc(&s.how), esc(&s.notes.join("; ")));
            }
            let _ = writeln!(o, "\n</details>\n");
        }
    }

    let _ = writeln!(o, "### Allowlist (known asserted failures)\n");
    if KNOWN_FAILURES.is_empty() {
        let _ = writeln!(o, "Empty.\n");
    } else {
        let _ = writeln!(o, "| Subject | Check | Finding | Reason | Firing |\n|---|---|---|---|---:|");
        for k in KNOWN_FAILURES {
            let firing: usize = subjects
                .iter()
                .filter(|s| s.key.starts_with(k.subject))
                .map(|s| {
                    s.findings
                        .iter()
                        .filter(|(c, d)| *c == k.check && d.contains(k.contains))
                        .count()
                })
                .sum();
            let _ = writeln!(
                o,
                "| `{}` | {} | \"{}\" | {} | {} |",
                k.subject,
                k.check.label(),
                k.contains,
                esc(k.reason),
                if firing > 0 { firing.to_string() } else { "**0: remove the entry**".into() }
            );
        }
        let _ = writeln!(o);
    }
    let _ = writeln!(o, "{END}");
    o
}

fn write_report(body: &str) {
    let path = repo().join("docs/economy-audit.md");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let new = match (old.find(BEGIN), old.find(END)) {
        (Some(a), Some(b)) if b > a => {
            format!("{}{}{}", &old[..a], body.trim_end(), &old[b + END.len()..])
        }
        _ => format!("{}\n\n{}", old.trim_end(), body),
    };
    std::fs::write(&path, new).expect("write docs/economy-audit.md");
}

#[test]
fn economy_audit() {
    let started = std::time::Instant::now();
    let ctx = Ctx::load();
    let mut subjects = audit_chests(&ctx);
    subjects.extend(audit_merchants(&ctx));
    let avail = audit_availability(&ctx);
    subjects.extend(audit_products(&ctx));
    let Availability { per_level, subjects: av, live_any } = avail;
    subjects.extend(av);
    let avail = Availability { per_level, subjects: Vec::new(), live_any };

    let report = render(&ctx, &subjects, &avail);
    if std::env::var_os("ECONOMY_AUDIT_WRITE").is_some() {
        write_report(&report);
    }
    eprintln!("economy audit: {} subjects in {:?}", subjects.len(), started.elapsed());

    let mut failures = Vec::new();
    for s in &subjects {
        for (c, d) in &s.findings {
            if c.asserted() && s.known(*c, d).is_none() {
                failures.push(format!("[{}] {} — {}: {d}", s.key, s.name, c.label()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} unallowlisted economy-audit failures (see docs/economy-audit.md):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The audit is only as good as its rules: each must reject the defect it names
/// (a probe needs a control). Every item here is wrong in exactly one way, and
/// the last is right in every way.
#[test]
fn economy_audit_rules_reject_known_bad_items() {
    use blades_lib::user_data::{ItemPropertiesAll, ItemSingleProperty};
    let ctx = Ctx::load();
    // Brass Pearl Ring, a plain (non-fixed) level-1 ring the chests pay.
    let ring = Uuid::parse_str("869bf6f4-f7fb-43cf-b264-02b84f9a5425").unwrap();
    assert_eq!(ctx.item_type(&ring), Some(RING), "control: the fixture is a ring");
    let ring_props: Vec<Uuid> = crate::arena::combat::gamedata::GRADE_PROPERTIES
        .iter()
        .filter(|g| g.slot == "Ring")
        .filter_map(|g| Uuid::parse_str(g.uuid).ok())
        .collect();
    let necklace_prop = crate::arena::combat::gamedata::GRADE_PROPERTIES
        .iter()
        .find(|g| g.slot == "Necklace" && !ring_props.iter().any(|r| r.to_string() == g.uuid))
        .and_then(|g| Uuid::parse_str(g.uuid).ok())
        .expect("a necklace-only property");
    let p = |id: Uuid, tier: u64| ItemSingleProperty { id, tier };
    let jewel = |grade: Option<u64>, grading: Vec<ItemSingleProperty>| Item {
        item_template_id: ring,
        grade,
        tempering_level: 0,
        durability: 0.0,
        properties: ItemPropertiesAll { enchanting: Vec::new(), grading },
        arcane_tier: None,
    };
    let rule = LevelRule::Character { level: 100, slack: REQUIRED_LEVEL_LOOKAHEAD };
    let cases: Vec<(&str, Item)> = vec![
        ("grade but no GRADING", jewel(Some(3), Vec::new())),
        ("ungraded", jewel(None, Vec::new())),
        ("not on the slot's APK list", jewel(Some(1), vec![p(necklace_prop, 1)])),
        ("!= sum of tiers", jewel(Some(3), vec![p(ring_props[0], 1), p(ring_props[1], 1)])),
        ("twice", jewel(Some(2), vec![p(ring_props[0], 1), p(ring_props[0], 1)])),
        ("not a JewelryGradeData row", jewel(Some(3), vec![p(ring_props[0], 2), p(ring_props[1], 1)])),
    ];
    for (want, item) in &cases {
        let got = validate(&ctx, item, rule).join("; ");
        let want = if *want == "grade but no GRADING" { "but no GRADING properties" } else { want };
        assert!(got.contains(want), "expected '{want}', got '{got}'");
    }
    // Arcane tier 1 with one enchantment: 0 or 2 are the only counts.
    let mut arcane = jewel(Some(1), vec![p(ring_props[0], 1)]);
    arcane.arcane_tier = Some(1);
    let table = ctx.sd.enchanting.table_for(&ring).expect("rings have a secondary table");
    arcane.properties.enchanting = vec![p(table.properties[0].id, 1)];
    assert!(validate(&ctx, &arcane, rule).join(";").contains("on arcane tier 1"));
    // Required level: a Dragon piece at character level 10.
    let dragon = ctx
        .apk
        .item_required_level
        .iter()
        .find(|(t, l)| **l == 45 && !ctx.is_jewelry(t) && !store_bundles::is_artifact(t))
        .map(|(t, _)| *t)
        .expect("a level-45 template");
    let mut gear = jewel(None, Vec::new());
    gear.item_template_id = dragon;
    gear.durability = ctx.repair.max_durability(dragon, 0).unwrap_or(100.0);
    let low = LevelRule::Character { level: 10, slack: REQUIRED_LEVEL_LOOKAHEAD };
    assert!(validate(&ctx, &gear, low).join(";").contains("requires level 45"));
    assert!(validate(&ctx, &gear, rule).is_empty(), "control: fine at level 100");
    // And a valid ring passes: grade 3 = three tier-1 properties.
    let good = jewel(Some(3), vec![p(ring_props[0], 1), p(ring_props[1], 1), p(ring_props[2], 1)]);
    assert_eq!(validate(&ctx, &good, rule), Vec::<String>::new());
    // Variety: a frozen list is caught, a varied one is not.
    let frozen: Vec<String> = (0..10).map(|_| "a".to_string()).collect();
    assert!(modal_share(&frozen).0 > VARIETY_MAX_SHARE);
    let varied: Vec<String> = (0..10).map(|i| i.to_string()).collect();
    assert!(modal_share(&varied).0 <= VARIETY_MAX_SHARE);
}

/// Report #397: the merchant rules must reject the defect they name (a probe
/// needs a control). The level-9 Alchemist pool as it was before the fix, built
/// only from what 108 retail catalogs listed, must be flagged; the committed
/// pool must not. A pool no larger than its catalog is a frozen catalog.
#[test]
fn economy_audit_flags_unreachable_merchant_potions() {
    let ctx = Ctx::load();
    let alchemist = crate::shop::VENDOR_BUILDING_TYPES[3];
    let committed = ctx.shop_stock.generation[&alchemist].clone();
    assert_eq!(merchant_potion_reach(&ctx, &committed, 9), None, "committed L9 pool reaches every potion");
    assert_eq!(frozen_catalog(&committed, 9), None);

    // The pre-#397 pool: the same pool without the twelve potions no captured
    // catalog listed (Health T9 among them).
    let mut thin = committed.clone();
    let pool = thin.level_pools.get_mut("9").expect("L9 pool");
    let filled = [
        "11603218-", "44f78c79-", "55183a20-", "68dc8e66-", "6b34cb1d-", "6f6ce4e9-",
        "7432e1ab-", "8da4936d-", "9c07230e-", "a9be5d5f-", "e0ffce3a-", "fec18f17-",
    ];
    pool.retain(|e| !filled.iter().any(|p| e.bundle_id.to_string().starts_with(p)));
    assert_eq!(pool.len(), 42, "control: retail listed 42 distinct L9 bundles");
    let finding = merchant_potion_reach(&ctx, &thin, 9).expect("the thin pool is flagged");
    assert!(finding.starts_with("L9: 12/42 potions"), "{finding}");

    // Frozen: shrink the pool to the catalog size.
    let k = thin.levels["9"].max_items as usize;
    thin.level_pools.get_mut("9").unwrap().truncate(k);
    assert!(frozen_catalog(&thin, 9).is_some_and(|f| f.contains("frozen catalog")));
}

#[cfg(test)]
mod report_398;
