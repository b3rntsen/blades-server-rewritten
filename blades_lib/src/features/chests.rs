//! Chests — `POST /chests/{id}/collect`.
//!
//! Treasury chests (earned from dungeons / daily rewards, or already present on an
//! imported character) are opened for loot. Retail rolled each chest's contents
//! server-side at open time and never shipped the loot tables in the APK, so we
//! cannot roll them the way retail did. What we do have is 741 retail-era captures
//! of real chest openings with their tier and level recovered (see
//! `script/extract_chest_loot.py` and `docs/chest-loot-extraction.md`), so we draw a
//! bundle Bethesda's server actually returned **for a chest of that tier, at the
//! nearest observed chest level**.
//!
//! Each chest is composed from parts of the retail openings nearest its level
//! ([`compose_loot`], #335), deterministically in the chest's key, so a given
//! chest always yields the same thing however many times the client retries. The
//! handler re-mints the instanced item ids before granting (capture ids would
//! collide across players).

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::economy::RewardGrant;

/// `ChestData.ChestRarity.TierSpecial_TutorialFirstChest` in the APK: the first
/// story quest's first chest. Retail generated it, listed it in the treasury and
/// paid it at this tier, from a table of its own (report #307).
pub const TUTORIAL_FIRST_CHEST: i64 = -1;

/// Every retail opening of the tutorial chest in the capture snapshot (3). A
/// scripted starter bundle, not a wooden or silver roll: 58-65 gold, one item
/// and the same four stackables each time.
static TUTORIAL_CHEST_LOOT_RAW: &str = include_str!("../tutorial_chest_loot.json");

#[derive(Deserialize)]
struct TutorialChestLoot {
    samples: Vec<ChestLootSample>,
}

fn tutorial_chest_loot() -> &'static [ChestLootSample] {
    static TABLE: std::sync::OnceLock<Vec<ChestLootSample>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<TutorialChestLoot>(TUTORIAL_CHEST_LOOT_RAW)
            .map(|t| t.samples)
            .unwrap_or_default()
    })
}

/// One captured chest opening: the reward, and the level of the chest it came out of.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChestLootSample {
    /// The `level` of the treasury chest this bundle was rolled for. Retail scaled
    /// chest contents with it: tier-1 gold runs 176..357 at chest level 1-10 and
    /// 967..1054 at 91-100.
    pub chest_level: u64,
    pub reward: RewardGrant,
}

/// `deploy/static/chest_loots.json` — capture-derived loot pools, keyed by chest tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChestLootTables {
    /// Tiers with enough samples to stand as a table (1, 2, 3: 154/305/270 openings).
    #[serde(default)]
    pub tiers: BTreeMap<u64, Vec<ChestLootSample>>,
    /// Tiers observed too few times to be a table (4: 11 openings, 5: 1). Kept
    /// because repeating one of eleven *real* tier-4 bundles is closer to retail
    /// than handing a tier-4 chest the tier-3 table, but it is a thin pool and
    /// `docs/chest-loot-extraction.md` says so plainly.
    #[serde(default)]
    pub provisional_tiers: BTreeMap<u64, Vec<ChestLootSample>>,
}

impl ChestLootTables {
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty() && self.provisional_tiers.is_empty()
    }

    /// Whether this tier has captured loot of its own, rather than borrowing
    /// another tier's table. Asserted at startup for every grantable tier.
    pub fn has_own_pool(&self, tier: u64) -> bool {
        self.tiers.get(&tier).is_some_and(|p| !p.is_empty())
            || self.provisional_tiers.get(&tier).is_some_and(|p| !p.is_empty())
    }

    /// The pool to draw a `tier` chest's loot from, and whether it is that tier's own.
    fn pool_for(&self, tier: u64) -> Option<(&[ChestLootSample], bool)> {
        if let Some(pool) = self.tiers.get(&tier).filter(|p| !p.is_empty()) {
            return Some((pool.as_slice(), true));
        }
        if let Some(pool) = self.provisional_tiers.get(&tier).filter(|p| !p.is_empty()) {
            return Some((pool.as_slice(), true));
        }
        // No observations for this tier at all: fall back to the richest table at or
        // below it, else the poorest one above. Not faithful — but a chest that pays
        // nothing is worse, and the only tiers this can hit are ones we never saw.
        let below = self.tiers.range(..=tier).next_back();
        let above = self.tiers.range(tier..).next();
        below
            .or(above)
            .filter(|(_, pool)| !pool.is_empty())
            .map(|(_, pool)| (pool.as_slice(), false))
    }
}

