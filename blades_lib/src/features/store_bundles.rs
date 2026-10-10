//! Randomised global-shop bundles — the products retail re-rolled per purchase.
//!
//! THE BUG (#170, "store chests still give the same loot"). A purchase takes its
//! reward from `global_shop_grants`, a capture-derived recording of ONE retail
//! purchase per product, cloned on every buy. For a fixed product that is exactly
//! right. For a randomised bundle it means every player, every time, receives the
//! same gold and the same two items.
//!
//! Retail rolled. Of the 85 products bought more than once in the captures, six
//! varied — and four of those varied on EVERY purchase: 4,697 buys of the biggest
//! bundle produced 4,697 distinct rewards. Those same four are ABSENT from the
//! APK offer catalogue, which is why there was no authored content to grant and
//! the recording was all there was.
//!
//! THE PAYOUT SCALES WITH THE BUYER'S LEVEL, and that is the part worth getting
//! right. On the biggest bundle the median gold runs 9,735 at levels 1-5 and
//! 60,315 at 54-89, monotonically, r = 0.83; the two mid-size bundles score 0.93
//! and 0.90. Drawing pooled would hand a level-3 player a level-60 payout. This
//! is the same trap the enemy-loot corpus was built to avoid, and it was caught
//! here only because the bands came out non-monotonic on the first attempt — an
//! attribution bug in the miner, not a property of the data.
//!
//! The two remaining varying products are `needs_roll` arcane jewelry whose
//! TEMPLATE is fixed and whose enchant roll varies. They are handled by the
//! authored-contents path, not here.

use std::collections::HashSet;

use serde::Deserialize;
use uuid::Uuid;

use crate::economy::{RewardGrant, RewardItem};
use crate::user_data::Item;

static STORE_BUNDLE_LOOT_RAW: &str = include_str!("../store_bundle_loot.json");

#[derive(Deserialize)]
struct Corpus {
    products: Vec<Product>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Product {
    product_id: Uuid,
    observations: u64,
    by_level: Vec<Band>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Band {
    min_buyer_level: u64,
    max_buyer_level: u64,
    results: Vec<Drawn>,
}

#[derive(Deserialize)]
struct Drawn {
    reward: ObservedReward,
    n: u64,
}

/// One observed payout. `Item` deserializes straight from retail's own shape;
/// the corpus strips `items[].id` so a fresh one is minted per purchase.
#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct ObservedReward {
    #[serde(default)]
    currencies: std::collections::HashMap<Uuid, u64>,
    #[serde(default)]
    stackable_items: std::collections::HashMap<Uuid, u64>,
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    town_xp: u64,
}

fn corpus() -> &'static Corpus {
    static TABLE: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(STORE_BUNDLE_LOOT_RAW)
            .unwrap_or_else(|_| Corpus { products: Vec::new() })
    })
}

/// A uuid4-shaped instance id derived from the draw. `blades_lib` does not carry
/// uuid's `v4` feature, and deriving it is better anyway: the same purchase
/// described twice keeps the same id, while a different purchase gets a
/// different one because the nonce is part of the seed.
fn instance_uuid(seed: u64, ordinal: usize) -> Uuid {
    let hi = mix(seed ^ (ordinal as u64).rotate_left(17));
    let lo = mix(hi ^ 0x5DEE_CE66_D1B2_4E35);
    let mut b = (((hi as u128) << 64) | lo as u128).to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

pub(crate) fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The three store chests at their normal gem price, each paired with the 1-gem
/// shutdown-promotion window that retail sold the SAME chest under (#215/#216).
///
/// The catalogue says so itself: every promo entry's bare `purchaseTrackingId`
/// is the base product (`global_shop_overrides.json`), and the base prices are
/// Rare 250 / Epic 750 / Legendary 2500 gems. Retail's purchases were captured
/// only under the promo ids — 4,697 Legendary, 405 Epic, 355 Rare — so the base
/// ids had no corpus of their own. Legendary fell through to an invented grant
/// (one tier-5 treasury chest), which froze the client's store opening sequence
/// and paid every buyer the same single tier-5 bundle; Rare and Epic had no
/// grant at all and were refused. Retail answered a chest purchase with the
/// rolled contents in `reward` and no treasury chest, which is what the promo
/// corpus holds.
const STORE_CHEST_ALIASES: [(Uuid, Uuid); 3] = [
    // Legendary (2500 gems) -> its promo window.
    (
        Uuid::from_u128(0x1275d959_bbe5_460d_8f6a_1c31106a8eb2),
        Uuid::from_u128(0x11102495_fde7_4e77_b6c4_d13b9303f1f5),
    ),
    // Epic (750 gems).
    (
        Uuid::from_u128(0x7bf00a9c_6a08_4b55_a60d_53915baa38a3),
        Uuid::from_u128(0x9e4dc391_422e_4b25_ba90_faab39e0769f),
    ),
    // Rare (250 gems).
    (
        Uuid::from_u128(0x0e224ca0_1506_490f_884a_8871ffe6399b),
        Uuid::from_u128(0x11b322d2_1b5e_4e90_bb41_a8d90ca9548b),
    ),
];

const LEGENDARY_CHEST_PRODUCT_ID: Uuid =
    Uuid::from_u128(0x1275d959_bbe5_460d_8f6a_1c31106a8eb2);

/// The Epic Chest (750 gems). The APK catalogue sells it as `chests: [{_rarity: 4}]`,
/// i.e. `ChestRarity.Tier4`, the Elder Chest — so its corpus is the Elder corpus.
const EPIC_CHEST_PRODUCT_ID: Uuid = Uuid::from_u128(0x7bf00a9c_6a08_4b55_a60d_53915baa38a3);

/// Every item template carrying the APK's `Item.Tag.Artifact`
/// (`8511534e-ef06-4e6a-910a-745a2cc72a47`): the 24 unique artifacts.
pub const ARTIFACT_TEMPLATES: [Uuid; 24] = [
    Uuid::from_u128(0x05862206_b6db_4cd4_9f7a_ed2e03fc90af), // Ring of Namira
    Uuid::from_u128(0x12c4920e_a9e0_4498_b134_3ce66fb9558f), // Savior's Hide
    Uuid::from_u128(0x21ed2758_5cf5_4b7d_9047_59bbdbd61820), // Onyx Cleaver
    Uuid::from_u128(0x21f8b677_c1a9_4d5f_87ef_294fc6ba4ba5), // Ebony Blade
    Uuid::from_u128(0x23607f09_a103_4ed3_a0de_33e0498f8018), // Warlock's Ring
    Uuid::from_u128(0x4b59cdbe_f857_4673_9e40_741ca06aa5d0), // Spellbane
    Uuid::from_u128(0x4e841f5e_c5d7_47cd_899a_116846dbbfc0), // Bloodthirst
    Uuid::from_u128(0x515450b1_ab21_4cbf_a8ed_c4e0e51df4c0), // Dragon's Blight
    Uuid::from_u128(0x59c720fb_d761_4ad1_b14c_2aa8c1a4d554), // Mace of Molag Bal
    Uuid::from_u128(0x6ab76008_dd08_461f_bdcc_6373799ea489), // Rueful Axe
    Uuid::from_u128(0x7d2dfa88_e7ca_4fd6_a173_862cfb077c04), // Stendarr's Hammer
    Uuid::from_u128(0x86a4ad9d_e2e8_49cd_9642_b553475b6580), // Pandemonium
    Uuid::from_u128(0x899b63a7_6eb7_4689_a450_8c6b507763c6), // Guardian's Claymore
    Uuid::from_u128(0x8a69bdb2_d179_4f2d_984a_322f9397aedf), // Rimelink
    Uuid::from_u128(0x9107fb63_f88b_45fa_bca9_7fe2f9bceb2c), // Chillrend
    Uuid::from_u128(0x975509a9_196f_41c9_8df4_6f0e2d1f02ce), // Lord's Mail
    Uuid::from_u128(0x9a22b59a_cdbc_4987_bf4a_0cc812428aeb), // Spellbreaker
    Uuid::from_u128(0xa0d7dd5e_692e_4fbb_bbf2_19c6a874f3f5), // Mehrunes' Razor
    Uuid::from_u128(0xaf7759d9_f38e_4384_bf36_1de0c3d150a7), // Volendrung
    Uuid::from_u128(0xbe1484e6_c156_47db_bdab_3cf5b75096a0), // Chrysamere
    Uuid::from_u128(0xca1e2d0f_b902_4c82_b8d6_e82b83dcc9e0), // Dawnbreaker
    Uuid::from_u128(0xda0cf5aa_6d37_48da_8b18_3f4e914145d3), // Onyx Saber
    Uuid::from_u128(0xdef810af_e9f5_4e23_9247_1edf391d82e1), // Ebony Mail
    Uuid::from_u128(0xe86a6f9a_4e40_40d6_a9dd_1036b4190caa), // Fork of Horripilation
];

pub fn is_artifact(template: &Uuid) -> bool {
    ARTIFACT_TEMPLATES.contains(template)
}

/// The number of item slots every reward in the band has. Retail's chest paid
/// that many regular pieces; a reward with more carries the artifact slot.
fn core_slots(band: &Band) -> usize {
    band.results.iter().map(|r| r.reward.items.len()).min().unwrap_or(0)
}

/// What retail paid in the artifact slot when it was NOT an artifact: one per
/// observation, with the buyer level band it was paid in. Retail's
/// `LootEnhancementData` sets `artifactFallbackRarityLevel: 4` (Legendary) and
/// `artifactFallbackLootRarityLevelId` = "Legendary - High (Chests)"; these are
/// the pieces that rule produced. It is one global setting, so every chest
/// product's observations are pooled.
fn artifact_fallbacks() -> &'static [(u64, u64, Item)] {
    static TABLE: std::sync::OnceLock<Vec<(u64, u64, Item)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = Vec::new();
        for band in corpus().products.iter().flat_map(|p| &p.by_level) {
            let core = core_slots(band);
            for r in &band.results {
                for item in r.reward.items.iter().skip(core) {
                    if !is_artifact(&item.item_template_id) {
                        out.push((band.min_buyer_level, band.max_buyer_level, item.clone()));
                    }
                }
            }
        }
        out
    })
}

