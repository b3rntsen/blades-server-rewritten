//! Generated ring/necklace rolls for Sigil offers and Enchanter shop stock.
//!
//! Retail did not sell one frozen jewelry instance forever. The 2026-06-07
//! snapshot has 251 global-shop jewelry grants from randomized jewelry bundles,
//! all distinct; the named Sigil jewelry offers themselves were not bought in
//! that snapshot, so `deploy/static/jewelry_roll_ranges.json` records the measured
//! count/grade ranges of those Sigil grants and this module draws property ids
//! from the APK pools.
//!
//! THEMED Sigil jewellery is the exception (#403): an offer whose GRADING the APK
//! authors (`..._FrostbiteIceSpikeBlizzardArmor_NoEnch`) is sold exactly as
//! authored, because every retail purchase of one we hold is that exact set.
//! `global_shop::is_themed_jewelry` keeps those offers away from this module.
//!
//! Town merchants are different (#321): every retail town-shop ring/necklace was
//! a blank — a rolled grade and GRADING, never ENCHANTING — so the Sigil counts
//! must not be applied to Enchanter stock.

use std::collections::HashMap;

use blades_lib::{
    game_data::GameDataItem,
    static_data::{EnchantingData, SecondaryEnchantTable, TopTierJewellery},
    user_data::{Item, ItemSingleProperty},
};
use rand::{Rng, RngExt, SeedableRng, rngs::StdRng};
use uuid::Uuid;

use crate::jewelry_grade;

