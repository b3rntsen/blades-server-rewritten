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
//! The pick is deterministic in the chest id, so a given chest always yields the
//! same thing however many times the client retries. The handler re-mints the
//! instanced item ids before granting (capture ids would collide across players).

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

/// Roll the grant for a chest open.
///
/// Tier-5 (Legendary) and tier-4 (Elder) treasury captures are too sparse to use
/// directly (one and eleven openings), while the retail Legendary and Epic store
/// chest corpora hold 4,697 and 405 same-shaped chest rewards. Other tiers stay on
/// the capture-derived treasury table. `owned` is every item template the
/// character holds: an artifact among them is never paid again (#310).
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
    crate::features::store_bundles::roll_treasury_chest(tier, level, stable_nonce(chest_key), owned)
        .or_else(|| pick_loot(tables, tier, level, chest_key).cloned())
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
        assert_eq!(roll_loot(&t, 1, 1, "1", &HashSet::new()).unwrap().currencies[&GOLD], 100);
    }
}
