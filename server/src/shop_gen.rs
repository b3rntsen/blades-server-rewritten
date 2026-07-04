//! Authored, data-driven per-level town-shop STOCK generation.
//!
//! The retail server rolled each vendor's catalog from server-only
//! `CatalogGenerationData` tables (per building type × level) that were never
//! captured. This module AUTHORS a faithful stand-in: it reads an
//! admin-editable config (`shop_stock.json`, loaded into
//! [`crate::ServerGlobal::shop_stock`]) and GENERATES a catalog for a
//! `(buildingTypeId, level)` at shop-open time.
//!
//! Design goals:
//! - **Data-driven.** All tunables (item pool, per-level `maxItems`, `tierCap`,
//!   weights, quantities, refresh window) live in the JSON config, never in Rust,
//!   so a backoffice can tweak the stock without a rebuild.
//! - **Pure.** [`generate_catalog`] is a pure function of `(config, typeId, level,
//!   shopId, window_index)` — no IO, no DB, no clock — so it is unit-testable and a
//!   future admin route can hot-reload the config and rebuild
//!   [`crate::ServerGlobal::shop_stock`] without a restart (the RELOAD HOOK: swap
//!   the parsed `ShopStockConfig` behind the `Arc` — nothing here holds state).
//! - **Deterministic per window.** The roll is seeded from `shopId + window_index`,
//!   so a shop's stock is stable within a refresh window and re-rolls when the
//!   window advances.
//! - **Graceful.** A missing/partial config (unknown building typeId or level)
//!   yields an EMPTY catalog rather than panicking; the caller then falls back to
//!   the capture-derived templates so a vendor is never empty/timing-out.

use std::collections::HashMap;

use blades_lib::static_data::ShopBundleRef;
use serde::Deserialize;
use uuid::Uuid;

/// The parsed `generation` block of `shop_stock.json`, keyed by building `typeId`.
/// Everything is `#[serde(default)]` so a partially-authored config still loads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShopStockConfig {
    /// `generation.<buildingTypeId>` — the per-building pool + per-level params.
    #[serde(default)]
    pub generation: HashMap<Uuid, BuildingGeneration>,
}

/// One building's authored generation data (its draw pool + per-level rules).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuildingGeneration {
    /// The weighted draw pool for this shop type.
    #[serde(default, rename = "itemPool")]
    pub item_pool: Vec<PoolEntry>,
    /// Per-level generation parameters, keyed by the level number as a string
    /// (`"1"`..`"9"`, matching the JSON object keys).
    #[serde(default)]
    pub levels: HashMap<String, LevelParams>,
}

/// One weighted bundle in a shop's draw pool.
#[derive(Debug, Clone, Deserialize)]
pub struct PoolEntry {
    #[serde(rename = "bundleId")]
    pub bundle_id: Uuid,
    /// Quality-ladder index 1..10 (Fine=1 .. Mythical=10). The entry is only
    /// eligible once the level's `tierCap` reaches this tier.
    #[serde(default = "one_u32")]
    pub tier: u32,
    /// Relative pick weight within the unlocked pool.
    #[serde(default = "one_u32")]
    pub weight: u32,
    #[serde(default = "one", rename = "minQuantity")]
    pub min_quantity: u64,
    #[serde(default = "one", rename = "maxQuantity")]
    pub max_quantity: u64,
}

fn one() -> u64 {
    1
}
fn one_u32() -> u32 {
    1
}
// `tier`/`weight` are u32; a tiny generic helper isn't worth it, so provide a u32 one.
impl PoolEntry {
    #[cfg(test)]
    fn new(bundle_id: Uuid, tier: u32, weight: u32, min_q: u64, max_q: u64) -> Self {
        PoolEntry {
            bundle_id,
            tier,
            weight,
            min_quantity: min_q,
            max_quantity: max_q,
        }
    }
}