/// Pick a loot bundle for a chest of `tier` and `level`, keyed deterministically by
/// its id: the samples closest to `level` are the candidates, and the chest id picks
/// among them. Returns `None` only when there is no loot data at all.
pub fn pick_loot<'a>(
    tables: &'a ChestLootTables,
    tier: u64,
    level: u64,
    chest_id: &str,
) -> Option<&'a RewardGrant> {
    let (pool, exact_tier) = tables.pool_for(tier)?;
    // A fallback is a silent infidelity, so it is not allowed to go unnoticed:
    // `has_own_pool` is asserted at startup for every tier the game can grant
    // (see `static_loader`'s static-data test), which makes this branch unreachable
    // for real data rather than merely unlikely.
    let _ = exact_tier;

    let nearest = pool
        .iter()
        .map(|s| s.chest_level.abs_diff(level))
        .min()
        .expect("pool_for never returns an empty pool");
    let candidates: Vec<&ChestLootSample> = pool
        .iter()
        .filter(|s| s.chest_level.abs_diff(level) == nearest)
        .collect();

    let hash = chest_id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    Some(&candidates[(hash as usize) % candidates.len()].reward)
}

fn stable_nonce(key: &str) -> u64 {
    key.bytes().fold(0xCBF2_9CE4_8422_2325, |acc, b| {
        (acc ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// The fewest retail openings a tier 1-3 roll draws its parts from (#335).
///
/// Twelve keeps a level-100 Gold chest on chests retail opened at 87-100 (the
/// whole pool is 270 openings over levels 3-100), while turning the 1-4 candidates
/// the nearest-level pick left at high levels into twelve or more.
pub const MIN_NEIGHBOURS: usize = 12;

/// The retail openings of this tier nearest to `level`: every opening within the
/// distance of the [`MIN_NEIGHBOURS`]-th nearest one, ties included.
pub fn neighbourhood(pool: &[ChestLootSample], level: u64) -> Vec<&ChestLootSample> {
    let mut distances: Vec<u64> = pool.iter().map(|s| s.chest_level.abs_diff(level)).collect();
    if distances.is_empty() {
        return Vec::new();
    }
    distances.sort_unstable();
    let radius = distances[MIN_NEIGHBOURS.min(distances.len()) - 1];
    pool.iter()
        .filter(|s| s.chest_level.abs_diff(level) <= radius)
        .collect()
}

/// One draw from `samples`, uniform (every retail opening is one observation).
fn draw<'a>(samples: &[&'a ChestLootSample], seed: u64) -> &'a ChestLootSample {
    samples[(seed % samples.len() as u64) as usize]
}

/// Compose one chest's contents from parts of retail openings at its level (#335).
///
/// THE BUG. `pick_loot` replayed one whole recorded opening, chosen among the
/// openings at the single nearest observed level. Above level 40 that is one to
/// four openings for most levels: a Gold chest at level 80, 81, 82 or 87 could
/// only ever pay one reward, a Wooden chest at 82-87 one, at 88-100 three. Retail
/// rolled every chest afresh: 154/154, 305/305 and 270/270 distinct.
///
/// THE ROLL. Like the store chests (#310/#455): from the [`neighbourhood`] of
/// openings at this level, the currencies (gold, and gems on Wooden and Silver)
/// come from one opening, the stackables from another, and each item slot from
/// its own opening, position for position. Whether an optional item slot is
/// filled (Wooden chests pay an item 44 times in 154) follows the currencies'
/// opening. Every part is something retail paid for this tier at this level;
/// nothing is invented, and the payout stays in the neighbourhood's range.
pub fn compose_loot(pool: &[ChestLootSample], level: u64, nonce: u64) -> Option<RewardGrant> {
    use crate::features::store_bundles::mix;

    let near = neighbourhood(pool, level);
    if near.is_empty() {
        return None;
    }
    let seed = mix(nonce ^ level.rotate_left(29));
    let base = draw(&near, seed);
    let stacks = draw(&near, mix(seed ^ 0x2545_F491_4F6C_DD1D));

    let mut grant = RewardGrant {
        currencies: base.reward.currencies.clone(),
        stackable_items: stacks.reward.stackable_items.clone(),
        character_xp: base.reward.character_xp,
        town_xp: base.reward.town_xp,
        ..RewardGrant::default()
    };
    for slot in 0..base.reward.items.len() {
        let filled: Vec<&ChestLootSample> = near
            .iter()
            .copied()
            .filter(|s| s.reward.items.len() > slot)
            .collect();
        let slot_seed = mix(seed ^ 0xA076_1D64_78BD_642F_u64.wrapping_mul(slot as u64 + 1));
        grant.items.push(draw(&filled, slot_seed).reward.items[slot].clone());
    }
    Some(grant)
}

/// Roll the grant for a chest open.
///
/// Tier-5 (Legendary) and tier-4 (Elder) treasury captures are too sparse to use
/// directly (one and eleven openings), while the retail Legendary and Epic store
/// chest corpora hold 4,697 and 405 same-shaped chest rewards. Tiers 1-3 are
/// composed from the capture-derived treasury table ([`compose_loot`]). `owned`
/// is every item template the character holds: an artifact among them is never
/// paid again (#310). Deterministic in `chest_key`, so a retried open pays the same.
pub fn roll_loot(
    tables: &ChestLootTables,
    tier: i64,
    level: u64,
    chest_key: &str,
    owned: &HashSet<Uuid>,
) -> Option<RewardGrant> {
    if tier == TUTORIAL_FIRST_CHEST {
        let pool = tutorial_chest_loot();
        if !pool.is_empty() {
            let pick = (stable_nonce(chest_key) as usize) % pool.len();
            return Some(pool[pick].reward.clone());
        }
    }
    // No other tier below 1 exists in the APK enum; treat one as tier 1.
    let tier = tier.max(1) as u64;
    let nonce = stable_nonce(chest_key);
    crate::features::store_bundles::roll_treasury_chest(tier, level, nonce, owned).or_else(|| {
        let (pool, _) = tables.pool_for(tier)?;
        compose_loot(pool, level, nonce)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::GOLD;
    use std::collections::{HashMap, HashSet};

    fn sample(chest_level: u64, gold: u64) -> ChestLootSample {
        ChestLootSample {
            chest_level,
            reward: RewardGrant {
                currencies: HashMap::from([(GOLD, gold)]),
                ..Default::default()
            },
        }
    }

    fn tables() -> ChestLootTables {
        ChestLootTables {
            tiers: BTreeMap::from([
                (1, vec![sample(1, 100), sample(90, 900)]),
                (2, vec![sample(1, 200), sample(90, 2000)]),
            ]),
            provisional_tiers: BTreeMap::from([
                (4, vec![sample(80, 20000)]),
                (5, vec![sample(90, 5)]),
            ]),
        }
    }

    #[test]
    fn pick_is_deterministic_per_chest_id() {
        let t = tables();
        let a = pick_loot(&t, 1, 5, "7").unwrap().currencies[&GOLD];
        let b = pick_loot(&t, 1, 5, "7").unwrap().currencies[&GOLD];
        assert_eq!(a, b, "same chest -> same loot");
    }

    #[test]
    fn tier_selects_the_tier_table() {
        let t = tables();
        assert_eq!(pick_loot(&t, 1, 1, "1").unwrap().currencies[&GOLD], 100);
        assert_eq!(pick_loot(&t, 2, 1, "1").unwrap().currencies[&GOLD], 200);
    }

    #[test]
    fn level_selects_the_nearest_sample() {
        let t = tables();
        assert_eq!(pick_loot(&t, 1, 3, "1").unwrap().currencies[&GOLD], 100);
        assert_eq!(pick_loot(&t, 1, 88, "1").unwrap().currencies[&GOLD], 900);
    }

    #[test]
    fn provisional_tier_is_used_before_another_tiers_table() {
        let t = tables();
        assert_eq!(pick_loot(&t, 4, 80, "1").unwrap().currencies[&GOLD], 20000);
    }

    #[test]
    fn unobserved_tier_falls_back_to_the_nearest_table_below() {
        let t = tables();
        // Tier 3 was never observed in this fixture: prefer tier 2 over tier 1.
        assert_eq!(pick_loot(&t, 3, 1, "1").unwrap().currencies[&GOLD], 200);
    }

    #[test]
    fn has_own_pool_distinguishes_a_fallback() {
        let t = tables();
        assert!(t.has_own_pool(1));
        assert!(t.has_own_pool(4), "provisional counts as its own");
        assert!(!t.has_own_pool(3), "tier 3 would borrow tier 2's table");
    }

    #[test]
    fn empty_tables_yield_none() {
        assert!(pick_loot(&ChestLootTables::default(), 1, 1, "1").is_none());
    }

    #[test]
    fn legendary_chests_use_the_large_retail_corpus() {
        let t = tables();
        assert_eq!(
            pick_loot(&t, 5, 90, "anything").unwrap().currencies[&GOLD],
            5,
            "control: the provisional tier-5 treasury table is a one-roll trap"
        );

        let mut seen = HashSet::new();
        for i in 0..40 {
            let reward = roll_loot(&t, 5, 90, &format!("legendary-{i}"), &HashSet::new()).unwrap();
            seen.insert(serde_json::to_string(&reward).unwrap());
        }
        assert!(
            seen.len() > 5,
            "40 Legendary chests only produced {} distinct rewards",
            seen.len()
        );
    }

    /// Report #307. The tutorial's first chest is `TierSpecial_TutorialFirstChest`
    /// (-1) in the treasury, and retail paid it from its own scripted table, not
    /// the wooden (tier 1) or silver (tier 2) one: in all 3 captured openings,
    /// 58-65 gold, exactly one item, and the same four starter stackables. A
    /// tier-1 chest at level 1 paid 176-201 gold and no item.
    #[test]
    fn tutorial_first_chest_pays_the_retail_tutorial_bundle() {
        use crate::user_data::Chest;
        const TUTORIAL_STACKABLES: [&str; 4] = [
            "f00f350d-97c2-47cd-a554-e1a37c9ff7f2",
            "05a7d501-f096-41e9-8443-fd6a090d8425",
            "b81952e0-c3c8-4a5c-92c0-8215d3eb71af",
            "42d91529-c88b-4c5b-815b-b55508b4e7ef",
        ];
        let chest: Chest =
            serde_json::from_value(serde_json::json!({"id": "1", "tier": -1, "level": 1}))
                .expect("retail's treasury listing of the tutorial chest must load");

        let t = tables();
        let mut seen = HashSet::new();
        for i in 0..30 {
            let key = format!("char-{i}:1:-1:1:0");
            let reward = roll_loot(&t, chest.tier, chest.level, &key, &HashSet::new())
                .expect("the tutorial chest pays");
            let gold = reward.currencies[&GOLD];
            assert!(
                (58..=65).contains(&gold),
                "tutorial gold {gold}, retail paid 58-65"
            );
            assert_eq!(
                reward.items.len(),
                1,
                "retail's tutorial chest has exactly one item"
            );
            for template in TUTORIAL_STACKABLES {
                let id = template.parse().unwrap();
                assert!(
                    reward.stackable_items.contains_key(&id),
                    "tutorial chest missing starter stackable {template}"
                );
            }
            seen.insert(gold);
        }
        assert!(seen.len() > 1, "all three retail bundles must be reachable");
    }

    #[test]
    fn lower_tiers_still_use_the_treasury_table() {
        let t = tables();
        for i in 0..20 {
            let gold = roll_loot(&t, 1, 1, &format!("k{i}"), &HashSet::new()).unwrap().currencies
                [&GOLD];
            assert!([100, 900].contains(&gold), "tier 1 paid {gold}, not a tier-1 value");
        }
    }

    // ---- #335: the committed retail table ------------------------------------

    fn committed() -> ChestLootTables {
        serde_json::from_str(include_str!("../../../deploy/static/chest_loots.json"))
            .expect("deploy/static/chest_loots.json parses")
    }

    fn fingerprint(r: &RewardGrant) -> String {
        let mut currencies: Vec<_> = r.currencies.iter().collect();
        currencies.sort();
        let mut stacks: Vec<_> = r.stackable_items.iter().collect();
        stacks.sort();
        let items: Vec<String> = r
            .items
            .iter()
            .map(|i| serde_json::to_string(&i.item).unwrap())
            .collect();
        format!("{currencies:?}|{stacks:?}|{items:?}")
    }

    /// The levels a level-100 character's chests come at: its jobs (difficulty
    /// 73-92 on HauDrauf's board) and its own level (Abyss, arena).
    const HIGH_LEVELS: [u64; 7] = [73, 80, 81, 82, 87, 92, 100];

    /// Report #335, "the wooden and silver chests repeat constantly". The old pick
    /// replayed one whole opening from the single nearest observed level: one
    /// possible Gold chest at 80, 81, 82 and 87, one Wooden chest at 82-87, three at
    /// 88-100. Retail never repeated (154/154, 305/305, 270/270 distinct).
    #[test]
    fn high_level_chests_do_not_repeat() {
        let t = committed();
        const ROLLS: usize = 200;
        for tier in 1..=3u64 {
            for level in HIGH_LEVELS {
                let mut seen: HashMap<String, usize> = HashMap::new();
                let mut old = HashSet::new();
                for i in 0..ROLLS {
                    let key = format!("489620db:{}:{tier}:{level}:{}", i % 20 + 1, 300 + i);
                    let r = roll_loot(&t, tier as i64, level, &key, &HashSet::new()).unwrap();
                    *seen.entry(fingerprint(&r)).or_default() += 1;
                    old.insert(fingerprint(pick_loot(&t, tier, level, &key).unwrap()));
                }
                let most = seen.values().copied().max().unwrap();
                let floor = if tier == 1 { 100 } else { 180 };
                assert!(
                    seen.len() >= floor,
                    "tier {tier} level {level}: {ROLLS} chests gave {} distinct rewards \
                     (the old pick gave {})",
                    seen.len(),
                    old.len()
                );
                assert!(
                    most <= 10,
                    "tier {tier} level {level}: one reward came {most} times in {ROLLS}"
                );
            }
        }
        // Control: the measurement can see the bug. Gold at 80 had one reward.
        let old: HashSet<String> = (0..ROLLS)
            .map(|i| fingerprint(pick_loot(&t, 3, 80, &format!("k{i}")).unwrap()))
            .collect();
        assert_eq!(old.len(), 1, "control: the nearest-level pick is a one-reward trap at 80");
    }

    /// Report #335, "gold chests only contain low-level content". A level-100
    /// Gold chest must pay what retail paid for Gold chests near level 100 — the
    /// wider pool must not drag it down to mid-level openings. Every item is a
    /// template retail paid in a Gold chest at level 81+, and the gold sits in the
    /// level 81-100 range (8,069-9,675), far above level 31-40's floor (6,258).
    #[test]
    fn a_level_100_gold_chest_pays_level_100_loot() {
        let t = committed();
        let pool = &t.tiers[&3];
        let high: Vec<&ChestLootSample> = pool.iter().filter(|s| s.chest_level >= 81).collect();
        let high_templates: HashSet<Uuid> = high
            .iter()
            .flat_map(|s| &s.reward.items)
            .map(|i| i.item.item_template_id)
            .collect();
        let gold_of = |r: &RewardGrant| r.currencies.get(&GOLD).copied().unwrap_or(0);
        let lo = high.iter().map(|s| gold_of(&s.reward)).min().unwrap();
        let hi = high.iter().map(|s| gold_of(&s.reward)).max().unwrap();

        for i in 0..300 {
            let r = roll_loot(&t, 3, 100, &format!("c:{i}:3:100:{i}"), &HashSet::new()).unwrap();
            assert_eq!(r.items.len(), 2, "retail's Gold chest pays exactly two items");
            let gold = gold_of(&r);
            assert!((lo..=hi).contains(&gold), "level-100 Gold chest paid {gold} gold");
            for item in &r.items {
                assert!(
                    high_templates.contains(&item.item.item_template_id),
                    "level-100 Gold chest paid {}, which retail paid only below level 81",
                    item.item.item_template_id
                );
            }
        }
        for s in neighbourhood(pool, 100) {
            assert!(s.chest_level >= 85, "a level-100 roll drew on a level-{} opening", s.chest_level);
        }
    }

    /// The tier shape survives composition: Gold always two items, Silver one,
    /// Wooden zero or one; gems only on Wooden and Silver.
    #[test]
    fn composed_chests_keep_the_retail_shape() {
        let t = committed();
        let gem: Uuid = "470c8f58-a8dd-4c07-8c92-843b785e1139".parse().unwrap();
        for tier in 1..=3u64 {
            for level in [1, 10, 25, 40, 60, 86, 100] {
                for i in 0..50 {
                    let r = roll_loot(&t, tier as i64, level, &format!("s{i}"), &HashSet::new())
                        .unwrap();
                    match tier {
                        1 => assert!(r.items.len() <= 1),
                        2 => assert_eq!(r.items.len(), 1),
                        _ => assert_eq!(r.items.len(), 2),
                    }
                    if tier == 1 {
                        assert!(r.currencies.contains_key(&gem), "every Wooden chest paid gems");
                    }
                    if tier == 3 {
                        assert!(!r.currencies.contains_key(&gem), "no Gold chest paid gems");
                    }
                    let key = format!("s{i}");
                    let again = roll_loot(&t, tier as i64, level, &key, &HashSet::new()).unwrap();
                    assert_eq!(fingerprint(&r), fingerprint(&again), "a retried open pays the same");
                }
            }
        }
    }
}