#[cfg(test)]
fn artifact_fallback_templates() -> HashSet<Uuid> {
    artifact_fallbacks().iter().map(|(_, _, i)| i.item_template_id).collect()
}

/// The mined product that backs `product_id`: its own, else its promo twin's.
fn corpus_product(product_id: &Uuid) -> Option<&'static Product> {
    let mined_as = STORE_CHEST_ALIASES
        .iter()
        .find(|(base, _)| base == product_id)
        .map_or(product_id, |(_, promo)| promo);
    corpus().products.iter().find(|p| &p.product_id == mined_as)
}

/// Whether this product is one the server must roll rather than replay.
pub fn is_randomised_bundle(product_id: &Uuid) -> bool {
    corpus_product(product_id).is_some()
}

/// How many retail purchases back this product, for tests and diagnostics.
pub fn bundle_observations(product_id: &Uuid) -> u64 {
    corpus_product(product_id).map(|p| p.observations).unwrap_or(0)
}

/// Roll one purchase of a randomised bundle for a buyer at `buyer_level`.
///
/// `nonce` must differ per purchase — buying the same bundle twice has to be able
/// to give different things, which is the entire complaint. The caller passes
/// something that moves, such as the player's purchase count.
///
/// `None` when the product is not a randomised bundle, which leaves every other
/// product on the existing recorded-grant path.
pub fn roll_bundle(product_id: &Uuid, buyer_level: u64, nonce: u64) -> Option<RewardGrant> {
    roll_bundle_for(product_id, buyer_level, nonce, &HashSet::new())
}

/// How far `level` sits outside `min..=max` (0 inside).
fn level_distance(level: u64, min: u64, max: u64) -> u64 {
    if level < min {
        min - level
    } else {
        level.saturating_sub(max)
    }
}

/// One whole recorded reward from the band, weighted by its count.
fn draw(band: &Band, seed: u64) -> Option<&Drawn> {
    let total: u64 = band.results.iter().map(|r| r.n).sum();
    if total == 0 {
        return None;
    }
    let mut pick = seed % total;
    band.results
        .iter()
        .find(|r| {
            if pick < r.n {
                true
            } else {
                pick -= r.n;
                false
            }
        })
        .or(band.results.last())
}

/// A retail artifact-slot fallback for a buyer at `level`: one paid in a band
/// containing the level, else in the nearest band that paid any.
fn artifact_fallback(level: u64, seed: u64) -> Option<&'static Item> {
    let all = artifact_fallbacks();
    let nearest = all.iter().map(|(lo, hi, _)| level_distance(level, *lo, *hi)).min()?;
    let candidates: Vec<&Item> = all
        .iter()
        .filter(|(lo, hi, _)| level_distance(level, *lo, *hi) == nearest)
        .map(|(_, _, item)| item)
        .collect();
    Some(candidates[(seed % candidates.len() as u64) as usize])
}

/// The corpus the Legendary chest (and its 1-gem promo window) is mined under.
const LEGENDARY_CHEST_CORPUS_ID: Uuid = Uuid::from_u128(0x11102495_fde7_4e77_b6c4_d13b9303f1f5);

static ITEM_REQUIRED_LEVELS_RAW: &str = include_str!("../item_required_levels.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequiredLevels {
    required_level: std::collections::HashMap<Uuid, u64>,
}

fn required_levels() -> &'static std::collections::HashMap<Uuid, u64> {
    static TABLE: std::sync::OnceLock<std::collections::HashMap<Uuid, u64>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<RequiredLevels>(ITEM_REQUIRED_LEVELS_RAW)
            .map(|t| t.required_level)
            .unwrap_or_default()
    })
}

/// The APK's `ItemTemplate.requiredLevel` for a template the store corpus pays
/// (`script/extract_item_required_levels.py`). It is the item's material tier as
/// the unlock level the gear UI shows: Iron/Steel 1, Silver 8, Orcish 13,
/// Dwarven 18, Elven 23, Glass 28, Ebony/Stalhrim 33, Daedric 39, Dragon 45.
/// A generated piece (#368) can be any template of the tier's loot pool, so the
/// pool answers for the few templates the corpus never paid.
fn required_level(template: &Uuid) -> Option<u64> {
    required_levels()
        .get(template)
        .copied()
        .or_else(|| super::legendary_gear::required_level(template))
}

/// How many levels short of the next tier's unlock a buyer can already see it
/// in the top slot, and how often (one roll in [`LOOKAHEAD_ONE_IN`]).
const LOOKAHEAD_LEVELS: u64 = 2;
const LOOKAHEAD_ONE_IN: u64 = 5;
/// How many levels past an unlock the lower slot catches up to the new tier.
const CATCH_UP_LEVELS: u64 = 3;
/// How often slot 0 pays the other of its two tiers (one roll in this many):
/// retail's lower slot matched the rule in 167 of 193 purchases at exact
/// levels and 150 of 161 in the 54-89 band.
const SLOT0_OTHER_ONE_IN: u64 = 7;

