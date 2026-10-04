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
//!   shopId, window_key)` — no IO, no DB, no clock — so it is unit-testable and a
//!   future admin route can hot-reload the config and rebuild
//!   [`crate::ServerGlobal::shop_stock`] without a restart (the RELOAD HOOK: swap
//!   the parsed `ShopStockConfig` behind the `Arc` — nothing here holds state).
//! - **Deterministic per window.** The roll is seeded from `shopId + window_key`,
//!   where the key is the window's start time ([`window_key`]). A window is stored
//!   once rolled, so its stock is stable while it lives, and every new window —
//!   an expired one reopened, or a restock (`/auth/refreshloot`) minutes after the
//!   last — rolls fresh stock.
//! - **Graceful.** A missing/partial config (unknown building typeId or level)
//!   yields an EMPTY catalog rather than panicking; the caller then falls back to
//!   the capture-derived templates so a vendor is never empty/timing-out.

use std::collections::HashMap;

use blades_lib::features::merchant::MerchantGoldBand;
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
    /// Capture-measured pools for specific building levels. These override the
    /// broad authored tier pool when present, because retail's stock quantities
    /// are level-specific: the same bundle can be 1..4 in the approximation but
    /// 18..22 in a high-level captured catalog.
    #[serde(default, rename = "levelPools")]
    pub level_pools: HashMap<String, Vec<PoolEntry>>,
    /// Per-level generation parameters, keyed by the level number as a string
    /// (`"0"`..`"9"`, matching the JSON object keys).
    #[serde(default)]
    pub levels: HashMap<String, LevelParams>,
    /// The merchant's GOLD BUDGET band per building level — how much it can spend
    /// buying the player's items during one 10-hour window. Measured from retail
    /// `catalog.wallet`; per-cell provenance (MEASURED / INTERPOLATED / AUTHORED,
    /// plus observation counts) sits next to the numbers in `shop_stock.json`.
    /// Absent → the merchant has no budget and pays nothing, which is exactly the
    /// behaviour tracker #30 reported.
    #[serde(default, rename = "merchantGold")]
    pub merchant_gold: HashMap<String, MerchantGoldBand>,
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

    /// The merchant's gold band for a `(buildingTypeId, level)`. `None` when the
    /// config does not cover it — the caller then serves a merchant with no
    /// budget, and says so rather than inventing a number.
    pub fn merchant_gold(&self, type_id: &Uuid, level: u64) -> Option<&MerchantGoldBand> {
        self.generation
            .get(type_id)?
            .merchant_gold
            .get(&level.to_string())
    }
}

