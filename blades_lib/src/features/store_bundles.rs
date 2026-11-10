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
        serde_json::from_str(STORE_BUNDLE_LOOT_RAW).unwrap_or_else(|_| Corpus {
            products: Vec::new(),
        })
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

const LEGENDARY_CHEST_PRODUCT_ID: Uuid = Uuid::from_u128(0x1275d959_bbe5_460d_8f6a_1c31106a8eb2);

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

/// Every artifact with its APK `requiredLevel` and `lootWeight`
/// (`reference/game-defs/items.json`; durability is the APK `maxDurability`,
/// 0 for the two rings, which retail sent without tempering or durability).
///
/// The lootWeight is authored per kind: 2 for a weapon, 7 for a shield, 9 for
/// armour and rings. The corpus's 27 artifacts are 13 weapons, 11 armour pieces
/// or rings and 3 shields, of 17, 5 and 2 artifacts: the weights predict 10 /
/// 13 / 4, a uniform pick 19 / 6 / 2.
const ARTIFACT_LOOT: [(Uuid, u64, u64, f64); 24] = [
    (
        Uuid::from_u128(0x4e841f5e_c5d7_47cd_899a_116846dbbfc0),
        6,
        2,
        355.0,
    ), // Bloodthirst
    (
        Uuid::from_u128(0x12c4920e_a9e0_4498_b134_3ce66fb9558f),
        6,
        9,
        473.0,
    ), // Savior's Hide
    (
        Uuid::from_u128(0xe86a6f9a_4e40_40d6_a9dd_1036b4190caa),
        13,
        2,
        220.0,
    ), // Fork of Horripilation
    (
        Uuid::from_u128(0x899b63a7_6eb7_4689_a450_8c6b507763c6),
        13,
        2,
        585.0,
    ), // Guardian's Claymore
    (
        Uuid::from_u128(0x05862206_b6db_4cd4_9f7a_ed2e03fc90af),
        13,
        9,
        0.0,
    ), // Ring of Namira
    (
        Uuid::from_u128(0x59c720fb_d761_4ad1_b14c_2aa8c1a4d554),
        18,
        2,
        446.0,
    ), // Mace of Molag Bal
    (
        Uuid::from_u128(0x6ab76008_dd08_461f_bdcc_6373799ea489),
        18,
        2,
        446.0,
    ), // Rueful Axe
    (
        Uuid::from_u128(0x86a4ad9d_e2e8_49cd_9642_b553475b6580),
        18,
        7,
        405.0,
    ), // Pandemonium
    (
        Uuid::from_u128(0xbe1484e6_c156_47db_bdab_3cf5b75096a0),
        23,
        2,
        410.0,
    ), // Chrysamere
    (
        Uuid::from_u128(0x4b59cdbe_f857_4673_9e40_741ca06aa5d0),
        23,
        2,
        338.0,
    ), // Spellbane
    (
        Uuid::from_u128(0x8a69bdb2_d179_4f2d_984a_322f9397aedf),
        28,
        2,
        340.0,
    ), // Rimelink
    (
        Uuid::from_u128(0xaf7759d9_f38e_4384_bf36_1de0c3d150a7),
        28,
        2,
        453.0,
    ), // Volendrung
    (
        Uuid::from_u128(0xdef810af_e9f5_4e23_9247_1edf391d82e1),
        28,
        9,
        453.0,
    ), // Ebony Mail
    (
        Uuid::from_u128(0x515450b1_ab21_4cbf_a8ed_c4e0e51df4c0),
        33,
        2,
        496.0,
    ), // Dragon's Blight
    (
        Uuid::from_u128(0x21f8b677_c1a9_4d5f_87ef_294fc6ba4ba5),
        33,
        2,
        454.0,
    ), // Ebony Blade
    (
        Uuid::from_u128(0xa0d7dd5e_692e_4fbb_bbf2_19c6a874f3f5),
        33,
        2,
        248.0,
    ), // Mehrunes' Razor
    (
        Uuid::from_u128(0xca1e2d0f_b902_4c82_b8d6_e82b83dcc9e0),
        38,
        2,
        346.0,
    ), // Dawnbreaker
    (
        Uuid::from_u128(0x9a22b59a_cdbc_4987_bf4a_0cc812428aeb),
        38,
        7,
        462.0,
    ), // Spellbreaker
    (
        Uuid::from_u128(0x7d2dfa88_e7ca_4fd6_a173_862cfb077c04),
        44,
        2,
        537.0,
    ), // Stendarr's Hammer
    (
        Uuid::from_u128(0x23607f09_a103_4ed3_a0de_33e0498f8018),
        44,
        9,
        0.0,
    ), // Warlock's Ring
    (
        Uuid::from_u128(0x9107fb63_f88b_45fa_bca9_7fe2f9bceb2c),
        50,
        2,
        434.0,
    ), // Chillrend
    (
        Uuid::from_u128(0x21ed2758_5cf5_4b7d_9047_59bbdbd61820),
        50,
        2,
        626.0,
    ), // Onyx Cleaver
    (
        Uuid::from_u128(0xda0cf5aa_6d37_48da_8b18_3f4e914145d3),
        50,
        2,
        626.0,
    ), // Onyx Saber
    (
        Uuid::from_u128(0x975509a9_196f_41c9_8df4_6f0e2d1f02ce),
        50,
        9,
        497.0,
    ), // Lord's Mail
];

/// How often a chest has its third, artifact slot: (chests with it, chests).
///
/// THE REPORT (#378). Sephoris at 45, ~200 Legendary chests: "no artifact,
/// although many chests had three items; the third item is always the same
/// sword, ring or shield". The slot used to be copied from one recorded chest of
/// the buyer's band, so the band decided WHICH artifact: the 36-50 band holds one
/// (Onyx Cleaver, which he owns), so every artifact roll at 36-50 was his owned
/// Onyx Cleaver and fell back, to one of the band's four recorded fallback pieces.
///
/// Retail: 60 of the 1,186 Legendary corpus chests have the slot (5.1%; the
/// 306 exact-level snapshot purchases: 16, 5.2%), flat in level (3.2% to 7.9%
/// per band, no trend). The Elder (Epic) chest has it in 4 of 405 (snapshot: 3
/// of 287). Gold chests: see [`GOLD_ARTIFACT_SLOT`]; Silver and Wooden never
/// (0 of 459 treasury openings).
const LEGENDARY_ARTIFACT_SLOT: (u64, u64) = (60, 1186);
const ELDER_ARTIFACT_SLOT: (u64, u64) = (4, 405);

/// A Gold chest's artifact slot (#378 follow-up): one chest in a thousand.
///
/// HauDrauf recorded three retail Gold chest openings (BigHump, level 60, and
/// others) each paying an artifact as a third item beside the usual two gear
/// pieces, gold (8,304-9,712) and materials: Spellbreaker, Warlock's Ring and
/// Chillrend. He puts it at about one Gold chest in a thousand. The 270
/// captured Gold openings hold none, which a 0.1% rate predicts (76% chance of
/// none in 270), so the rate is his.
pub const GOLD_ARTIFACT_SLOT: (u64, u64) = (1, 1000);

/// The levels at which retail's chests paid an artifact of each tier, keyed by
/// the tier's APK `requiredLevel`: (required level, first level, last level).
///
/// THE EVIDENCE (#378 follow-up). A community table HauDrauf supplied (Discord,
/// IWonderMilkl, 2026-04-04) gives, per tier, the level an artifact stops
/// dropping ("findable before", tiers 2-7) and one player's lowest level for
/// each. Retail's own captures are the harder evidence and win where the two
/// disagree; the first level is the lowest either source shows:
///
/// | tier (usable) | first: table / captures -> used | last (table) |
/// |---|---|---|
/// | 2 (6)  | 1 (Savior's Hide) / 4 -> 1 | 22 |
/// | 3 (13) | 1 (Guardian's Claymore) / 3 (Fork) -> 1 | 27 |
/// | 4 (18) | 11 / 10 (Pandemonium, snapshot) -> 10 | 32 |
/// | 5 (23) | 23 / 21 (Chrysamere, snapshot) -> 21 | 37 |
/// | 6 (28) | 23 / 19 (Ebony Mail, snapshot) -> 19 | 43 |
/// | 7 (33) | - / 26 (Dragon's Blight; Ebony Blade in an Elder chest) -> 26 | 49 |
/// | 8 (38) | - / Spellbreaker in a Glass+Glass chest -> 31 | none |
/// | 9-10 (44, 50) | HauDrauf: from about 39 -> 39 | none |
///
/// Tier 8 has no dated drop: retail's one tier-8 Legendary drop is Spellbreaker
/// in a 29-35 chest whose top slot is Glass (paid at 28-32, Ebony from 33) with
/// a Greater soul gem (the ladder's from about 31), so a buyer of 31-32. 31 is
/// also the lead every dated tier shows over its usable level (tier 4 at 10, 8
/// ahead; tier 6 at 19, 9; tier 7 at 26, 7): 38 - 7. The 33 a "usable at
/// tier 7" guess would give is contradicted by that chest. Tiers 8-10 still
/// drop at any higher level (the table has no cap; HauDrauf; retail paid
/// Dawnbreaker, tier 8, and Warlock's Ring, tier 9, in the 54-89 band).
///
/// Every one of the 29 retail artifacts with a buyer level falls inside its
/// tier's window (`retail_artifact_drops_fall_inside_their_windows`).
pub(crate) const ARTIFACT_WINDOWS: [(u64, u64, u64); 9] = [
    (6, 1, 22),
    (13, 1, 27),
    (18, 10, 32),
    (23, 21, 37),
    (28, 19, 43),
    (33, 26, 49),
    (38, 31, u64::MAX),
    (44, 39, u64::MAX),
    (50, 39, u64::MAX),
];

/// Whether an artifact of APK `requiredLevel` `required` can drop at `level`.
fn artifact_in_window(required: u64, level: u64) -> bool {
    ARTIFACT_WINDOWS
        .iter()
        .find(|(r, _, _)| *r == required)
        .is_some_and(|(_, first, last)| (*first..=*last).contains(&level.max(1)))
}

/// The artifact a chest's artifact slot rolls at `level`: weighted by the APK
/// lootWeight among the artifacts whose tier's window holds the level
/// ([`ARTIFACT_WINDOWS`], #378).
fn roll_artifact(level: u64, seed: u64) -> Option<(Uuid, f64)> {
    let pool: Vec<&(Uuid, u64, u64, f64)> = ARTIFACT_LOOT
        .iter()
        .filter(|(_, required, _, _)| artifact_in_window(*required, level))
        .collect();
    let total: u64 = pool.iter().map(|a| a.2).sum();
    if total == 0 {
        return None;
    }
    let mut at = mix(seed) % total;
    pool.iter()
        .find(|a| {
            if at < a.2 {
                true
            } else {
                at -= a.2;
                false
            }
        })
        .map(|a| (a.0, a.3))
}

/// What a chest's artifact slot pays at `level`: the rolled artifact, or for one
/// the character holds the generated "Legendary - High (Chests)" piece (#310).
pub(crate) fn artifact_slot_item(level: u64, seed: u64, owned: &HashSet<Uuid>) -> Option<Item> {
    let art_seed = mix(seed ^ 0xE703_7ED1_A0B4_28DB);
    match roll_artifact(level, art_seed) {
        Some((template, durability)) if !owned.contains(&template) => {
            Some(artifact_item(template, durability))
        }
        _ => artifact_fallback_piece(level, mix(art_seed)),
    }
}

/// An artifact as retail paid it: untempered, unenchanted, full durability.
fn artifact_item(template: Uuid, durability: f64) -> Item {
    Item {
        item_template_id: template,
        grade: None,
        tempering_level: 0,
        durability,
        properties: Default::default(),
        arcane_tier: None,
    }
}

/// The "Legendary - High (Chests)" piece paid when the rolled artifact is one the
/// buyer holds (#310), GENERATED like a regular Legendary slot (#378).
///
/// It used to be one of the pieces retail happened to pay in the slot in the
/// buyer's band, four or five per band, frozen: Sephoris holds five identical
/// Stalhrim Shields and five identical Ebony Atronite Rings from it. Retail's
/// seven fallbacks at an exact buyer level (snapshot, player 78f2b668 at 8-28)
/// are all at the tier Legendary slot 0 pays at that level ([`legendary_slot_tier`]:
/// the tier below until three levels past an unlock, e.g. Orcish at 17, 19 and
/// 20, Dwarven at 21, Elven at 28), with slot 0's tempering and enchantments. So
/// it is slot 0's roll: a retail slot-0 shell at that tier, generated around.
/// Of the 42 retail fallbacks two are jewellery and 40 equipment, which is what
/// slot 0's shells give; HauDrauf expected mostly jewellery, the record does not
/// bear that out.
fn artifact_fallback_piece(level: u64, seed: u64) -> Option<Item> {
    let legendary = corpus_product(&LEGENDARY_CHEST_PRODUCT_ID)?;
    let (shell, tier) = legendary_slot_part(legendary, 0, level, seed)?;
    super::legendary_gear::generate_at(shell, tier, mix(seed ^ 0x4F1B_BCDC_BFA5_3E0B))
        .or_else(|| Some(shell.clone()))
}

