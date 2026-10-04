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
/// different item families, so they are never mixed). Nothing is invented and
/// the payout stays in the band's observed range.
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
        items.push(draw(band, slot_seed)?.reward.items[slot].clone());
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

    /// A widened roll must still be made of retail parts: each item slot draws from
    /// what retail paid in that slot within the band, and gold stays inside the
    /// band's observed range. This is the control on the widening.
    #[test]
    fn a_composed_chest_is_made_of_retail_parts() {
        let product = corpus_product(&legendary()).unwrap();
        for band in &product.by_level {
            let level = band.min_buyer_level;
            let golds: Vec<u64> =
                band.results.iter().map(|r| r.reward.currencies[&gold_id()]).collect();
            let (lo, hi) = (*golds.iter().min().unwrap(), *golds.iter().max().unwrap());
            let slot_pool = |k: usize| -> HashSet<String> {
                band.results
                    .iter()
                    .filter_map(|r| r.reward.items.get(k))
                    .map(|i| serde_json::to_string(i).unwrap())
                    .collect()
            };
            let (s0, s1) = (slot_pool(0), slot_pool(1));
            for nonce in 0..200u64 {
                let g = roll_bundle(&legendary(), level, nonce).unwrap();
                let gold = gold_of(&g);
                assert!((lo..=hi).contains(&gold), "level {level}: gold {gold} not in {lo}..={hi}");
                assert!((2..=3).contains(&g.items.len()), "level {level}: {} items", g.items.len());
                assert!(s0.contains(&serde_json::to_string(&g.items[0].item).unwrap()));
                assert!(s1.contains(&serde_json::to_string(&g.items[1].item).unwrap()));
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
}
