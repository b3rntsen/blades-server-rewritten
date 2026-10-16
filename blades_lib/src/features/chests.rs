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
    nearest(pool.iter().collect(), level)
}

/// [`neighbourhood`] over any set of openings.
fn nearest(samples: Vec<&ChestLootSample>, level: u64) -> Vec<&ChestLootSample> {
    let mut distances: Vec<u64> = samples.iter().map(|s| s.chest_level.abs_diff(level)).collect();
    if distances.is_empty() {
        return Vec::new();
    }
    distances.sort_unstable();
    let radius = distances[MIN_NEIGHBOURS.min(distances.len()) - 1];
    samples
        .into_iter()
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

    let mut stackable_items = compose_stackables(&near, stacks, mix(seed ^ 0x381_5EED_C4E5_7381));
    settle_enchanting(&mut stackable_items, pool, level, mix(seed ^ 0x0E4C_4A47_0381_0381));
    let mut grant = RewardGrant {
        currencies: base.reward.currencies.clone(),
        stackable_items,
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

/// A material in the sense of [`compose_stackables`]: a crafting ingredient (it
/// has a gem price) that is not a soul gem.
fn is_material(id: &Uuid) -> bool {
    crate::economy::missing_resources::gem_cost_for_ingredient(*id, 1).is_some()
        && !crate::features::store_bundles::SOUL_GEMS.contains(id)
}

/// A chest's stackables, composed from the retail openings at its level (#381).
///
/// THE BUG. [`compose_loot`] copied the whole stackable set of one opening, so a
/// level-45 Wooden chest could only ever hold one of the 16 sets retail paid at
/// 34-45: two chests matched about one time in sixteen (6%), and opening a few
/// dozen showed the same handful of materials again and again ("Wooden and Silver
/// chests repeat very often"). Retail's never did: two retail chests of a tier
/// within a neighbourhood had the same set 0-2% of the time.
///
/// THE ROLL. `shape` (one opening) still decides the chest's structure: its soul
/// gem and its potion are kept whole, so a chest never holds two potions or two
/// kinds of gem and the soul-gem ladder by level (#368) is untouched, and so is a
/// material every neighbouring opening paid (Clay, in every Silver and Gold
/// chest). Each of its other materials is replaced by a material drawn from all
/// the neighbourhood's materials, entry by entry, a material at most once. The
/// chance a chest holds any one material — Frost Salts, Pearl — stays retail's at
/// that level; only the combinations vary.
fn compose_stackables(
    near: &[&ChestLootSample],
    shape: &ChestLootSample,
    seed: u64,
) -> std::collections::HashMap<Uuid, u64> {
    use crate::features::store_bundles::mix;

    let everywhere = |id: &Uuid| near.iter().all(|s| s.reward.stackable_items.contains_key(id));
    // Sorted, so the draw does not depend on HashMap iteration order.
    let mut pool: Vec<(Uuid, u64)> = near
        .iter()
        .flat_map(|s| s.reward.stackable_items.iter())
        .filter(|(id, _)| is_material(id) && !everywhere(id))
        .map(|(id, q)| (*id, *q))
        .collect();
    pool.sort_unstable();

    let mut out = std::collections::HashMap::new();
    let mut slots = 0u64;
    for (id, q) in &shape.reward.stackable_items {
        if is_material(id) && !everywhere(id) {
            slots += 1;
        } else {
            out.insert(*id, *q);
        }
    }
    if pool.is_empty() {
        return shape.reward.stackable_items.clone();
    }
    for slot in 0..slots {
        // A material already in the chest is drawn again rather than stacked.
        for attempt in 0..16u64 {
            let roll = mix(seed ^ slot.wrapping_mul(0x9E37_79B9) ^ (attempt << 40));
            let (id, q) = pool[(roll % pool.len() as u64) as usize];
            if let std::collections::hash_map::Entry::Vacant(e) = out.entry(id) {
                e.insert(q);
                break;
            }
        }
    }
    out
}

/// The enchanting ingredients: the non-soul-gem inputs of `enchanting.json`.
pub(crate) const ENCHANTING_INGREDIENTS: [Uuid; 7] = [
    Uuid::from_u128(0x05b4dd6b_796f_4088_a772_0e33ea3db976), // Frost Salts
    Uuid::from_u128(0xf1c5c03c_5297_48fb_a9d2_5f32bffb467c), // Fire Salts
    Uuid::from_u128(0x10c54e43_a2a1_4833_9491_4c19149f43b5), // Void Salts
    Uuid::from_u128(0x9687d83c_aa7b_4cf3_a69f_0bba8204fa61), // Glow Dust
    Uuid::from_u128(0x07f380dd_f123_4ef7_9b12_8b9fdb03c3e1), // Imp Stool
    Uuid::from_u128(0x7116a2a8_ac2d_4cd9_8b7c_b80c397d3f50), // Honeycomb
    Uuid::from_u128(0x8b7d0044_3a38_4ee0_af45_d2892b0f508d), // Beehive Husk
];

fn holds_enchanting(r: &RewardGrant) -> bool {
    r.stackable_items.keys().any(|id| ENCHANTING_INGREDIENTS.contains(id))
}

/// Share of chests holding an enchanting ingredient, for the tiers that paid any
/// in retail (only Wooden; Silver and Gold paid none and stay at none).
///
/// OWNER-APPROVED DEVIATION FROM RETAIL (#381/#383, 2026-10-13): retail's pooled
/// Wooden rate is 51 of 154 openings (33%); the owner raised it to help new and
/// returning players who must rebuild their characters. Tune here.
pub const ENCHANTING_CHEST_SHARE: f64 = 0.50;

/// Whether a chest holds an enchanting ingredient: [`ENCHANTING_CHEST_SHARE`], the
/// same at every level (#381, HauDrauf: "the quantity varied, but not the
/// occurrence itself").
///
/// Per level band, retail's Wooden chests held one in 41% of openings at 1-20,
/// 17% at 21-40 and 56% at 61-80, but every opening at 21-60 is one character's
/// last three days of retail, and a test that keeps each capture session's
/// openings together does not reject one rate for all levels (p = 0.06; 1-20
/// against 61-100, where several characters played, 41% vs 55%, p = 0.29). So the
/// rate is level-independent; retail's pooled rate is 51 of 154 Wooden openings
/// (33%), raised by the owner to [`ENCHANTING_CHEST_SHARE`]. Silver and Gold paid
/// none in 575 and stay at none. Which ingredients, how many kinds and how many
/// of each still come from the retail openings nearest the chest's level that
/// paid any. Every other stackable is left as [`compose_stackables`] made it.
fn settle_enchanting(
    stacks: &mut std::collections::HashMap<Uuid, u64>,
    pool: &[ChestLootSample],
    level: u64,
    seed: u64,
) {
    use crate::features::store_bundles::mix;

    let paid: Vec<&ChestLootSample> = pool.iter().filter(|s| holds_enchanting(&s.reward)).collect();
    if paid.is_empty() {
        return;
    }
    stacks.retain(|id, _| !ENCHANTING_INGREDIENTS.contains(id));
    if (mix(seed) % 10_000) as f64 >= ENCHANTING_CHEST_SHARE * 10_000.0 {
        return;
    }
    let source = draw(&nearest(paid, level), mix(seed ^ 0x5EED_E4C4_A475_0381));
    for (id, q) in &source.reward.stackable_items {
        if ENCHANTING_INGREDIENTS.contains(id) {
            stacks.insert(*id, *q);
        }
    }
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
    let elder_treasury = tables.provisional_tiers.get(&4).map(Vec::as_slice);
    crate::features::store_bundles::roll_treasury_chest(tier, level, nonce, owned, elder_treasury)
        .or_else(|| {
            let (pool, _) = tables.pool_for(tier)?;
            let mut grant = compose_loot(pool, level, nonce)?;
            if tier == 1 {
                widen_wooden_metals(tables, &mut grant, level, nonce);
            }
            generate_treasury_gear(&mut grant, nonce);
            if tier == 3 {
                add_gold_chest_artifact(&mut grant, level, nonce, owned);
            }
            fill_jewelry_grading(&mut grant, nonce);
            Some(grant)
        })
}

/// [`roll_loot`] for a chest a character of `character_level` opens.
///
/// From level 49 a chest is minted at level 100 (#398), whose tables pay almost no
/// gems. For a 49-99 character the gem materials a chest at the character's own
/// level would have paid, rolled under the same key, are kept on top
/// ([`crate::features::loot_level::keeps_own_gems`]). Otherwise exactly
/// [`roll_loot`].
pub fn roll_loot_for_character(
    tables: &ChestLootTables,
    tier: i64,
    level: u64,
    chest_key: &str,
    owned: &HashSet<Uuid>,
    character_level: u64,
) -> Option<RewardGrant> {
    use crate::features::loot_level::{keep_own_gems, keeps_own_gems};
    let mut grant = roll_loot(tables, tier, level, chest_key, owned)?;
    let own_level = level.min(character_level);
    if keeps_own_gems(character_level as i64) && own_level != level {
        if let Some(own) = roll_loot(tables, tier, own_level, chest_key, owned) {
            keep_own_gems(&mut grant.stackable_items, &own.stackable_items);
        }
    }
    Some(grant)
}

/// A Wooden, Silver or Gold chest's gear, generated around the retail piece
/// [`compose_loot`] picked for each slot, as a Legendary chest's is (#368; #392
/// audit).
///
/// THE BUG. Each item slot copied one retail piece verbatim from the openings
/// nearest the chest's level. Where few of them paid an item that was one piece
/// for every chest: a Wooden chest paid an item in 44 of 154 retail openings, so
/// at level 1 every item was the same Hide Gauntlets, at 49 the same Dwarven Plate
/// Armor, at 60 the same Stalhrim Gauntlets; over ten levels 1,088 Wooden items
/// were 26 instances. Silver and Gold jewellery repeated the same way (a Gold
/// Emerald ring with the same bonuses in over half the rings at 40-49).
///
/// THE EVIDENCE that retail generated these too: every one of the 889 items in
/// the retail treasury openings (44 Wooden, 305 Silver, 540 Gold) is a member of
/// the APK loot pool for its material tier, and retail repeated an instance at
/// most three times.
///
/// THE ROLL is [`super::legendary_gear::generate`]: the retail piece stays the
/// shell — its material tier, tempering, enchant count and enchant tier — and
/// around it the kind, the template of that tier and the enchantments are drawn
/// from the APK tables. A jewel is then graded by the one jewellery rule (#393:
/// 8 / 17 / 75% one / two / three properties), not the shell's grade, which the
/// Elder and Legendary chests keep. Nothing a chest paid before is out
/// of reach: the shell's own template and enchantments remain possible, at the
/// same tier. A piece outside the pool (none today) is paid as before.
fn generate_treasury_gear(grant: &mut RewardGrant, nonce: u64) {
    use crate::features::store_bundles::{is_artifact, mix};
    for (slot, reward) in grant.items.iter_mut().enumerate() {
        if is_artifact(&reward.item.item_template_id) {
            continue;
        }
        let seed = mix(nonce ^ 0x3C79_AC49_2BA7_B653 ^ (slot as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93));
        if let Some(mut item) = super::legendary_gear::generate(&reward.item, seed) {
            super::legendary_gear::regrade_jewelry(&mut item, mix(seed ^ 0x393));
            reward.item = item;
        }
    }
}

/// The grade and GRADING of a treasury chest's ring or necklace whose GRADING the
/// corpus lost and that [`generate_treasury_gear`] did not generate (see
/// [`super::legendary_gear::fill_missing_grading`]).
/// Deterministic in the chest's nonce, so a retried open pays the same ring.
fn fill_jewelry_grading(grant: &mut RewardGrant, nonce: u64) {
    use crate::features::store_bundles::mix;
    for (slot, item) in grant.items.iter_mut().enumerate() {
        let seed = mix(nonce ^ 0x6A4F_3D2C_0392_A0D1 ^ (slot as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        super::legendary_gear::fill_missing_grading(&mut item.item, seed);
    }
}

/// The smithing metals: every ingot, plus Iron Bar, Brass, Moonstone Ore and
/// Dragon Bone (`ingredient_values.json` names).
pub const METALS: [u128; 13] = [
    0x55e82826_2d68_469c_8870_753665ca62cd, // Iron Bar
    0x92b77b2f_bd33_469f_8aad_8a228b9537eb, // Brass
    0xb81952e0_c3c8_4a5c_92c0_8215d3eb71af, // Steel Ingot
    0xb74a5c55_a687_4604_aa59_ba3ddfddcd2a, // Silver Ingot
    0x71bb8562_07ad_4f18_855a_96f9156b8e6b, // Gold Ingot
    0x74f091b5_fd88_464b_a98a_f60a5e8a0f25, // Orichalcum Ingot
    0xf11fb90b_b441_4d72_a33f_50d14d3d6778, // Dwarven Ingot
    0xe80bee76_f92c_4005_9eff_20d1e8c64d24, // Quicksilver Ingot
    0x4312b0ed_e397_4815_9693_a511ecda71de, // Moonstone Ore
    0x85ed5500_3581_4699_8095_4b5ff6514355, // Malachite Ingot
    0x75112030_b248_49b0_9c70_0da8dea150d1, // Ebony Ingot
    0xba9fe442_2cf8_48a8_9416_6d91c2a00cab, // Stalhrim Ingot
    0xd523932f_8c7f_4192_9112_5dbd60883c2b, // Dragon Bone
];

fn is_metal(id: &Uuid) -> bool {
    METALS.contains(&id.as_u128())
}

/// How far either side of a chest's level the metal kinds are read from.
pub const METAL_WINDOW: u64 = 10;

/// Below this many metal stacks in the Wooden openings within [`METAL_WINDOW`],
/// the Wooden table is too thin to say which metals a Wooden chest pays.
pub const THIN_WOODEN_METALS: usize = 8;

/// The metal kinds retail's treasury chests of every tier (Wooden, Silver, Gold)
/// paid within [`METAL_WINDOW`] levels of `level`, sorted.
pub fn treasury_metals_near(tables: &ChestLootTables, level: u64) -> Vec<Uuid> {
    let mut kinds: Vec<Uuid> = (1..=3u64)
        .filter_map(|t| tables.tiers.get(&t))
        .flatten()
        .filter(|s| s.chest_level.abs_diff(level) <= METAL_WINDOW)
        .flat_map(|s| s.reward.stackable_items.keys())
        .copied()
        .filter(is_metal)
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// The number of metal stacks the Wooden openings within [`METAL_WINDOW`] of
/// `level` paid.
pub fn wooden_metal_stacks_near(tables: &ChestLootTables, level: u64) -> usize {
    tables
        .tiers
        .get(&1)
        .into_iter()
        .flatten()
        .filter(|s| s.chest_level.abs_diff(level) <= METAL_WINDOW)
        .map(|s| s.reward.stackable_items.keys().filter(|id| is_metal(id)).count())
        .sum()
}

/// A Wooden chest's metal, varied where the Wooden table is thin (report #381).
///
/// THE COMPLAINT. HauDrauf (#381, Sephoris, level 46): "an excess of ONE kind of
/// ingot everywhere, in this case Quicksilver, doesn't fit." A level-46 Wooden
/// chest paid a metal in 31% of openings, as retail's did, but 79% of those were
/// Quicksilver: the twelve-nearest neighbourhood ([`neighbourhood`]) is the 16
/// Wooden openings at levels 34-45, and their five metal stacks are four
/// Quicksilver and one Malachite. Retail captured 4 Wooden openings above level 40
/// at all, so that is thin data, not a measured retail mix. Retail's Silver and
/// Gold chests at 36-56 (88 openings) paid Malachite, Ebony, Stalhrim and
/// Moonstone, and its dungeon enemies and containers at 41-50 paid Quicksilver
/// beside Moonstone, Malachite, Stalhrim and Ebony.
///
/// THE ROLL. Where the Wooden openings within [`METAL_WINDOW`] levels hold fewer
/// than [`THIN_WOODEN_METALS`] metal stacks (chest level 41 and up), a metal the
/// composition put in the chest keeps its quantity, but its kind is drawn
/// uniformly from [`treasury_metals_near`]: every kind any treasury tier paid
/// within ten levels. Quicksilver stays among them wherever retail paid it. How
/// often a chest holds a metal, and how many, is unchanged; below 41 nothing
/// changes.
fn widen_wooden_metals(tables: &ChestLootTables, grant: &mut RewardGrant, level: u64, nonce: u64) {
    use crate::features::store_bundles::mix;
    if wooden_metal_stacks_near(tables, level) >= THIN_WOODEN_METALS {
        return;
    }
    let kinds = treasury_metals_near(tables, level);
    if kinds.is_empty() {
        return;
    }
    let mut metals: Vec<(Uuid, u64)> = grant
        .stackable_items
        .iter()
        .filter(|(id, _)| is_metal(id))
        .map(|(id, q)| (*id, *q))
        .collect();
    metals.sort_unstable();
    for (slot, (id, qty)) in metals.into_iter().enumerate() {
        let roll = mix(nonce ^ 0x0381_4E7A_15D0_0381 ^ (slot as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let kind = kinds[(roll % kinds.len() as u64) as usize];
        if kind == id || grant.stackable_items.contains_key(&kind) {
            continue;
        }
        grant.stackable_items.remove(&id);
        grant.stackable_items.insert(kind, qty);
    }
}

/// A Gold chest's rare third item (#378 follow-up): one chest in
/// [`GOLD_ARTIFACT_SLOT`](crate::features::store_bundles::GOLD_ARTIFACT_SLOT)
/// has an artifact slot, rolled exactly as a Legendary chest's: an artifact
/// whose level window holds the chest's level, or for one the character holds
/// the generated Legendary piece (#310).
fn add_gold_chest_artifact(grant: &mut RewardGrant, level: u64, nonce: u64, owned: &HashSet<Uuid>) {
    use crate::features::store_bundles::{GOLD_ARTIFACT_SLOT, artifact_slot_item, mix};
    let seed = mix(nonce ^ 0x9B05_688C_2B3E_6C1F);
    let (hits, chests) = GOLD_ARTIFACT_SLOT;
    if mix(seed) % chests >= hits {
        return;
    }
    if let Some(item) = artifact_slot_item(level, seed, owned) {
        // The handler re-mints instance ids; this one only has to be distinct.
        let id = Uuid::from_u64_pair(mix(seed ^ 1), mix(seed ^ 2));
        grant.items.push(crate::economy::RewardItem { id, item });
    }
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
    /// wider pool must not drag it down to mid-level openings. Every item is of a
    /// material tier retail paid in a Gold chest at level 81+ (the template itself
    /// is generated within its tier since the #392 audit), and the gold sits in the
    /// level 81-100 range (8,069-9,675), far above level 31-40's floor (6,258).
    #[test]
    fn a_level_100_gold_chest_pays_level_100_loot() {
        use crate::features::legendary_gear::required_level;
        let t = committed();
        let pool = &t.tiers[&3];
        let high: Vec<&ChestLootSample> = pool.iter().filter(|s| s.chest_level >= 81).collect();
        let high_tiers: HashSet<Option<u64>> = high
            .iter()
            .flat_map(|s| &s.reward.items)
            .map(|i| required_level(&i.item.item_template_id))
            .collect();
        assert!(!high_tiers.contains(&None), "control: every retail piece has a tier");
        let gold_of = |r: &RewardGrant| r.currencies.get(&GOLD).copied().unwrap_or(0);
        let lo = high.iter().map(|s| gold_of(&s.reward)).min().unwrap();
        let hi = high.iter().map(|s| gold_of(&s.reward)).max().unwrap();

        for i in 0..300 {
            let r = roll_loot(&t, 3, 100, &format!("c:{i}:3:100:{i}"), &HashSet::new()).unwrap();
            // Two items; a third only in the rare artifact slot (#378).
            assert!((2..=3).contains(&r.items.len()), "a Gold chest pays two items");
            let gold = gold_of(&r);
            assert!((lo..=hi).contains(&gold), "level-100 Gold chest paid {gold} gold");
            for item in &r.items[..2] {
                assert!(
                    high_tiers.contains(&required_level(&item.item.item_template_id)),
                    "level-100 Gold chest paid {}, a tier retail paid only below level 81",
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
                        _ => assert!((2..=3).contains(&r.items.len())),
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

    /// Report #368: "check that Wooden, Silver and Gold chests ramp up with
    /// level; players built their characters mainly from these three". They do,
    /// as retail's did: at every level with at least five retail openings within
    /// two levels, the median gold sits within 30% of retail's there (single
    /// levels vary that much in retail itself: the 12 Silver openings at 25 paid
    /// a median 2,701, the 42 at 23-27 2,126), and it
    /// never falls by more than a tenth as the level rises (Gold: 3,847 at 10,
    /// 7,433 at 30, 8,185 at 43, 8,965 at 80; retail 4,145 / 7,508 / 8,185 / 8,965).
    #[test]
    fn treasury_chests_ramp_with_level_like_retail() {
        let t = committed();
        let median = |mut v: Vec<u64>| {
            v.sort_unstable();
            v[v.len() / 2]
        };
        for tier in 1..=3u64 {
            let mut previous = 0;
            for level in (5..=100u64).step_by(5) {
                let ours = median(
                    (0..400)
                        .map(|i| {
                            let key = format!("ramp:{i}:{tier}:{level}");
                            let r =
                                roll_loot(&t, tier as i64, level, &key, &HashSet::new()).unwrap();
                            r.currencies.get(&GOLD).copied().unwrap_or(0)
                        })
                        .collect(),
                );
                let retail: Vec<u64> = t.tiers[&tier]
                    .iter()
                    .filter(|s| s.chest_level.abs_diff(level) <= 2)
                    .map(|s| s.reward.currencies.get(&GOLD).copied().unwrap_or(0))
                    .collect();
                if retail.len() >= 5 {
                    let retail = median(retail);
                    assert!(
                        ours.abs_diff(retail) * 10 <= retail * 3,
                        "tier {tier} level {level}: median gold {ours}, retail {retail}"
                    );
                }
                assert!(
                    ours * 10 >= previous * 9,
                    "tier {tier}: median gold fell from {previous} to {ours} at level {level}"
                );
                previous = ours;
            }
        }
    }

    // ---- #381: materials vary, at retail's rates ------------------------------

    /// The enchanting ingredients: the non-soul-gem inputs of `enchanting.json`.
    fn enchanting() -> HashSet<Uuid> {
        let names = ["Frost Salts", "Fire Salts", "Void Salts", "Glow Dust", "Imp Stool", "Honeycomb", "Beehive Husk"];
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../ingredient_values.json")).unwrap();
        let ids: HashSet<Uuid> = raw["_meta"]["names"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| names.contains(&v.as_str().unwrap()))
            .map(|(k, _)| k.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), names.len(), "every enchanting ingredient is named");
        ids
    }

    fn stack_ids(r: &RewardGrant) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = r.stackable_items.keys().copied().collect();
        ids.sort();
        ids
    }

    /// Share of pairs among `sets` that are the same set.
    fn same_pair_share(sets: &[Vec<Uuid>]) -> f64 {
        let mut counts: HashMap<&Vec<Uuid>, u64> = HashMap::new();
        for s in sets {
            *counts.entry(s).or_default() += 1;
        }
        let n = sets.len() as u64;
        let same: u64 = counts.values().map(|c| c * (c - 1) / 2).sum();
        same as f64 / (n * (n - 1) / 2) as f64
    }

    /// Report #381, "Wooden and Silver chests repeat very often". Two chests of one
    /// tier and level held the same stackables about one time in sixteen (each
    /// chest copied one of the 12-22 retail sets at its level); two retail chests in
    /// such a neighbourhood matched 0-2% of the time. Now 1-2%.
    #[test]
    fn chest_stackables_do_not_repeat_like_whole_copied_sets() {
        let t = committed();
        for tier in 1..=3u64 {
            for level in [15u64, 30, 45, 70, 90] {
                let near = neighbourhood(&t.tiers[&tier], level);
                // The old roll: one of the neighbourhood's sets, uniformly.
                let old_sets: Vec<Vec<Uuid>> = near.iter().map(|s| stack_ids(&s.reward)).collect();
                let mut old = 0.0;
                for a in &old_sets {
                    old += old_sets.iter().filter(|b| *b == a).count() as f64;
                }
                let old = old / (old_sets.len() * old_sets.len()) as f64;
                let retail = same_pair_share(&old_sets);
                let ours: Vec<Vec<Uuid>> = (0..400)
                    .map(|i| {
                        let key = format!("381:{i}:{tier}:{level}");
                        stack_ids(&roll_loot(&t, tier as i64, level, &key, &HashSet::new()).unwrap())
                    })
                    .collect();
                let ours = same_pair_share(&ours);
                println!("tier {tier} L{level}: same-set pairs retail {retail:.3}, old roll {old:.3}, now {ours:.3}");
                assert!(old >= 0.025, "control: the old roll repeated ({old:.3})");
                assert!(
                    ours <= 0.03 && ours * 2.0 <= old,
                    "tier {tier} L{level}: {ours:.3} of chest pairs hold the same set (old {old:.3})"
                );
            }
        }
    }

    /// The composition keeps retail's rates. Per material, the share of chests
    /// holding it stays within a few points of the retail openings at that level.
    /// Enchanting ingredients are the exception (#381, HauDrauf): whether a chest
    /// holds one is [`ENCHANTING_CHEST_SHARE`] at every level for the tiers that
    /// paid any (owner-raised from retail's pooled Wooden 51 of 154, 33%, where the per-band 41/17/56% at 1-20/21-40/61-80 is
    /// one character's run at 21-60) — and Silver and Gold never (0 of 575). Which
    /// ones, and how many, follow the retail openings nearest the level that paid
    /// any. Soul gems and potions stay whole: at most one potion, Clay in every
    /// Silver and Gold chest.
    #[test]
    fn composed_stackables_keep_retails_rates() {
        let t = committed();
        let enchanting = enchanting();
        assert_eq!(
            enchanting,
            ENCHANTING_INGREDIENTS.into_iter().collect::<HashSet<Uuid>>(),
            "ENCHANTING_INGREDIENTS names the enchanting ingredients"
        );
        let pooled = |tier: u64| {
            let pool = &t.tiers[&tier];
            (pool.iter().filter(|s| holds_enchanting(&s.reward)).count(), pool.len())
        };
        assert_eq!(pooled(1), (51, 154), "Wooden: retail's pooled enchanting rate");
        assert_eq!(pooled(2).0 + pooled(3).0, 0, "Silver and Gold never paid one");
        let clay: Uuid = "42d91529-c88b-4c5b-815b-b55508b4e7ef".parse().unwrap();
        let rolls = 3000;
        for tier in 1..=3u64 {
            for level in [15u64, 30, 45, 70, 90] {
                let near = neighbourhood(&t.tiers[&tier], level);
                let share = |rewards: &mut dyn Iterator<Item = &RewardGrant>, test: &dyn Fn(&RewardGrant) -> bool| {
                    let (mut hit, mut n) = (0.0f64, 0.0f64);
                    for r in rewards {
                        n += 1.0;
                        if test(r) {
                            hit += 1.0;
                        }
                    }
                    hit / n
                };
                let ours: Vec<RewardGrant> = (0..rolls)
                    .map(|i| {
                        roll_loot(&t, tier as i64, level, &format!("rate:{i}:{tier}:{level}"), &HashSet::new())
                            .unwrap()
                    })
                    .collect();
                let has_enchanting =
                    |r: &RewardGrant| r.stackable_items.keys().any(|id| enchanting.contains(id));
                let retail_e = share(&mut near.iter().map(|s| &s.reward), &has_enchanting);
                let ours_e = share(&mut ours.iter(), &has_enchanting);
                let (k, n) = pooled(tier);
                let want = if k == 0 { 0.0 } else { ENCHANTING_CHEST_SHARE };
                let _ = n;
                println!(
                    "tier {tier} L{level}: chests with an enchanting ingredient retail here {retail_e:.2}, \
                     target {want:.2}, ours {ours_e:.2}"
                );
                if k == 0 {
                    assert_eq!(ours_e, 0.0, "tier {tier} L{level}: retail never paid one");
                } else {
                    assert!((ours_e - want).abs() <= 0.04, "tier {tier} L{level}: {ours_e:.2} vs target {want:.2}");
                    // Given a chest holds one, which ones follow the nearest retail
                    // openings that paid any.
                    let paid = nearest(t.tiers[&tier].iter().filter(|s| holds_enchanting(&s.reward)).collect(), level);
                    let with: Vec<&RewardGrant> = ours.iter().filter(|r| has_enchanting(r)).collect();
                    for id in &enchanting {
                        let has = |r: &RewardGrant| r.stackable_items.contains_key(id);
                        let retail = share(&mut paid.iter().map(|s| &s.reward), &has);
                        let ours = share(&mut with.iter().copied(), &has);
                        assert!(
                            (ours - retail).abs() <= 0.08,
                            "tier {tier} L{level} {id} given an enchanting ingredient: {ours:.2} vs retail {retail:.2}"
                        );
                    }
                }
                let materials: HashSet<Uuid> = near
                    .iter()
                    .flat_map(|s| s.reward.stackable_items.keys())
                    .copied()
                    .filter(|id| is_material(id) && !enchanting.contains(id))
                    .collect();
                // A Wooden chest's metal KIND is widened where the table is thin
                // (#381, `widen_wooden_metals`); how often it holds one is not.
                let widened = tier == 1 && wooden_metal_stacks_near(&t, level) < THIN_WOODEN_METALS;
                if widened {
                    let has_metal = |r: &RewardGrant| r.stackable_items.keys().any(is_metal);
                    let retail = share(&mut near.iter().map(|s| &s.reward), &has_metal);
                    let ours = share(&mut ours.iter(), &has_metal);
                    assert!((ours - retail).abs() <= 0.08, "tier {tier} L{level} any metal: {ours:.2} vs retail {retail:.2}");
                }
                for id in materials {
                    if widened && is_metal(&id) {
                        continue;
                    }
                    let has = |r: &RewardGrant| r.stackable_items.contains_key(&id);
                    let retail = share(&mut near.iter().map(|s| &s.reward), &has);
                    let ours = share(&mut ours.iter(), &has);
                    assert!((ours - retail).abs() <= 0.08, "tier {tier} L{level} {id}: {ours:.2} vs retail {retail:.2}");
                }
                for r in &ours {
                    let potions = r.stackable_items.keys().filter(|id| {
                        crate::economy::missing_resources::gem_cost_for_ingredient(**id, 1).is_none()
                    });
                    assert!(potions.count() <= 1, "two potions in one chest");
                    if tier >= 2 {
                        assert!(r.stackable_items.contains_key(&clay), "tier {tier}: Clay in every chest");
                    }
                }
            }
        }
    }

    /// Report #381, HauDrauf (Sephoris, level 46): "an excess of ONE kind of ingot
    /// everywhere, in this case Quicksilver, doesn't fit." 79% of a level-46 Wooden
    /// chest's metal stacks were Quicksilver, from five retail metal stacks at
    /// 34-45. The kind now comes from every metal retail's treasury chests paid
    /// within ten levels; Quicksilver stays one of them, and the share of chests
    /// holding a metal stays retail's. Below level 41 nothing moves.
    #[test]
    fn report_381_wooden_chest_metals_vary_where_the_table_is_thin() {
        let t = committed();
        let quicksilver = Uuid::from_u128(0xe80bee76_f92c_4005_9eff_20d1e8c64d24);
        let rolls = 3000;
        let roll = |level: u64, i: usize| {
            roll_loot(&t, 1, level, &format!("381-metal:{i}:{level}"), &HashSet::new()).unwrap()
        };
        // The unwidened composition, as live before this change: the control.
        let before = |level: u64, i: usize| {
            let key = format!("381-metal:{i}:{level}");
            compose_loot(&t.tiers[&1], level, stable_nonce(&key)).unwrap()
        };
        for level in [46u64, 49] {
            let (mut kinds, mut was_kinds) = (HashMap::<Uuid, usize>::new(), HashMap::<Uuid, usize>::new());
            let (mut with_metal, mut was_with_metal) = (0usize, 0usize);
            for i in 0..rolls {
                let (now, was) = (roll(level, i), before(level, i));
                let metal = |r: &RewardGrant| r.stackable_items.keys().copied().filter(is_metal).collect::<Vec<_>>();
                with_metal += usize::from(!metal(&now).is_empty());
                was_with_metal += usize::from(!metal(&was).is_empty());
                for id in metal(&now) {
                    *kinds.entry(id).or_default() += 1;
                }
                for id in metal(&was) {
                    *was_kinds.entry(id).or_default() += 1;
                }
                // Quantities are untouched: the same multiset of metal amounts.
                let amounts = |r: &RewardGrant| {
                    let mut a: Vec<u64> = r.stackable_items.iter().filter(|(id, _)| is_metal(id)).map(|(_, q)| *q).collect();
                    a.sort_unstable();
                    a
                };
                assert_eq!(amounts(&now), amounts(&was), "L{level} chest {i}: metal amounts moved");
            }
            let total: usize = kinds.values().sum();
            let was_total: usize = was_kinds.values().sum();
            let qs = *kinds.get(&quicksilver).unwrap_or(&0) as f64 / total as f64;
            let was_qs = *was_kinds.get(&quicksilver).unwrap_or(&0) as f64 / was_total.max(1) as f64;
            println!("L{level}: Quicksilver {was_qs:.2} -> {qs:.2} of metal stacks; kinds {} -> {}; chests with a metal {was_with_metal} -> {with_metal}",
                was_kinds.len(), kinds.len());
            assert_eq!(with_metal, was_with_metal, "L{level}: how often a chest pays a metal moved");
            assert!(kinds.len() >= 4, "L{level}: only {} metal kinds", kinds.len());
            assert!(qs > 0.10 && qs <= 0.35, "L{level}: Quicksilver is {qs:.2} of metal stacks");
        }
        // Control: the level-46 composition alone was mostly Quicksilver.
        let was: Vec<RewardGrant> = (0..rolls).map(|i| before(46, i)).collect();
        let (qs, all) = was.iter().flat_map(|r| r.stackable_items.keys().filter(|id| is_metal(id))).fold((0, 0), |(q, a), id| {
            (q + usize::from(*id == quicksilver), a + 1)
        });
        assert!(qs * 100 >= all * 60, "control: Quicksilver was {qs}/{all} of level-46 metal stacks");
        // Where the Wooden table is rich, nothing changes.
        for level in [10u64, 20, 30, 40] {
            assert!(wooden_metal_stacks_near(&t, level) >= THIN_WOODEN_METALS, "L{level} is not thin");
            for i in 0..300 {
                assert_eq!(roll(level, i).stackable_items, before(level, i).stackable_items, "L{level} chest {i} moved");
            }
        }
        // Retail's richer tiers are untouched at every level.
        for tier in [2i64, 3] {
            for level in [46u64, 100] {
                for i in 0..200 {
                    let key = format!("381-metal:{i}:{level}");
                    let now = roll_loot(&t, tier, level, &key, &HashSet::new()).unwrap();
                    let was = compose_loot(&t.tiers[&(tier as u64)], level, stable_nonce(&key)).unwrap();
                    assert_eq!(now.stackable_items, was.stackable_items, "tier {tier} L{level} chest {i} moved");
                }
            }
        }
    }

    /// Wooden, Silver and Gold chests by chest level, ours against the retail
    /// openings within two levels (#368). `cargo test -p blades_lib
    /// print_treasury_progression -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_treasury_progression() {
        use crate::features::store_bundles::SOUL_GEMS;
        let t = committed();
        let names = [
            "Pet", "Les", "Mid", "Com", "Exc", "Gre", "Ele", "Gra", "Glo", "Tra",
        ];
        let summarise = |rewards: &[RewardGrant]| -> String {
            let mut gold: Vec<u64> = rewards
                .iter()
                .map(|r| r.currencies.get(&GOLD).copied().unwrap_or(0))
                .collect();
            gold.sort_unstable();
            let mut gems = [0usize; 10];
            let mut tiers: BTreeMap<u64, usize> = BTreeMap::new();
            let mut pieces = 0;
            for r in rewards {
                for (k, id) in SOUL_GEMS.iter().enumerate() {
                    gems[k] += usize::from(r.stackable_items.contains_key(id));
                }
                for i in &r.items {
                    if let Some(l) =
                        crate::features::legendary_gear::required_level(&i.item.item_template_id)
                    {
                        *tiers.entry(l).or_default() += 1;
                        pieces += 1;
                    }
                }
            }
            let n = rewards.len().max(1) as f64;
            let g: Vec<String> = (0..10)
                .filter(|&k| gems[k] > 0)
                .map(|k| format!("{} {:.0}%", names[k], gems[k] as f64 * 100.0 / n))
                .collect();
            let tier_txt: Vec<String> = tiers
                .iter()
                .map(|(l, c)| format!("T{l} {:.0}%", *c as f64 * 100.0 / pieces.max(1) as f64))
                .collect();
            format!(
                "n={:4} gold~{:5} | gems {} | gear {}",
                rewards.len(),
                gold.get(gold.len() / 2).copied().unwrap_or(0),
                g.join(" "),
                tier_txt.join(" ")
            )
        };
        for tier in 1..=3u64 {
            println!("== tier {tier}");
            for level in [10u64, 20, 30, 37, 43, 47, 49, 60, 80, 100] {
                let retail: Vec<RewardGrant> = t.tiers[&tier]
                    .iter()
                    .filter(|s| s.chest_level.abs_diff(level) <= 2)
                    .map(|s| s.reward.clone())
                    .collect();
                let ours: Vec<RewardGrant> = (0..2_000)
                    .map(|i| {
                        roll_loot(
                            &t,
                            tier as i64,
                            level,
                            &format!("p:{i}:{tier}:{level}"),
                            &HashSet::new(),
                        )
                        .unwrap()
                    })
                    .collect();
                println!("L{level:3} retail {}", summarise(&retail));
                println!("     ours   {}", summarise(&ours));
            }
        }
    }

    // ---- #392 audit: chest jewellery carries its GRADING ------------------------

    /// The economy audit (#392): 77 of the 100 retail chest jewels — every
    /// enchanted one — reached the corpus with a grade and no GRADING, so a Silver
    /// or Gold chest's ring usually had no ability bonuses (`extract_chest_loot.py`
    /// kept only ENCHANTING). The open now draws them from the grade, by the APK's
    /// `JewelryGradeData`.
    #[test]
    fn treasury_chest_jewellery_carries_the_grading_of_its_grade() {
        use crate::features::jewelry_grades::JEWELRY_GRADES;
        use crate::features::legendary_gear::{kind_of, rule_broken};
        let t = committed();
        let is_jewel = |id: &Uuid| matches!(kind_of(id), Some("ring" | "necklace"));

        // Control: the committed corpus really pays graded jewellery without
        // GRADING, so the assertions below can fail.
        let bare = t
            .tiers
            .values()
            .flatten()
            .flat_map(|s| &s.reward.items)
            .filter(|i| is_jewel(&i.item.item_template_id))
            .filter(|i| i.item.grade.is_some() && i.item.properties.grading.is_empty())
            .count();
        assert!(bare > 50, "control: {bare} bare graded jewels in the corpus");

        let mut jewels = 0;
        let mut distinct = HashSet::new();
        for tier in 1..=3i64 {
            for level in [1u64, 10, 20, 30, 40, 45, 49, 60, 80, 100] {
                for i in 0..200 {
                    let key = format!("audit:{i}:{tier}:{level}");
                    let reward = roll_loot(&t, tier, level, &key, &HashSet::new()).unwrap();
                    for item in reward.items.iter().map(|r| &r.item) {
                        if !is_jewel(&item.item_template_id) {
                            continue;
                        }
                        jewels += 1;
                        let grade = item.grade.expect("chest jewellery is graded");
                        let mut tiers: Vec<u64> = item.properties.grading.iter().map(|p| p.tier).collect();
                        tiers.sort_unstable_by(|a, b| b.cmp(a));
                        let want = JEWELRY_GRADES.iter().find(|(g, _, _)| *g == grade).unwrap().2;
                        assert_eq!(tiers, want, "grade {grade} GRADING tiers");
                        assert_eq!(rule_broken(item), None, "{item:?}");
                        distinct.insert(serde_json::to_string(&item.properties.grading).unwrap());
                        // Deterministic per chest: a retried open pays the same ring.
                        let again = roll_loot(&t, tier, level, &key, &HashSet::new()).unwrap();
                        assert!(again.items.iter().any(|r| r.item == *item));
                    }
                }
            }
        }
        assert!(jewels > 500, "only {jewels} jewels sampled");
        assert!(
            distinct.len() * 2 > jewels,
            "{} distinct GRADING sets in {jewels} jewels",
            distinct.len()
        );
    }

    /// #393 (HauDrauf, 2026-10-15, "überall, keine Unterschiede"): a Wooden,
    /// Silver or Gold chest's ring or necklace has one, two or three GRADING
    /// properties 8 / 17 / 75% of the time, like every other source. Before, the
    /// grade was the recorded one: 25 / 20 / 57% over the corpus.
    #[test]
    fn treasury_chest_jewellery_follows_the_property_count_split() {
        use crate::features::jewelry_grades::JEWELRY_PROPERTY_COUNT_PERCENT;
        use crate::features::legendary_gear::kind_of;
        let t = committed();
        let mut count = [0u64; 4];
        let mut by_tier = [0u64; 4];
        let mut i = 0u64;
        while count.iter().sum::<u64>() < 3000 && i < 200_000 {
            let tier = 1 + (i % 3) as i64;
            let level = [5u64, 20, 35, 45, 60, 80, 100][(i / 3 % 7) as usize];
            let reward = roll_loot(&t, tier, level, &format!("split:{i}"), &HashSet::new()).unwrap();
            for item in reward.items.iter().map(|r| &r.item) {
                if matches!(kind_of(&item.item_template_id), Some("ring" | "necklace")) {
                    let g = &item.properties.grading;
                    assert_eq!(item.grade, Some(g.iter().map(|p| p.tier).sum()), "grade = sum of tiers");
                    count[g.len()] += 1;
                    by_tier[tier as usize] += 1;
                }
            }
            i += 1;
        }
        let n = count.iter().sum::<u64>();
        assert!(n >= 2000, "only {n} treasury chest jewels in {i} chests");
        eprintln!("#393 treasury chests: {count:?} of {n} (by tier {by_tier:?})");
        assert_eq!(count[0], 0, "a chest jewel without GRADING");
        for (props, pct) in JEWELRY_PROPERTY_COUNT_PERCENT {
            let got = count[props] as f64 * 100.0 / n as f64;
            assert!((got - f64::from(pct)).abs() <= 3.0, "{props} properties: {got:.1}% (want {pct}%)");
        }
    }

    /// The economy audit (#392): treasury chest gear repeated — a level-1 Wooden
    /// chest paid the same Hide Gauntlets every time, 1,088 Wooden items over ten
    /// levels were 26 instances. Now generated around the retail piece; every
    /// piece obeys the APK rules and keeps the retail piece's tier.
    #[test]
    fn treasury_chest_gear_is_generated_not_replayed() {
        use crate::features::legendary_gear::{required_level, rule_broken};
        let t = committed();
        for tier in 1..=3i64 {
            for level in [1u64, 10, 20, 30, 40, 45, 49, 60, 80, 100] {
                let mut seen: HashMap<String, usize> = HashMap::new();
                let mut items = 0usize;
                let mut tiers_paid = HashSet::new();
                for i in 0..300 {
                    let key = format!("gear:{i}:{tier}:{level}");
                    let reward = roll_loot(&t, tier, level, &key, &HashSet::new()).unwrap();
                    for item in reward.items.iter().map(|r| &r.item) {
                        if crate::features::store_bundles::is_artifact(&item.item_template_id) {
                            continue;
                        }
                        items += 1;
                        assert_eq!(rule_broken(item), None, "tier {tier} L{level}: {item:?}");
                        tiers_paid.insert(required_level(&item.item_template_id));
                        *seen.entry(serde_json::to_string(item).unwrap()).or_insert(0) += 1;
                    }
                }
                if items < 20 {
                    continue;
                }
                // The economy audit's variety limit: no piece above a quarter.
                let top = seen.values().copied().max().unwrap_or(0);
                assert!(
                    top * 4 <= items,
                    "tier {tier} L{level}: one piece is {top} of {items} items"
                );
                // The generator keeps the retail piece's material tier: only tiers
                // the composed retail pieces could have are paid.
                let (pool, _) = t.pool_for(tier as u64).unwrap();
                let retail: HashSet<Option<u64>> = neighbourhood(pool, level)
                    .iter()
                    .flat_map(|s| &s.reward.items)
                    .map(|i| required_level(&i.item.item_template_id))
                    .collect();
                assert!(tiers_paid.is_subset(&retail), "tier {tier} L{level}: {tiers_paid:?} vs {retail:?}");
            }
        }
    }
}