pub fn seed(parts: &[&[u8]], nonce: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let feed = |h: &mut u64, b: u8| {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for part in parts {
        for b in *part {
            feed(&mut h, *b);
        }
    }
    for b in nonce.to_le_bytes() {
        feed(&mut h, b);
    }
    h
}

pub fn seeded(parts: &[&[u8]], nonce: u64) -> StdRng {
    StdRng::seed_from_u64(seed(parts, nonce))
}

pub fn is_jewelry_template(template: Uuid, items: &HashMap<Uuid, GameDataItem>) -> bool {
    matches!(
        items.get(&template).map(|i| i.r#type),
        Some(jewelry_grade::RING | jewelry_grade::NECKLACE)
    )
}

fn weighted_index<R: Rng + ?Sized>(rng: &mut R, weights: &[f64]) -> Option<usize> {
    let total: f64 = weights.iter().filter(|w| **w > 0.0).sum();
    if total <= 0.0 {
        return None;
    }
    let mut x = rng.random::<f64>() * total;
    let mut last = None;
    for (i, &w) in weights.iter().enumerate() {
        if w <= 0.0 {
            continue;
        }
        last = Some(i);
        if x < w {
            return Some(i);
        }
        x -= w;
    }
    last
}

pub fn roll_enchanting_exact<R: Rng + ?Sized>(
    rng: &mut R,
    table: Option<&SecondaryEnchantTable>,
    count: u64,
    tier: u64,
) -> Vec<ItemSingleProperty> {
    let Some(table) = table else {
        return Vec::new();
    };
    let mut pool: Vec<(Uuid, f64)> = table
        .properties
        .iter()
        .filter(|p| p.weight > 0.0)
        .map(|p| (p.id, p.weight))
        .collect();
    let mut out = Vec::new();
    for _ in 0..count {
        let weights: Vec<f64> = pool.iter().map(|p| p.1).collect();
        let Some(i) = weighted_index(rng, &weights) else {
            break;
        };
        let (id, _) = pool.remove(i);
        out.push(ItemSingleProperty { id, tier });
    }
    out
}

/// Roll a town merchant's ring or necklace the way retail sold it: a fresh
/// grade with its GRADING (the random skills), and no ENCHANTING — a blank the
/// player enchants. Retail's 4 town-shop jewellery purchases (1,516 purchase
/// responses, 2026-05-09..06-30) were graded 1..=3 with 0 ENCHANTING each.
pub fn roll_generated_jewelry<R: Rng + ?Sized>(
    item: &mut Item,
    items: &HashMap<Uuid, GameDataItem>,
    rng: &mut R,
) -> bool {
    roll_generated_jewelry_tuned(item, items, &TopTierJewellery::default(), rng)
}

/// [`roll_generated_jewelry`] with HauDrauf's top-tier rule (#581): a covered
/// template is graded by the crafted split (#560), like a forged one.
pub fn roll_generated_jewelry_tuned<R: Rng + ?Sized>(
    item: &mut Item,
    items: &HashMap<Uuid, GameDataItem>,
    top_tier: &TopTierJewellery,
    rng: &mut R,
) -> bool {
    let Some(item_type) = items.get(&item.item_template_id).map(|i| i.r#type) else {
        return false;
    };
    if !matches!(item_type, jewelry_grade::RING | jewelry_grade::NECKLACE) {
        return false;
    }
    // A legendary ring arrives as its template defines it (#8).
    if crate::fixed_templates::has_mandatory_properties(item.item_template_id) {
        return false;
    }
    if let Some((grade, grading)) = shop_grade(item_type, item.item_template_id, top_tier, rng) {
        item.grade = Some(grade);
        item.properties.grading = grading;
        item.tempering_level = 0;
        item.durability = 0.0;
    }
    item.properties.enchanting.clear();
    true
}

/// The grade a shop sells a ring or necklace at: the crafted split (#560) for a
/// top-tier template under HauDrauf's #581 rule, the every-source split (#393)
/// otherwise.
pub fn shop_grade<R: Rng + ?Sized>(
    item_type: u64,
    item_template_id: Uuid,
    top_tier: &TopTierJewellery,
    rng: &mut R,
) -> Option<(u64, Vec<ItemSingleProperty>)> {
    if top_tier.shop_grades_as_crafted(&item_template_id) {
        jewelry_grade::roll_crafted(item_type, rng)
    } else {
        jewelry_grade::roll(item_type, rng)
    }
}

pub fn reroll_sigil_jewelry<R: Rng + ?Sized>(
    item: &mut Item,
    authored_enchanting_count: usize,
    authored_enchanting_tier: u64,
    reroll_grading: bool,
    items: &HashMap<Uuid, GameDataItem>,
    enchanting: &EnchantingData,
    rng: &mut R,
) -> bool {
    if !is_jewelry_template(item.item_template_id, items) {
        return false;
    }
    // The Master Ring of Shock bug (#8): its template already carries six
    // mandatory properties, and retail never graded one (0 of 25 captured
    // mandatory-property ring instances). Leave it exactly as authored.
    if crate::fixed_templates::has_mandatory_properties(item.item_template_id) {
        return false;
    }
    if reroll_grading {
        if let Some(item_type) = items.get(&item.item_template_id).map(|i| i.r#type) {
            let top_tier = &enchanting.tuning.top_tier_jewellery;
            if let Some((grade, grading)) =
                shop_grade(item_type, item.item_template_id, top_tier, rng)
            {
                item.grade = Some(grade);
                item.properties.grading = grading;
                item.tempering_level = 0;
                item.durability = 0.0;
            }
        }
    }
    if authored_enchanting_count > 0 {
        item.properties.enchanting = roll_enchanting_exact(
            rng,
            enchanting.table_for(&item.item_template_id),
            authored_enchanting_count as u64,
            authored_enchanting_tier.max(1),
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::static_data::WeightedProperty;

    fn deploy_items() -> &'static HashMap<Uuid, GameDataItem> {
        static T: std::sync::OnceLock<HashMap<Uuid, GameDataItem>> = std::sync::OnceLock::new();
        T.get_or_init(|| {
            let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
            let gd: blades_lib::game_data::GameData =
                serde_json::from_str(&std::fs::read_to_string(p).expect("parsed.json"))
                    .expect("game data");
            gd.items_template
        })
    }

    /// The shipped `enchant_tuning.json` (#581 lives in `topTierJewellery`).
    pub(crate) fn shipped_tuning() -> blades_lib::static_data::EnchantTuning {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/enchant_tuning.json"
        );
        serde_json::from_str(&std::fs::read_to_string(p).expect("enchant_tuning.json"))
            .expect("the shipped tuning parses (an unparsable file silently reverts ALL tuning)")
    }

    const EBONY_FAERITE_RING: &str = "240eb001-fe3e-4899-b0cc-dd87cb8a72b6";
    const EBONY_FAERITE_NECKLACE: &str = "54a7fc91-93e7-4908-a29a-0667d0b07179";
    const GOLD_EMERALD_RING: &str = "d408a912-3a4f-4b2a-bf4d-a55b78910f08";

    fn bare(template: &str) -> Item {
        serde_json::from_value(serde_json::json!({ "itemTemplateId": template }))
            .expect("a bare item")
    }

    /// Grade histogram (index = grade) of `n` town-merchant rolls.
    fn merchant_grades(template: &str, top: &TopTierJewellery, seed: u64, n: u32) -> [u32; 7] {
        let mut rng = seeded(&[b"581-merchant"], seed);
        let mut h = [0u32; 7];
        for _ in 0..n {
            let mut item = bare(template);
            assert!(roll_generated_jewelry_tuned(
                &mut item,
                deploy_items(),
                top,
                &mut rng
            ));
            assert!(
                item.properties.enchanting.is_empty(),
                "merchants sell blanks (#321)"
            );
            h[item.grade.unwrap() as usize] += 1;
        }
        h
    }

    /// Grade histogram of `n` forged ones (#560's crafted split).
    fn crafted_grades(template: &str, seed: u64, n: u32) -> [u32; 7] {
        let mut rng = seeded(&[b"581-forge"], seed);
        let mut h = [0u32; 7];
        for _ in 0..n {
            let mut item = bare(template);
            jewelry_grade::regrade_crafted(&mut item, deploy_items(), &mut rng);
            h[item.grade.unwrap() as usize] += 1;
        }
        h
    }

    fn assert_same_split(shop: [u32; 7], forge: [u32; 7], what: &str) {
        let n_s: u32 = shop.iter().sum();
        let n_f: u32 = forge.iter().sum();
        assert_eq!(
            shop[1], 0,
            "{what}: never grade 1 (crafted split): {shop:?}"
        );
        for g in 2..=6 {
            let a = f64::from(shop[g]) * 100.0 / f64::from(n_s);
            let b = f64::from(forge[g]) * 100.0 / f64::from(n_f);
            assert!(
                (a - b).abs() < 2.0,
                "{what}: grade {g} shop {a:.1}% forge {b:.1}%"
            );
        }
    }

    /// #581 (HauDrauf, 2026-11-28): a town merchant's top-tier ring or necklace is
    /// graded exactly like a forged one (#560: 2/3/4/5/6 at 5/15/20/25/35%).
    #[test]
    fn top_tier_shop_jewellery_is_graded_like_crafted() {
        let tuning = shipped_tuning();
        let top = &tuning.top_tier_jewellery;
        for template in [EBONY_FAERITE_RING, EBONY_FAERITE_NECKLACE] {
            assert_same_split(
                merchant_grades(template, top, 1, 20_000),
                crafted_grades(template, 2, 20_000),
                template,
            );
        }
    }

    /// #581: everything else a merchant sells rolls draw for draw as before (#393),
    /// and switching the rule off puts the top tier back on #393 (grade 1 occurs).
    #[test]
    fn top_tier_rule_leaves_other_shop_jewellery_and_reverts() {
        let tuning = shipped_tuning();
        let off = TopTierJewellery::default();
        assert_eq!(
            merchant_grades(GOLD_EMERALD_RING, &tuning.top_tier_jewellery, 9, 5_000),
            merchant_grades(GOLD_EMERALD_RING, &off, 9, 5_000),
        );
        let mut disabled = tuning.top_tier_jewellery.clone();
        disabled.enabled = false;
        let h = merchant_grades(EBONY_FAERITE_RING, &disabled, 9, 5_000);
        assert_eq!(h, merchant_grades(EBONY_FAERITE_RING, &off, 9, 5_000));
        assert!(h[1] > 0, "#393 rolls grade 1 sometimes: {h:?}");
    }

    /// #581: the Sigil shop's non-arcane reroll grades a top-tier piece like a forged
    /// one too.
    #[test]
    fn top_tier_sigil_reroll_is_graded_like_crafted() {
        let mut enchanting = EnchantingData::default();
        enchanting.tuning = shipped_tuning();
        let mut rng = seeded(&[b"581-sigil"], 0);
        let mut h = [0u32; 7];
        for _ in 0..20_000 {
            let mut item = bare(EBONY_FAERITE_RING);
            assert!(reroll_sigil_jewelry(
                &mut item,
                0,
                1,
                true,
                deploy_items(),
                &enchanting,
                &mut rng
            ));
            h[item.grade.unwrap() as usize] += 1;
        }
        assert_same_split(h, crafted_grades(EBONY_FAERITE_RING, 3, 20_000), "sigil");
    }

    #[test]
    fn exact_enchanting_rolls_distinct_properties() {
        let table = SecondaryEnchantTable {
            count_odds: vec![],
            properties: (0..5)
                .map(|i| WeightedProperty {
                    id: Uuid::from_u128(i + 1),
                    weight: 1.0,
                })
                .collect(),
            ..Default::default()
        };
        let mut rng = seeded(&[b"x"], 2);
        let out = roll_enchanting_exact(&mut rng, Some(&table), 3, 6);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|p| p.tier == 6));
        let ids: std::collections::HashSet<_> = out.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), out.len());
    }
}