/// The number of item slots every reward in the band has. Retail's chest paid
/// that many regular pieces; a reward with more carries the artifact slot.
fn core_slots(band: &Band) -> usize {
    band.results
        .iter()
        .map(|r| r.reward.items.len())
        .min()
        .unwrap_or(0)
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
    corpus_product(product_id)
        .map(|p| p.observations)
        .unwrap_or(0)
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
    let nearest = all
        .iter()
        .map(|(lo, hi, _)| level_distance(level, *lo, *hi))
        .min()?;
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
/// THE SLOT LADDERS (#368, #495; slot 1's half superseded below). Retail paid
/// Daedric in the upper slot once in 4,697 purchases and Ebony in the lower slot
/// five times; the 36-50 band's pairs are Glass+Ebony 18, Daedric+Dragon 38,
/// Dragon+Dragon 4, and one each of Ebony+Ebony, Glass+Glass and Ebony+Daedric;
/// 54-89 is Daedric+Dragon. #495 read that as slot 1 skipping Daedric
/// (Ebony -> Dragon). Slot 0 still catches up only to a tier it actually paid.
///
/// SLOT 1 AT DAEDRIC (#368 cont.). Skipping Daedric in slot 1 sent every chest at
/// 39-44 to Dragon, six levels before Dragon unlocks: Sephoris at 43, "too many
/// Dragon items; they should be there, but not the majority, and ramp up slowly".
/// Retail's slot 1 never paid Daedric because retail had almost no buyers at
/// 39-48: the 36-50 band is two populations, 19 chests with Ebony on top, Ebony
/// ingots and Exceptional-Elevated soul gems (a 36-38 buyer's), and 42 with
/// Dragon on top, identical in every stackable to the 54-89 band; the one chest
/// between them is Ebony+Daedric. At every level retail was measured, slot 1 pays the tier
/// unlocked at the buyer's level, so slot 1 now climbs the whole ladder: Daedric
/// at 39-44 (Dragon one roll in five at 43-44, the lookahead), Dragon from 45.
/// Gear is generated (#496), so a tier with few retail shells borrows the slot's
/// shells from its neighbours ([`legendary_slot_part`]). Slot 0 pays the tier
/// below slot 1's on the same whole ladder, catching up only to a tier retail
/// paid there: Glass under Ebony at 33-38 (retail 18 of 19 at 36-38, no catch-up),
/// Ebony under Daedric at 39-41 (the one dated chest, Ebony+Daedric), Daedric
/// from 42, and Daedric under Dragon from 45 (150 of 161 at 54-89).
fn legendary_slot_tier(
    ladder: &[u64],
    slot0_ladder: &[u64],
    slot: usize,
    level: u64,
    seed: u64,
) -> Option<u64> {
    let top_idx = ladder.iter().rposition(|&r| r <= level.max(1)).unwrap_or(0);
    let top = *ladder.get(top_idx)?;
    // Slot 1 climbs the whole ladder (#368 cont.): the tier unlocked at the
    // buyer's level, Daedric included.
    let upper = |tier: u64| ladder.iter().copied().find(|&t| t >= tier);
    let t1 = upper(top)?;
    match slot {
        0 => {
            let lower = slot0_ladder;
            // The tier below slot 1's on the whole ladder; slot 0 catches up only
            // to a tier it paid itself (it never did to Ebony, #368).
            let prev = ladder
                .iter()
                .copied()
                .filter(|&t| t < t1)
                .max()
                .unwrap_or(t1);
            let ceiling = Some(&t1) == ladder.last();
            let caught_up = !ceiling && level >= t1 + CATCH_UP_LEVELS && lower.contains(&t1);
            let (usual, other) = if caught_up { (t1, prev) } else { (prev, t1) };
            if mix(seed ^ 0x7F4A_7C15_9E37_79B9) % SLOT0_OTHER_ONE_IN == 0 {
                Some(other)
            } else {
                Some(usual)
            }
        }
        1 => match ladder
            .get(top_idx + 1)
            .and_then(|&next| upper(next).map(|a| (next, a)))
        {
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
            b.results
                .iter()
                .flat_map(move |r| r.reward.items.iter().take(core))
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
/// however many recorded chests carried it). Returns the shell and the tier to
/// generate it at. A tier the slot paid too rarely to reach [`MIN_TIER_PARTS`]
/// anywhere (slot 1 at Daedric: one part in 4,697 chests) borrows the slot's
/// shells from the nearest tier below or above that has enough, one side or the
/// other with equal chance; they carry the slot's own tempering and
/// enchantments, and the piece is then generated at the slot's tier.
/// `None` leaves the slot on the band draw.
fn legendary_slot_part(
    product: &'static Product,
    slot: usize,
    level: u64,
    seed: u64,
) -> Option<(&'static Item, u64)> {
    let (ladder, slot0_ladder) = legendary_ladders(product);
    let tier = legendary_slot_tier(ladder, slot0_ladder, slot, level, seed)?;

    let mut shell_tier = tier;
    if slot_parts(product, slot, tier) < MIN_TIER_PARTS {
        // Below and above equally: slot 1 at Daedric would otherwise take two
        // shells in three from Dragon, i.e. a level-54 buyer's enchant tiers.
        let idx = ladder.iter().position(|&t| t == tier)?;
        let enough = |t: &&u64| slot_parts(product, slot, **t) >= MIN_TIER_PARTS;
        let below = ladder[..idx].iter().rev().find(enough);
        let above = ladder[idx + 1..].iter().find(enough);
        let sides: Vec<u64> = below.into_iter().chain(above).copied().collect();
        if !sides.is_empty() {
            shell_tier = sides[(mix(seed ^ 0xBB67_AE85_84CA_A73B) % sides.len() as u64) as usize];
        }
    }

    let candidates = slot_shells(product, &[slot], shell_tier, level);
    if candidates.is_empty() {
        return None;
    }
    Some((
        candidates[(mix(seed) % candidates.len() as u64) as usize],
        tier,
    ))
}

/// What `product` paid in regular `slots` at material tier `tier`, from the bands
/// nearest `level`, widening band by band until there are at least
/// [`MIN_TIER_PARTS`] DISTINCT parts (#368).
fn slot_shells(
    product: &'static Product,
    slots: &[usize],
    tier: u64,
    level: u64,
) -> Vec<&'static Item> {
    let mut bands: Vec<&Band> = product.by_level.iter().collect();
    bands.sort_by_key(|b| level_distance(level, b.min_buyer_level, b.max_buyer_level));
    let mut candidates: Vec<&Item> = Vec::new();
    for band in bands {
        if candidates.len() >= MIN_TIER_PARTS {
            break;
        }
        for &slot in slots {
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
    }
    candidates
}

/// A part for an Elder chest's regular slot at `level` and the tier to generate
/// it at (#378).
///
/// THE REPORT. "The contents of Gold and Elder chests are set too low." For
/// Elder they were: every buyer from 31 up drew from the one 31-89 band, a
/// population of mid-30s buyers (Ebony and Stalhrim ingots): 74% Glass gear
/// with tier 3-4 enchantments for a level-45 or level-90 chest. Retail's own
/// Elder chests at 54-94 (the 11 tier-4 treasury openings) paid Daedric in 19
/// of 22 slots and Dragon in 3, with tier 5-7 enchantments.
///
/// THE RULE, measured on the 287 retail Elder purchases at their exact buyer
/// level (2026-06-07 snapshot): both slots pay what a Legendary chest's LOWER
/// slot pays ([`legendary_slot_tier`] slot 0) — the tier below the buyer's
/// until three levels past an unlock, then the buyer's (retail: 249 slots a
/// tier down, 306 at the buyer's tier, 19 a tier up, of 574; the two slots
/// alike) — and at the top of the ladder Daedric, Dragon one roll in seven
/// (retail 19 / 3 of 22).
/// The shell is Elder's own retail piece at that tier from the nearest bands;
/// where Elder paid the tier too rarely (Daedric: 8 pieces, Dragon: none), a
/// Legendary slot-0 piece of the tier stands in, and the piece is generated
/// around it like a Legendary one (#496).
fn elder_slot_part(
    elder: &'static Product,
    slot: usize,
    level: u64,
    seed: u64,
) -> Option<(&'static Item, u64)> {
    let legendary = corpus_product(&LEGENDARY_CHEST_PRODUCT_ID)?;
    let (ladder, slot0_ladder) = legendary_ladders(legendary);
    let tier = legendary_slot_tier(ladder, slot0_ladder, 0, level, mix(seed ^ slot as u64))?;
    let mut candidates = slot_shells(elder, &[0, 1], tier, level);
    if candidates.len() < MIN_TIER_PARTS {
        candidates = slot_shells(legendary, &[0], tier, level);
    }
    if candidates.is_empty() {
        candidates = slot_shells(legendary, &[1], tier, level);
    }
    if candidates.is_empty() {
        return None;
    }
    Some((
        candidates[(mix(seed) % candidates.len() as u64) as usize],
        tier,
    ))
}

/// The Legendary corpus's whole tier ladder and slot 0's own ladder.
fn legendary_ladders(product: &Product) -> (&'static [u64], &'static [u64]) {
    static LADDER: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    static SLOT0_LADDER: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    (
        LADDER.get_or_init(|| tier_ladder(product)),
        SLOT0_LADDER.get_or_init(|| slot_ladder(product, 0)),
    )
}

/// The fewest recorded chests a Legendary base draw is narrowed to.
const MIN_BASE_CHESTS: usize = 12;

/// The recorded reward a Legendary chest takes its gold and stackables from
/// (#368 cont.): one from the band whose top slot paid the tier
/// unlocked at the buyer's level, else the highest tier below it that the band
/// holds at least [`MIN_BASE_CHESTS`] chests of, else the whole band. The
/// artifact slot is not taken from it (#378: [`LEGENDARY_ARTIFACT_SLOT`]).
///
/// THE BUG. The 36-50 band is two retail populations, not one: 19 chests with
/// Ebony on top (Glass+Ebony 18) carrying Ebony and Stalhrim ingots, diamonds
/// and Exceptional to Elevated soul gems, and 42 with Dragon on top whose every
/// stackable (Daedra hearts,
/// dragon bone and scale, Glorious and Transcendent soul gems) is the 54-89
/// band's. Drawn whole-band, a level-37 buyer got the level-54 chest two times in
/// three, and a level-43 buyer every soul gem from Exceptional to Transcendent.
/// The top slot's tier says which population a chest is: Dragon is paid there
/// from 45, Ebony at 33-38.
fn legendary_base(
    product: &Product,
    band: &'static Band,
    level: u64,
    seed: u64,
) -> Option<&'static Drawn> {
    let (ladder, _) = legendary_ladders(product);
    let own = *ladder.iter().rev().find(|&&r| r <= level.max(1))?;
    let top_tier = |r: &Drawn| {
        r.reward
            .items
            .get(1)
            .and_then(|i| required_level(&i.item_template_id))
    };
    let core = core_slots(band);
    let mut chosen: Vec<&'static Drawn> = Vec::new();
    for &tier in ladder.iter().rev().filter(|&&t| t <= own) {
        chosen = band
            .results
            .iter()
            .filter(|r| core >= 2 && top_tier(r) == Some(tier))
            .collect();
        if chosen.len() >= MIN_BASE_CHESTS {
            break;
        }
    }
    if chosen.len() < MIN_BASE_CHESTS {
        return draw(band, seed);
    }
    let total: u64 = chosen.iter().map(|r| r.n).sum();
    let mut pick = seed % total.max(1);
    chosen
        .iter()
        .copied()
        .find(|r| {
            if pick < r.n {
                true
            } else {
                pick -= r.n;
                false
            }
        })
        .or(chosen.last().copied())
}

/// How many distinct retail parts the Legendary corpus paid in regular `slot` at
/// `tier`.
fn slot_parts(product: &Product, slot: usize, tier: u64) -> usize {
    static COUNTS: std::sync::OnceLock<std::collections::HashMap<(usize, u64), usize>> =
        std::sync::OnceLock::new();
    let counts = COUNTS.get_or_init(|| {
        let mut seen: HashSet<(usize, String)> = HashSet::new();
        let mut counts = std::collections::HashMap::new();
        for band in &product.by_level {
            let core = core_slots(band);
            for r in &band.results {
                for (s, item) in r.reward.items.iter().take(core).enumerate() {
                    let Some(t) = required_level(&item.item_template_id) else {
                        continue;
                    };
                    let key = serde_json::to_string(item).unwrap_or_default();
                    if seen.insert((s, key)) {
                        *counts.entry((s, t)).or_insert(0) += 1;
                    }
                }
            }
        }
        counts
    });
    counts.get(&(slot, tier)).copied().unwrap_or(0)
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
/// THE ARTIFACT RULE (#310, #378). An artifact is unique. A Legendary or Elder
/// chest pays its artifact slot at retail's rate, with an artifact drawn by the
/// APK weights within reach of the buyer's level ([`roll_artifact`]); one the
/// buyer already holds is replaced by a generated "Legendary - High (Chests)"
/// piece, as `LootEnhancementData`'s fallback dictates
/// ([`artifact_fallback_piece`]). Other products keep the recorded slot, with a
/// held artifact replaced by a piece retail paid there.
pub fn roll_bundle_for(
    product_id: &Uuid,
    buyer_level: u64,
    nonce: u64,
    owned: &HashSet<Uuid>,
) -> Option<RewardGrant> {
    roll_bundle_with(product_id, buyer_level, nonce, owned, None)
}

/// The level from which an Elder chest's gold and stackables come from retail's
/// own high-level Elder chests rather than the Epic corpus's 31-89 band (#378).
///
/// The band is a mid-30s population (Ebony and Stalhrim ingots, Exceptional
/// soul gems, median 19,885 gold); the 11 retail tier-4 treasury openings at
/// 54-94 pay Daedra hearts, dragon scale and bone and a median 22,066 gold. No
/// retail Elder chest is dated at 39-53, so the switch follows the Legendary
/// chest's (#368): from 45, where Dragon unlocks and its population changes.
pub const ELDER_HIGH_LEVEL: u64 = 45;

/// The chest tier a store chest product is: the Golden chest a Gold chest (3),
/// the Epic chest an Elder chest (4), the Legendary chest a Legendary chest (5).
/// `None` for any other product.
pub fn store_chest_tier(product_id: &Uuid) -> Option<u64> {
    match corpus_product(product_id)?.product_id {
        GOLDEN_CHEST_CORPUS_ID => Some(3),
        ELDER_CHEST_CORPUS_ID => Some(4),
        LEGENDARY_CHEST_CORPUS_ID => Some(5),
        _ => None,
    }
}

/// A store purchase: [`roll_bundle_with`] at the buyer's loot level, then a chest
/// product's soul gems by the #421/#429 rule for a `character_level` buyer
/// ([`crate::features::chest_soul_gems`]; owner-approved deviation from retail).
/// The rule is applied here, not in [`roll_bundle_with`], because a treasury
/// Elder or Legendary chest also rolls through that and is paid the rule when it
/// is opened, at the opener's level.
pub fn roll_store_chest(
    product_id: &Uuid,
    character_level: u64,
    nonce: u64,
    owned: &HashSet<Uuid>,
    treasury: Option<&crate::features::chests::ChestLootTables>,
) -> Option<RewardGrant> {
    let buyer_level = crate::features::loot_level::effective_loot_level_u64(character_level);
    let mut grant = roll_bundle_with(product_id, buyer_level, nonce, owned, treasury)?;
    if let Some(tier) = store_chest_tier(product_id) {
        crate::features::chest_soul_gems::pay_chest_soul_gems(
            &mut grant.stackable_items,
            tier,
            i64::try_from(character_level).unwrap_or(i64::MAX),
            mix(product_id.as_u128() as u64 ^ mix(nonce) ^ 0x0421_0429_5701_6E35),
        );
        // Levels 45-100: tempering 1-3, effect ingredients 30, dragon 1-7 (#438).
        crate::features::high_level_materials::pay_chest(
            &mut grant.stackable_items,
            tier,
            i64::try_from(character_level).unwrap_or(i64::MAX),
            mix(product_id.as_u128() as u64 ^ mix(nonce) ^ 0x0438_5701_C4E5),
        );
    }
    Some(grant)
}

/// [`roll_bundle_for`], with retail's treasury chest openings (`chest_loots.json`):
/// tier 4 for an Elder chest at [`ELDER_HIGH_LEVEL`] and above, and the Gold chest
/// table for a Golden chest from [`TOP_LOOT_FROM_LEVEL`] (`None` keeps the band).
///
/// [`TOP_LOOT_FROM_LEVEL`]: crate::features::loot_level::TOP_LOOT_FROM_LEVEL
pub fn roll_bundle_with(
    product_id: &Uuid,
    buyer_level: u64,
    nonce: u64,
    owned: &HashSet<Uuid>,
    treasury: Option<&crate::features::chests::ChestLootTables>,
) -> Option<RewardGrant> {
    let product = corpus_product(product_id)?;
    let is_legendary = product.product_id == LEGENDARY_CHEST_CORPUS_ID;
    let is_elder = product.product_id == ELDER_CHEST_CORPUS_ID;
    let is_golden = product.product_id == GOLDEN_CHEST_CORPUS_ID;
    let elder_treasury = treasury
        .and_then(|t| t.provisional_tiers.get(&4))
        .map(Vec::as_slice);

    // THE GOLDEN CHEST FROM 49 (#421). The store's Golden Chest is the APK's
    // `_rarity: 3`: a Gold chest, sold. From 49 it is bought for a level-100
    // buyer (#398), but its corpus's top band is 24-89, a mid-level population,
    // so a level-100 Golden chest paid what a level-30 one did: Middling and
    // Common soul gems, Iron to Elven gear, Ebony in 2% of slots, a median 6,700
    // gold. Retail's own Gold chests at 61-100 paid Grand, Glorious and
    // Transcendent soul gems, Ebony in two slots of three and about 8,800 gold,
    // and a treasury Gold chest here already pays them (#335, #421). So from 49
    // the Golden chest is that Gold chest, with fresh item ids per purchase and
    // the one jewellery rule (#393).
    if is_golden && buyer_level >= crate::features::loot_level::TOP_LOOT_FROM_LEVEL as u64 {
        if let Some(tables) = treasury.filter(|t| t.has_own_pool(3)) {
            let seed = mix(product_id.as_u128() as u64 ^ mix(nonce) ^ buyer_level.rotate_left(13));
            let key = format!("store-golden:{seed:016x}");
            let level = crate::features::loot_level::effective_loot_level_u64(buyer_level);
            let mut grant = crate::features::chests::roll_loot(tables, 3, level, &key, owned)?;
            for (ordinal, reward) in grant.items.iter_mut().enumerate() {
                reward.id = instance_uuid(seed, ordinal);
                super::legendary_gear::regrade_jewelry(
                    &mut reward.item,
                    mix(seed ^ 0x9E6C_63D0_676A_9A99 ^ ordinal as u64),
                );
            }
            return Some(grant);
        }
    }

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
    let base = if is_legendary {
        legendary_base(product, band, buyer_level, seed)?
    } else {
        draw(band, seed)?
    };
    let core = core_slots(band);

    let mut items: Vec<Item> = Vec::with_capacity(base.reward.items.len());
    for slot in 0..core {
        let slot_seed = mix(seed ^ 0xA076_1D64_78BD_642F_u64.wrapping_mul(slot as u64 + 1));
        // Legendary and Elder gear is picked at the tier retail paid at the
        // buyer's level (#339, #378), not anywhere in the band; the other chests
        // keep the band draw.
        let tiered = if is_legendary {
            legendary_slot_part(product, slot, buyer_level, slot_seed)
        } else if is_elder {
            elder_slot_part(product, slot, buyer_level, slot_seed)
        } else {
            None
        };
        let (shell, tier) = match tiered {
            Some((item, tier)) => (item.clone(), Some(tier)),
            None => (draw(band, slot_seed)?.reward.items[slot].clone(), None),
        };
        // The piece is then GENERATED around that retail piece, as retail
        // generated it (#368, #378; the Golden chest since the #392 audit, see
        // [`GOLDEN_CHEST_CORPUS_ID`]); the other bundles keep the retail piece.
        let piece = if is_legendary || is_elder || is_golden {
            let gen_seed = mix(slot_seed ^ 0x4F1B_BCDC_BFA5_3E0B);
            let mut piece = match tier {
                Some(tier) => super::legendary_gear::generate_at(&shell, tier, gen_seed),
                None => super::legendary_gear::generate(&shell, gen_seed),
            }
            .unwrap_or(shell);
            // The Golden chest's jewellery is then regraded by the one jewellery
            // rule, 8 / 17 / 75% (HauDrauf, #393: "the same split as everything
            // else"); Elder and Legendary keep the shell's grade. Its own seed, so
            // every other draw, and every non-jewel, is as it was.
            if is_golden {
                super::legendary_gear::regrade_jewelry(
                    &mut piece,
                    mix(slot_seed ^ 0x9E6C_63D0_676A_9A99),
                );
            }
            piece
        } else {
            shell
        };
        items.push(piece);
    }
    // The artifact slot (#378): at retail's rate, an artifact by the APK weights
    // among those whose level window holds the buyer's level, and for one the buyer holds the
    // generated "Legendary - High (Chests)" piece (#310).
    let artifact_slot = if is_legendary {
        Some(LEGENDARY_ARTIFACT_SLOT)
    } else if is_elder {
        Some(ELDER_ARTIFACT_SLOT)
    } else {
        None
    };
    match artifact_slot {
        Some((hits, chests)) => {
            if mix(seed ^ 0x6A09_E667_F3BC_C909) % chests < hits {
                items.extend(artifact_slot_item(buyer_level, seed, owned));
            }
        }
        None => items.extend(base.reward.items.iter().skip(core).cloned()),
    }

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
    // A high-level Elder chest pays retail's high-level Elder gold and
    // stackables: one treasury opening for the currencies, another for the
    // stackables, as for Gold chests (#335).
    if is_elder && buyer_level >= ELDER_HIGH_LEVEL {
        if let Some(pool) = elder_treasury.filter(|p| !p.is_empty()) {
            let near = crate::features::chests::neighbourhood(pool, buyer_level);
            if !near.is_empty() {
                let pick = |k: u64| near[(mix(seed ^ k) % near.len() as u64) as usize];
                grant.currencies = pick(0x1F83_D9AB_FB41_BD6B).reward.currencies.clone();
                grant.stackable_items = pick(0x5BE0_CD19_137E_2179).reward.stackable_items.clone();
            }
        }
    }
    if is_legendary || is_elder {
        apply_soul_gem_ladder(
            &mut grant.stackable_items,
            buyer_level,
            mix(seed ^ 0x3C6E_F372_FE94_F82B),
        );
    }
    if is_golden || is_legendary || is_elder {
        // A store chest's crafting materials come in 5, 10 or 20 (owner-approved
        // deviation from retail, #388, 2026-10-16), as a treasury chest's.
        crate::features::beginner_economy::size_material_stacks(
            &mut grant.stackable_items,
            mix(seed ^ 0x388_5743_C4E5),
        );
    }
    if is_golden {
        // The three best soul gems at 45-48 (owner-approved deviation from retail,
        // #388, 2026-10-16); Legendary and Elder get them from the ladder above.
        crate::features::beginner_economy::top_soul_gems(
            &mut grant.stackable_items,
            i64::try_from(buyer_level).unwrap_or(i64::MAX),
            mix(seed ^ 0x388_5047_6E35),
        );
    }
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

/// The corpus the Epic Chest, i.e. the Elder chest, is mined under.
const ELDER_CHEST_CORPUS_ID: Uuid = Uuid::from_u128(0x9e4dc391_422e_4b25_ba90_faab39e0769f);

/// The corpus the Golden (Rare, 250 gems) chest is mined under.
///
/// THE AUDIT (#392). The Golden chest's gear was still the retail piece verbatim,
/// drawn from its buyer band: 500 purchases at each of ten levels paid 552
/// distinct pieces, the enchantments and a ring's bonuses frozen as recorded, and
/// eight of the 34 ring GRADING properties and two of the 15 necklace ones could
/// never appear. The corpus is the same shape as the Legendary and Elder ones that
/// are already generated: all 710 of its 355 purchases' pieces are APK loot-pool
/// members of their tier, and retail repeated a piece at most four times. So its
/// pieces are generated around the retail piece the same way (tier, tempering,
/// enchant count and tier kept; kind, template, enchantments and GRADING from the
/// APK tables). Unlike Elder and Legendary, a jewel's grade is then drawn by the
/// one jewellery rule, 8 / 17 / 75% one / two / three properties (HauDrauf, #393;
/// the shell's grade gave 16 / 20 / 64). The retail piece itself stays reachable.
const GOLDEN_CHEST_CORPUS_ID: Uuid = Uuid::from_u128(0x11b322d2_1b5e_4e90_bb41_a8d90ca9548b);

/// The ten soul gems, `SoulGem1` to `SoulGem10`. English / German client names
/// (APK `localization` bundle, `Items.Name.SoulGemN`):
/// Petty / Winziger, Lesser / Niederer, Middling / Mittelgroßer, Common /
/// Gewöhnlicher, Exceptional / Außergewöhnlicher, Greater / Größerer, Elevated /
/// Erhabener, Grand / Mächtiger, Glorious / Herrlicher, Transcendent /
/// Transzendenter Seelenstein.
pub(crate) const SOUL_GEMS: [Uuid; 10] = [
    Uuid::from_u128(0x19ce1a65_057f_4f34_a0ed_27de7c085662), // Petty
    Uuid::from_u128(0x790a188b_3fa0_4f38_99d9_bc8d3675bc46), // Lesser
    Uuid::from_u128(0xeca5bd64_5e5d_4d0d_bfa3_b6fd427be029), // Middling
    Uuid::from_u128(0x1ba210b4_8cca_4f2f_b942_8fab80a52fd8), // Common
    Uuid::from_u128(0x3932e499_441e_4c6d_b671_9a03131ebe6f), // Exceptional
    Uuid::from_u128(0xa1d41da0_51e0_4a80_ba9a_b8e9046be27e), // Greater
    Uuid::from_u128(0xa3351353_f613_4368_bac7_05783f857b07), // Elevated
    Uuid::from_u128(0x68d7941e_8c8d_47bf_9f66_becb058f1817), // Grand
    Uuid::from_u128(0xbafe6ed5_6473_4a4c_aef5_421d3af5c8cb), // Glorious
    Uuid::from_u128(0xd94bab85_53d5_4c9c_a637_acd94fc66c98), // Transcendent
];

/// The soul gems a Legendary or Elder chest pays at a buyer level: from each
/// first level, the kinds (index into [`SOUL_GEMS`]) and their relative weight.
///
/// THE REPORT (#368). Sephoris at 43: "I find every kind except Grand in the
/// chests. At my level the highest should be Glorious; at 47 and 48 Grand joins,
/// only on those two levels, together with Glorious; from 49 Glorious and
/// Transcendent, and never Transcendent earlier. The same for Elder chests, in
/// smaller amounts." He was right about the bug: a level-43 buyer drew from the
/// 36-50 band, where Exceptional, Greater, Elevated, Glorious and Transcendent all
/// occur, each from a different population of buyers.
///
/// THE LADDER. 13-38 is measured: the 594 retail Legendary and Elder purchases in the
/// 2026-06-07 capture snapshot at their exact buyer level (about ten per level at
/// 3-31; the two chests climb the same ladder), extended by the corpus bands 29-35
/// and 36-50, whose chests the top-slot tier dates. Retail pays one or two
/// neighbouring kinds per level, the older one about twice as often, and none
/// below 13 (0 of 207 chests at 1-12). 49+ is measured too: the 54-89 band and
/// the 36-50 band's Daedric+Dragon chests pay only Glorious (96 of 161) and
/// Transcendent (41), never Grand. 39-48 has no retail buyers; there it is
/// Sephoris's description: Greater and Elevated continue the measured ladder at
/// 39-42 (the one retail chest dated there, Ebony+Daedric, paid Greater),
/// Glorious is the highest at 43-46, Grand joins Glorious at 47-48, and
/// Transcendent first appears at 49.
const CHEST_SOUL_GEM_LADDER: &[(u64, &[(usize, u64)])] = &[
    (1, &[]),
    (13, &[(0, 1)]),                  // Petty: 26 of 26
    (15, &[(1, 1)]),                  // Lesser 39, Petty 4
    (18, &[(1, 27), (2, 13)]),        // Lesser 27, Middling 13
    (20, &[(2, 1)]),                  // Middling: 28 of 28
    (22, &[(2, 35), (3, 11)]),        // Middling 35, Common 11
    (26, &[(2, 3), (3, 19)]),         // Middling 3, Common 19
    (27, &[(3, 61), (4, 22)]),        // Common 61, Exceptional 22
    (33, &[(4, 17), (5, 8)]),         // band 29-35, Ebony top slot
    (36, &[(4, 12), (5, 6), (6, 4)]), // band 36-50, Glass+Ebony chests
    (39, &[(5, 1), (6, 1)]),          // Sephoris; one retail chest: Greater
    (43, &[(6, 2), (8, 1)]),          // Sephoris: Glorious the highest
    // Owner-approved deviation from retail, #388, 2026-10-16: the three best from
    // 45 (beginner_economy::TOP_SOUL_GEM_WEIGHTS). Sephoris had Elevated + Glorious
    // at 45-46 and Grand + Glorious at 47-48.
    (45, &[(7, 1), (8, 2), (9, 1)]),
    (49, &[(8, 96), (9, 41)]), // 54-89 band: Glorious 96, Transcendent 41
];

/// The ladder step for `level`.
fn soul_gem_window(level: u64) -> &'static [(usize, u64)] {
    CHEST_SOUL_GEM_LADDER
        .iter()
        .rev()
        .find(|(from, _)| level >= *from)
        .map_or(&[], |(_, kinds)| *kinds)
}

/// Re-key the soul gems of a retail reward to the kinds paid at `level`.
///
/// The reward keeps retail's own shape: whether it pays soul gems at all, how
/// many stacks and how many gems in each (Elder chests pay 4-23, Legendary
/// 15-82, which is Sephoris's "smaller amounts"). Only the kinds are drawn, by
/// [`CHEST_SOUL_GEM_LADDER`]'s weights and without repeats; the lower kind goes
/// to the stack retail paid as the lower one. Below 13 retail paid none, so a
/// stray stack from a band reaching past 12 is dropped.
fn apply_soul_gem_ladder(
    stackables: &mut std::collections::HashMap<Uuid, u64>,
    level: u64,
    seed: u64,
) {
    let mut stacks: Vec<(usize, u64)> = SOUL_GEMS
        .iter()
        .enumerate()
        .filter_map(|(k, id)| stackables.remove(id).map(|n| (k, n)))
        .collect();
    if stacks.is_empty() {
        return;
    }
    stacks.sort_unstable();
    let mut window: Vec<(usize, u64)> = soul_gem_window(level).to_vec();
    if window.is_empty() {
        return;
    }
    let mut kinds: Vec<usize> = Vec::new();
    for draw_no in 0..stacks.len().min(window.len()) {
        let total: u64 = window.iter().map(|(_, w)| w).sum();
        let mut at = mix(seed ^ (draw_no as u64 + 1).rotate_left(29)) % total;
        let i = window
            .iter()
            .position(|(_, w)| {
                if at < *w {
                    true
                } else {
                    at -= w;
                    false
                }
            })
            .unwrap_or(0);
        kinds.push(window.remove(i).0);
    }
    kinds.sort_unstable();
    for (i, (_, n)) in stacks.into_iter().enumerate() {
        // More stacks than kinds at this level: the extra gems join the last kind.
        let kind = kinds[i.min(kinds.len() - 1)];
        *stackables.entry(SOUL_GEMS[kind]).or_insert(0) += n;
    }
}

/// Roll a treasury chest through the retail store-chest corpus when the treasury
/// table is too thin to model the tier. Tier 5 (Legendary) has one captured
/// treasury open but 4,697 retail Legendary chest purchases with the same reward
/// shape; tier 4 (Elder) has eleven opens, at levels 54-94 only, against 405 Epic
/// Chest purchases — the product the APK catalogue sells as `_rarity: 4`.
///
/// `treasury` holds retail's own tier-4 treasury openings (54-94), which pay
/// a high-level Elder chest's gold and stackables ([`ELDER_HIGH_LEVEL`], #378).
pub fn roll_treasury_chest(
    tier: u64,
    chest_level: u64,
    nonce: u64,
    owned: &HashSet<Uuid>,
    treasury: Option<&crate::features::chests::ChestLootTables>,
) -> Option<RewardGrant> {
    let product = match tier {
        5 => LEGENDARY_CHEST_PRODUCT_ID,
        4 => EPIC_CHEST_PRODUCT_ID,
        _ => return None,
    };
    roll_bundle_with(&product, chest_level, nonce, owned, treasury)
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
        *g.currencies
            .get(&GOLD.parse::<Uuid>().unwrap())
            .unwrap_or(&0)
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
        assert_eq!(
            corpus().products.len(),
            4,
            "four randomised bundles were mined"
        );
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
            (
                "7bf00a9c-6a08-4b55-a60d-53915baa38a3",
                "9e4dc391-422e-4b25-ba90-faab39e0769f",
                405,
            ),
            (
                "0e224ca0-1506-490f-884a-8871ffe6399b",
                "11b322d2-1b5e-4e90-bb41-a8d90ca9548b",
                355,
            ),
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
                    !g.currencies.is_empty()
                        || !g.stackable_items.is_empty()
                        || !g.items.is_empty(),
                    "{base} rolled an empty chest"
                );
                seen.insert((
                    gold_of(&g),
                    g.stackable_items.values().sum::<u64>(),
                    g.items.len(),
                ));
            }
            assert!(
                seen.len() > 5,
                "{base}: 40 purchases gave {} distinct rewards",
                seen.len()
            );
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
        assert!(
            high > 45_000,
            "a level-70 buyer received only {high}; retail paid about 60,000"
        );
        assert!(
            high > low * 3,
            "the payout barely moved with level: {low} -> {high}"
        );
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
        assert!(
            roll_bundle(&big(), 0, 5).is_some(),
            "level 0 must still roll"
        );
        assert!(
            roll_bundle(&big(), 500, 5).is_some(),
            "level 500 must still roll"
        );

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
        assert!(
            below < above,
            "clamping must still respect the ends of the ladder"
        );
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
            assert_eq!(
                distinct.len(),
                artifacts.len(),
                "level {level}: {artifacts:?}"
            );
        }
    }

    /// The replacement is retail's: the chest keeps its shape and its other
    /// contents, and the artifact slot pays a non-artifact Legendary piece.
    /// Retail player 78f2b668 shows it seven times — e.g. holding Pandemonium at
    /// level 18, the slot paid an Orcish Scaled Shield. Since #378 the piece is
    /// generated at the tier Legendary slot 0 pays at the level, as all seven
    /// of retail's were (at 15: Silver, Orcish one roll in seven).
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

        assert_eq!(
            held.items.len(),
            fresh.items.len(),
            "the slot is replaced, not dropped"
        );
        assert_eq!(
            held.currencies, fresh.currencies,
            "only the artifact slot changes"
        );
        assert_eq!(held.stackable_items, fresh.stackable_items);
        for (a, b) in fresh.items.iter().zip(&held.items) {
            if a.item.item_template_id == PANDEMONIUM {
                let t = b.item.item_template_id;
                assert!(!is_artifact(&t), "replaced with another artifact {t}");
                assert!(
                    matches!(required_level(&t), Some(8 | 13)),
                    "{t} is not slot 0's Silver or Orcish"
                );
                assert_eq!(legendary_gear::rule_broken(&b.item), None);
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
            let golds: Vec<u64> = band
                .results
                .iter()
                .map(|r| r.reward.currencies[&gold_id()])
                .collect();
            let (lo, hi) = (*golds.iter().min().unwrap(), *golds.iter().max().unwrap());
            for nonce in 0..200u64 {
                let g = roll_bundle(&legendary(), level, nonce).unwrap();
                let gold = gold_of(&g);
                assert!(
                    (lo..=hi).contains(&gold),
                    "level {level}: gold {gold} not in {lo}..={hi}"
                );
                assert!(
                    (2..=3).contains(&g.items.len()),
                    "level {level}: {} items",
                    g.items.len()
                );
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
            let g = roll_treasury_chest(4, 60, nonce, &HashSet::new(), None)
                .expect("a tier-4 chest must roll from the Epic corpus");
            assert!(g.chests.is_empty());
            seen.insert(fingerprint(&g));
        }
        assert!(
            seen.len() >= 38,
            "40 Elder chests gave {} distinct",
            seen.len()
        );
        assert!(
            roll_treasury_chest(3, 60, 1, &HashSet::new(), None).is_none(),
            "tier 3 keeps its table"
        );
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
            assert!(
                is_artifact(&t),
                "{t} is paid bare (no enchant/temper/grade) but not listed"
            );
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
            assert!(
                required_level(&item.item_template_id).is_some(),
                "{}",
                item.item_template_id
            );
        }
        let ladder = tier_ladder(corpus_product(&legendary()).unwrap());
        assert_eq!(
            ladder,
            vec![1, 8, 13, 18, 23, 28, 33, 39, 45],
            "the APK material ladder"
        );
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
        assert!(
            daedric > 0.8,
            "level 74 lower slot: Daedric {:.0}%",
            daedric * 100.0
        );
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
            let shares: Vec<String> = counts
                .iter()
                .map(|c| format!("{:3.0}", *c as f64 / 100.0))
                .collect();
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

    /// The tiers at 36-50 follow retail's pairs where retail was measured:
    /// Glass+Ebony at 36-38 (18 of 19 dated chests) and Daedric+Dragon from 45
    /// (all 161 at 54-89 in slot 1, 150 Daedric in slot 0). Between them, where
    /// retail had no buyers, slot 1 pays the tier unlocked at the buyer's level as
    /// at every measured level: Ebony+Daedric at 39-41 (the one dated chest
    /// there), Daedric+Daedric at 42-44 (#368 cont.).
    #[test]
    fn legendary_slots_at_36_to_50_pair_as_retail_did() {
        let (glass, _) = slot_share(37, 0, 28, 1_000);
        let (ebony, _) = slot_share(37, 1, 33, 1_000);
        assert!(
            glass > 0.7 && ebony > 0.7,
            "level 37: Glass {glass:.2}, Ebony {ebony:.2}"
        );
        // (level, slot-0 tier, slot-1 tier)
        for (level, t0, t1) in [
            (39u64, 33u64, 39u64),
            (40, 33, 39),
            (42, 39, 39),
            (45, 39, 45),
            (50, 39, 45),
        ] {
            let (lower, _) = slot_share(level, 0, t0, 1_000);
            let (upper, _) = slot_share(level, 1, t1, 1_000);
            assert!(
                lower > 0.8,
                "level {level}: tier {t0} in {lower:.2} of lower slots"
            );
            assert!(
                upper > 0.99,
                "level {level}: tier {t1} in {upper:.2} of upper slots"
            );
        }
    }

    /// Share of Dragon pieces among a Legendary chest's regular slots at `level`.
    fn dragon_share(level: u64, rolls: u64) -> f64 {
        let mut dragon = 0;
        for nonce in 0..rolls {
            let g = roll_bundle(&legendary(), level, nonce).unwrap();
            dragon += g.items[..2]
                .iter()
                .filter(|i| required_level(&i.item.item_template_id) == Some(45))
                .count();
        }
        dragon as f64 / (2 * rolls) as f64
    }

    /// THE REPORT (#368 cont.). Sephoris at 43: "too many Dragon items. They
    /// should be there, but not the majority; it has to ramp up slowly." The tier
    /// rule sent every slot 1 at 39-44 to Dragon (56% of regular-slot gear at 40
    /// and 43). Dragon unlocks at 45: none at 39-42, the lookahead's one roll in
    /// five at 43-44, then retail's Daedric+Dragon from 45.
    #[test]
    fn dragon_gear_ramps_up_gradually() {
        let mut previous = 0.0;
        for level in 36u64..=60 {
            let share = dragon_share(level, 1_000);
            match level {
                ..=42 => assert!(
                    share < 0.02,
                    "level {level}: Dragon in {:.0}% of gear",
                    share * 100.0
                ),
                43..=44 => assert!(
                    (0.05..0.2).contains(&share),
                    "level {level}: Dragon in {:.0}% of gear, want a few, not the majority",
                    share * 100.0
                ),
                // Slot 0's one roll in seven pays slot 1's tier (retail: Dragon in
                // 15 of 161 lower slots at 54-89).
                _ => assert!(
                    (0.45..=0.62).contains(&share),
                    "level {level}: Dragon in {:.0}% of gear; retail paid it in every top slot",
                    share * 100.0
                ),
            }
            assert!(
                share + 0.03 >= previous,
                "Dragon fell from {previous:.2} to {share:.2} at {level}"
            );
            previous = share;
        }
    }

    // ---- #368 (cont.): soul gems by level ----

    /// Retail's soul gems at the buyer's EXACT level: every Legendary (`true`) and
    /// Elder (`false`) chest purchase in the 2026-06-07 capture snapshot at 3-31
    /// (buyer level from the nearest `/characters` observation). (legendary,
    /// level, purchases, chests paying each of [`SOUL_GEMS`]).
    const RETAIL_SOUL_GEMS: [(bool, u64, u64, [u64; 10]); 57] = [
        (true, 3, 22, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 4, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 5, 12, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 6, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 7, 11, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 8, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 9, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 10, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 11, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 12, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 13, 10, [6, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 14, 10, [9, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 15, 10, [0, 6, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 16, 10, [0, 8, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 17, 10, [0, 8, 0, 0, 0, 0, 0, 0, 0, 0]),
        (true, 18, 10, [0, 8, 4, 0, 0, 0, 0, 0, 0, 0]),
        (true, 19, 10, [0, 7, 3, 0, 0, 0, 0, 0, 0, 0]),
        (true, 20, 10, [0, 0, 7, 0, 0, 0, 0, 0, 0, 0]),
        (true, 21, 10, [0, 0, 7, 0, 0, 0, 0, 0, 0, 0]),
        (true, 22, 10, [0, 0, 4, 2, 0, 0, 0, 0, 0, 0]),
        (true, 23, 10, [0, 0, 6, 1, 0, 0, 0, 0, 0, 0]),
        (true, 24, 10, [0, 0, 8, 3, 0, 0, 0, 0, 0, 0]),
        (true, 25, 10, [0, 0, 4, 3, 0, 0, 0, 0, 0, 0]),
        (true, 26, 10, [0, 0, 0, 8, 0, 0, 0, 0, 0, 0]),
        (true, 27, 10, [0, 0, 0, 6, 3, 0, 0, 0, 0, 0]),
        (true, 28, 10, [0, 0, 0, 7, 1, 0, 0, 0, 0, 0]),
        (true, 29, 10, [0, 0, 0, 5, 4, 0, 0, 0, 0, 0]),
        (true, 30, 10, [0, 0, 0, 4, 2, 0, 0, 0, 0, 0]),
        (true, 31, 10, [0, 0, 0, 6, 4, 0, 0, 0, 0, 0]),
        (false, 3, 8, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 4, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 5, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 6, 9, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 7, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 8, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 9, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 10, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 11, 4, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 12, 10, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 13, 10, [4, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 14, 10, [7, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 15, 16, [4, 6, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 16, 10, [0, 7, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 17, 10, [0, 4, 0, 0, 0, 0, 0, 0, 0, 0]),
        (false, 18, 10, [0, 7, 1, 0, 0, 0, 0, 0, 0, 0]),
        (false, 19, 10, [0, 5, 5, 0, 0, 0, 0, 0, 0, 0]),
        (false, 20, 10, [0, 0, 7, 0, 0, 0, 0, 0, 0, 0]),
        (false, 21, 10, [0, 0, 7, 0, 0, 0, 0, 0, 0, 0]),
        (false, 22, 10, [0, 0, 3, 2, 0, 0, 0, 0, 0, 0]),
        (false, 23, 10, [0, 0, 6, 0, 0, 0, 0, 0, 0, 0]),
        (false, 24, 10, [0, 0, 4, 0, 0, 0, 0, 0, 0, 0]),
        (false, 26, 20, [0, 0, 3, 11, 0, 0, 0, 0, 0, 0]),
        (false, 27, 10, [0, 0, 0, 5, 2, 0, 0, 0, 0, 0]),
        (false, 28, 10, [0, 0, 0, 7, 1, 0, 0, 0, 0, 0]),
        (false, 29, 10, [0, 0, 0, 8, 1, 0, 0, 0, 0, 0]),
        (false, 30, 10, [0, 0, 0, 5, 0, 0, 0, 0, 0, 0]),
        (false, 31, 10, [0, 0, 0, 8, 4, 0, 0, 0, 0, 0]),
    ];

    /// Share of `rolls` chests of `product` at `level` paying each soul gem.
    fn soul_gem_shares(product: &Uuid, level: u64, rolls: u64) -> [f64; 10] {
        let mut counts = [0u64; 10];
        for nonce in 0..rolls {
            let g = roll_bundle(product, level, nonce).unwrap();
            for (k, id) in SOUL_GEMS.iter().enumerate() {
                counts[k] += u64::from(g.stackable_items.contains_key(id));
            }
        }
        counts.map(|c| c as f64 / rolls as f64)
    }

    /// The ladder against retail at every measured level, both chests: no soul gem
    /// below 13, no kind retail did not pay within two levels, and the
    /// kind retail paid most is the one we pay most. The band draw failed all
    /// three: Petty at 9-12 (0 of 82 retail chests), Middling as the main kind at
    /// 18-19 (retail: Lesser 27, Middling 13).
    #[test]
    fn soul_gems_follow_retails_ladder_at_the_exact_level() {
        for (is_legendary, level, _, retail) in RETAIL_SOUL_GEMS {
            let product = if is_legendary {
                legendary()
            } else {
                EPIC_CHEST_PRODUCT_ID
            };
            let name = if is_legendary { "Legendary" } else { "Elder" };
            let ours = soul_gem_shares(&product, level, 600);
            let near: Vec<[u64; 10]> = RETAIL_SOUL_GEMS
                .iter()
                .filter(|(l, lv, _, _)| *l == is_legendary && lv.abs_diff(level) <= 2)
                .map(|(_, _, _, c)| *c)
                .collect();
            for k in 0..10 {
                let paid_near = near.iter().any(|c| c[k] > 0);
                assert!(
                    paid_near || ours[k] < 0.04,
                    "{name} level {level}: {} in {:.0}% of chests; retail paid none at {}-{}",
                    SOUL_GEM_NAMES[k],
                    ours[k] * 100.0,
                    level - 2,
                    level + 2
                );
            }
            let retail_top = *retail.iter().max().unwrap();
            if retail_top == 0 {
                assert!(
                    ours.iter().all(|&s| s == 0.0),
                    "{name} level {level}: soul gems retail never paid"
                );
                continue;
            }
            let ours_top = (0..10)
                .max_by(|&a, &b| ours[a].total_cmp(&ours[b]))
                .unwrap();
            assert!(
                retail[ours_top] == retail_top,
                "{name} level {level}: we pay mostly {}, retail {:?}",
                SOUL_GEM_NAMES[ours_top],
                retail
            );
        }
    }

    /// THE REPORT (#368). Sephoris at 43: every kind but Grand turned up; at his
    /// level Glorious should be the highest; Grand only at 47 and 48, with
    /// Glorious; Glorious and Transcendent from 49, Transcendent never earlier;
    /// the same in Elder chests. On the band draw a level-43 buyer got
    /// Exceptional, Greater, Elevated, Glorious and Transcendent (22% of chests),
    /// and Grand never.
    #[test]
    fn soul_gems_at_39_and_up_follow_sephoris_and_the_54_89_band() {
        // Index into SOUL_GEMS: 5 Greater, 6 Elevated, 7 Grand, 8 Glorious, 9 Transcendent.
        for product in [legendary(), EPIC_CHEST_PRODUCT_ID] {
            for level in 36u64..=100 {
                let ours = soul_gem_shares(&product, level, 400);
                let paid: Vec<usize> = (0..10).filter(|&k| ours[k] > 0.0).collect();
                let allowed: &[usize] = match level {
                    36..=38 => &[4, 5, 6],
                    39..=42 => &[5, 6],
                    43..=44 => &[6, 8],
                    // The three best from 45 (owner-approved deviation, #388).
                    45..=48 => &[7, 8, 9],
                    _ => &[8, 9],
                };
                assert!(
                    paid.iter().all(|k| allowed.contains(k)),
                    "{product} level {level}: paid {:?}",
                    paid.iter().map(|&k| SOUL_GEM_NAMES[k]).collect::<Vec<_>>()
                );
                assert!(
                    allowed.iter().all(|&k| ours[k] > 0.05),
                    "{product} level {level}: {:?} must all come up, got {ours:?}",
                    allowed
                        .iter()
                        .map(|&k| SOUL_GEM_NAMES[k])
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    /// "In smaller amounts": an Elder chest's soul-gem stack is retail's Elder
    /// stack (4-23 gems), a Legendary chest's retail's Legendary stack (15-82).
    #[test]
    fn elder_chests_pay_fewer_soul_gems_than_legendary_chests() {
        let median = |product: &Uuid, level: u64| {
            let mut v: Vec<u64> = (0..400u64)
                .filter_map(|n| {
                    let g = roll_bundle(product, level, n).unwrap();
                    let sum: u64 = SOUL_GEMS
                        .iter()
                        .filter_map(|id| g.stackable_items.get(id))
                        .sum();
                    (sum > 0).then_some(sum)
                })
                .collect();
            v.sort_unstable();
            v[v.len() / 2]
        };
        for level in [20u64, 43, 47, 49, 80] {
            let legendary_gems = median(&legendary(), level);
            let elder_gems = median(&EPIC_CHEST_PRODUCT_ID, level);
            assert!(
                elder_gems * 2 < legendary_gems,
                "level {level}: Elder {elder_gems} soul gems against Legendary {legendary_gems}"
            );
        }
    }

    /// A Legendary chest's stackables come from a retail chest of the same
    /// population as its gear (#368 cont.): Daedra hearts and dragon bone are what
    /// retail's Daedric+Dragon chests paid (54-89 band, and 43 of the 36-50 band),
    /// never its Glass+Ebony ones. On the band draw a level-37 buyer got them in
    /// two chests of three.
    #[test]
    fn legendary_stackables_match_the_chests_gear_population() {
        let daedra_heart: Uuid = "f9181a67-b094-4c37-a145-ced9dfe610d6".parse().unwrap();
        let share = |level: u64| {
            (0..600u64)
                .filter(|&n| {
                    let g = roll_bundle(&legendary(), level, n).unwrap();
                    g.stackable_items.contains_key(&daedra_heart)
                })
                .count() as f64
                / 600.0
        };
        for level in [36u64, 37, 38, 40, 43, 44] {
            let s = share(level);
            assert!(
                s == 0.0,
                "level {level}: Daedra hearts in {:.0}% of chests",
                s * 100.0
            );
        }
        for level in [45u64, 49, 60, 89] {
            let s = share(level);
            assert!(
                s > 0.3,
                "level {level}: Daedra hearts in only {:.0}%",
                s * 100.0
            );
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
        let mut e: Vec<String> = item
            .properties
            .enchanting
            .iter()
            .map(|p| format!("{}@{}", p.id, p.tier))
            .collect();
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
            .flat_map(|(b, band)| {
                band.results
                    .iter()
                    .map(move |r| (b, r.reward.items.clone()))
            })
            .collect();
        for (name, set) in [("retail", &retail), ("ours", &ours)] {
            for slot in 0..2 {
                let (rep, n) = banded_repeats(slot, set);
                let mut by_tier: std::collections::BTreeMap<u64, HashSet<Uuid>> =
                    Default::default();
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
                let tiers: Vec<String> = by_tier
                    .iter()
                    .map(|(t, s)| format!("{t}:{}", s.len()))
                    .collect();
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
            .flat_map(|(b, band)| {
                band.results
                    .iter()
                    .map(move |r| (b, r.reward.items.clone()))
            })
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
                    let mut e: Vec<Uuid> = items[slot]
                        .properties
                        .enchanting
                        .iter()
                        .map(|p| p.id)
                        .collect();
                    e.sort();
                    e
                })
                .collect::<HashSet<_>>()
                .len()
        };
        let (retail, ours) = (retail_banded(), ours_banded());
        for slot in 0..2 {
            let (r, o) = (sets(&retail, slot), sets(&ours, slot));
            assert!(
                o * 10 >= r * 9,
                "slot {slot}: {o} distinct enchantment sets, retail {r}"
            );
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
    /// from the secondaries, and jewellery keeping its kind, grade and GRADING
    /// tiers (HauDrauf, #393: Elder and Legendary chest jewellery stays as retail
    /// paid it; the Golden chest regrades it afterwards, outside `generate`).
    #[test]
    fn every_retail_shell_generates_at_its_own_tier() {
        for (i, (_, items)) in retail_banded().iter().enumerate() {
            for shell in &items[..2] {
                for k in 0..4u64 {
                    let g = legendary_gear::generate(shell, mix(i as u64 ^ k.rotate_left(40)))
                        .expect("a retail shell must generate");
                    let (s, t) = (&shell.item_template_id, &g.item_template_id);
                    assert_eq!(
                        legendary_gear::tier_group_of(t),
                        legendary_gear::tier_group_of(s)
                    );
                    assert_eq!(g.tempering_level, shell.tempering_level);
                    let (ge, se) = (&g.properties.enchanting, &shell.properties.enchanting);
                    assert_eq!(ge.len(), se.len());
                    assert!(ge.iter().zip(se).all(|(a, b)| a.tier == b.tier));
                    if let Some(p) = ge.first() {
                        assert!(
                            ge[1..].iter().all(|e| e.id != p.id),
                            "secondary repeats primary"
                        );
                    }
                    let kind = legendary_gear::kind_of(s).unwrap();
                    if kind == "ring" || kind == "necklace" {
                        assert_eq!(legendary_gear::kind_of(t), Some(kind));
                        assert_eq!(g.grade, shell.grade);
                        // The grade's tiers stay; the properties are drawn (#378).
                        let tiers = |v: &[crate::user_data::ItemSingleProperty]| {
                            v.iter().map(|p| p.tier).collect::<Vec<_>>()
                        };
                        assert_eq!(
                            tiers(&g.properties.grading),
                            tiers(&shell.properties.grading)
                        );
                    }
                    if let Some(why) = legendary_gear::rule_broken(&g) {
                        panic!("{why}");
                    }
                }
            }
        }
    }

    /// The property-count split of a chest's jewellery, `[_, one, two, three]`,
    /// over at least 2000 jewels, each with its grade equal to its tiers' sum.
    fn chest_jewel_split(name: &str, product: &Uuid) -> [u64; 4] {
        let mut count = [0u64; 4];
        let mut nonce = 0u64;
        while count.iter().sum::<u64>() < 2500 && nonce < 100_000 {
            let level = [10u64, 25, 40, 49, 60, 80, 100][(nonce % 7) as usize];
            let g = roll_bundle(product, level, nonce).expect("the chest rolls");
            for item in g.items.iter().map(|r| &r.item) {
                if matches!(
                    legendary_gear::kind_of(&item.item_template_id),
                    Some("ring" | "necklace")
                ) {
                    let gr = &item.properties.grading;
                    assert_eq!(item.grade, Some(gr.iter().map(|p| p.tier).sum()), "{name}");
                    count[gr.len()] += 1;
                }
            }
            nonce += 1;
        }
        let n = count.iter().sum::<u64>();
        assert!(n >= 2000, "{name}: only {n} jewels in {nonce} chests");
        eprintln!("#393 {name}: {count:?} of {n}");
        count
    }

    /// The largest gap, in percentage points, between `count` and 8 / 17 / 75.
    fn off_the_split(count: &[u64; 4]) -> f64 {
        use crate::features::jewelry_grades::JEWELRY_PROPERTY_COUNT_PERCENT;
        let n = count.iter().sum::<u64>() as f64;
        JEWELRY_PROPERTY_COUNT_PERCENT
            .iter()
            .map(|(props, pct)| (count[*props] as f64 * 100.0 / n - f64::from(*pct)).abs())
            .fold(0.0, f64::max)
    }

    /// #393 (HauDrauf, 2026-10-15): the 8 / 17 / 75% property-count split every
    /// other source follows is NOT applied to the Elder and Legendary chests.
    /// Their jewellery keeps the retail shell's grade (sampled about 7 / 1 / 92
    /// Legendary, 5 / 10 / 85 Elder), so each chest's split is far from
    /// 8 / 17 / 75. `every_retail_shell_generates_at_its_own_tier` checks the
    /// grade piece by piece.
    #[test]
    fn chest_jewellery_keeps_the_shell_grade() {
        for (name, product) in [("Legendary", legendary()), ("Elder", EPIC_CHEST_PRODUCT_ID)] {
            let count = chest_jewel_split(name, &product);
            let off = off_the_split(&count);
            assert!(
                off > 4.0,
                "{name}: {count:?} is the 8/17/75 split (within {off:.1} points); the shell grade is not kept"
            );
        }
    }

    /// #393 (HauDrauf, 2026-10-15): the Golden store chest's jewellery follows
    /// the 8 / 17 / 75% split like everything else. It kept the shell's grade
    /// before, about 16 / 20 / 64.
    #[test]
    fn golden_chest_jewellery_follows_the_split() {
        let golden = Uuid::from_u128(0x0e224ca0_1506_490f_884a_8871ffe6399b);
        let count = chest_jewel_split("Golden", &golden);
        let off = off_the_split(&count);
        assert!(
            off <= 3.0,
            "Golden: {count:?} is {off:.1} points off 8/17/75"
        );
    }

    /// The Golden regrade touches jewellery GRADING only: the same nonces pay the
    /// same non-jewel pieces, and the same jewel templates, tempering and
    /// enchantments, as the generated pieces did before it.
    #[test]
    fn golden_regrade_changes_only_jewel_grading() {
        let golden = Uuid::from_u128(0x0e224ca0_1506_490f_884a_8871ffe6399b);
        let corpus = corpus_product(&golden).unwrap();
        let mut jewels = 0;
        for level in [5u64, 30, 49, 80] {
            let band = corpus
                .by_level
                .iter()
                .find(|b| level >= b.min_buyer_level && level <= b.max_buyer_level)
                .or_else(|| {
                    corpus
                        .by_level
                        .iter()
                        .min_by_key(|b| level_distance(level, b.min_buyer_level, b.max_buyer_level))
                })
                .unwrap();
            for nonce in 0..300u64 {
                let g = roll_bundle(&golden, level, nonce).unwrap();
                // The pre-#393 piece, from the same seeds.
                let seed = mix(golden.as_u128() as u64 ^ mix(nonce) ^ level.rotate_left(13));
                for slot in 0..core_slots(band) {
                    let slot_seed =
                        mix(seed ^ 0xA076_1D64_78BD_642F_u64.wrapping_mul(slot as u64 + 1));
                    let shell = draw(band, slot_seed).unwrap().reward.items[slot].clone();
                    let gen_seed = mix(slot_seed ^ 0x4F1B_BCDC_BFA5_3E0B);
                    let old = legendary_gear::generate(&shell, gen_seed).unwrap_or(shell);
                    let new = &g.items[slot].item;
                    assert_eq!(new.item_template_id, old.item_template_id);
                    assert_eq!(new.tempering_level, old.tempering_level);
                    assert_eq!(new.properties.enchanting, old.properties.enchanting);
                    if matches!(
                        legendary_gear::kind_of(&old.item_template_id),
                        Some("ring" | "necklace")
                    ) {
                        jewels += 1;
                    } else {
                        assert_eq!(new, &old, "L{level} #{nonce}: a non-jewel changed");
                    }
                }
            }
        }
        assert!(jewels > 100, "only {jewels} jewels");
    }

    // ---- #368 (cont.): the progression tables for the PR ----

    const SOUL_GEM_NAMES: [&str; 10] = [
        "Pet", "Les", "Mid", "Com", "Exc", "Gre", "Ele", "Gra", "Glo", "Tra",
    ];

    /// Soul-gem kinds and gear tiers per buyer level, Legendary and Elder.
    /// `cargo test -p blades_lib print_chest_progression -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_chest_progression() {
        let ladder = [1u64, 8, 13, 18, 23, 28, 33, 39, 45];
        let tier_names = ["Fe", "Si", "Or", "Dw", "El", "Gl", "Eb", "Da", "Dr"];
        for (name, product) in [("Legendary", legendary()), ("Elder", EPIC_CHEST_PRODUCT_ID)] {
            println!("== {name}");
            for level in [20u64, 30, 37, 40, 43, 45, 47, 49, 60, 80, 100] {
                const N: u64 = 2_000;
                let mut gems = [0u64; 10];
                let mut tiers = [0u64; 9];
                let mut pieces = 0u64;
                let mut with_gems = 0u64;
                for nonce in 0..N {
                    let g = roll_bundle(&product, level, nonce).unwrap();
                    let mut any = false;
                    for (k, id) in SOUL_GEMS.iter().enumerate() {
                        if g.stackable_items.contains_key(id) {
                            gems[k] += 1;
                            any = true;
                        }
                    }
                    with_gems += u64::from(any);
                    for item in &g.items {
                        if let Some(r) = required_level(&item.item.item_template_id)
                            && let Some(i) = ladder.iter().position(|&x| x == r)
                        {
                            tiers[i] += 1;
                            pieces += 1;
                        }
                    }
                }
                let gem_txt: Vec<String> = gems
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c > 0)
                    .map(|(k, c)| {
                        format!("{} {:.0}%", SOUL_GEM_NAMES[k], *c as f64 * 100.0 / N as f64)
                    })
                    .collect();
                let tier_txt: Vec<String> = tiers
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c > 0)
                    .map(|(k, c)| {
                        format!(
                            "{} {:.0}%",
                            tier_names[k],
                            *c as f64 * 100.0 / pieces as f64
                        )
                    })
                    .collect();
                println!(
                    "L{level:3}: gems in {:.0}% | {} || gear {}",
                    with_gems as f64 * 100.0 / N as f64,
                    gem_txt.join(", "),
                    tier_txt.join(", ")
                );
            }
        }
    }

    // ---- #378: artifacts, the third slot, Elder chests by level ----------------

    /// The six artifacts Sephoris (f7817aa6, level 45) held on 2026-10-12.
    const SEPHORIS_ARTIFACTS: [u128; 6] = [
        0xdef810af_e9f5_4e23_9247_1edf391d82e1, // Ebony Mail
        0x9a22b59a_cdbc_4987_bf4a_0cc812428aeb, // Spellbreaker
        0x21ed2758_5cf5_4b7d_9047_59bbdbd61820, // Onyx Cleaver
        0xa0d7dd5e_692e_4fbb_bbf2_19c6a874f3f5, // Mehrunes' Razor
        0x86a4ad9d_e2e8_49cd_9642_b553475b6580, // Pandemonium
        0x12c4920e_a9e0_4498_b134_3ce66fb9558f, // Savior's Hide
    ];

    fn sephoris_owned() -> HashSet<Uuid> {
        SEPHORIS_ARTIFACTS
            .iter()
            .map(|&u| Uuid::from_u128(u))
            .collect()
    }

    /// (chests with a third item, artifacts paid, artifact templates seen).
    fn third_slots(
        product: &Uuid,
        level: u64,
        rolls: u64,
        owned: &HashSet<Uuid>,
    ) -> (u64, u64, HashSet<Uuid>) {
        let (mut third, mut arts, mut kinds) = (0, 0, HashSet::new());
        for nonce in 0..rolls {
            let g = roll_bundle_for(product, level, nonce.wrapping_mul(7919), owned).unwrap();
            third += u64::from(g.items.len() > 2);
            for i in &g.items {
                if is_artifact(&i.item.item_template_id) {
                    arts += 1;
                    kinds.insert(i.item.item_template_id);
                }
            }
        }
        (third, arts, kinds)
    }

    /// THE REPORT (#378). Sephoris at 45 opened ~200 Legendary chests after #500:
    /// "no artifact came, although many chests had three items". The slot copied
    /// one recorded chest of his band, and the 36-50 band's one artifact is Onyx
    /// Cleaver, which he holds: 0 artifacts in 6,000 chests. Retail paid the slot
    /// in 5.1% of chests (60 of 1,186) and an artifact by the APK weights among
    /// those whose window holds the level: at 45 tiers 7-10, weight 41, of which
    /// he holds Mehrunes' Razor, Spellbreaker and Onyx Cleaver (11), so 5.1% x
    /// 30/41 = 3.7% of his chests pay one, any of the other eight.
    #[test]
    fn sephoris_gets_artifacts_at_retails_rate() {
        let (third, arts, kinds) = third_slots(&legendary(), 45, 6_000, &sephoris_owned());
        let (third, arts) = (third as f64 / 6_000.0, arts as f64 / 6_000.0);
        assert!(
            (0.04..=0.062).contains(&third),
            "third slot in {:.1}% of chests",
            third * 100.0
        );
        assert!(
            (0.028..=0.046).contains(&arts),
            "artifacts in {:.2}% of chests",
            arts * 100.0
        );
        assert_eq!(kinds.len(), 8, "the eight unheld artifacts of tiers 7-10");
        assert!(
            kinds.is_disjoint(&sephoris_owned()),
            "a held artifact was paid again"
        );
    }

    /// Retail's artifact slot at every level: the slot in about 5.1% of Legendary
    /// chests, every artifact inside its tier's [`ARTIFACT_WINDOWS`], and every
    /// artifact whose window holds the level turning up in 4,000 chests.
    #[test]
    fn the_artifact_slot_follows_retail_at_every_level() {
        let none = HashSet::new();
        for level in [3u64, 10, 19, 26, 33, 45, 60, 90] {
            let (third, arts, seen) = third_slots(&legendary(), level, 4_000, &none);
            assert_eq!(
                third, arts,
                "level {level}: nothing held, every slot is an artifact"
            );
            let rate = third as f64 / 4_000.0;
            assert!(
                (0.04..=0.062).contains(&rate),
                "level {level}: slot in {:.1}%",
                rate * 100.0
            );
            for t in &seen {
                let req = ARTIFACT_LOOT.iter().find(|a| a.0 == *t).unwrap().1;
                assert!(
                    artifact_in_window(req, level),
                    "level {level}: artifact of level {req}"
                );
            }
            let reachable = ARTIFACT_LOOT
                .iter()
                .filter(|a| artifact_in_window(a.1, level))
                .count();
            assert_eq!(seen.len(), reachable, "level {level}");
        }
    }

    /// The APK lootWeight decides the kind: the corpus's 27 artifacts are 13
    /// weapons (lootWeight 2, 17 artifacts), 11 armour pieces or rings (9, 5) and
    /// 3 shields (7, 2) — the weights give 10 / 13 / 4, a uniform pick 19 / 6 / 2.
    #[test]
    fn artifacts_follow_the_apk_loot_weights() {
        let mut by_weight: std::collections::HashMap<u64, u64> = Default::default();
        for nonce in 0..400_000u64 {
            if let Some((t, _)) = roll_artifact(90, mix(nonce)) {
                let w = ARTIFACT_LOOT.iter().find(|a| a.0 == t).unwrap().2;
                *by_weight.entry(w).or_default() += 1;
            }
        }
        let n: u64 = by_weight.values().sum();
        let share = |w: u64| by_weight.get(&w).copied().unwrap_or(0) as f64 / n as f64;
        // At 90 the windows hold tiers 8-10: Warlock's Ring and Lord's Mail (9
        // each), Spellbreaker (7), five weapons (2 each), 35 in all.
        assert!(
            (share(9) - 18.0 / 35.0).abs() < 0.01,
            "armour and rings {:.3}",
            share(9)
        );
        assert!(
            (share(7) - 7.0 / 35.0).abs() < 0.01,
            "shields {:.3}",
            share(7)
        );
        assert!(
            (share(2) - 10.0 / 35.0).abs() < 0.01,
            "weapons {:.3}",
            share(2)
        );
    }

    /// THE REPORT (#378): "the third item is always the same sword, ring or
    /// shield". With the rolled artifact held, the slot paid one of the four or
    /// five pieces retail recorded in that slot in the band, frozen: at 45 the
    /// Ebony Longsword, Stalhrim Shield, Ebony Atronite Ring or Dragonplate
    /// Helmet, each with the same enchantments every time (Sephoris holds five
    /// Stalhrim Shields and five identical Ebony Atronite Rings). It is now
    /// generated like Legendary slot 0 (Daedric at 45, Dragon one roll in seven).
    #[test]
    fn the_fallback_piece_is_generated_and_varied() {
        let all: HashSet<Uuid> = ARTIFACT_TEMPLATES.iter().copied().collect();
        let mut pieces: std::collections::HashMap<String, u64> = Default::default();
        let mut templates = HashSet::new();
        let mut n = 0;
        for nonce in 0..12_000u64 {
            let g = roll_bundle_for(&legendary(), 45, nonce, &all).unwrap();
            let Some(third) = g.items.get(2) else {
                continue;
            };
            let item = &third.item;
            assert!(!is_artifact(&item.item_template_id));
            assert_eq!(legendary_gear::rule_broken(item), None);
            let tier = required_level(&item.item_template_id);
            assert!(matches!(tier, Some(39 | 45)), "fallback at tier {tier:?}");
            templates.insert(item.item_template_id);
            *pieces
                .entry(serde_json::to_string(item).unwrap())
                .or_default() += 1;
            n += 1;
        }
        assert!(n > 400, "{n} fallbacks");
        assert!(
            templates.len() >= 25,
            "{} templates in {n} fallbacks",
            templates.len()
        );
        let most = pieces.values().max().unwrap();
        assert!(*most * 50 <= n, "one piece came {most} times in {n}");
    }

    /// THE REPORT (#378): "the jewellery is always the same". The generator
    /// copied a jewellery shell's GRADING (its ability bonus ranks), so a ring
    /// was one of the two or three recorded rings of the tier: Sephoris holds
    /// twelve Ebony Faerite Rings with Adrenaline Dodge 2, Fireball 2 and
    /// Indomitable Smash 1. Retail paid all 34 ring and 15 necklace abilities
    /// (`jewelry_grade`), and its 307 regular-slot jewellery pieces are 288
    /// distinct template-and-ability sets, none more than three times.
    #[test]
    fn generated_jewellery_rolls_its_abilities() {
        let mut sets: std::collections::HashMap<(Uuid, Vec<Uuid>), u64> = Default::default();
        let mut props: HashSet<Uuid> = HashSet::new();
        let mut n = 0;
        let levels = [8u64, 18, 23, 28, 39, 45, 60];
        for nonce in 0..40_000u64 {
            let level = levels[(nonce % levels.len() as u64) as usize];
            let g = roll_bundle(&legendary(), level, nonce).unwrap();
            for i in &g.items[..2] {
                let kind = legendary_gear::kind_of(&i.item.item_template_id);
                if !matches!(kind, Some("ring" | "necklace")) {
                    continue;
                }
                assert_eq!(legendary_gear::rule_broken(&i.item), None);
                let mut ids: Vec<Uuid> = i.item.properties.grading.iter().map(|p| p.id).collect();
                props.extend(ids.iter().copied());
                ids.sort();
                if n < 307 {
                    *sets.entry((i.item.item_template_id, ids)).or_default() += 1;
                }
                n += 1;
            }
        }
        assert_eq!(
            props.len(),
            34 + 15,
            "every ring and necklace ability must be reachable"
        );
        let most = sets.values().max().unwrap();
        assert!(
            sets.len() >= 260,
            "{} distinct in 307 pieces (retail 288)",
            sets.len()
        );
        assert!(
            *most <= 4,
            "one ring or necklace came {most} times in 307 (retail 3)"
        );
    }

    fn committed_chest_tables() -> crate::features::chests::ChestLootTables {
        serde_json::from_str(include_str!("../../../deploy/static/chest_loots.json")).unwrap()
    }

    /// The store's Golden Chest (Rare, 250 gems) as `purchase_global_shop` rolls
    /// it, with the shipped chest tables.
    fn store_golden(
        t: &crate::features::chests::ChestLootTables,
        level: u64,
        n: u64,
    ) -> RewardGrant {
        let golden = Uuid::from_u128(0x0e224ca0_1506_490f_884a_8871ffe6399b);
        roll_store_chest(&golden, level, n, &HashSet::new(), Some(t))
            .expect("the Golden chest rolls")
    }

    /// Report #421 (HauDrauf, 2026-10-24): "Goldtruhen auf Stufe 100 aus dem
    /// Shop geöffnet, nur die kleinsten Seelensteine, Waffen und Tränke".
    ///
    /// THE BUG. The store's Golden Chest is the APK's `_rarity: 3`, the Gold
    /// chest. From 49 the store rolls it for a level-100 buyer (#398), but its
    /// corpus's top band is 24-89, a mid-level population, so a level-100 Golden
    /// chest paid exactly what a level-30 one did: Middling and Common soul gems
    /// (the #388 three-best rule only covered 45-48, and 49+ was assumed to be
    /// level-100 loot already), Iron to Elven gear, Ebony in 2% of slots.
    /// Retail's own Gold chests at 61-100 paid Grand, Glorious and Transcendent
    /// and Ebony in two slots of three; a treasury Gold chest here already does.
    #[test]
    fn a_store_golden_chest_from_49_pays_a_level_100_gold_chest() {
        use super::super::legendary_gear::required_level;
        let t = committed_chest_tables();
        const ROLLS: u64 = 2000;
        for level in [49u64, 60, 100] {
            let (mut with_gems, mut kinds, mut ebony, mut items) = (0u64, [0u64; 10], 0u64, 0u64);
            for n in 0..ROLLS {
                let r = store_golden(&t, level, n);
                let mut any = false;
                for (k, id) in SOUL_GEMS.iter().enumerate() {
                    if r.stackable_items.contains_key(id) {
                        kinds[k] += 1;
                        any = true;
                    }
                }
                with_gems += u64::from(any);
                for i in &r.items {
                    items += 1;
                    let tier = required_level(&i.item.item_template_id);
                    // Retail's Gold chests near 100 never paid below Dwarven.
                    if let Some(tier) = tier.filter(|&t| t > 1) {
                        assert!(tier >= 18, "L{level} chest {n}: a tier-{tier} piece");
                    }
                    ebony += u64::from(tier == Some(33));
                }
            }
            assert_eq!(
                kinds[..7].iter().sum::<u64>(),
                0,
                "L{level}: soul gems below Grand: {kinds:?}"
            );
            let share = with_gems as f64 / ROLLS as f64;
            assert!(
                (0.30..=0.40).contains(&share),
                "L{level}: {share:.3} of chests paid soul gems"
            );
            assert!(
                kinds[7] > kinds[8] && kinds[8] > 0 && kinds[9] > 0,
                "L{level}: {kinds:?}"
            );
            assert!(
                ebony * 2 > items,
                "L{level}: Ebony in {ebony} of {items} slots"
            );
        }
        // 45-48 keep the #388 rule: the three best, Glorious the likeliest.
        for n in 0..ROLLS {
            let r = store_golden(&t, 46, n);
            assert!(
                SOUL_GEMS[..7]
                    .iter()
                    .all(|id| !r.stackable_items.contains_key(id)),
                "L46 chest {n}"
            );
        }
    }

    /// Report #421, owner decision d21: the store's Golden chest is the Gold chest,
    /// so from buyer level 45 to 100 its soul gems, when it pays any, are Grand 60%
    /// / Glorious 20% / Transcendent 20%, 3 to 7 of them, one kind a chest. At 44 it
    /// is untouched: lower soul gems still come.
    #[test]
    fn a_store_golden_chest_at_45_to_100_pays_the_owners_soul_gems() {
        let t = committed_chest_tables();
        const ROLLS: u64 = 3000;
        for level in [45u64, 70, 100] {
            let (mut with_gems, mut kinds, mut counts) = (0u64, [0u64; 10], HashSet::new());
            for n in 0..ROLLS {
                let r = store_golden(&t, level, n);
                let gems: Vec<(usize, u64)> = SOUL_GEMS
                    .iter()
                    .enumerate()
                    .filter_map(|(k, id)| r.stackable_items.get(id).map(|q| (k, *q)))
                    .collect();
                assert!(gems.len() <= 1, "L{level} chest {n}: {gems:?}");
                for (k, q) in gems {
                    with_gems += 1;
                    kinds[k] += 1;
                    counts.insert(q);
                }
            }
            let share = with_gems as f64 / ROLLS as f64;
            assert!(
                (0.20..=0.45).contains(&share),
                "L{level}: {share:.3} of chests paid soul gems"
            );
            let of = |k: usize| kinds[k] as f64 / with_gems as f64;
            assert_eq!(
                kinds[..7].iter().sum::<u64>(),
                0,
                "L{level}: below Grand {kinds:?}"
            );
            assert!(
                (0.53..=0.67).contains(&of(7)),
                "L{level}: Grand {:.3}",
                of(7)
            );
            assert!(
                (0.14..=0.26).contains(&of(8)),
                "L{level}: Glorious {:.3}",
                of(8)
            );
            assert!(
                (0.14..=0.26).contains(&of(9)),
                "L{level}: Transcendent {:.3}",
                of(9)
            );
            assert_eq!(
                counts,
                (3..=7).collect::<HashSet<u64>>(),
                "L{level}: counts"
            );
        }
        let lower = (0..ROLLS)
            .filter(|&n| {
                SOUL_GEMS[..7]
                    .iter()
                    .any(|id| store_golden(&t, 44, n).stackable_items.contains_key(id))
            })
            .count();
        assert!(
            lower > 0,
            "L44: the Golden chest's own soul gems must be left alone"
        );
    }

    /// Report #429: the store's Epic (Elder) and Legendary chests bought at 45-100
    /// re-grade each soul-gem stack Grand 60% / Glorious 20% / Transcendent 20%,
    /// keeping every count; whether a chest holds soul gems, and how many, is what
    /// [`roll_bundle_with`] rolled. At 44 nothing moves.
    #[test]
    fn store_elder_and_legendary_chests_at_45_to_100_regrade_their_soul_gems() {
        let t = committed_chest_tables();
        let total = |r: &RewardGrant| -> u64 {
            r.stackable_items
                .iter()
                .filter(|(id, _)| SOUL_GEMS.contains(id))
                .map(|(_, n)| n)
                .sum()
        };
        for product in [EPIC_CHEST_PRODUCT_ID, LEGENDARY_CHEST_PRODUCT_ID] {
            assert!(store_chest_tier(&product).is_some_and(|t| t >= 4));
            for level in [45u64, 70, 100] {
                let (mut single, mut kinds) = (0usize, [0usize; 10]);
                for n in 0..2000u64 {
                    let rolled = roll_bundle_with(
                        &product,
                        crate::features::loot_level::effective_loot_level_u64(level),
                        n,
                        &HashSet::new(),
                        Some(&t),
                    )
                    .unwrap();
                    let paid =
                        roll_store_chest(&product, level, n, &HashSet::new(), Some(&t)).unwrap();
                    assert_eq!(total(&rolled), total(&paid), "{product} L{level} chest {n}");
                    assert!(
                        paid.stackable_items
                            .keys()
                            .all(|id| !SOUL_GEMS[..7].contains(id)),
                        "L{level}"
                    );
                    if rolled
                        .stackable_items
                        .keys()
                        .filter(|id| SOUL_GEMS.contains(id))
                        .count()
                        == 1
                    {
                        single += 1;
                        for (k, id) in SOUL_GEMS.iter().enumerate() {
                            kinds[k] += usize::from(paid.stackable_items.contains_key(id));
                        }
                    }
                }
                assert!(
                    single >= 200,
                    "{product} L{level}: {single} one-stack chests"
                );
                let of = |k: usize| kinds[k] as f64 / single as f64;
                assert!(
                    (0.52..=0.68).contains(&of(7)),
                    "{product} L{level}: Grand {:.3}",
                    of(7)
                );
                assert!(
                    (0.13..=0.27).contains(&of(8)),
                    "{product} L{level}: Glorious {:.3}",
                    of(8)
                );
                assert!(
                    (0.13..=0.27).contains(&of(9)),
                    "{product} L{level}: Transcendent {:.3}",
                    of(9)
                );
            }
            for n in 0..300u64 {
                let rolled = roll_bundle_with(&product, 44, n, &HashSet::new(), Some(&t)).unwrap();
                let paid = roll_store_chest(&product, 44, n, &HashSet::new(), Some(&t)).unwrap();
                let sorted = |m: &std::collections::HashMap<Uuid, u64>| {
                    let mut v: Vec<_> = m.iter().map(|(k, q)| (*k, *q)).collect();
                    v.sort_unstable();
                    v
                };
                assert_eq!(
                    sorted(&rolled.stackable_items),
                    sorted(&paid.stackable_items),
                    "L44 chest {n}"
                );
            }
        }
    }

    /// THE REPORT (#378): "the contents of Gold and Elder chests are set too low".
    /// For Elder they were: from 31 up every chest drew from the 31-89 band, a
    /// mid-30s population, so a level-45 to 94 Elder chest paid Glass in 74% of
    /// slots at enchant tier 3-4 and a median 19,885 gold. Retail's own Elder
    /// chests at 54-94 (11 treasury openings) paid Daedric in 19 of 22 slots and
    /// Dragon in 3, and 20,958-23,920 gold.
    #[test]
    fn high_level_elder_chests_pay_retails_high_level_elder_loot() {
        let t = committed_chest_tables();
        let retail = &t.provisional_tiers[&4];
        let gold = |r: &RewardGrant| r.currencies.get(&gold_id()).copied().unwrap_or(0);
        let lo = retail.iter().map(|s| gold(&s.reward)).min().unwrap();
        let hi = retail.iter().map(|s| gold(&s.reward)).max().unwrap();
        for level in [45u64, 54, 72, 86, 94, 100] {
            let mut tiers: std::collections::HashMap<u64, u64> = Default::default();
            for i in 0..600u64 {
                let key = format!("elder:{i}:4:{level}");
                let r = crate::features::chests::roll_loot(&t, 4, level, &key, &HashSet::new())
                    .unwrap();
                assert!(
                    (lo..=hi).contains(&gold(&r)),
                    "level {level}: {} gold",
                    gold(&r)
                );
                for item in &r.items[..2] {
                    assert_eq!(legendary_gear::rule_broken(&item.item), None);
                    let tier = required_level(&item.item.item_template_id).unwrap();
                    *tiers.entry(tier).or_default() += 1;
                }
            }
            let share = |t: u64| tiers.get(&t).copied().unwrap_or(0) as f64 / 1200.0;
            assert!(
                (0.78..=0.94).contains(&share(39)),
                "level {level}: Daedric {:.0}%: {tiers:?}",
                share(39) * 100.0
            );
            assert!(
                (0.06..=0.22).contains(&share(45)),
                "level {level}: {tiers:?}"
            );
        }
    }

    /// Elder slots at the buyer's exact level climb like a Legendary chest's
    /// lower slot (#378). Retail's 287 Elder purchases in the 2026-06-07 snapshot
    /// (level, purchases) paid a tier below the buyer's in 249 of 574 slots, the
    /// buyer's tier in 306 and a tier up in 19, never two tiers down. Rolled at
    /// the same levels, ours is within twelve points of each share (the
    /// Legendary slot-0 rule leans a little low: 54% a tier down), and from 31
    /// on, where the band draw paid Glass, never two tiers down.
    #[test]
    fn elder_gear_follows_the_buyers_tier() {
        const RETAIL_LEVELS: [(u64, u64); 15] = [
            (3, 8),
            (4, 20),
            (6, 9),
            (7, 10),
            (8, 20),
            (10, 60),
            (17, 30),
            (19, 20),
            (20, 10),
            (21, 30),
            (25, 20),
            (26, 10),
            (28, 20),
            (29, 10),
            (30, 10),
        ];
        let ladder = [1u64, 8, 13, 18, 23, 28, 33, 39, 45];
        let epic: Uuid = "7bf00a9c-6a08-4b55-a60d-53915baa38a3".parse().unwrap();
        let offset = |level: u64, item: &Item| {
            let own = ladder.iter().rposition(|&t| t <= level).unwrap() as i64;
            let t = required_level(&item.item_template_id).unwrap();
            ladder.iter().position(|&x| x == t).unwrap() as i64 - own
        };
        let mut tally: std::collections::HashMap<i64, u64> = Default::default();
        let mut n = 0;
        for (level, purchases) in RETAIL_LEVELS {
            for nonce in 0..purchases * 20 {
                for i in &roll_bundle(&epic, level, nonce).unwrap().items[..2] {
                    *tally.entry(offset(level, &i.item)).or_default() += 1;
                    n += 1;
                }
            }
        }
        let share = |k: i64| tally.get(&k).copied().unwrap_or(0) as f64 / n as f64;
        assert!((share(-1) - 249.0 / 574.0).abs() <= 0.12, "{tally:?}");
        assert!((share(0) - 306.0 / 574.0).abs() <= 0.12, "{tally:?}");
        assert!(share(1) <= 0.08 && share(-2) == 0.0, "{tally:?}");
        for level in 31..=100u64 {
            for nonce in 0..40u64 {
                for i in &roll_bundle(&epic, level, nonce).unwrap().items[..2] {
                    let at = offset(level, &i.item);
                    assert!(
                        (-1..=1).contains(&at),
                        "level {level}: {at} tiers from the buyer's"
                    );
                }
            }
        }
    }

    /// Every retail artifact drop that can be placed in level falls inside its
    /// tier's window: the snapshot's nine at an exact buyer level, the corpus's
    /// by band (dated by the chest's gear where the band is wide), and
    /// HauDrauf's three Gold chests opened at level 60 (#378 follow-up). The old
    /// rule (required level at most ten ahead, no floor) let a level-60 buyer
    /// draw Savior's Hide, which the community table caps at 22.
    #[test]
    fn retail_artifact_drops_fall_inside_their_windows() {
        // (required level, lowest and highest level the buyer can have been).
        const DROPS: [(u64, u64, u64, &str); 31] = [
            (13, 3, 3, "Fork of Horripilation, snapshot"),
            (6, 4, 4, "Savior's Hide, snapshot"),
            (6, 5, 5, "Savior's Hide, snapshot"),
            (13, 10, 10, "Ring of Namira, snapshot"),
            (18, 10, 10, "Pandemonium, snapshot"),
            (28, 19, 19, "Ebony Mail, snapshot"),
            (23, 21, 21, "Chrysamere, snapshot"),
            (28, 25, 25, "Rimelink, snapshot"),
            (33, 26, 26, "Dragon's Blight, snapshot"),
            (33, 26, 26, "Ebony Blade (Elder), snapshot"),
            (13, 1, 5, "Fork of Horripilation"),
            (13, 1, 5, "Fork of Horripilation"),
            (6, 1, 5, "Savior's Hide"),
            (6, 1, 5, "Savior's Hide"),
            (6, 1, 5, "Savior's Hide"),
            (13, 1, 5, "Guardian's Claymore"),
            (13, 6, 8, "Guardian's Claymore"),
            (6, 6, 8, "Savior's Hide"),
            (13, 6, 8, "Ring of Namira"),
            (13, 9, 14, "Ring of Namira"),
            (18, 15, 17, "Pandemonium"),
            (28, 18, 23, "Ebony Mail"),
            (23, 18, 23, "Chrysamere"),
            (28, 18, 23, "Rimelink"),
            (18, 18, 23, "Pandemonium"),
            (
                38,
                31,
                32,
                "Spellbreaker, Glass+Glass chest, Greater soul gem",
            ),
            (33, 33, 35, "Mehrunes' Razor, Ebony on top"),
            (50, 43, 50, "Onyx Cleaver, Daedric+Dragon chest"),
            (38, 31, 89, "Dawnbreaker (Elder)"),
            (50, 54, 89, "Lord's Mail / Chillrend"),
            (44, 54, 89, "Warlock's Ring"),
        ];
        for (req, lo, hi, what) in DROPS {
            assert!(
                (lo..=hi).any(|l| artifact_in_window(req, l)),
                "{what} (required level {req}) at {lo}-{hi} is outside its window"
            );
        }
        for (req, what) in [
            (38, "Spellbreaker"),
            (44, "Warlock's Ring"),
            (50, "Chillrend"),
        ] {
            assert!(
                artifact_in_window(req, 60),
                "HauDrauf's Gold chest {what} at 60"
            );
        }
        // The windows bite: no tier 2-7 artifact past its "findable before"
        // level, no tier 9-10 artifact before 39.
        assert!(!artifact_in_window(6, 23) && !artifact_in_window(33, 50));
        assert!(!artifact_in_window(44, 38) && !artifact_in_window(50, 38));
        assert!(artifact_in_window(50, 39) && artifact_in_window(38, 100));
    }

    /// HauDrauf's three retail Gold chests with an artifact (#378 follow-up):
    /// about one Gold chest in a thousand, beside its usual two items, rolled
    /// like any artifact slot. The 270 captured Gold openings have none (a 76%
    /// outcome at 0.1%). Wooden and Silver chests never (0 of 459). Elder chests
    /// keep 4 in 405, with the same windows.
    #[test]
    fn gold_chests_pay_an_artifact_about_once_in_a_thousand() {
        let t = committed_chest_tables();
        for tier in 1..=2u64 {
            for level in [10u64, 45, 90] {
                for i in 0..2_000 {
                    let key = format!("g{i}");
                    let r = crate::features::chests::roll_loot(
                        &t,
                        tier as i64,
                        level,
                        &key,
                        &HashSet::new(),
                    )
                    .unwrap();
                    assert!(r.items.len() <= 1, "tier {tier} paid a third item");
                }
            }
        }
        let n = 60_000u64;
        let (mut third, mut arts) = (0u64, 0u64);
        for i in 0..n {
            let level = [20u64, 45, 60][(i % 3) as usize];
            let key = format!("gold:{i}:3:{level}");
            let r =
                crate::features::chests::roll_loot(&t, 3, level, &key, &HashSet::new()).unwrap();
            assert!((2..=3).contains(&r.items.len()));
            if let Some(x) = r.items.get(2) {
                third += 1;
                let tpl = x.item.item_template_id;
                assert!(is_artifact(&tpl), "nothing held, the slot is an artifact");
                let req = ARTIFACT_LOOT.iter().find(|a| a.0 == tpl).unwrap().1;
                assert!(
                    artifact_in_window(req, level),
                    "level {level}: required level {req}"
                );
                arts += 1;
            }
        }
        assert!(
            (35..=90).contains(&third),
            "{third} Gold chests of {n} had a third item"
        );
        assert_eq!(third, arts);
        // A held artifact is replaced, never paid twice (#310).
        let all: HashSet<Uuid> = ARTIFACT_TEMPLATES.iter().copied().collect();
        for i in 0..n / 3 {
            let r = crate::features::chests::roll_loot(&t, 3, 60, &format!("gold:{i}:3:60"), &all)
                .unwrap();
            assert!(
                r.items
                    .iter()
                    .all(|x| !is_artifact(&x.item.item_template_id))
            );
        }
        let epic: Uuid = "7bf00a9c-6a08-4b55-a60d-53915baa38a3".parse().unwrap();
        let (third, arts, seen) = third_slots(&epic, 45, 20_000, &HashSet::new());
        let rate = third as f64 / 20_000.0;
        assert_eq!(third, arts);
        assert!(
            (0.005..=0.015).contains(&rate),
            "Elder artifact slot in {:.2}%",
            rate * 100.0
        );
        for tpl in seen {
            let req = ARTIFACT_LOOT.iter().find(|a| a.0 == tpl).unwrap().1;
            assert!(
                artifact_in_window(req, 45),
                "Elder at 45: required level {req}"
            );
        }
    }

    /// The #378 tables for the PR, ours against fork/main.
    /// `cargo test -p blades_lib print_378_table -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_378_table() {
        let seph: HashSet<Uuid> = [
            0xdef810af_e9f5_4e23_9247_1edf391d82e1u128,
            0x9a22b59a_cdbc_4987_bf4a_0cc812428aeb,
            0x21ed2758_5cf5_4b7d_9047_59bbdbd61820,
            0xa0d7dd5e_692e_4fbb_bbf2_19c6a874f3f5,
            0x86a4ad9d_e2e8_49cd_9642_b553475b6580,
            0x12c4920e_a9e0_4498_b134_3ce66fb9558f,
        ]
        .iter()
        .map(|&u| Uuid::from_u128(u))
        .collect();
        let leg: Uuid = BIG.parse().unwrap();
        let epic: Uuid = "7bf00a9c-6a08-4b55-a60d-53915baa38a3".parse().unwrap();
        for (name, product) in [("Legendary", leg), ("Elder", epic)] {
            for (who, owned) in [
                ("nothing held", HashSet::new()),
                ("Sephoris's 6", seph.clone()),
            ] {
                for level in [10u64, 20, 30, 45, 60, 90] {
                    let n = 10_000u64;
                    let (mut third, mut arts) = (0, 0);
                    let mut art_kinds = HashSet::new();
                    let mut fb: HashSet<String> = HashSet::new();
                    let mut fb_n = 0;
                    for nonce in 0..n {
                        let g = roll_bundle_for(&product, level, nonce.wrapping_mul(7919), &owned)
                            .unwrap();
                        if let Some(x) = g.items.get(2) {
                            third += 1;
                            if is_artifact(&x.item.item_template_id) {
                                arts += 1;
                                art_kinds.insert(x.item.item_template_id);
                            } else {
                                fb_n += 1;
                                fb.insert(serde_json::to_string(&x.item).unwrap());
                            }
                        }
                    }
                    println!(
                        "{name:9} {who:12} L{level:3}: third slot {:4.1}%  artifact {:4.2}%  ({} kinds)  fallback {} distinct of {}",
                        third as f64 * 100.0 / n as f64,
                        arts as f64 * 100.0 / n as f64,
                        art_kinds.len(),
                        fb.len(),
                        fb_n
                    );
                }
            }
        }
        let t: crate::features::chests::ChestLootTables =
            serde_json::from_str(include_str!("../../../deploy/static/chest_loots.json")).unwrap();
        for level in [20u64, 33, 40, 45, 60, 90] {
            let mut tiers: std::collections::BTreeMap<u64, u64> = Default::default();
            let mut ench: std::collections::BTreeMap<u64, u64> = Default::default();
            let mut golds = Vec::new();
            for i in 0..2_000u64 {
                let r = crate::features::chests::roll_loot(
                    &t,
                    4,
                    level,
                    &format!("e:{i}:{level}"),
                    &HashSet::new(),
                )
                .unwrap();
                golds.push(
                    r.currencies
                        .get(&GOLD.parse::<Uuid>().unwrap())
                        .copied()
                        .unwrap_or(0),
                );
                for i in &r.items[..2] {
                    *tiers
                        .entry(required_level(&i.item.item_template_id).unwrap_or(0))
                        .or_default() += 1;
                    *ench
                        .entry(i.item.properties.enchanting.first().map_or(0, |e| e.tier))
                        .or_default() += 1;
                }
            }
            golds.sort_unstable();
            let pct = |m: &std::collections::BTreeMap<u64, u64>| {
                m.iter()
                    .map(|(k, v)| format!("{k}:{:.0}%", *v as f64 * 100.0 / 4000.0))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            println!(
                "Elder treasury L{level:3}: gold median {}  tiers {}  ench tier {}",
                golds[1000],
                pct(&tiers),
                pct(&ench)
            );
        }
    }

    /// #387: at 45 and up the Legendary chest's shields are Dragonscale and
    /// Dragonplate one for one (the tier's two shield templates, uniformly), and a
    /// Dragonscale Armor with Fire Resistance is rare, not impossible. Fire
    /// Resistance is a primary only at enchant tiers 1, 3, 5, 7, 9 and 10, and
    /// never a cuirass secondary; at 46 the cuirass carries tier 6, 7 or 8, so it
    /// takes a tier-7 piece and one primary in three: about one chest in 115, and
    /// none in 200 chests about one time in six.
    #[test]
    fn dragon_shields_split_evenly_and_fire_resistant_dragonscale_is_rare() {
        let fire: Uuid = "464bedb7-a631-43b6-a2df-f65f089d39da".parse().unwrap();
        let cuirass: Uuid = "659dd496-f71e-4cf8-aaee-8f0c4723410e".parse().unwrap();
        let scale_shield: Uuid = "6362ecf3-42d9-4d8d-af30-2b9fc2deb92a".parse().unwrap();
        let plate_shield: Uuid = "ded7cfb8-5e4c-419a-8acb-792fb5b6c646".parse().unwrap();
        let (chests, mut fire_cuirass, mut scale, mut plate) = (40_000u64, 0u64, 0u64, 0u64);
        for nonce in 0..chests {
            let g = roll_bundle(&big(), 46, nonce).unwrap();
            let mut hit = false;
            for i in g.items.iter().map(|i| &i.item) {
                scale += u64::from(i.item_template_id == scale_shield);
                plate += u64::from(i.item_template_id == plate_shield);
                if i.item_template_id == cuirass
                    && i.properties.enchanting.iter().any(|e| e.id == fire)
                {
                    assert_eq!(i.properties.enchanting[0].id, fire, "a primary only");
                    assert!([7, 9, 10].contains(&i.properties.enchanting[0].tier));
                    hit = true;
                }
            }
            fire_cuirass += u64::from(hit);
        }
        let share = scale as f64 / (scale + plate) as f64;
        assert!(
            (0.47..=0.53).contains(&share),
            "Dragonscale shields {share:.3} of {}",
            scale + plate
        );
        let p = fire_cuirass as f64 / chests as f64;
        assert!(
            (0.006..=0.012).contains(&p),
            "Fire Resistance Dragonscale Armor in {p:.4} of chests"
        );
    }

    /// The #392 audit: the Golden chest's gear was replayed verbatim from its
    /// buyer band, so eight ring and two necklace bonuses could never appear and
    /// the enchantments were frozen. Now generated like the Legendary and Elder
    /// chests': every piece obeys the APK rules, keeps the band's material tiers,
    /// and every GRADING property is reachable.
    #[test]
    fn golden_chest_gear_is_generated_around_the_retail_piece() {
        use super::super::legendary_gear::{kind_of, required_level, rule_broken};
        let golden = Uuid::from_u128(0x0e224ca0_1506_490f_884a_8871ffe6399b);
        let corpus = corpus_product(&golden).expect("the Golden chest has a corpus");
        let mut grading: std::collections::HashMap<&str, HashSet<Uuid>> = Default::default();
        let mut pieces = HashSet::new();
        let mut items = 0usize;
        for level in [1u64, 10, 20, 30, 40, 45, 49, 60, 80, 100] {
            let band = corpus
                .by_level
                .iter()
                .find(|b| level >= b.min_buyer_level && level <= b.max_buyer_level)
                .or_else(|| {
                    corpus
                        .by_level
                        .iter()
                        .min_by_key(|b| level_distance(level, b.min_buyer_level, b.max_buyer_level))
                })
                .unwrap();
            let band_tiers: HashSet<Option<u64>> = band
                .results
                .iter()
                .flat_map(|r| &r.reward.items)
                .map(|i| required_level(&i.item_template_id))
                .collect();
            for n in 0..400u64 {
                let r = roll_bundle(&golden, level, n).unwrap();
                for item in r.items.iter().map(|i| &i.item) {
                    items += 1;
                    assert_eq!(rule_broken(item), None, "L{level}: {item:?}");
                    assert!(
                        band_tiers.contains(&required_level(&item.item_template_id)),
                        "L{level}: a tier the band never paid"
                    );
                    if let Some(kind @ ("ring" | "necklace")) = kind_of(&item.item_template_id) {
                        grading
                            .entry(kind)
                            .or_default()
                            .extend(item.properties.grading.iter().map(|g| g.id));
                    }
                    pieces.insert(serde_json::to_string(item).unwrap());
                }
            }
        }
        assert_eq!(
            grading["ring"].len(),
            34,
            "every RingGradePropertyList entry"
        );
        assert_eq!(
            grading["necklace"].len(),
            15,
            "every NecklaceGradePropertyList entry"
        );
        // Control: the band draw alone paid 552 distinct pieces in 10,000.
        assert!(
            pieces.len() * 2 > items,
            "{} distinct pieces in {items}",
            pieces.len()
        );
    }
}
