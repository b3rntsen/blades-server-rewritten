//! Legendary chest gear, generated the way retail generated it (#368).
//!
//! THE REPORT. After #489/#495 a Legendary chest's gear slot came from the few
//! dozen pieces retail happened to pay at that tier in the recorded purchases —
//! about 30 per slot, with their enchantments frozen as recorded. Sephoris: retail
//! could give any weapon, armour piece, necklace or ring of the tier, with the
//! enchantments in every combination.
//!
//! THE EVIDENCE says he is right (`script/extract_legendary_gear.py` checks it on
//! every run). Every one of the 2,372 regular-slot pieces in the 4,697-purchase
//! corpus is a member of the APK's loot pool for its material tier — 21 templates
//! per tier — and at the tiers with enough draws retail paid all 21 of them. Every
//! enchantment obeys the APK's enchanting tables: the first is a recipe property
//! allowed on that template at the item's enchant tier, the rest are distinct
//! members of the template's secondary table, all at one tier. The 307 distinct
//! retail purchases in the 2026-06-07 capture snapshot repeat a slot exactly (same
//! template and same enchantments) 4.2% and 1.3% of the time. That is a generator,
//! not a list.
//!
//! THE GENERATOR. The retail piece the tier rule picks (#489/#495) is kept as the
//! SHELL: it carries what retail decided jointly with the tier and that no table
//! states — the tempering level, how many enchantments and at what tier, and for
//! jewellery the grade. Around it the piece is generated:
//! - the kind of equipment, by the share retail paid each kind (`categoryWeights`:
//!   weapons about three times as often as each armour slot); jewellery keeps the
//!   shell's kind, because ring and necklace GRADING properties are disjoint sets;
//! - a template of that kind at the shell's material tier, uniformly (the APK
//!   lootWeight is the same for every template of a tier);
//! - durability, the APK max durability at the shell's tempering level;
//! - the primary enchantment, uniformly among the recipes allowed on the template
//!   at the shell's enchant tier, and the secondaries uniformly without repeats
//!   from the template's secondary table. Retail's frequencies are flat; the
//!   crafting weights in `enchanting.json` are not what the chest used.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use uuid::Uuid;

use super::store_bundles::mix;
use crate::user_data::{Item, ItemPropertiesAll, ItemSingleProperty};

static LEGENDARY_GEAR_RAW: &str = include_str!("../legendary_gear.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Table {
    category_weights: BTreeMap<String, u64>,
    secondary_tables: Vec<Vec<Uuid>>,
    primary_groups: Vec<BTreeMap<Uuid, Vec<u64>>>,
    templates: HashMap<Uuid, Template>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Template {
    required_level: u64,
    tier: u64,
    category: String,
    secondary_table: usize,
    primary_group: usize,
    /// Max durability by tempering level 0..=10; `None` for jewellery.
    durability: Option<Vec<f64>>,
}

struct Pool {
    table: Table,
    /// Templates by (required level, APK tier, kind), sorted so a seed always
    /// lands on the same template whatever the hash map's order.
    by_kind: BTreeMap<(u64, u64, String), Vec<Uuid>>,
}

fn pool() -> &'static Pool {
    static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let table: Table = serde_json::from_str(LEGENDARY_GEAR_RAW).unwrap_or(Table {
            category_weights: BTreeMap::new(),
            secondary_tables: Vec::new(),
            primary_groups: Vec::new(),
            templates: HashMap::new(),
        });
        let mut by_kind: BTreeMap<(u64, u64, String), Vec<Uuid>> = BTreeMap::new();
        for (id, t) in &table.templates {
            by_kind
                .entry((t.required_level, t.tier, t.category.clone()))
                .or_default()
                .push(*id);
        }
        for ids in by_kind.values_mut() {
            ids.sort_unstable();
        }
        Pool { table, by_kind }
    })
}

/// The APK `requiredLevel` of a template in the generation pool.
pub(crate) fn required_level(template: &Uuid) -> Option<u64> {
    pool()
        .table
        .templates
        .get(template)
        .map(|t| t.required_level)
}

fn is_jewelry(category: &str) -> bool {
    category == "ring" || category == "necklace"
}

/// A uniform index below `n` from `seed`.
fn pick(seed: u64, n: usize) -> usize {
    (mix(seed) % n as u64) as usize
}