/// FNV-1a 64-bit over the shop id's bytes plus the window key — a stable,
/// dependency-free seed so a shop's stock is deterministic for one window.
fn seed(shop_id: &Uuid, window_key: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let feed = |h: &mut u64, b: u8| {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in shop_id.as_bytes() {
        feed(&mut h, *b);
    }
    for b in window_key.to_le_bytes() {
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
/// `(shop_id, window_key)`. Returns an EMPTY vec if the config lacks the building
/// or level, or if no pool entry is unlocked at the level's `tierCap` (caller falls
/// back to the capture-derived templates). Pure — safe to call from a test or a hot
/// reload path.
pub fn generate_catalog(
    config: &ShopStockConfig,
    type_id: &Uuid,
    level: u64,
    shop_id: &Uuid,
    window_key: u64,
) -> Vec<ShopBundleRef> {
    let Some(building) = config.generation.get(type_id) else {
        return Vec::new();
    };
    let Some(params) = building.levels.get(&level.to_string()) else {
        return Vec::new();
    };

    // Prefer capture-measured exact pools for levels where we have them. Otherwise
    // fall back to the authored broad tier pool.
    let measured_pool = building
        .level_pools
        .get(&level.to_string())
        .filter(|pool| !pool.is_empty());
    let mut unlocked: Vec<&PoolEntry> = match measured_pool {
        Some(pool) => pool.iter().collect(),
        None => building
            .item_pool
            .iter()
            .filter(|e| e.tier <= params.tier_cap)
            .collect(),
    };
    if unlocked.is_empty() {
        return Vec::new();
    }
    unlocked.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));

    let mut rng = SplitMix64(seed(shop_id, window_key));

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

/// The seed key for a window that starts at `start_ms`: its start time.
///
/// Report #335: the key used to be the wall-clock-aligned 10-hour period
/// (`start / refreshSeconds`). Retail windows start at the first visit, not on a
/// wall-clock boundary, so a restock (`/auth/refreshloot`) or a reopen after a
/// build completed usually fell in the same period as the window it replaced and
/// re-rolled the identical catalog — same bundles, same quantities. Keyed on the
/// start time, every window rolls its own stock.
pub fn window_key(start_ms: i64) -> u64 {
    start_ms.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const FORGE: &str = "26fdb92f-a4df-4928-a97b-dee8699af605";
    const ENCHANTER: &str = "82108d94-ebf7-434f-8623-ca66d7504f27";
    const ALCHEMIST: &str = "e1dd10fc-8b14-4288-9b23-99b0d58388de";
    const WORKSHOP: &str = "b6c023e6-3b81-497f-9c2c-f532ecff3bb2";

    /// Building typeId -> the merchant whose goods belong in it.
    ///
    /// GROUND TRUTH, not inference. Established by chaining
    /// `shops.json.byShop` (captured shop id -> catalog template) through the
    /// live towns in prod (that shop id is a building INSTANCE, which carries
    /// its `typeId`) to the template's bundle names:
    ///
    ///   26fdb92f  49/49 Forge      e1dd10fc  16/16 Alchemist
    ///   b6c023e6  16/16 Workshop   82108d94  the one a player found serving
    ///                                        forge goods -> Enchanter
    ///
    /// It agrees with the `editorName` already on each block. Those labels were
    /// always right; only the pools were wrong.
    const MERCHANT_OF: [(&str, &str); 4] = [
        (FORGE, "Forge"),
        (ENCHANTER, "Enchanter"),
        (ALCHEMIST, "Alchemist"),
        (WORKSHOP, "Workshop"),
    ];

    /// Load the committed `deploy/static/shop_stock.json` into the typed config.
    fn load_committed() -> ShopStockConfig {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/shop_stock.json");
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
            // Levels 0..9 — retail served level-0 vendors too, measured for all
            // four shop types, so the config covers 10 levels not 9.
            assert_eq!(b.levels.len(), 10, "{s} has levels 0..9");
            // Every level must carry the merchant's gold band, or that vendor pays
            // nothing for the player's items — the tracker #30 defect.
            assert_eq!(b.merchant_gold.len(), 10, "{s} has merchantGold for 0..9");
            for lvl in 0..=9u64 {
                let band = b
                    .merchant_gold
                    .get(&lvl.to_string())
                    .unwrap_or_else(|| panic!("{s} level {lvl} has no merchantGold"));
                assert!(band.base_gold > 0, "{s} level {lvl} merchant gold is 0");
                assert!(
                    band.band_min > 0 && band.band_max >= band.band_min,
                    "{s} level {lvl} has a nonsense gold band"
                );
            }
            // The budget must grow with the shop level — a level-9 vendor holding
            // less than a level-0 one would be a bad interpolation.
            let mut prev = 0u64;
            for lvl in 0..=9u64 {
                let base = b.merchant_gold[&lvl.to_string()].base_gold;
                assert!(
                    base > prev,
                    "{s} merchant gold is not increasing at level {lvl}: {base} <= {prev}"
                );
                prev = base;
            }
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

    /// Retail's Enchanter keeps whole enchant families locked until the shop is
    /// upgraded: no frost salts before level 3, no sapphire before 5, no emerald
    /// before 6. Our authored `tier` numbers do not encode that (they rank by
    /// price, and retail's level-2 Enchanter already sold tier-8 Void Salts), so
    /// this pins the gate to what retail actually listed. Levels 0-1 were never
    /// captured and are derived from level 2, so they must inherit the same lock.
    #[test]
    fn enchanter_low_levels_keep_retails_locked_families_locked() {
        let cfg = load_committed();
        let ench = ty(ENCHANTER);
        // Frost Salts used to be listed here as locked until level 3, and Emerald
        // until level 6. Both rested on the June snapshot, which held no level-0
        // Enchanter open and two level-5 ones. Prod's capture database holds six
        // level-0 opens (two list Frost Salts) and five level-5 opens (two list an
        // Emerald) (report #335).
        let locked = [
            ("3cb7b827-75f6-48f1-ae6b-47743e128e58", "Sapphire", 5),
            ("c60c71e7-8bc3-4495-95c8-fbaee3c6860e", "Emerald", 5),
            (
                "01ada486-b642-43b2-8776-e444dbe9343e",
                "Gold Emerald Ring",
                6,
            ),
        ];
        let shop = Uuid::new_v4();
        for (id, name, first_retail_level) in locked {
            let id = ty(id);
            for level in 0..first_retail_level {
                for w in 0..64 {
                    assert!(
                        generate_catalog(&cfg, &ench, level, &shop, w)
                            .iter()
                            .all(|b| b.id != id),
                        "{name} offered by a level-{level} Enchanter (window {w}); retail first \
                         listed it at level {first_retail_level}"
                    );
                }
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
            let mut ids: Vec<Uuid> = generate_catalog(&cfg, &forge, 7, &shop, w)
                .into_iter()
                .map(|x| x.id)
                .collect();
            ids.sort();
            seen.push(ids);
        }
        let all_same = seen.iter().all(|s| *s == seen[0]);
        assert!(
            !all_same,
            "stock should re-roll as the refresh window advances"
        );
    }

    #[derive(Clone, Copy)]
    struct RetailMeasuredCell {
        type_id: &'static str,
        merchant: &'static str,
        level: u64,
        min_count: usize,
        max_count: usize,
    }

    const RETAIL_MEASURED_CELLS: &[RetailMeasuredCell] = &[
        // The snapshot has exact level-1 Blacksmith data. Enchanter/Alchemist
        // did not have level-1 shop opens with a prior town snapshot, so their
        // nearest low-level measured controls are Enchanter L2 and Alchemist L0.
        RetailMeasuredCell {
            type_id: FORGE,
            merchant: "Blacksmith",
            level: 1,
            min_count: 5,
            max_count: 5,
        },
        RetailMeasuredCell {
            type_id: FORGE,
            merchant: "Blacksmith",
            level: 5,
            min_count: 7,
            max_count: 7,
        },
        RetailMeasuredCell {
            type_id: FORGE,
            merchant: "Blacksmith",
            level: 9,
            min_count: 11,
            max_count: 11,
        },
        RetailMeasuredCell {
            type_id: ENCHANTER,
            merchant: "Enchanter",
            level: 2,
            min_count: 4,
            max_count: 5,
        },
        RetailMeasuredCell {
            type_id: ENCHANTER,
            merchant: "Enchanter",
            level: 5,
            min_count: 5,
            max_count: 5,
        },
        RetailMeasuredCell {
            type_id: ENCHANTER,
            merchant: "Enchanter",
            level: 9,
            min_count: 6,
            max_count: 6,
        },
        RetailMeasuredCell {
            type_id: ALCHEMIST,
            merchant: "Alchemist",
            level: 0,
            min_count: 4,
            max_count: 4,
        },
        RetailMeasuredCell {
            type_id: ALCHEMIST,
            merchant: "Alchemist",
            level: 5,
            min_count: 6,
            max_count: 6,
        },
        RetailMeasuredCell {
            type_id: ALCHEMIST,
            merchant: "Alchemist",
            level: 9,
            min_count: 8,
            max_count: 8,
        },
    ];

    fn measured_pool<'a>(
        cfg: &'a ShopStockConfig,
        type_id: &Uuid,
        level: u64,
    ) -> &'a Vec<PoolEntry> {
        cfg.generation
            .get(type_id)
            .and_then(|b| b.level_pools.get(&level.to_string()))
            .unwrap_or_else(|| panic!("{type_id} level {level} has no measured levelPool"))
    }

    fn measured_band(pool: &[PoolEntry], bundle_id: Uuid) -> Option<(u64, u64)> {
        pool.iter()
            .find(|e| e.bundle_id == bundle_id)
            .map(|e| (e.min_quantity, e.max_quantity))
    }

    fn assert_key_band(
        cfg: &ShopStockConfig,
        type_id: &str,
        level: u64,
        bundle_id: &str,
        lo: u64,
        hi: u64,
    ) {
        let type_id = ty(type_id);
        let bundle_id = ty(bundle_id);
        let pool = measured_pool(cfg, &type_id, level);
        assert_eq!(
            measured_band(pool, bundle_id),
            Some((lo, hi)),
            "{type_id} level {level} measured band for {bundle_id}"
        );
    }

    fn assert_generated_stock_matches_measured_cells(cfg: &ShopStockConfig) {
        let shop = ty("11111111-2222-3333-4444-555555555555");
        for cell in RETAIL_MEASURED_CELLS {
            let type_id = ty(cell.type_id);
            let pool = measured_pool(cfg, &type_id, cell.level);
            let allowed: std::collections::HashMap<Uuid, (u64, u64)> = pool
                .iter()
                .map(|e| (e.bundle_id, (e.min_quantity, e.max_quantity)))
                .collect();
            for window in 0..32 {
                let out = generate_catalog(cfg, &type_id, cell.level, &shop, window);
                assert!(
                    (cell.min_count..=cell.max_count).contains(&out.len()),
                    "{} L{} window {window} generated {} bundles, retail measured {}..{}",
                    cell.merchant,
                    cell.level,
                    out.len(),
                    cell.min_count,
                    cell.max_count
                );
                for bundle in out {
                    let (lo, hi) = allowed.get(&bundle.id).unwrap_or_else(|| {
                        panic!(
                            "{} L{} generated unmeasured bundle {}",
                            cell.merchant, cell.level, bundle.id
                        )
                    });
                    assert!(
                        (*lo..=*hi).contains(&bundle.quantity),
                        "{} L{} generated {} qty {}, retail band is {}..{}",
                        cell.merchant,
                        cell.level,
                        bundle.id,
                        bundle.quantity,
                        lo,
                        hi
                    );
                }
            }
        }
    }

    #[test]
    fn measured_retail_stock_cells_drive_generated_catalogs() {
        let cfg = load_committed();
        assert_generated_stock_matches_measured_cells(&cfg);

        // Tracker #277 controls: these are the retail bands that the old broad
        // authored pool could not express because every entry was capped at 1..4.
        assert_key_band(
            &cfg,
            ENCHANTER,
            9,
            "a52da45d-5a6b-439c-92a6-7bc04048058f", // Imp Stool
            27,
            33,
        );
        assert_key_band(
            &cfg,
            ENCHANTER,
            9,
            "76889fa6-4f69-4425-be4a-81ff991903aa", // Void Salts
            18,
            22,
        );
        assert_key_band(
            &cfg,
            ALCHEMIST,
            9,
            "3059daf9-5d16-445d-8024-405dd988c433", // Blue Dartwing
            20,
            26,
        );
        assert_key_band(
            &cfg,
            ALCHEMIST,
            9,
            "ec0c90cf-7af6-407a-889a-65c03bc317a1", // Potion of Intense Healing
            2,
            4,
        );
    }

    #[test]
    #[should_panic(expected = "measured band")]
    fn measured_retail_stock_negative_control_rejects_old_low_quantity_band() {
        let mut cfg = load_committed();
        let enchanter = ty(ENCHANTER);
        let imp_stool = ty("a52da45d-5a6b-439c-92a6-7bc04048058f");
        let pool = cfg
            .generation
            .get_mut(&enchanter)
            .and_then(|b| b.level_pools.get_mut("9"))
            .expect("enchanter L9 measured pool");
        let entry = pool
            .iter_mut()
            .find(|e| e.bundle_id == imp_stool)
            .expect("Imp Stool measured for L9 Enchanter");
        entry.min_quantity = 1;
        entry.max_quantity = 4;

        assert_key_band(
            &cfg,
            ENCHANTER,
            9,
            "a52da45d-5a6b-439c-92a6-7bc04048058f",
            27,
            33,
        );
    }

    // ── Report #309: stock per building level, pinned to retail ────────────────
    //
    // Every number below is a retail measurement from prod's capture database
    // (retail traffic 2026-05-02..06-30; report #335 re-measured it, #309 had only
    // the 2026-06-07 snapshot), with each shop open labelled by its building's
    // level from the latest town-bearing response
    // (script/extract_shop_level_stock.py). Every catalog template maps to exactly
    // one level.

    const LUMBER: &str = "77bd02df-98c1-46f1-b170-1415bc6b51ae";
    const LIMESTONE: &str = "16b94858-6a39-4b53-b360-b944514407a8";

    /// Retail Workshop lumber and limestone per building level:
    /// `(level, lumber band, limestone band)`; `None` = retail never listed it.
    const RETAIL_WORKSHOP_STACKS: &[(u64, (u64, u64), Option<(u64, u64)>)] = &[
        (0, (5, 10), Some((5, 15))),
        (2, (16, 16), None),
        (3, (20, 22), Some((22, 25))),
        (4, (28, 30), Some((27, 30))),
        (5, (30, 38), Some((32, 35))),
        (6, (40, 46), Some((40, 47))),
        (7, (51, 71), Some((50, 63))),
        (8, (78, 93), Some((99, 99))),
        (9, (175, 200), Some((175, 200))),
    ];

    /// The reported case: a level-4 Workshop offered one limestone. Retail's
    /// level-4 Workshop listed 27-30.
    #[test]
    fn a_level_4_workshop_sells_limestone_by_the_stack_as_retail_did() {
        let cfg = load_committed();
        let workshop = ty(WORKSHOP);
        let limestone = ty(LIMESTONE);
        let shop = ty("902859f2-c817-4ad1-b6dc-0a7cc8cba79b");
        let mut listed = 0;
        for w in 0..256 {
            if let Some(b) = generate_catalog(&cfg, &workshop, 4, &shop, w)
                .into_iter()
                .find(|b| b.id == limestone)
            {
                listed += 1;
                assert!(
                    (27..=30).contains(&b.quantity),
                    "level-4 Workshop limestone {} (window {w}); retail listed 27..30",
                    b.quantity
                );
            }
        }
        // Retail listed it in 3 of 3 level-4 catalogs.
        assert!(
            listed >= 128,
            "level-4 Workshop listed limestone in only {listed}/256 windows"
        );
    }

    /// The stacks grow with every upgrade, exactly as retail's did.
    #[test]
    fn workshop_stacks_grow_with_the_building_level_as_retail() {
        let cfg = load_committed();
        let (lumber, limestone) = (ty(LUMBER), ty(LIMESTONE));
        for &(level, lumber_band, limestone_band) in RETAIL_WORKSHOP_STACKS {
            let pool = measured_pool(&cfg, &ty(WORKSHOP), level);
            assert_eq!(
                measured_band(pool, lumber),
                Some(lumber_band),
                "Workshop L{level} lumber"
            );
            assert_eq!(
                measured_band(pool, limestone),
                limestone_band,
                "Workshop L{level} limestone"
            );
        }
        // Level 1 was never captured (level 0 is, six opens): both must still be smaller than level 2,
        // not the old 1..4 fallback and not bigger than an upgraded shop.
        let l2 = measured_band(measured_pool(&cfg, &ty(WORKSHOP), 2), lumber).unwrap();
        let mut prev = 0;
        for level in 0..=1u64 {
            let (lo, hi) = measured_band(measured_pool(&cfg, &ty(WORKSHOP), level), lumber)
                .unwrap_or_else(|| panic!("Workshop L{level} stocks no lumber"));
            assert!(
                lo > 4,
                "Workshop L{level} lumber {lo}..{hi} is the old 1..4 fallback"
            );
            assert!(
                hi < l2.0,
                "Workshop L{level} lumber {lo}..{hi} >= level 2's {l2:?}"
            );
            assert!(lo > prev, "Workshop lumber does not grow into L{level}");
            prev = lo;
        }
    }

    /// Retail's Workshop listed lumber in 81 and limestone in 82 of 82 level-9
    /// catalogs. The pick weights are fitted so ours does too, near enough.
    #[test]
    fn a_level_9_workshop_nearly_always_lists_lumber_and_limestone() {
        let cfg = load_committed();
        let workshop = ty(WORKSHOP);
        let shop = Uuid::new_v4();
        let (lumber, limestone) = (ty(LUMBER), ty(LIMESTONE));
        let (mut both, n) = (0, 2000);
        for w in 0..n {
            let out = generate_catalog(&cfg, &workshop, 9, &shop, w);
            if out.iter().any(|b| b.id == lumber) && out.iter().any(|b| b.id == limestone) {
                both += 1;
            }
        }
        assert!(
            both * 100 >= n * 90,
            "level-9 Workshop listed lumber and limestone together in {both}/{n} windows; retail 81/82"
        );
    }

    /// Retail's catalog length per building level (constant within a level):
    /// `[level 0..9]`, `0` = no retail open at that level.
    const RETAIL_CATALOG_LENGTH: [(&str, [usize; 10]); 4] = [
        (FORGE, [4, 5, 0, 6, 6, 7, 8, 9, 10, 11]),
        (ENCHANTER, [4, 0, 4, 5, 5, 5, 5, 5, 5, 6]),
        (WORKSHOP, [4, 0, 5, 5, 6, 6, 7, 7, 8, 8]),
        (ALCHEMIST, [4, 5, 5, 6, 6, 6, 7, 7, 8, 8]),
    ];

    #[test]
    fn every_measured_level_lists_as_many_bundles_as_retail() {
        let cfg = load_committed();
        let shop = Uuid::new_v4();
        for (type_id, lengths) in RETAIL_CATALOG_LENGTH {
            for (level, &want) in lengths.iter().enumerate() {
                if want == 0 {
                    continue;
                }
                let got = generate_catalog(&cfg, &ty(type_id), level as u64, &shop, 7).len();
                assert_eq!(
                    got, want,
                    "{type_id} level {level} lists {got} bundles, retail {want}"
                );
            }
        }
    }

    /// The old pools were filed one level low: a catalog opened right after an
    /// upgrade finished was labelled with the level before it. Each bundle below
    /// was in our level-N pool but retail only ever listed it from level N+1.
    #[test]
    fn no_level_pool_holds_the_next_levels_stock() {
        let cfg = load_committed();
        let cases = [
            (
                ALCHEMIST,
                6,
                "7cc1bb62-d65d-4145-b5b6-b9711c79f1ad",
                "Health Potion T6 (retail L7+)",
            ),
            (
                FORGE,
                6,
                "2bd6b2a1-d82c-424b-a4b6-32d1d7f7f612",
                "Glass Dagger (retail L7+)",
            ),
            (
                FORGE,
                0,
                "7334deaf-b2fc-435d-84f7-aa192349f877",
                "Leather (retail L1)",
            ),
            (
                ENCHANTER,
                2,
                "337a0bbc-2408-49a9-9dca-598863bdd210",
                "Amethyst (retail L3+)",
            ),
        ];
        for (type_id, level, bundle, what) in cases {
            let pool = measured_pool(&cfg, &ty(type_id), level);
            assert!(
                measured_band(pool, ty(bundle)).is_none(),
                "{type_id} level-{level} pool still holds {what}"
            );
        }
    }

    /// Every merchant wallet retail showed must fit its level's gold band. The
    /// Forge's level-1 row was an invented 800..1,000; retail's three level-1
    /// Forges held 1,174, 1,264 and 1,272.
    #[test]
    fn merchant_gold_bands_hold_every_retail_wallet() {
        let cfg = load_committed();
        // (shop, level, lowest and highest retail wallet at that level)
        let wallets = [
            (FORGE, 0, 545, 724),
            (FORGE, 1, 1174, 1272),
            (FORGE, 9, 22066, 26907),
            (ENCHANTER, 6, 13896, 14188),
            (ENCHANTER, 7, 16851, 19529),
            (WORKSHOP, 4, 8143, 9474),
            (WORKSHOP, 8, 26147, 27923),
            (WORKSHOP, 9, 30582, 38336),
            (ALCHEMIST, 8, 17330, 19309),
            (ALCHEMIST, 9, 21749, 26951),
        ];
        for (type_id, level, lo, hi) in wallets {
            let band = cfg
                .merchant_gold(&ty(type_id), level)
                .unwrap_or_else(|| panic!("{type_id} level {level} has no gold band"));
            assert!(
                band.band_min <= lo && hi <= band.band_max,
                "{type_id} level {level} gold band {}..{} misses retail wallets {lo}..{hi}",
                band.band_min,
                band.band_max
            );
        }
        // The level-2 Forge row used to hold the level-1 catalog's gold.
        let forge = ty(FORGE);
        let l1 = cfg.merchant_gold(&forge, 1).unwrap().base_gold;
        let l2 = cfg.merchant_gold(&forge, 2).unwrap().base_gold;
        let l3 = cfg.merchant_gold(&forge, 3).unwrap().base_gold;
        assert!(
            l1 < l2 && l2 < l3,
            "Forge gold not rising 1->2->3: {l1}, {l2}, {l3}"
        );
        assert!(
            l1 >= 1174,
            "Forge level-1 base gold {l1} below every retail level-1 wallet"
        );
    }

    // ── Report #335: variety at the top level, pinned to retail ─────────────────
    //
    // Prod's capture database (retail traffic 2026-05-02..06-30) holds 507 level-9
    // catalogs from the four merchants; the June snapshot #309 measured from held
    // 91. Per merchant: catalogs opened, distinct bundles across them, and distinct
    // bundle SETS (how many of those catalogs differed from every other).
    const RETAIL_L9_VARIETY: [(&str, usize, usize, usize); 4] = [
        (FORGE, 146, 52, 146),
        (ENCHANTER, 171, 20, 170),
        (WORKSHOP, 82, 34, 80),
        (ALCHEMIST, 108, 42, 95),
    ];

    /// A level-9 merchant offers every bundle retail's level-9 merchant listed,
    /// and its catalogs differ from one another as often as retail's did. The
    /// June snapshot held 17 level-9 Alchemist catalogs and 23 distinct bundles,
    /// so our pool lacked 19 of the bundles retail sold there.
    #[test]
    fn level_9_merchants_vary_their_stock_like_retail() {
        let cfg = load_committed();
        let shop = ty("4271dbf6-52bf-4fb0-85a7-3d5b69a35dab");
        for (type_id, catalogs, bundles, distinct_sets) in RETAIL_L9_VARIETY {
            let pool = measured_pool(&cfg, &ty(type_id), 9);
            assert_eq!(
                pool.len(),
                bundles,
                "{type_id} level-9 pool size vs retail's distinct bundles"
            );

            // Every pool bundle comes up within 40 retail-sized samples.
            let mut listed = std::collections::HashSet::new();
            for w in 0..(40 * catalogs as u64) {
                listed.extend(
                    generate_catalog(&cfg, &ty(type_id), 9, &shop, w)
                        .into_iter()
                        .map(|b| b.id),
                );
            }
            assert_eq!(
                listed.len(),
                bundles,
                "{type_id} level-9 never lists some pool bundles"
            );

            // As many distinct catalogs per retail-sized sample as retail had, give
            // or take 10 points (retail's own sample is one draw).
            let mut sets = std::collections::HashSet::new();
            for w in 0..catalogs as u64 {
                let mut ids: Vec<Uuid> = generate_catalog(&cfg, &ty(type_id), 9, &shop, w)
                    .into_iter()
                    .map(|b| b.id)
                    .collect();
                ids.sort();
                sets.insert(ids);
            }
            assert!(
                sets.len() * 100 >= distinct_sets * 100 - catalogs * 10,
                "{type_id} level 9: {} distinct catalogs in {catalogs}; retail {distinct_sets}",
                sets.len()
            );
        }
    }

    const HEALTH_T8: &str = "ec0c90cf-7af6-407a-889a-65c03bc317a1";
    const MAGICKA_T8: &str = "c7ca5ee2-2e59-45b3-b512-82915b3e7ff3";
    const STAMINA_T8: &str = "814f52c4-0285-4414-9d21-24ba2ff45a20";
    const MAGICKA_T9: &str = "ee0ef15a-d2f4-46f2-865b-399ff7a91c21";
    const STAMINA_T9: &str = "86b3d8a7-16ea-4db5-a2a2-85db62c4ed22";
    /// The one shop bundle holding a lone Health T9 potion. Retail's Alchemist
    /// never listed it; T9/T10 healing came in the gem store's revive packs.
    const HEALTH_T9: &str = "e0ffce3a-5180-4cbe-b5d4-1ac8a7755970";

    /// The report: a level-9 Alchemist never offered the two highest potion tiers.
    /// Neither did retail's, mostly. Of 108 retail level-9 Alchemist catalogs (22
    /// characters), 4 listed a T9 restoration potion (Magicka T9 3, Stamina T9 1),
    /// none listed Health T9, and none listed any T10. Every one listed Health T8
    /// (2-4 of them); Magicka T8 19/108, Stamina T8 12/108.
    #[test]
    fn a_level_9_alchemist_sells_top_tier_potions_at_retails_rate() {
        let cfg = load_committed();
        let alch = ty(ALCHEMIST);
        let pool = measured_pool(&cfg, &alch, 9);
        for id in [MAGICKA_T9, STAMINA_T9] {
            assert!(
                measured_band(pool, ty(id)).is_some(),
                "level-9 Alchemist pool lacks {id}"
            );
        }
        assert!(
            measured_band(pool, ty(HEALTH_T9)).is_none(),
            "retail never listed a lone Health T9"
        );

        let shop = ty("523dcee9-677e-4c70-8efd-a5d02b099eed");
        let n = 10_800u64;
        let rate = |ids: &[&str]| {
            let ids: Vec<Uuid> = ids.iter().map(|s| ty(s)).collect();
            (0..n)
                .filter(|w| {
                    generate_catalog(&cfg, &alch, 9, &shop, *w)
                        .iter()
                        .any(|b| ids.contains(&b.id))
                })
                .count() as f64
                / n as f64
        };
        let within = |what: &str, got: f64, retail: f64, slack: f64| {
            assert!(
                (got - retail).abs() <= slack,
                "level-9 Alchemist lists {what} in {:.1}% of catalogs; retail {:.1}%",
                got * 100.0,
                retail * 100.0
            );
        };
        within(
            "a T9 restoration potion",
            rate(&[MAGICKA_T9, STAMINA_T9]),
            4.0 / 108.0,
            0.025,
        );
        within("Health T8", rate(&[HEALTH_T8]), 1.0, 0.03);
        within("Magicka T8", rate(&[MAGICKA_T8]), 19.0 / 108.0, 0.06);
        within("Stamina T8", rate(&[STAMINA_T8]), 12.0 / 108.0, 0.05);
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
            LevelParams {
                max_items: 2,
                tier_cap: 1,
                refresh_seconds: 3600,
            },
        );
        let mut generation = HashMap::new();
        generation.insert(
            type_id,
            BuildingGeneration {
                item_pool: pool,
                level_pools: HashMap::new(),
                levels,
                merchant_gold: HashMap::new(),
            },
        );
        let cfg = ShopStockConfig { generation };

        let out = generate_catalog(&cfg, &type_id, 1, &Uuid::new_v4(), 0);
        assert_eq!(out.len(), 2, "maxItems caps the roll count");
        // No duplicate bundles (selection without replacement).
        let uniq: std::collections::HashSet<_> = out.iter().map(|x| x.id).collect();
        assert_eq!(uniq.len(), out.len(), "no duplicate bundles in a catalog");
    }

    /// One window's catalog as comparable `(bundle, quantity)` pairs.
    fn roll(
        cfg: &ShopStockConfig,
        type_id: &Uuid,
        level: u64,
        shop: &Uuid,
        key: u64,
    ) -> Vec<(Uuid, u64)> {
        generate_catalog(cfg, type_id, level, shop, key)
            .into_iter()
            .map(|b| (b.id, b.quantity))
            .collect()
    }

    /// Report #335: a restock minutes after a window opened must roll new stock.
    /// Under the old key (the wall-clock 10-hour period) both windows below fell in
    /// the same period and rolled the identical catalog; the control asserts that.
    #[test]
    fn a_restock_minutes_later_rolls_new_stock() {
        let cfg = load_committed();
        let alch = ty(ALCHEMIST);
        let shop = ty("4271dbf6-52bf-4fb0-85a7-3d5b69a35dab");
        let opened: i64 = 1_791_084_464_258; // a level-9 Alchemist window on prod
        let restock = opened + 5 * 60 * 1000;
        let ten_hours = 36_000_000;

        // Control: the old key gave both windows the same seed, hence the same stock.
        assert_eq!(opened / ten_hours, restock / ten_hours);
        assert_eq!(
            roll(&cfg, &alch, 9, &shop, (opened / ten_hours) as u64),
            roll(&cfg, &alch, 9, &shop, (restock / ten_hours) as u64),
        );

        // Fix: across 64 restocks five minutes apart, no two consecutive windows
        // carry the same catalog (bundles AND quantities).
        let mut same = 0;
        for i in 0..64 {
            let a = opened + i * 300_000;
            let b = a + 300_000;
            if roll(&cfg, &alch, 9, &shop, window_key(a))
                == roll(&cfg, &alch, 9, &shop, window_key(b))
            {
                same += 1;
            }
        }
        assert_eq!(same, 0, "{same}/64 restocks re-served the previous catalog");
    }

    /// EVERY shop stocks its OWN merchant's goods.
    ///
    /// WHY THIS EXISTS
    ///
    /// Severius the enchanter was selling elven longswords, hide armour and
    /// quicksilver ingots. All four town shops held another merchant's stock in
    /// a clean four-cycle rotation — Forge's slot had Alchemist goods,
    /// Enchanter's had Forge, Workshop's had Enchanter, Alchemist's had
    /// Workshop.
    ///
    /// It shipped because the pools were never derived. The commit that
    /// authored them says they were "attributed to shop types by clustering the
    /// 94 observed capture-catalog bundles on shared ids" — a heuristic over an
    /// incomplete sample, with nothing checking the result against the merchant
    /// each bundle actually belongs to. Every other test here checks tier gates,
    /// determinism and counts; not one asked whether the stock was the right
    /// SHOP's, so all of them stayed green through it.
    ///
    /// The fork's `shop_bundles.json` carries no names, so the merchant of each
    /// bundle comes from a committed manifest generated from the shipped
    /// asset's `editor_name`s.
    #[test]
    fn every_shop_stocks_its_own_merchants_goods() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data/static/shop_bundle_merchants.json");
        let f = std::fs::File::open(&p).expect("shop_bundle_merchants.json present");
        let manifest: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_reader(std::io::BufReader::new(f)).expect("manifest parses");

        let cfg = load_committed();
        for (type_id, merchant) in MERCHANT_OF {
            let block = cfg
                .generation
                .get(&ty(type_id))
                .unwrap_or_else(|| panic!("{merchant} has no generation block"));
            assert!(!block.item_pool.is_empty(), "{merchant} pool is empty");

            let mut wrong: Vec<String> = Vec::new();
            for entry in &block.item_pool {
                match manifest.get(&entry.bundle_id.to_string()) {
                    Some(v) if merchant_manifest_allows(v, merchant) => {}
                    // A bundle the manifest does not know is NOT a pass: the
                    // manifest covers every town-merchant bundle, so an unknown
                    // id means the pool holds something that is not town stock.
                    other => wrong.push(format!("{} is {:?}", entry.bundle_id, other)),
                }
            }
            assert!(
                wrong.is_empty(),
                "{} ({}) stocks {} bundle(s) that are not its own: {:?}",
                merchant,
                type_id,
                wrong.len(),
                &wrong[..wrong.len().min(5)]
            );
        }
    }

    fn merchant_manifest_allows(v: &serde_json::Value, merchant: &str) -> bool {
        match v {
            serde_json::Value::String(s) => s == merchant,
            serde_json::Value::Array(list) => list.iter().any(|x| x.as_str() == Some(merchant)),
            _ => false,
        }
    }
}