/// Per-level generation parameters (the roll count + quality cap + refresh window).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LevelParams {
    /// How many distinct bundles to roll into the catalog at this level.
    #[serde(default, rename = "maxItems")]
    pub max_items: u32,
    /// Max item tier (Fine..Mythical index) this level unlocks.
    #[serde(default, rename = "tierCap")]
    pub tier_cap: u32,
    /// Restock window (seconds); the client refetches once it elapses.
    #[serde(default, rename = "refreshSeconds")]
    pub refresh_seconds: i64,
}

impl ShopStockConfig {
    /// Look up a building's per-level params. `None` if the config lacks the
    /// building typeId or that level (→ caller falls back to templates).
    pub fn level_params(&self, type_id: &Uuid, level: u64) -> Option<&LevelParams> {
        self.generation
            .get(type_id)
            .and_then(|b| b.levels.get(&level.to_string()))
    }

    /// The refresh window (seconds) for a building level, if configured.
    pub fn refresh_seconds(&self, type_id: &Uuid, level: u64) -> Option<i64> {
        self.level_params(type_id, level)
            .map(|p| p.refresh_seconds)
            .filter(|s| *s > 0)
    }
}

/// FNV-1a 64-bit over the shop id's bytes plus the window index — a stable,
/// dependency-free seed so a shop's stock is deterministic within a refresh window.
fn seed(shop_id: &Uuid, window_index: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let feed = |h: &mut u64, b: u8| {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in shop_id.as_bytes() {
        feed(&mut h, *b);
    }
    for b in window_index.to_le_bytes() {
        feed(&mut h, b);
    }
    h
}

/// A tiny SplitMix64 PRNG — deterministic, seedable, no external crate.
struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, n)` (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Generate a shop's catalog bundle list for `(type_id, level)`, deterministic per
/// `(shop_id, window_index)`. Returns an EMPTY vec if the config lacks the building
/// or level, or if no pool entry is unlocked at the level's `tierCap` (caller falls
/// back to the capture-derived templates). Pure — safe to call from a test or a hot
/// reload path.
pub fn generate_catalog(
    config: &ShopStockConfig,
    type_id: &Uuid,
    level: u64,
    shop_id: &Uuid,
    window_index: u64,
) -> Vec<ShopBundleRef> {
    let Some(building) = config.generation.get(type_id) else {
        return Vec::new();
    };
    let Some(params) = building.levels.get(&level.to_string()) else {
        return Vec::new();
    };

    // Unlocked pool = entries whose tier the level's cap has reached. Sort by
    // bundle_id so the eligible set is order-stable regardless of JSON/HashMap order
    // (the seed drives the pick, not input ordering).
    let mut unlocked: Vec<&PoolEntry> = building
        .item_pool
        .iter()
        .filter(|e| e.tier <= params.tier_cap)
        .collect();
    if unlocked.is_empty() {
        return Vec::new();
    }
    unlocked.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));

    let mut rng = SplitMix64(seed(shop_id, window_index));

    // Roll up to `maxItems` DISTINCT bundles by weighted selection without
    // replacement (retail catalogs never list the same bundle twice).
    let want = (params.max_items as usize).min(unlocked.len());
    let mut chosen: Vec<&PoolEntry> = Vec::with_capacity(want);
    let mut remaining = unlocked;
    for _ in 0..want {
        let total: u64 = remaining.iter().map(|e| e.weight.max(1) as u64).sum();
        if total == 0 {
            break;
        }
        let mut pick = rng.below(total);
        let mut idx = 0;
        for (i, e) in remaining.iter().enumerate() {
            let w = e.weight.max(1) as u64;
            if pick < w {
                idx = i;
                break;
            }
            pick -= w;
        }
        chosen.push(remaining.remove(idx));
    }

    // Assign each chosen bundle a stable quantity in its [min, max] range.
    chosen
        .into_iter()
        .map(|e| {
            let lo = e.min_quantity.max(1);
            let hi = e.max_quantity.max(lo);
            let span = hi - lo + 1;
            let qty = lo + rng.below(span);
            ShopBundleRef {
                id: e.bundle_id,
                quantity: qty,
            }
        })
        .collect()
}

/// The current refresh-window index for a wall-clock time and window length. Kept
/// here (pure) so the handler can compute it from `SystemTime::now()` and pass it
/// into [`generate_catalog`] without this module touching the clock.
pub fn window_index(now_ms: i64, refresh_seconds: i64) -> u64 {
    let win_ms = (refresh_seconds.max(1)) * 1000;
    (now_ms.max(0) / win_ms) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const FORGE: &str = "26fdb92f-a4df-4928-a97b-dee8699af605";
    const ENCHANTER: &str = "82108d94-ebf7-434f-8623-ca66d7504f27";

    /// Load the committed `deploy/static/shop_stock.json` into the typed config.
    fn load_committed() -> ShopStockConfig {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/shop_stock.json");
        let f = std::fs::File::open(&p).expect("shop_stock.json present");
        serde_json::from_reader(std::io::BufReader::new(f)).expect("shop_stock.json parses")
    }

    fn ty(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    #[test]
    fn committed_config_loads_all_four_shops() {
        let cfg = load_committed();
        for s in [
            FORGE,
            ENCHANTER,
            "e1dd10fc-8b14-4288-9b23-99b0d58388de", // Alchemist
            "b6c023e6-3b81-497f-9c2c-f532ecff3bb2", // Workshop
        ] {
            let b = cfg.generation.get(&ty(s)).expect("building present");
            assert!(!b.item_pool.is_empty(), "{s} pool non-empty");
            assert_eq!(b.levels.len(), 9, "{s} has levels 1..9");
        }
    }

    #[test]
    fn forge_l9_stocks_more_and_higher_tier_than_l1() {
        let cfg = load_committed();
        let forge = ty(FORGE);
        let shop = Uuid::new_v4();

        let l1 = generate_catalog(&cfg, &forge, 1, &shop, 0);
        let l9 = generate_catalog(&cfg, &forge, 9, &shop, 0);

        assert!(!l1.is_empty(), "L1 forge stocks something");
        assert!(!l9.is_empty(), "L9 forge stocks something");
        // Higher level rolls at least as many items (maxItems grows with level).
        assert!(
            l9.len() >= l1.len(),
            "L9 ({}) >= L1 ({}) item count",
            l9.len(),
            l1.len()
        );

        // The L1 catalog must only contain bundles whose tier is within L1's cap;
        // L9 may draw from the whole (higher-tier) pool. Verify by checking that the
        // set of tiers available to L9 strictly exceeds L1's cap.
        let b = cfg.generation.get(&forge).unwrap();
        let cap1 = b.levels.get("1").unwrap().tier_cap;
        let cap9 = b.levels.get("9").unwrap().tier_cap;
        assert!(cap9 > cap1, "L9 tierCap {cap9} > L1 tierCap {cap1}");

        // Every L1-rolled bundle exists in the unlocked-at-L1 subset.
        let l1_allowed: std::collections::HashSet<Uuid> = b
            .item_pool
            .iter()
            .filter(|e| e.tier <= cap1)
            .map(|e| e.bundle_id)
            .collect();
        for entry in &l1 {
            assert!(
                l1_allowed.contains(&entry.id),
                "L1 stocked a bundle above its tierCap: {}",
                entry.id
            );
        }
    }

    #[test]
    fn enchanter_l1_excludes_high_tier_gated_enchants() {
        let cfg = load_committed();
        let ench = ty(ENCHANTER);
        let b = cfg.generation.get(&ench).unwrap();
        let cap1 = b.levels.get("1").unwrap().tier_cap;

        // There ARE gated (tier >= 4) enchant bundles in the pool...
        let gated: Vec<Uuid> = b
            .item_pool
            .iter()
            .filter(|e| e.tier >= 4)
            .map(|e| e.bundle_id)
            .collect();
        assert!(!gated.is_empty(), "enchanter pool has gated tier>=4 bundles");
        // ...and L1's tierCap is below that gate, so none can be rolled at L1.
        assert!(cap1 < 4, "enchanter L1 tierCap {cap1} < gate 4");

        // Roll many windows at L1 and confirm no gated bundle ever appears.
        let gated_set: std::collections::HashSet<Uuid> = gated.into_iter().collect();
        let shop = Uuid::new_v4();
        for w in 0..64 {
            for entry in generate_catalog(&cfg, &ench, 1, &shop, w) {
                assert!(
                    !gated_set.contains(&entry.id),
                    "gated enchant {} leaked into L1 stock (window {w})",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn deterministic_within_a_window() {
        let cfg = load_committed();
        let forge = ty(FORGE);
        let shop = Uuid::new_v4();
        let a = generate_catalog(&cfg, &forge, 5, &shop, 42);
        let b = generate_catalog(&cfg, &forge, 5, &shop, 42);
        assert_eq!(
            a.iter().map(|x| (x.id, x.quantity)).collect::<Vec<_>>(),
            b.iter().map(|x| (x.id, x.quantity)).collect::<Vec<_>>(),
            "same (shop, window) must produce identical stock"
        );
    }

    #[test]
    fn stock_rerolls_across_windows() {
        let cfg = load_committed();
        let forge = ty(FORGE);
        let shop = Uuid::new_v4();
        // Across a spread of windows the stock should not be frozen forever (a
        // deterministic reshuffle each window). Collect several windows' item sets;
        // at least two must differ.
        let mut seen: Vec<Vec<Uuid>> = Vec::new();
        for w in 0..8 {
            let mut ids: Vec<Uuid> =
                generate_catalog(&cfg, &forge, 7, &shop, w).into_iter().map(|x| x.id).collect();
            ids.sort();
            seen.push(ids);
        }
        let all_same = seen.iter().all(|s| *s == seen[0]);
        assert!(!all_same, "stock should re-roll as the refresh window advances");
    }

    #[test]
    fn empty_config_yields_empty_stock_no_panic() {
        let cfg = ShopStockConfig::default();
        let out = generate_catalog(&cfg, &ty(FORGE), 3, &Uuid::new_v4(), 0);
        assert!(out.is_empty(), "empty config → empty stock, no panic");
    }

    #[test]
    fn unknown_level_yields_empty_stock() {
        let cfg = load_committed();
        // Level 99 is not authored → empty, no panic.
        let out = generate_catalog(&cfg, &ty(FORGE), 99, &Uuid::new_v4(), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn respects_max_items_cap() {
        // A synthetic config with a big pool but maxItems = 2 must roll exactly 2.
        let type_id = Uuid::new_v4();
        let pool: Vec<PoolEntry> = (0..20)
            .map(|_| PoolEntry::new(Uuid::new_v4(), 1, 1, 1, 3))
            .collect();
        let mut levels = HashMap::new();
        levels.insert(
            "1".to_string(),
            LevelParams { max_items: 2, tier_cap: 1, refresh_seconds: 3600 },
        );
        let mut generation = HashMap::new();
        generation.insert(type_id, BuildingGeneration { item_pool: pool, levels });
        let cfg = ShopStockConfig { generation };

        let out = generate_catalog(&cfg, &type_id, 1, &Uuid::new_v4(), 0);
        assert_eq!(out.len(), 2, "maxItems caps the roll count");
        // No duplicate bundles (selection without replacement).
        let uniq: std::collections::HashSet<_> = out.iter().map(|x| x.id).collect();
        assert_eq!(uniq.len(), out.len(), "no duplicate bundles in a catalog");
    }

    #[test]
    fn window_index_advances_with_time() {
        assert_eq!(window_index(0, 3600), 0);
        assert_eq!(window_index(3_600_000 - 1, 3600), 0);
        assert_eq!(window_index(3_600_000, 3600), 1);
        assert_eq!(window_index(7_200_000, 3600), 2);
    }
}
