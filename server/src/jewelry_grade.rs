//! The grade a ring or necklace gets when the game mints it bare (#240).
//!
//! THE BUG. A town merchant's jewellery arrived with no grade and no GRADING
//! ("secondary enchantments"): `shop_bundles.json` carries only the template, and
//! the purchase handler cloned it verbatim. Retail never handed out an ungraded
//! ring or necklace from a merchant — the four retail merchant purchases of
//! jewellery we hold are all graded, and the SAME bundle (Gold Emerald Ring,
//! `01ada486`) came out at grade 1 once and grade 3 another time, so the grade
//! was generated rather than authored per bundle. The APK agrees: every one of
//! the 16 jewellery bundles a merchant can stock has no `_itemEnhancementPointer`
//! at all, while `LootEnhancementData._alwaysGradedItemTypes` is `[10, 11]` —
//! rings and necklaces are ALWAYS graded.
//!
//! THE RULE, all of it APK-authored:
//!
//! * `JewelryGradeData._jewelryGradeData` — the grade is drawn by weight, and the
//!   grade fixes the tiers of its GRADING properties (`_providedBonuses`).
//! * `RingGradePropertyList` (34) / `NecklaceGradePropertyList` (15) — which
//!   properties a ring or a necklace can carry (already compiled in as
//!   [`GRADE_PROPERTIES`], hash-pinned to `grade_properties.json`).
//!
//! Checked against every graded, non-arcane retail ring and necklace in the
//! corpora we ship (enemy loot 552, store bundles 542, chests 23, merchant 4):
//! 1,121 of 1,121 have `grade == sum(tiers)`, tiers equal to `_providedBonuses`
//! of their grade, every property from their own slot's list, and no property
//! twice. All 34 ring and all 15 necklace properties occur.
//!
//! WHAT IS NOT KNOWN. Whether retail's merchant used exactly these weights, or
//! scaled them by shop level, is not recoverable: four purchases (grades 1, 1, 2,
//! 3) cannot separate the two. The weights are the APK's own table, which is the
//! only authored answer there is.
//!
//! Arcane jewellery (the Sigil shop) takes its PROPERTY COUNT from
//! [`SIGIL_ARCANE_PROPERTY_COUNT_PERCENT`] (#393), its grade within that count
//! from this table, and its properties from [`fill`] (#392); this module never
//! touches an item that already has a grade or GRADING.

use blades_lib::game_data::GameDataItem;
use blades_lib::user_data::{Item, ItemSingleProperty};
use rand::{Rng, RngExt};
use std::collections::HashMap;
use uuid::Uuid;

use crate::arena::combat::gamedata::GRADE_PROPERTIES;

/// `ItemType` of a ring.
pub const RING: u64 = 10;
/// `ItemType` of a necklace ("jewelry" in the client's enum).
pub const NECKLACE: u64 = 11;

/// `JewelryGradeData._jewelryGradeData`: `(grade, weight, providedBonuses)`.
///
/// Read from the APK (`reference/apk/blades.apk`, SHA-256 fd6e55f5…, bundle
/// `gameplaymetadata`). `providedBonuses` is the tier of each GRADING property,
/// in the order retail lists them (highest first).
pub const JEWELRY_GRADES: [(u64, u32, &[u64]); 6] = [
    (1, 22, &[1]),
    (2, 20, &[1, 1]),
    (3, 18, &[1, 1, 1]),
    (4, 16, &[2, 1, 1]),
    (5, 14, &[2, 2, 1]),
    (6, 10, &[2, 2, 2]),
];

/// Percent of Sigil-shop arcane rings and necklaces carrying 1, 2 and 3 GRADING
/// properties: `(property count, percent)`, summing to 100.
///
/// Per HauDrauf #393 ("thousands of rings" bought in retail: one property maybe
/// 5%, two maybe 10%, the vast majority three); the retail corpus is thin. The
/// 32 tier-2 ring purchase outcomes we captured were 13 / 5 / 14, which is what
/// the shop sold until this: 41% single-property rings. The 451 distinct tier-2
/// arcane rings and necklaces in retail imports agree with HauDrauf (22 / 40 /
/// 389 = 5% / 9% / 86%), though a kept inventory is not an unfiltered roll.
///
/// Within three properties the grade (3-6) is drawn by the APK
/// [`JEWELRY_GRADES`] weights (18/16/14/10): 65 captured three-property
/// outcomes, 14 of them rings, cannot be told apart from those weights.
pub const SIGIL_ARCANE_PROPERTY_COUNT_PERCENT: [(usize, u32); 3] = [(1, 5), (2, 10), (3, 85)];

const _: () = {
    let mut sum = 0;
    let mut i = 0;
    while i < SIGIL_ARCANE_PROPERTY_COUNT_PERCENT.len() {
        sum += SIGIL_ARCANE_PROPERTY_COUNT_PERCENT[i].1;
        i += 1;
    }
    assert!(sum == 100, "SIGIL_ARCANE_PROPERTY_COUNT_PERCENT must sum to 100");
};

