//! Generated ring/necklace rolls for Sigil offers and Enchanter shop stock.
//!
//! Retail did not sell one frozen jewelry instance forever. The 2026-06-07
//! snapshot has 251 global-shop jewelry grants from randomized jewelry bundles,
//! all distinct; the named Sigil jewelry offers themselves were not bought in
//! that snapshot, so `deploy/static/jewelry_roll_ranges.json` records the measured
//! count/grade ranges of those Sigil grants and this module draws property ids
//! from the APK pools.
//!
//! Town merchants are different (#321): every retail town-shop ring/necklace was
//! a blank — a rolled grade and GRADING, never ENCHANTING — so the Sigil counts
//! must not be applied to Enchanter stock.

use std::collections::HashMap;

use blades_lib::{
    game_data::GameDataItem,
    static_data::{EnchantingData, SecondaryEnchantTable},
    user_data::{Item, ItemSingleProperty},
};
use rand::{rngs::StdRng, Rng, RngExt, SeedableRng};
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
    let Some(item_type) = items.get(&item.item_template_id).map(|i| i.r#type) else {
        return false;
    };
    if !matches!(item_type, jewelry_grade::RING | jewelry_grade::NECKLACE) {
        return false;
    }
    if let Some((grade, grading)) = jewelry_grade::roll(item_type, rng) {
        item.grade = Some(grade);
        item.properties.grading = grading;
        item.tempering_level = 0;
        item.durability = 0.0;
    }
    item.properties.enchanting.clear();
    true
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
    if reroll_grading {
        if let Some(item_type) = items.get(&item.item_template_id).map(|i| i.r#type) {
            if let Some((grade, grading)) = jewelry_grade::roll(item_type, rng) {
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
        };
        let mut rng = seeded(&[b"x"], 2);
        let out = roll_enchanting_exact(&mut rng, Some(&table), 3, 6);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|p| p.tier == 6));
        let ids: std::collections::HashSet<_> = out.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), out.len());
    }
}