/// The material tier (as its unlock level) retail paid in regular `slot` of a
/// Legendary chest bought at `level`, out of the ascending tier ladder `ladder`.
///
/// THE BUG (#339/#354/#366). Slots were drawn from the buyer's whole level band,
/// and the bands are wide: 18-23, 29-35, 54-89. A level-23 buyer drew from
/// chests retail paid mostly at 19-21, before Elven unlocks, so four in five
/// pieces were Dwarven; a level-33 buyer got Ebony in about one slot in five, and
/// over a third of the lower slot was Elven, two tiers down.
///
/// THE RULE, measured on 306 retail Legendary purchases at their exact buyer
/// level (2026-06-07 capture snapshot) and consistent with every band of the
/// 4,697-purchase corpus: the two regular slots are not interchangeable.
/// Slot 1 pays the tier unlocked at the buyer's level (Elven at 23-27, Glass at
/// 28-32, Ebony at 33-38). Slot 0 pays the tier below it until the buyer is
/// [`CATCH_UP_LEVELS`] past the unlock, then the same tier: levels 17, 21 and 26
/// paid the current tier in 58 of 70 lower slots, levels 19, 20, 25 and 28-30 the
/// tier below in 78 of 90; the rest paid the other of the two, which slot 0
/// does one roll in [`SLOT0_OTHER_ONE_IN`]. At the top of the ladder slot 0 stays a tier down:
/// the 54-89 band paid Daedric in 150 of 161 lower slots and Dragon in all 161
/// upper ones. Within [`LOOKAHEAD_LEVELS`] of the next unlock slot 1 sometimes
/// already pays it: 18 of 93 retail purchases within two levels of an unlock (8
/// of 30 at level 17, 10 of 30 at 21, none of 33 at 6, 7 and 26); the one outlier
/// is level 10, three short of Orcish, at 26 of 60.
///
/// THE SLOT LADDERS (#368). The two slots do not climb the same ladder at the
/// top. Retail paid Daedric in the upper slot once in 4,697 purchases and Ebony
/// in the lower slot five times; the 36-50 band pairs Glass+Ebony (18) and then
/// Daedric+Dragon (38), never Ebony+Daedric, and 54-89 is Daedric+Dragon. So
/// slot 1 pays the lowest tier of ITS ladder at or above the buyer's unlock
/// (Daedric -> Dragon), and slot 0 pays the highest tier of its own ladder below
/// that, catching up only to a tier slot 0 actually pays. Below Ebony both
/// ladders are the whole ladder and the rule above is unchanged.
fn legendary_slot_tier(
    ladder: &[u64],
    slot_ladders: &[Vec<u64>; 2],
    slot: usize,
    level: u64,
    seed: u64,
) -> Option<u64> {
    let top_idx = ladder.iter().rposition(|&r| r <= level.max(1)).unwrap_or(0);
    let top = *ladder.get(top_idx)?;
    // The lowest tier slot 1 pays at or above `tier`, else its highest.
    let upper = |tier: u64| {
        let l = &slot_ladders[1];
        l.iter().copied().find(|&t| t >= tier).or(l.last().copied())
    };
    let t1 = upper(top)?;
    match slot {
        0 => {
            let lower = &slot_ladders[0];
            let prev = lower.iter().copied().filter(|&t| t < t1).max().unwrap_or(t1);
            let ceiling = Some(&t1) == ladder.last();
            let caught_up = !ceiling && level >= t1 + CATCH_UP_LEVELS && lower.contains(&t1);
            let (usual, other) = if caught_up { (t1, prev) } else { (prev, t1) };
            if mix(seed ^ 0x7F4A_7C15_9E37_79B9) % SLOT0_OTHER_ONE_IN == 0 {
                Some(other)
            } else {
                Some(usual)
            }
        }
        1 => match ladder.get(top_idx + 1).and_then(|&next| upper(next).map(|a| (next, a))) {
            Some((next, ahead))
                if ahead > t1
                    && next.saturating_sub(level) <= LOOKAHEAD_LEVELS
                    && mix(seed ^ 0x2545_F491_4F6C_DD1D) % LOOKAHEAD_ONE_IN == 0 =>
            {
                Some(ahead)
            }
            _ => Some(t1),
        },
        _ => None,
    }
}

/// The tiers retail paid in regular `slot` often enough to be a tier of that
/// slot: at least [`MIN_TIER_PARTS`] distinct parts across the corpus (#368).
fn slot_ladder(product: &Product, slot: usize) -> Vec<u64> {
    let mut parts: Vec<(u64, &Item)> = Vec::new();
    for band in &product.by_level {
        if slot >= core_slots(band) {
            continue;
        }
        for item in band.results.iter().map(|r| &r.reward.items[slot]) {
            if let Some(tier) = required_level(&item.item_template_id)
                && !parts.contains(&(tier, item))
            {
                parts.push((tier, item));
            }
        }
    }
    let mut ladder: Vec<u64> = parts.iter().map(|(t, _)| *t).collect();
    ladder.sort_unstable();
    ladder.dedup();
    ladder.retain(|&t| parts.iter().filter(|(x, _)| *x == t).count() >= MIN_TIER_PARTS);
    ladder
}

/// Every tier unlock level the product pays in its regular slots, ascending.
fn tier_ladder(product: &Product) -> Vec<u64> {
    let mut ladder: Vec<u64> = product
        .by_level
        .iter()
        .flat_map(|b| {
            let core = core_slots(b);
            b.results.iter().flat_map(move |r| r.reward.items.iter().take(core))
        })
        .filter_map(|i| required_level(&i.item_template_id))
        .collect();
    ladder.sort_unstable();
    ladder.dedup();
    ladder
}

/// The fewest retail parts a tiered slot draws from, as for tier 1-3 chests (#335).
const MIN_TIER_PARTS: usize = 12;

