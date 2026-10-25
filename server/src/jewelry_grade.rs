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
//! THE RULE:
//!
//! * the property COUNT by [`JEWELRY_PROPERTY_COUNT_PERCENT`] — 8% one, 17% two,
//!   75% three — per HauDrauf #393 (2026-10-15, "überall, keine Unterschiede":
//!   every ring and necklace from every source, as in the Sigil shop, except the
//!   Elder and Legendary store chests, which keep retail's grade);
//! * `JewelryGradeData._jewelryGradeData` — the grade among those with that many
//!   properties by its APK weight, and the grade fixes the tiers of its GRADING
//!   properties (`_providedBonuses`).
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
//! WHAT IS NOT KNOWN. Retail's merchant record is four purchases (grades 1, 1,
//! 2, 3); the APK weights alone (22 / 20 / 58% one / two / three) fitted them as
//! well as anything. The count split is HauDrauf's retail experience, which the
//! ticket owner trusts on retail feel (#393).
//!
//! The rule itself lives in `blades_lib::features::jewelry_grades`, because the
//! chest and enemy-loot rolls are there; [`roll_tiers`] is its draw here. Arcane
//! jewellery (the Sigil shop) uses [`roll_tiers`] and [`fill`] directly; this
//! module never touches an item that already has a grade or GRADING.

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

#[cfg(test)]
pub use blades_lib::features::jewelry_grades::{JEWELRY_GRADES, JEWELRY_PROPERTY_COUNT_PERCENT};

/// The GRADING tiers (`_providedBonuses`) of one randomly graded ring or
/// necklace: the property count by [`JEWELRY_PROPERTY_COUNT_PERCENT`], then the
/// grade with that many properties by its APK weight. Every source (#393).
///
/// Two draws, `0..100` then `0..total weight`, exactly as the Sigil roll drew
/// them before it was shared, so a Sigil purchase nonce still rolls the same
/// item (#523).
pub fn roll_tiers<R: Rng + ?Sized>(rng: &mut R) -> &'static [u64] {
    let count_draw = rng.random_range(0..100u32);
    blades_lib::features::jewelry_grades::tiers(count_draw, |total| rng.random_range(0..total))
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
/// Only the economy audit and shop tests check this; live rolls draw from
/// [`pool`] directly.
#[cfg(test)]
pub fn slot_allows(item_type: u64, id: Uuid) -> bool {
    pool(item_type).is_some_and(|p| p.contains(&id))
}

/// Roll `(grade, GRADING)` for a freshly minted item of `item_type`: tiers by
/// [`roll_tiers`], properties by [`fill`].
///
/// `None` for anything that is not a ring or necklace — those are never graded.
pub fn roll<R: Rng + ?Sized>(
    item_type: u64,
    rng: &mut R,
) -> Option<(u64, Vec<ItemSingleProperty>)> {
    if !matches!(item_type, RING | NECKLACE) {
        return None;
    }
    let grading = fill(item_type, roll_tiers(rng), rng)?;
    let grade = grading.iter().map(|p| p.tier).sum();
    Some((grade, grading))
}

/// GRADING for known `tiers` on a ring or necklace: one property per tier,
/// drawn uniformly and without repeats (retail never repeats one on the same
/// item) from the slot's whole APK list.
///
/// [`roll`] and the arcane Sigil roll take their tiers from [`roll_tiers`] and
/// their properties from here (#392, #393). `None` for anything that is not
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
    use rand::{SeedableRng, rngs::StdRng};

    fn provided_bonuses(grade: u64) -> &'static [u64] {
        JEWELRY_GRADES.iter().find(|g| g.0 == grade).unwrap().2
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

    /// #393: one, two and three properties 8 / 17 / 75%, and within three the
    /// grades 3-6 by the APK weights 18/16/14/10; every grade is reached. Under
    /// the APK weights alone (the rule before) grade 1 was 22%.
    #[test]
    fn grades_follow_the_property_count_split() {
        let mut rng = StdRng::seed_from_u64(7);
        let n = 60_000u32;
        let mut seen = [0u32; 7];
        for _ in 0..n {
            seen[roll(RING, &mut rng).unwrap().0 as usize] += 1;
        }
        for (grade, weight, tiers) in JEWELRY_GRADES {
            let count_pct = JEWELRY_PROPERTY_COUNT_PERCENT
                .iter()
                .find(|(c, _)| *c == tiers.len())
                .unwrap()
                .1;
            let same_count: u32 = JEWELRY_GRADES
                .iter()
                .filter(|g| g.2.len() == tiers.len())
                .map(|g| g.1)
                .sum();
            let want = f64::from(count_pct) / 100.0 * f64::from(weight) / f64::from(same_count);
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