/// Generate a Legendary gear piece around the retail `shell` the tier rule
/// picked. `None` when the shell is not a pool template (it is then paid as is).
pub(crate) fn generate(shell: &Item, seed: u64) -> Option<Item> {
    let pool = pool();
    let table = &pool.table;
    let base = table.templates.get(&shell.item_template_id)?;
    let tier_key = (base.required_level, base.tier);

    // The kind: jewellery keeps its own; equipment draws by retail's shares among
    // the kinds this tier has.
    let kind = if is_jewelry(&base.category) {
        base.category.clone()
    } else {
        let kinds: Vec<(&String, u64)> = table
            .category_weights
            .iter()
            .filter(|(k, _)| {
                pool.by_kind
                    .contains_key(&(tier_key.0, tier_key.1, (*k).clone()))
            })
            .map(|(k, w)| (k, *w))
            .collect();
        let total: u64 = kinds.iter().map(|(_, w)| w).sum();
        if total == 0 {
            return None;
        }
        let mut at = mix(seed ^ 0x1B87_3593_CC9E_2D51) % total;
        kinds
            .iter()
            .find(|(_, w)| {
                if at < *w {
                    true
                } else {
                    at -= w;
                    false
                }
            })?
            .0
            .clone()
    };
    let candidates = pool.by_kind.get(&(tier_key.0, tier_key.1, kind))?;
    let template_id = candidates[pick(seed ^ 0x85EB_CA6B_C2B2_AE35, candidates.len())];
    let template = &table.templates[&template_id];

    let durability = match &template.durability {
        Some(levels) => *levels.get(shell.tempering_level.min(10) as usize)?,
        None => shell.durability,
    };
    let enchanting = roll_enchanting(
        template,
        shell.properties.enchanting.len(),
        shell.properties.enchanting.first().map_or(0, |e| e.tier),
        seed,
    )?;
    Some(Item {
        item_template_id: template_id,
        grade: shell.grade,
        tempering_level: shell.tempering_level,
        durability,
        properties: ItemPropertiesAll {
            enchanting,
            grading: shell.properties.grading.clone(),
        },
        arcane_tier: shell.arcane_tier,
    })
}

/// `count` enchantments at `tier`: a recipe primary allowed on the template at
/// that tier, then distinct secondaries from the template's table.
fn roll_enchanting(
    template: &Template,
    count: usize,
    tier: u64,
    seed: u64,
) -> Option<Vec<ItemSingleProperty>> {
    if count == 0 {
        return Some(Vec::new());
    }
    let table = &pool().table;
    let primaries: Vec<Uuid> = table
        .primary_groups
        .get(template.primary_group)?
        .iter()
        .filter(|(_, tiers)| tiers.contains(&tier))
        .map(|(id, _)| *id)
        .collect();
    if primaries.is_empty() {
        return None;
    }
    let primary = primaries[pick(seed ^ 0xC2B2_AE3D_27D4_EB4F, primaries.len())];
    let mut secondaries: Vec<Uuid> = table
        .secondary_tables
        .get(template.secondary_table)?
        .iter()
        .copied()
        .filter(|id| *id != primary)
        .collect();
    if secondaries.len() < count - 1 {
        return None;
    }
    let mut out = vec![ItemSingleProperty { id: primary, tier }];
    for k in 0..count - 1 {
        // A partial Fisher-Yates: each draw takes one of the ids not yet taken.
        let j = k + pick(
            seed ^ 0x1656_67B1_9E37_79F9 ^ (k as u64 + 1).rotate_left(23),
            secondaries.len() - k,
        );
        secondaries.swap(k, j);
        out.push(ItemSingleProperty {
            id: secondaries[k],
            tier,
        });
    }
    Some(out)
}

/// Why `item` could not have come out of the generator, if it could not: the
/// same rules `script/extract_legendary_gear.py` checks against retail.
#[cfg(test)]
pub(crate) fn rule_broken(item: &Item) -> Option<String> {
    let table = &pool().table;
    let Some(t) = table.templates.get(&item.item_template_id) else {
        return Some(format!("{} is not a pool template", item.item_template_id));
    };
    match &t.durability {
        Some(levels) => {
            let want = levels[item.tempering_level.min(10) as usize];
            if (want - item.durability).abs() > 0.005 {
                return Some(format!("durability {} != {want}", item.durability));
            }
        }
        None if item.grade.is_none() => return Some("jewellery without a grade".into()),
        None => {}
    }
    let ens = &item.properties.enchanting;
    let Some(first) = ens.first() else {
        return None;
    };
    if ens.iter().any(|e| e.tier != first.tier) {
        return Some("mixed enchant tiers".into());
    }
    if !table.primary_groups[t.primary_group]
        .get(&first.id)
        .is_some_and(|tiers| tiers.contains(&first.tier))
    {
        return Some(format!(
            "primary {} not allowed at tier {}",
            first.id, first.tier
        ));
    }
    let secs: Vec<Uuid> = ens[1..].iter().map(|e| e.id).collect();
    let distinct: std::collections::HashSet<_> = secs.iter().collect();
    if distinct.len() != secs.len() {
        return Some("repeated secondary".into());
    }
    if let Some(s) = secs
        .iter()
        .find(|s| !table.secondary_tables[t.secondary_table].contains(s))
    {
        return Some(format!("secondary {s} not in the template's table"));
    }
    None
}

#[cfg(test)]
pub(crate) fn kind_of(template: &Uuid) -> Option<&'static str> {
    pool()
        .table
        .templates
        .get(template)
        .map(|t| t.category.as_str())
}

#[cfg(test)]
pub(crate) fn tier_pool_size(required_level: u64) -> usize {
    pool()
        .table
        .templates
        .values()
        .filter(|t| t.required_level == required_level)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table must be compiled in; everything else is vacuous without it.
    #[test]
    fn the_gear_table_loads() {
        if let Err(e) = serde_json::from_str::<Table>(LEGENDARY_GEAR_RAW) {
            panic!("legendary_gear.json failed to parse: {e}");
        }
        let t = &pool().table;
        assert_eq!(
            t.templates.len(),
            210,
            "21 loot templates for each of 10 material tiers"
        );
        assert_eq!(t.secondary_tables.len(), 16);
        assert_eq!(
            t.category_weights.values().sum::<u64>(),
            2065,
            "retail's equipment pieces"
        );
    }
}