/// A retail part for a Legendary chest's regular `slot`, at the tier retail paid
/// there at `level`: what retail paid in that same slot at that tier, from the
/// bands nearest the buyer, widening band by band until there are at least
/// [`MIN_TIER_PARTS`] DISTINCT candidates (#368: a retail part is counted once,
/// however many recorded chests carried it). `None` leaves the slot on the band
/// draw.
fn legendary_slot_part(
    product: &'static Product,
    slot: usize,
    level: u64,
    seed: u64,
) -> Option<&'static Item> {
    static LADDER: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    static SLOT_LADDERS: std::sync::OnceLock<[Vec<u64>; 2]> = std::sync::OnceLock::new();
    let ladder = LADDER.get_or_init(|| tier_ladder(product));
    let slot_ladders =
        SLOT_LADDERS.get_or_init(|| [slot_ladder(product, 0), slot_ladder(product, 1)]);
    let tier = legendary_slot_tier(ladder, slot_ladders, slot, level, seed)?;

    let mut bands: Vec<&Band> = product.by_level.iter().collect();
    bands.sort_by_key(|b| level_distance(level, b.min_buyer_level, b.max_buyer_level));
    let mut candidates: Vec<&Item> = Vec::new();
    for band in bands {
        if candidates.len() >= MIN_TIER_PARTS {
            break;
        }
        if slot >= core_slots(band) {
            continue;
        }
        for item in band.results.iter().map(|r| &r.reward.items[slot]) {
            if required_level(&item.item_template_id) == Some(tier)
                && !candidates.contains(&item)
            {
                candidates.push(item);
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    Some(candidates[(mix(seed) % candidates.len() as u64) as usize])
}

/// [`roll_bundle`] for a buyer who already holds the item templates in `owned`.
///
/// THE POOL (#310). A band is a bank of 60-250 whole recorded rewards, and
/// replaying one whole reward per purchase meant a level-15 buyer could only
/// ever see 63 different Legendary chests: Sephoris bought 165 and got 58
/// distinct, each about three times. Retail rolled every slot on its own — 4,697
/// purchases, 4,697 distinct rewards. So the roll is composed from retail parts
/// of the SAME band: gold, stackables, town XP and the optional artifact slot
/// from one recorded reward, and each regular item slot from its own
/// independently drawn reward, position for position (slot 0 and slot 1 roll
/// different item families, so they are never mixed). The payout stays in the
/// band's observed range.
///
/// THE GEAR (#368). A Legendary chest's regular gear slot is then generated
/// around that retail piece, as retail generated it: a loot template of the same
/// tier and enchantments rolled from the APK tables, keeping the piece's
/// tempering, enchant count and tier (see [`super::legendary_gear`]).
///
/// THE ARTIFACT RULE (#310). An artifact is unique. An artifact the buyer
/// already holds — or one this same chest already paid — is replaced by a
/// piece retail itself paid in the artifact slot, as `LootEnhancementData`'s
/// "Legendary - High (Chests)" fallback dictates.
pub fn roll_bundle_for(
    product_id: &Uuid,
    buyer_level: u64,
    nonce: u64,
    owned: &HashSet<Uuid>,
) -> Option<RewardGrant> {
    let product = corpus_product(product_id)?;

    // The band containing the buyer's level, else the nearest one — a level above
    // or below everything retail was observed at clamps rather than falling back
    // to a pooled draw, which is the thing that would misprice the payout.
    let band = product
        .by_level
        .iter()
        .find(|b| buyer_level >= b.min_buyer_level && buyer_level <= b.max_buyer_level)
        .or_else(|| {
            product
                .by_level
                .iter()
                .min_by_key(|b| level_distance(buyer_level, b.min_buyer_level, b.max_buyer_level))
        })?;

    let seed = mix(product_id.as_u128() as u64 ^ mix(nonce) ^ buyer_level.rotate_left(13));
    let base = draw(band, seed)?;
    let core = core_slots(band);

    let mut items: Vec<Item> = Vec::with_capacity(base.reward.items.len());
    for slot in 0..core {
        let slot_seed = mix(seed ^ 0xA076_1D64_78BD_642F_u64.wrapping_mul(slot as u64 + 1));
        // Legendary gear is picked at the buyer's own tier (#339), not anywhere
        // in the band; the other chests keep the band draw.
        let tiered = if product.product_id == LEGENDARY_CHEST_CORPUS_ID {
            legendary_slot_part(product, slot, buyer_level, slot_seed)
        } else {
            None
        };
        let shell = match tiered {
            Some(item) => item.clone(),
            None => draw(band, slot_seed)?.reward.items[slot].clone(),
        };
        // Legendary gear is then GENERATED around that retail piece, as retail
        // generated it (#368); the other chests keep the retail piece.
        let piece = if product.product_id == LEGENDARY_CHEST_CORPUS_ID {
            super::legendary_gear::generate(&shell, mix(slot_seed ^ 0x4F1B_BCDC_BFA5_3E0B))
                .unwrap_or(shell)
        } else {
            shell
        };
        items.push(piece);
    }
    items.extend(base.reward.items.iter().skip(core).cloned());

    let mut paid_artifacts = HashSet::new();
    for (ordinal, item) in items.iter_mut().enumerate() {
        let template = item.item_template_id;
        if !is_artifact(&template) {
            continue;
        }
        if owned.contains(&template) || !paid_artifacts.insert(template) {
            let fallback_seed = mix(seed ^ 0xE703_7ED1_A0B4_28DB ^ ordinal as u64);
            match artifact_fallback(buyer_level, fallback_seed) {
                Some(fallback) => *item = fallback.clone(),
                // Unreachable with the shipped corpus (asserted in tests); never
                // pay a duplicate artifact even then.
                None => item.item_template_id = Uuid::nil(),
            }
        }
    }
    items.retain(|i| !i.item_template_id.is_nil());

    let mut grant = RewardGrant {
        currencies: base.reward.currencies.clone(),
        stackable_items: base.reward.stackable_items.clone(),
        town_xp: base.reward.town_xp,
        ..RewardGrant::default()
    };
    for (ordinal, item) in items.into_iter().enumerate() {
        grant.items.push(RewardItem {
            // A fresh instance per purchase. The frozen ids in the capture-derived
            // grants are what made buying twice overwrite the first item.
            id: instance_uuid(seed, ordinal),
            item,
        });
    }
    Some(grant)
}

/// Roll a treasury chest through the retail store-chest corpus when the treasury
/// table is too thin to model the tier. Tier 5 (Legendary) has one captured
/// treasury open but 4,697 retail Legendary chest purchases with the same reward
/// shape; tier 4 (Elder) has eleven opens, at levels 54-94 only, against 405 Epic
/// Chest purchases — the product the APK catalogue sells as `_rarity: 4`.
pub fn roll_treasury_chest(
    tier: u64,
    chest_level: u64,
    nonce: u64,
    owned: &HashSet<Uuid>,
) -> Option<RewardGrant> {
    let product = match tier {
        5 => LEGENDARY_CHEST_PRODUCT_ID,
        4 => EPIC_CHEST_PRODUCT_ID,
        _ => return None,
    };
    roll_bundle_for(&product, chest_level, nonce, owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::legendary_gear;

    const BIG: &str = "11102495-fde7-4e77-b6c4-d13b9303f1f5";
    const GOLD: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

    fn big() -> Uuid {
        BIG.parse().unwrap()
    }

    fn gold_of(g: &RewardGrant) -> u64 {
        *g.currencies.get(&GOLD.parse::<Uuid>().unwrap()).unwrap_or(&0)
    }

    /// The corpus must be compiled in and hold what was mined. Everything below
    /// is vacuous without it.
    #[test]
    fn the_bundle_corpus_loads() {
        // Parsed explicitly rather than through the OnceLock, so a deserialization
        // failure names itself instead of silently degrading to an empty corpus —
        // which is how a stripped property-definition id went unnoticed once.
        if let Err(e) = serde_json::from_str::<Corpus>(STORE_BUNDLE_LOOT_RAW) {
            panic!("the corpus failed to parse: {e}");
        }
        assert_eq!(corpus().products.len(), 4, "four randomised bundles were mined");
        assert_eq!(bundle_observations(&big()), 4697, "the big bundle corpus");
        assert!(is_randomised_bundle(&big()));
        assert!(
            !is_randomised_bundle(&"6ec8f67f-2cef-41aa-a7fc-f46237ae809c".parse().unwrap()),
            "a FIXED product must stay on the recorded-grant path"
        );
    }

    /// #215/#216: the chests at their normal gem price roll from the promo
    /// window retail sold them under, so they vary and never grant a treasury
    /// chest (the invented grant that froze the store and paid one fixed bundle).
    #[test]
    fn the_store_chests_at_full_price_roll_from_their_promo_corpus() {
        for (base, promo, observations) in [
            ("1275d959-bbe5-460d-8f6a-1c31106a8eb2", BIG, 4697),
            ("7bf00a9c-6a08-4b55-a60d-53915baa38a3", "9e4dc391-422e-4b25-ba90-faab39e0769f", 405),
            ("0e224ca0-1506-490f-884a-8871ffe6399b", "11b322d2-1b5e-4e90-bb41-a8d90ca9548b", 355),
        ] {
            let base: Uuid = base.parse().unwrap();
            assert!(is_randomised_bundle(&base), "{base} must roll");
            assert_eq!(bundle_observations(&base), observations, "{base} corpus");
            assert_eq!(bundle_observations(&promo.parse().unwrap()), observations);
            let mut seen = std::collections::HashSet::new();
            for nonce in 0..40u64 {
                let g = roll_bundle(&base, 48, nonce).expect("a store chest must roll");
                assert!(g.chests.is_empty(), "a bought chest is opened, not stored");
                assert!(
                    !g.currencies.is_empty() || !g.stackable_items.is_empty() || !g.items.is_empty(),
                    "{base} rolled an empty chest"
                );
                seen.insert((gold_of(&g), g.stackable_items.values().sum::<u64>(), g.items.len()));
            }
            assert!(seen.len() > 5, "{base}: 40 purchases gave {} distinct rewards", seen.len());
        }
    }

    /// THE BUG: every purchase returned the same thing.
    #[test]
    fn buying_twice_can_give_different_things() {
        let mut seen = std::collections::HashSet::new();
        for nonce in 0..80u64 {
            let g = roll_bundle(&big(), 30, nonce).expect("the big bundle must roll");
            seen.insert(gold_of(&g));
        }
        assert!(
            seen.len() > 10,
            "80 purchases produced only {} distinct gold amounts — still replaying",
            seen.len()
        );
    }

    /// THE LEVEL CONTROL. Median gold runs 9,735 at levels 1-5 and 60,315 at
    /// 54-89. A pooled draw would pay a level-3 player a level-60 payout, which
    /// is indistinguishable from working code without this test.
    #[test]
    fn the_payout_scales_with_the_buyers_level() {
        let median_at = |level: u64| -> u64 {
            let mut v: Vec<u64> = (0..120u64)
                .map(|n| gold_of(&roll_bundle(&big(), level, n).unwrap()))
                .collect();
            v.sort_unstable();
            v[v.len() / 2]
        };
        let low = median_at(3);
        let high = median_at(70);
        assert!(low > 0, "a low-level buyer got no gold at all");
        assert!(
            low < 20_000,
            "a level-3 buyer received a median {low} gold; retail paid about 9,700 there, \
             so the level bands are being ignored"
        );
        assert!(high > 45_000, "a level-70 buyer received only {high}; retail paid about 60,000");
        assert!(high > low * 3, "the payout barely moved with level: {low} -> {high}");
    }

    /// Bands must be monotonic in level. A non-monotonic band was what exposed
    /// the attribution bug in the miner, so it is worth pinning.
    #[test]
    fn median_gold_never_falls_as_level_rises() {
        let median_at = |level: u64| -> u64 {
            let mut v: Vec<u64> = (0..80u64)
                .map(|n| gold_of(&roll_bundle(&big(), level, n).unwrap()))
                .collect();
            v.sort_unstable();
            v[v.len() / 2]
        };
        let levels = [2u64, 7, 12, 16, 20, 27, 32, 45, 60, 85];
        let mut previous = 0;
        for level in levels {
            let m = median_at(level);
            assert!(
                m + m / 3 >= previous,
                "median gold fell sharply from {previous} to {m} going into level {level}"
            );
            previous = m;
        }
    }

    /// A level far outside anything retail was observed at must clamp to the
    /// nearest band, not fall back to a pooled draw.
    #[test]
    fn an_out_of_range_level_clamps_to_the_nearest_band() {
        // Compared as DISTRIBUTIONS, not single draws: the buyer level is part of
        // the draw seed, so level 0 and level 1 pick different rewards out of the
        // same band. Only the band is being asserted here.
        let median_at = |level: u64| -> u64 {
            let mut v: Vec<u64> = (0..120u64)
                .map(|n| gold_of(&roll_bundle(&big(), level, n).unwrap()))
                .collect();
            v.sort_unstable();
            v[v.len() / 2]
        };
        assert!(roll_bundle(&big(), 0, 5).is_some(), "level 0 must still roll");
        assert!(roll_bundle(&big(), 500, 5).is_some(), "level 500 must still roll");

        let below = median_at(0);
        let lowest = median_at(1);
        let above = median_at(500);
        let highest = median_at(89);
        assert!(
            below.abs_diff(lowest) * 4 < lowest,
            "level 0 drew {below} against the lowest band's {lowest} — it did not clamp"
        );
        assert!(
            above.abs_diff(highest) * 4 < highest,
            "level 500 drew {above} against the highest band's {highest} — it did not clamp"
        );
        assert!(below < above, "clamping must still respect the ends of the ladder");
    }

    /// Gear must come out as real items with a fresh instance id — the frozen
    /// capture ids are what made buying twice overwrite the first item.
    #[test]
    fn gear_gets_a_fresh_instance_id_every_purchase() {
        let mut ids = std::collections::HashSet::new();
        let mut items = 0;
        for nonce in 0..40u64 {
            for item in &roll_bundle(&big(), 30, nonce).unwrap().items {
                items += 1;
                assert!(!item.item.item_template_id.is_nil());
                ids.insert(item.id);
            }
        }
        assert!(items > 20, "only {items} items over 40 purchases");
        assert_eq!(
            ids.len(),
            items,
            "instance ids repeated across purchases — buying twice would overwrite"
        );
        assert!(
            ids.iter().all(|id| id.get_version_num() == 4),
            "instance ids must look like the uuid4 retail minted"
        );
    }

    /// Only the mined bundles roll; everything else stays on the recorded path.
    #[test]
    fn an_unknown_product_does_not_roll() {
        assert!(roll_bundle(&Uuid::from_u128(0xDEAD), 30, 1).is_none());
    }

    // ---- #310: Legendary chests repeat, and artifacts drop more than once ----

    const LEGENDARY_PROMO: &str = BIG;
    /// Pandemonium, the artifact Sephoris holds three of.
    const PANDEMONIUM: Uuid = Uuid::from_u128(0x86a4ad9d_e2e8_49cd_9642_b553475b6580);

    /// What a player sees: gold, stackables and each item minus its instance id.
    fn fingerprint(g: &RewardGrant) -> String {
        let mut stack: Vec<_> = g.stackable_items.iter().collect();
        stack.sort();
        let items: Vec<String> = g
            .items
            .iter()
            .map(|i| serde_json::to_string(&i.item).unwrap())
            .collect();
        format!("{}|{stack:?}|{items:?}", gold_of(g))
    }

    fn legendary() -> Uuid {
        LEGENDARY_PROMO.parse().unwrap()
    }

    /// THE BUG. Sephoris (level 15) bought 165 Legendary chests. The level-15-17
    /// band holds 63 whole recorded rewards and the server replayed one of them
    /// per purchase, so 165 chests could only ever be 63 different chests, each
    /// seen about three times: 58 distinct in his exact sequence. Retail never
    /// repeated one in 4,697 purchases.
    #[test]
    fn legendary_chests_at_one_level_do_not_cycle_through_a_small_bank() {
        let mut seen = HashSet::new();
        for nonce in 0..165u64 {
            seen.insert(fingerprint(&roll_bundle(&legendary(), 15, nonce).unwrap()));
        }
        assert!(
            seen.len() >= 160,
            "165 Legendary chests at level 15 were only {} different chests",
            seen.len()
        );
    }

    /// THE SECOND BUG. Retail never granted an artifact the character already
    /// held — 0 duplicates across 2,386 artifact holdings in 576 retail inventory
    /// listings — and the APK's `LootEnhancementData` says why: a held artifact
    /// falls back to a `Legendary - High (Chests)` piece. Sephoris holds three
    /// Pandemonium from exactly this sequence.
    #[test]
    fn a_held_artifact_is_never_rolled_again() {
        for level in [3u64, 7, 15, 20, 27, 60] {
            let mut owned = HashSet::new();
            let mut artifacts = Vec::new();
            for nonce in 0..600u64 {
                let g = roll_bundle_for(&legendary(), level, nonce, &owned).unwrap();
                for item in &g.items {
                    let t = item.item.item_template_id;
                    if is_artifact(&t) {
                        assert!(
                            !owned.contains(&t),
                            "level {level}, chest {nonce}: granted artifact {t} a second time"
                        );
                        artifacts.push(t);
                    }
                    owned.insert(t);
                }
            }
            let distinct: HashSet<_> = artifacts.iter().collect();
            assert_eq!(distinct.len(), artifacts.len(), "level {level}: {artifacts:?}");
        }
    }

    /// The replacement is retail's: the chest keeps its shape and its other
    /// contents, and the artifact slot pays a non-artifact piece that retail
    /// itself paid in that slot. Retail player 78f2b668 shows it seven times —
    /// e.g. holding Pandemonium at level 18, the slot paid an Orcish Scaled Shield.
    #[test]
    fn a_duplicate_artifact_becomes_a_retail_legendary_piece() {
        let nonce = (0..5_000u64)
            .find(|&n| {
                roll_bundle(&legendary(), 15, n)
                    .unwrap()
                    .items
                    .iter()
                    .any(|i| i.item.item_template_id == PANDEMONIUM)
            })
            .expect("control: Pandemonium must be rollable at level 15");
        let fresh = roll_bundle(&legendary(), 15, nonce).unwrap();
        let held = roll_bundle_for(&legendary(), 15, nonce, &HashSet::from([PANDEMONIUM])).unwrap();

        assert_eq!(held.items.len(), fresh.items.len(), "the slot is replaced, not dropped");
        assert_eq!(held.currencies, fresh.currencies, "only the artifact slot changes");
        assert_eq!(held.stackable_items, fresh.stackable_items);
        let fallbacks = artifact_fallback_templates();
        for (a, b) in fresh.items.iter().zip(&held.items) {
            if a.item.item_template_id == PANDEMONIUM {
                let t = b.item.item_template_id;
                assert!(!is_artifact(&t), "replaced with another artifact {t}");
                assert!(fallbacks.contains(&t), "{t} was never a retail artifact-slot payout");
            } else {
                assert_eq!(a.item, b.item);
            }
        }
    }

    /// A widened roll keeps retail's payout: gold stays inside the band's observed
    /// range and the chest keeps retail's shape. The gear is generated (#368), so
    /// the control on it is that every piece is one the generator's retail rules
    /// allow — the rules every retail piece obeys.
    #[test]
    fn a_composed_chest_keeps_retail_gold_and_generated_gear() {
        let product = corpus_product(&legendary()).unwrap();
        for band in &product.by_level {
            let level = band.min_buyer_level;
            let golds: Vec<u64> =
                band.results.iter().map(|r| r.reward.currencies[&gold_id()]).collect();
            let (lo, hi) = (*golds.iter().min().unwrap(), *golds.iter().max().unwrap());
            for nonce in 0..200u64 {
                let g = roll_bundle(&legendary(), level, nonce).unwrap();
                let gold = gold_of(&g);
                assert!((lo..=hi).contains(&gold), "level {level}: gold {gold} not in {lo}..={hi}");
                assert!((2..=3).contains(&g.items.len()), "level {level}: {} items", g.items.len());
                for piece in &g.items[..2] {
                    if let Some(why) = legendary_gear::rule_broken(&piece.item) {
                        panic!("level {level}: {why}");
                    }
                }
            }
        }
    }

    /// Elder chests (tier 4) had the same thin-pool problem as Legendary ones before
    /// #215: eleven captured treasury opens, so a tier-4 chest at a given level
    /// picks among one or two bundles. The APK's own catalogue sells the Epic Chest
    /// as `_rarity: 4`, and retail's 405 Epic purchases are the Elder corpus.
    #[test]
    fn elder_treasury_chests_roll_from_the_epic_chest_corpus() {
        let mut seen = HashSet::new();
        for nonce in 0..40u64 {
            let g = roll_treasury_chest(4, 60, nonce, &HashSet::new())
                .expect("a tier-4 chest must roll from the Epic corpus");
            assert!(g.chests.is_empty());
            seen.insert(fingerprint(&g));
        }
        assert!(seen.len() >= 38, "40 Elder chests gave {} distinct", seen.len());
        assert!(roll_treasury_chest(3, 60, 1, &HashSet::new()).is_none(), "tier 3 keeps its table");
    }

    /// The artifact list is the APK's `Item.Tag.Artifact` set, and every artifact
    /// the store corpus pays is on it.
    #[test]
    fn the_artifact_list_covers_every_artifact_the_corpus_pays() {
        assert_eq!(ARTIFACT_TEMPLATES.len(), 24);
        assert!(is_artifact(&PANDEMONIUM));
        let paid: HashSet<Uuid> = corpus()
            .products
            .iter()
            .flat_map(|p| &p.by_level)
            .flat_map(|b| &b.results)
            .flat_map(|r| &r.reward.items)
            .filter(|i| {
                i.properties.enchanting.is_empty() && i.tempering_level == 0 && i.grade.is_none()
            })
            .map(|i| i.item_template_id)
            .collect();
        for t in paid {
            assert!(is_artifact(&t), "{t} is paid bare (no enchant/temper/grade) but not listed");
        }
    }

    fn gold_id() -> Uuid {
        GOLD.parse().unwrap()
    }

    // ---- #339/#354/#366: Legendary gear came out below the buyer's tier ----

    /// Share of `rolls` Legendary chests at `level` whose regular `slot` is an item
    /// of material tier `tier` (its unlock level), and the lowest tier seen there.
    fn slot_share(level: u64, slot: usize, tier: u64, rolls: u64) -> (f64, u64) {
        let mut hits = 0;
        let mut lowest = u64::MAX;
        for nonce in 0..rolls {
            let g = roll_bundle(&legendary(), level, nonce).unwrap();
            let r = required_level(&g.items[slot].item.item_template_id)
                .expect("every paid template has an APK requiredLevel");
            hits += u64::from(r == tier);
            lowest = lowest.min(r);
        }
        (hits as f64 / rolls as f64, lowest)
    }

    #[test]
    fn every_template_the_corpus_pays_has_a_required_level() {
        if let Err(e) = serde_json::from_str::<RequiredLevels>(ITEM_REQUIRED_LEVELS_RAW) {
            panic!("item_required_levels.json failed to parse: {e}");
        }
        for item in corpus()
            .products
            .iter()
            .flat_map(|p| &p.by_level)
            .flat_map(|b| &b.results)
            .flat_map(|r| &r.reward.items)
        {
            assert!(required_level(&item.item_template_id).is_some(), "{}", item.item_template_id);
        }
        let ladder = tier_ladder(corpus_product(&legendary()).unwrap());
        assert_eq!(ladder, vec![1, 8, 13, 18, 23, 28, 33, 39, 45], "the APK material ladder");
    }

    /// THE REPORTS. Sephoris at 23 ("Elven should be most of it"), at 33 ("hardly
    /// any Ebony"), Huge Goober at 74 ("two armour pieces too low"). Slot 1 pays
    /// the tier unlocked at the buyer's level and nothing falls below the tier
    /// under it. On the band draw a level-23 buyer got Elven in 21% of top slots
    /// and a level-33 buyer Ebony in 41%, with Elven (two tiers down) in 38% of
    /// lower slots.
    #[test]
    fn legendary_gear_is_at_the_buyers_tier() {
        const ROLLS: u64 = 2_000;
        // (level, slot-1 tier, minimum slot-1 share, lowest tier allowed anywhere)
        for (level, top, share, floor) in [
            (23u64, 23u64, 0.70, 18u64), // Elven, Dwarven at worst
            (33, 33, 0.70, 28),          // Ebony/Stalhrim, Glass at worst
            (74, 45, 1.0, 39),           // Dragon, Daedric at worst
            (100, 45, 1.0, 39),
        ] {
            let (s1, low1) = slot_share(level, 1, top, ROLLS);
            let (_, low0) = slot_share(level, 0, top, ROLLS);
            assert!(
                s1 >= share,
                "level {level}: tier {top} in only {:.0}% of top slots",
                s1 * 100.0
            );
            assert!(
                low0.min(low1) >= floor,
                "level {level}: a piece of tier {} came out, below {floor}",
                low0.min(low1)
            );
        }
        let (daedric, _) = slot_share(74, 0, 39, ROLLS);
        assert!(daedric > 0.8, "level 74 lower slot: Daedric {:.0}%", daedric * 100.0);
    }

    /// The rule against retail at the buyer's EXACT level: 306 retail Legendary
    /// purchases in the 2026-06-07 capture snapshot (distinct rewards, buyer level
    /// from the nearest `/characters/{id}` observation). (level, slot, tier,
    /// retail count, retail purchases). Level 10 is left out: three levels short
    /// of Orcish it is the one level the lookahead rule under-predicts.
    #[test]
    fn legendary_slots_match_retail_at_the_buyers_exact_level() {
        const RETAIL: [(u64, usize, u64, u64, u64); 20] = [
            (8, 0, 1, 19, 20),
            (8, 1, 8, 18, 20),
            (17, 0, 13, 21, 30),
            (17, 1, 13, 21, 30),
            (19, 0, 13, 16, 20),
            (19, 1, 18, 20, 20),
            (20, 0, 13, 8, 10),
            (20, 1, 18, 10, 10),
            (21, 0, 18, 27, 30),
            (21, 1, 18, 20, 30),
            (25, 0, 18, 18, 20),
            (25, 1, 23, 18, 20),
            (26, 0, 23, 10, 10),
            (26, 1, 23, 10, 10),
            (28, 0, 23, 18, 20),
            (28, 1, 28, 18, 20),
            (29, 0, 23, 10, 10),
            (29, 1, 28, 10, 10),
            (30, 0, 23, 8, 10),
            (30, 1, 28, 10, 10),
        ];
        for (level, slot, tier, k, n) in RETAIL {
            let retail = k as f64 / n as f64;
            let (ours, _) = slot_share(level, slot, tier, 1_000);
            assert!(
                (ours - retail).abs() <= 0.25,
                "level {level} slot {slot}: tier {tier} in {:.0}% of ours, {:.0}% of retail",
                ours * 100.0,
                retail * 100.0
            );
        }
    }

    /// The tier table for the PR: share of each material tier over both regular
    /// slots. `cargo test -p blades_lib print_legendary_tier_table -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_legendary_tier_table() {
        let ladder = [1u64, 8, 13, 18, 23, 28, 33, 39, 45];
        for level in [23u64, 33, 37, 40, 43, 74, 100] {
            let mut counts = [0u64; 9];
            for nonce in 0..5_000u64 {
                let g = roll_bundle(&legendary(), level, nonce).unwrap();
                for item in &g.items[..2] {
                    let r = required_level(&item.item.item_template_id).unwrap();
                    counts[ladder.iter().position(|&x| x == r).unwrap()] += 1;
                }
            }
            let shares: Vec<String> =
                counts.iter().map(|c| format!("{:3.0}", *c as f64 / 100.0)).collect();
            println!("L{level}: {}", shares.join(" "));
        }
    }

    // ---- #368: tier right, but the same piece every time ----

    /// THE REPORT. Sephoris (level 43) after #489: "ten times the same gloves with
    /// the same enchantments". At 39-44 the tier rule sent slot 1 to Daedric, and
    /// retail paid Daedric in slot 1 once in 4,697 purchases, so the widening ran
    /// out of bands with one candidate: Daedric Plate Gauntlets in 200 of 200
    /// chests at 39-42 (he holds 17 identical pairs). Ebony in slot 0 (five parts)
    /// did the same at 36-41, one piece in a fifth of chests. Every level, both
    /// slots: no one part in more than 15% of chests, and a wide spread in 50.
    #[test]
    fn legendary_chests_never_collapse_onto_one_part() {
        for level in 1u64..=100 {
            for slot in 0..2 {
                let mut counts: std::collections::HashMap<String, u32> = Default::default();
                let mut first50 = HashSet::new();
                for nonce in 0..200u64 {
                    let g = roll_bundle(&legendary(), level, nonce).unwrap();
                    let part = serde_json::to_string(&g.items[slot].item).unwrap();
                    if nonce < 50 {
                        first50.insert(part.clone());
                    }
                    *counts.entry(part).or_default() += 1;
                }
                let top = *counts.values().max().unwrap();
                assert!(
                    top <= 30,
                    "level {level} slot {slot}: one part in {top} of 200 chests"
                );
                assert!(
                    first50.len() >= 12,
                    "level {level} slot {slot}: only {} distinct parts in 50 chests",
                    first50.len()
                );
            }
        }
    }

    /// The tiers at 36-50 follow retail's pairs there: Glass+Ebony (18 of 62) and
    /// then Daedric+Dragon (38), never Ebony+Daedric (once). So a level-37 buyer
    /// gets Glass below Ebony, and a level-40 buyer Daedric below Dragon.
    #[test]
    fn legendary_slots_at_36_to_50_pair_as_retail_did() {
        let (glass, _) = slot_share(37, 0, 28, 1_000);
        let (ebony, _) = slot_share(37, 1, 33, 1_000);
        assert!(glass > 0.7 && ebony > 0.7, "level 37: Glass {glass:.2}, Ebony {ebony:.2}");
        for level in [39u64, 40, 43, 50] {
            let (daedric, _) = slot_share(level, 0, 39, 1_000);
            let (dragon, _) = slot_share(level, 1, 45, 1_000);
            assert!(daedric > 0.8, "level {level}: Daedric in {daedric:.2} of lower slots");
            assert!(dragon > 0.99, "level {level}: Dragon in {dragon:.2} of upper slots");
        }
    }

    /// The slot ladders are the corpus's own: every tier except Ebony in slot 0
    /// and Daedric in slot 1, which retail paid 5 and 1 times.
    #[test]
    fn each_slot_climbs_the_tiers_retail_paid_in_it() {
        let product = corpus_product(&legendary()).unwrap();
        assert_eq!(slot_ladder(product, 0), vec![1, 8, 13, 18, 23, 28, 39, 45]);
        assert_eq!(slot_ladder(product, 1), vec![1, 8, 13, 18, 23, 28, 33, 45]);
    }

    // ---- #368 (cont.): retail GENERATED the gear; it did not pick from a list ----

    /// One chest per retail observation in every band of the Legendary corpus, at
    /// levels spread over the band: the same sample shape the retail numbers were
    /// measured on.
    fn banded_rolls() -> Vec<(usize, RewardGrant)> {
        let product = corpus_product(&legendary()).unwrap();
        let mut out = Vec::new();
        for (b, band) in product.by_level.iter().enumerate() {
            let span = band.max_buyer_level - band.min_buyer_level + 1;
            for i in 0..band.results.len() as u64 {
                let level = band.min_buyer_level + i % span;
                out.push((b, roll_bundle(&legendary(), level, i).unwrap()));
            }
        }
        out
    }

    fn piece_key(item: &Item) -> String {
        let mut e: Vec<String> =
            item.properties.enchanting.iter().map(|p| format!("{}@{}", p.id, p.tier)).collect();
        e.sort();
        format!("{}|{}", item.item_template_id, e.join(","))
    }

    /// Exact repeats (same template, same enchantments) of a regular slot within a
    /// band, summed over the bands, and the number of pieces.
    fn banded_repeats(slot: usize, chests: &[(usize, Vec<Item>)]) -> (usize, usize) {
        let mut seen: HashSet<(usize, String)> = HashSet::new();
        let mut n = 0;
        for (b, items) in chests {
            n += 1;
            seen.insert((*b, piece_key(&items[slot])));
        }
        (n - seen.len(), n)
    }

    /// The numbers for the PR, ours against retail's.
    /// `cargo test -p blades_lib print_legendary_gear_variety -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_legendary_gear_variety() {
        let ours: Vec<(usize, Vec<Item>)> = banded_rolls()
            .into_iter()
            .map(|(b, g)| (b, g.items.into_iter().map(|i| i.item).collect()))
            .collect();
        let product = corpus_product(&legendary()).unwrap();
        let retail: Vec<(usize, Vec<Item>)> = product
            .by_level
            .iter()
            .enumerate()
            .flat_map(|(b, band)| band.results.iter().map(move |r| (b, r.reward.items.clone())))
            .collect();
        for (name, set) in [("retail", &retail), ("ours", &ours)] {
            for slot in 0..2 {
                let (rep, n) = banded_repeats(slot, set);
                let mut by_tier: std::collections::BTreeMap<u64, HashSet<Uuid>> = Default::default();
                let mut counts = [0usize; 4];
                let mut sets: HashSet<String> = HashSet::new();
                for (_, items) in set.iter() {
                    let it = &items[slot];
                    by_tier
                        .entry(required_level(&it.item_template_id).unwrap())
                        .or_default()
                        .insert(it.item_template_id);
                    counts[it.properties.enchanting.len().min(3)] += 1;
                    let mut e: Vec<_> = it.properties.enchanting.iter().map(|p| p.id).collect();
                    e.sort();
                    sets.insert(format!("{e:?}"));
                }
                let tiers: Vec<String> =
                    by_tier.iter().map(|(t, s)| format!("{t}:{}", s.len())).collect();
                println!(
                    "{name} slot {slot}: n={n} repeats={rep} ({:.1}%) enchant# {counts:?} \
                     distinct enchant sets {} templates/tier {}",
                    100.0 * rep as f64 / n as f64,
                    sets.len(),
                    tiers.join(" ")
                );
            }
        }
    }

    fn retail_banded() -> Vec<(usize, Vec<Item>)> {
        corpus_product(&legendary())
            .unwrap()
            .by_level
            .iter()
            .enumerate()
            .flat_map(|(b, band)| band.results.iter().map(move |r| (b, r.reward.items.clone())))
            .collect()
    }

    fn ours_banded() -> Vec<(usize, Vec<Item>)> {
        banded_rolls()
            .into_iter()
            .map(|(b, g)| (b, g.items.into_iter().map(|i| i.item).collect()))
            .collect()
    }

    /// THE IDENTITY TEST for the generator: every regular-slot piece retail paid in
    /// 4,697 Legendary purchases is one the generator's rules allow — a loot
    /// template of its tier, durability at its tempering level, a primary allowed
    /// on the template at the enchant tier, distinct secondaries from its table.
    /// If retail broke a rule, the rule would be ours, not retail's.
    #[test]
    fn every_retail_legendary_piece_obeys_the_generation_rules() {
        let mut checked = 0;
        for (_, items) in retail_banded() {
            for item in &items[..2] {
                checked += 1;
                if let Some(why) = legendary_gear::rule_broken(item) {
                    panic!("a retail piece breaks a rule: {why}");
                }
            }
        }
        assert_eq!(checked, 2_372);
    }

    /// THE REPORT (#368). Sephoris: retail could give ANY piece of the tier, not
    /// ~30. In the corpus retail paid all 21 loot templates of Silver, Daedric and
    /// Dragon (and of every tier with enough draws); a slot at one tier now spans
    /// them all too.
    #[test]
    fn legendary_gear_spans_every_template_of_the_tier() {
        // (level, slot, tier as its unlock level)
        for (level, slot, tier) in [(10u64, 1usize, 8u64), (30, 1, 28), (60, 0, 39), (60, 1, 45)] {
            let mut templates = HashSet::new();
            for nonce in 0..400u64 {
                let g = roll_bundle(&legendary(), level, nonce).unwrap();
                let t = g.items[slot].item.item_template_id;
                if required_level(&t) == Some(tier) {
                    templates.insert(t);
                }
            }
            assert_eq!(
                templates.len(),
                legendary_gear::tier_pool_size(tier),
                "level {level} slot {slot}: {} of the tier's templates",
                templates.len()
            );
        }
    }

    /// Exact repeats (same template AND same enchantments) of a slot within a level
    /// band: retail 75 of 1,186 in slot 0 and 40 in slot 1. #495's 12+ recorded
    /// parts repeated 473 and 486 times; generated gear repeats about as rarely as
    /// retail did.
    #[test]
    fn a_slot_repeats_exactly_about_as_rarely_as_retail() {
        let (retail, ours) = (retail_banded(), ours_banded());
        for slot in 0..2 {
            let (r, n) = banded_repeats(slot, &retail);
            let (o, m) = banded_repeats(slot, &ours);
            assert_eq!(n, m);
            assert!(
                o * 10 <= r * 13,
                "slot {slot}: {o} exact repeats in {m} chests, retail {r} in {n}"
            );
        }
    }

    /// Share of each enchantment count, ours against retail, per slot. The count
    /// (and tier) is retail's own, carried by the retail piece the tier rule picks.
    #[test]
    fn enchantment_counts_follow_retail() {
        let shares = |set: &[(usize, Vec<Item>)], slot: usize| -> [f64; 4] {
            let mut c = [0f64; 4];
            for (_, items) in set {
                c[items[slot].properties.enchanting.len().min(3)] += 1.0;
            }
            c.map(|x| x / set.len() as f64)
        };
        let (retail, ours) = (retail_banded(), ours_banded());
        for slot in 0..2 {
            let (r, o) = (shares(&retail, slot), shares(&ours, slot));
            for k in 0..4 {
                assert!(
                    (r[k] - o[k]).abs() <= 0.04,
                    "slot {slot}: {k} enchantments on {:.1}% of ours, {:.1}% of retail",
                    o[k] * 100.0,
                    r[k] * 100.0
                );
            }
        }
    }

    /// "Enchantments in all combinations": distinct enchantment sets per slot are
    /// at least retail's variety (625 and 691 in 1,186 retail pieces).
    #[test]
    fn enchantments_come_in_retails_variety_of_combinations() {
        let sets = |set: &[(usize, Vec<Item>)], slot: usize| -> usize {
            set.iter()
                .map(|(_, items)| {
                    let mut e: Vec<Uuid> =
                        items[slot].properties.enchanting.iter().map(|p| p.id).collect();
                    e.sort();
                    e
                })
                .collect::<HashSet<_>>()
                .len()
        };
        let (retail, ours) = (retail_banded(), ours_banded());
        for slot in 0..2 {
            let (r, o) = (sets(&retail, slot), sets(&ours, slot));
            assert!(o * 10 >= r * 9, "slot {slot}: {o} distinct enchantment sets, retail {r}");
        }
    }

    /// Weapons, shields, each armour slot, rings and necklaces come up in retail's
    /// proportions (weapons 755 of 2,372 regular-slot pieces, each armour slot
    /// about 260, rings 148, necklaces 159).
    #[test]
    fn equipment_kinds_come_up_as_often_as_retail() {
        let kinds = |set: &[(usize, Vec<Item>)]| {
            let mut c: std::collections::BTreeMap<&'static str, f64> = Default::default();
            for (_, items) in set {
                for item in &items[..2] {
                    *c.entry(legendary_gear::kind_of(&item.item_template_id).unwrap())
                        .or_default() += 1.0 / (2 * set.len()) as f64;
                }
            }
            c
        };
        let (retail, ours) = (kinds(&retail_banded()), kinds(&ours_banded()));
        assert_eq!(retail.len(), 8, "{retail:?}");
        for (kind, r) in &retail {
            let o = ours.get(kind).copied().unwrap_or(0.0);
            assert!(
                (r - o).abs() <= 0.03,
                "{kind}: {:.1}% of ours, {:.1}% of retail",
                o * 100.0,
                r * 100.0
            );
        }
    }

    /// Every retail piece, used as a shell, generates: never `None` (which would
    /// silently pay the recorded piece), always at the shell's own material tier,
    /// with the shell's tempering, enchant count and tier, a primary distinct
    /// from the secondaries, and jewellery keeping its kind, grade and GRADING.
    #[test]
    fn every_retail_shell_generates_at_its_own_tier() {
        for (i, (_, items)) in retail_banded().iter().enumerate() {
            for shell in &items[..2] {
                for k in 0..4u64 {
                    let g = legendary_gear::generate(shell, mix(i as u64 ^ k.rotate_left(40)))
                        .expect("a retail shell must generate");
                    let (s, t) = (&shell.item_template_id, &g.item_template_id);
                    assert_eq!(legendary_gear::tier_group_of(t), legendary_gear::tier_group_of(s));
                    assert_eq!(g.tempering_level, shell.tempering_level);
                    let (ge, se) = (&g.properties.enchanting, &shell.properties.enchanting);
                    assert_eq!(ge.len(), se.len());
                    assert!(ge.iter().zip(se).all(|(a, b)| a.tier == b.tier));
                    if let Some(p) = ge.first() {
                        assert!(ge[1..].iter().all(|e| e.id != p.id), "secondary repeats primary");
                    }
                    let kind = legendary_gear::kind_of(s).unwrap();
                    if kind == "ring" || kind == "necklace" {
                        assert_eq!(legendary_gear::kind_of(t), Some(kind));
                        assert_eq!(g.grade, shell.grade);
                        assert_eq!(g.properties.grading, shell.properties.grading);
                    }
                    if let Some(why) = legendary_gear::rule_broken(&g) {
                        panic!("{why}");
                    }
                }
            }
        }
    }
}