/// Draw one entry of [`JEWELRY_GRADES`] among those `keep` accepts, by weight.
fn pick_grade<R: Rng + ?Sized>(
    rng: &mut R,
    keep: impl Fn(&(u64, u32, &'static [u64])) -> bool,
) -> (u64, u32, &'static [u64]) {
    let kept: Vec<_> = JEWELRY_GRADES.iter().filter(|g| keep(g)).copied().collect();
    let total: u32 = kept.iter().map(|g| g.1).sum();
    if total == 0 {
        return JEWELRY_GRADES[0];
    }
    let mut pick = rng.random_range(0..total);
    for g in &kept {
        if pick < g.1 {
            return *g;
        }
        pick -= g.1;
    }
    JEWELRY_GRADES[0]
}

/// The GRADING tiers (`_providedBonuses`) of one Sigil-shop arcane ring or
/// necklace: the property count by [`SIGIL_ARCANE_PROPERTY_COUNT_PERCENT`],
/// then the grade with that many properties by its APK weight.
pub fn roll_sigil_arcane_tiers<R: Rng + ?Sized>(rng: &mut R) -> &'static [u64] {
    let mut pick = rng.random_range(0..100u32);
    let count = SIGIL_ARCANE_PROPERTY_COUNT_PERCENT
        .iter()
        .find(|(_, pct)| {
            if pick < *pct {
                true
            } else {
                pick -= pct;
                false
            }
        })
        .map_or(3, |(n, _)| *n);
    pick_grade(rng, |g| g.2.len() == count).2
}

/// The GRADING property ids a ring or necklace can carry, or `None` for any
/// other item type.
fn pool(item_type: u64) -> Option<Vec<Uuid>> {
    let slot = match item_type {
        RING => "Ring",
        NECKLACE => "Necklace",
        _ => return None,
    };
    Some(
        GRADE_PROPERTIES
            .iter()
            .filter(|g| g.slot == slot)
            .filter_map(|g| Uuid::parse_str(g.uuid).ok())
            .collect(),
    )
}

/// Whether a ring (`RING`) or necklace (`NECKLACE`) can carry GRADING property
/// `id` — i.e. it is on that slot's APK list. `false` for any other item type.
pub fn slot_allows(item_type: u64, id: Uuid) -> bool {
    pool(item_type).is_some_and(|p| p.contains(&id))
}

/// Roll `(grade, GRADING)` for a freshly minted item of `item_type`.
///
/// `None` for anything that is not a ring or necklace — those are never graded.
pub fn roll<R: Rng + ?Sized>(
    item_type: u64,
    rng: &mut R,
) -> Option<(u64, Vec<ItemSingleProperty>)> {
    if !matches!(item_type, RING | NECKLACE) {
        return None;
    }
    let (_, _, tiers) = pick_grade(rng, |_| true);
    let grading = fill(item_type, tiers, rng)?;
    let grade = grading.iter().map(|p| p.tier).sum();
    Some((grade, grading))
}

/// GRADING for known `tiers` on a ring or necklace: one property per tier,
/// drawn uniformly and without repeats (retail never repeats one on the same
/// item) from the slot's whole APK list.
///
/// The arcane Sigil roll takes its tiers from [`roll_sigil_arcane_tiers`] and
/// its properties from here (#392, #393). `None` for anything that is not
/// jewellery.
pub fn fill<R: Rng + ?Sized>(
    item_type: u64,
    tiers: &[u64],
    rng: &mut R,
) -> Option<Vec<ItemSingleProperty>> {
    let mut pool = pool(item_type)?;
    let mut grading = Vec::with_capacity(tiers.len());
    for &tier in tiers {
        if pool.is_empty() {
            break;
        }
        let id = pool.swap_remove(rng.random_range(0..pool.len()));
        grading.push(ItemSingleProperty { id, tier });
    }
    Some(grading)
}

/// Give a freshly minted ring or necklace the grade retail would have.
///
/// Leaves the item alone when it is not jewellery, or when it already carries a
/// grade or GRADING (an authored or captured instance — those are what retail
/// sent and must survive verbatim). A graded item also sheds wear: retail never
/// sent `grade` beside `temperingLevel`/`durability`.
pub fn grade_if_bare<R: Rng + ?Sized>(
    item: &mut Item,
    items: &HashMap<Uuid, GameDataItem>,
    rng: &mut R,
) {
    if item.grade.is_some() || !item.properties.grading.is_empty() {
        return;
    }
    // A fixed (mandatory-property) template is never bare: its template IS its
    // properties (#8).
    if crate::fixed_templates::has_mandatory_properties(item.item_template_id) {
        return;
    }
    let Some(item_type) = items.get(&item.item_template_id).map(|t| t.r#type) else {
        return;
    };
    let Some((grade, grading)) = roll(item_type, rng) else {
        return;
    };
    item.grade = Some(grade);
    item.properties.grading = grading;
    item.tempering_level = 0;
    item.durability = 0.0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    fn provided_bonuses(grade: u64) -> &'static [u64] {
        JEWELRY_GRADES.iter().find(|g| g.0 == grade).unwrap().2
    }

    /// `blades_lib` fills a treasury chest jewel's GRADING from its grade with its
    /// own copy of the `_providedBonuses` (#392 audit); the two must not drift.
    #[test]
    fn blades_lib_grade_bonuses_are_these() {
        let ours: Vec<(u64, &[u64])> = JEWELRY_GRADES.iter().map(|g| (g.0, g.2)).collect();
        let theirs: Vec<(u64, &[u64])> =
            blades_lib::features::legendary_gear::JEWELRY_GRADE_BONUSES.to_vec();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn the_pools_are_the_apk_lists() {
        assert_eq!(pool(RING).unwrap().len(), 34, "RingGradePropertyList");
        assert_eq!(
            pool(NECKLACE).unwrap().len(),
            15,
            "NecklaceGradePropertyList"
        );
        // Negative control: the lookup must not answer for other item types.
        for t in [2u64, 3, 9, 1, 8] {
            assert!(pool(t).is_none(), "item type {t} must have no grading pool");
        }
    }

    /// Every rolled ring and necklace has the shape all 1,121 retail ones have.
    #[test]
    fn a_roll_has_the_retail_shape() {
        let ring: std::collections::HashSet<Uuid> = pool(RING).unwrap().into_iter().collect();
        let neck: std::collections::HashSet<Uuid> = pool(NECKLACE).unwrap().into_iter().collect();
        let mut rng = StdRng::seed_from_u64(240);
        for (item_type, own) in [(RING, &ring), (NECKLACE, &neck)] {
            for _ in 0..500 {
                let (grade, g) = roll(item_type, &mut rng).unwrap();
                assert!((1..=6).contains(&grade), "grade {grade}");
                assert_eq!(
                    grade,
                    g.iter().map(|p| p.tier).sum::<u64>(),
                    "grade is the tier sum"
                );
                let tiers: Vec<u64> = g.iter().map(|p| p.tier).collect();
                assert_eq!(
                    tiers,
                    provided_bonuses(grade),
                    "tiers follow _providedBonuses"
                );
                assert!(
                    g.iter().all(|p| own.contains(&p.id)),
                    "property from the wrong slot"
                );
                let distinct: std::collections::HashSet<_> = g.iter().map(|p| p.id).collect();
                assert_eq!(distinct.len(), g.len(), "a property repeated");
            }
        }
    }

    /// The draw follows `JewelryGradeData` weights and reaches every grade.
    #[test]
    fn grades_follow_the_apk_weights() {
        let mut rng = StdRng::seed_from_u64(7);
        let n = 60_000u32;
        let mut seen = [0u32; 7];
        for _ in 0..n {
            seen[roll(RING, &mut rng).unwrap().0 as usize] += 1;
        }
        for (grade, weight, _) in JEWELRY_GRADES {
            let want = f64::from(weight) / 100.0;
            let got = f64::from(seen[grade as usize]) / f64::from(n);
            assert!(
                (got - want).abs() < 0.01,
                "grade {grade}: {got:.3} vs {want:.3}"
            );
        }
    }

    #[test]
    fn non_jewellery_is_never_graded() {
        let mut rng = StdRng::seed_from_u64(1);
        for t in [2u64, 3, 9] {
            assert!(roll(t, &mut rng).is_none());
        }
    }

    /// #8: `grade_if_bare` (craft results, enchants, merchant mint) leaves a
    /// fixed ring alone and still grades an ordinary one.
    #[test]
    fn a_fixed_ring_is_never_bare() {
        let mut items = HashMap::new();
        let shock: Uuid = "85c44edf-2fc6-4b4d-b8a9-3340f127f9f0".parse().unwrap();
        let pearl: Uuid = "869bf6f4-f7fb-43cf-b264-02b84f9a5425".parse().unwrap();
        for id in [shock, pearl] {
            items.insert(
                id,
                GameDataItem {
                    name: String::new(),
                    r#type: RING,
                },
            );
        }
        let bare = |t: Uuid| -> Item {
            serde_json::from_value(serde_json::json!({
                "itemTemplateId": t, "temperingLevel": 0, "durability": 0.0
            }))
            .unwrap()
        };
        let mut rng = StdRng::seed_from_u64(8);
        for _ in 0..20 {
            let mut fixed = bare(shock);
            grade_if_bare(&mut fixed, &items, &mut rng);
            assert_eq!(fixed.grade, None);
            assert!(fixed.properties.grading.is_empty());
            let mut plain = bare(pearl);
            grade_if_bare(&mut plain, &items, &mut rng);
            assert!(plain.grade.is_some());
        }
    }
}
