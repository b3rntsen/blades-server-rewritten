//! How many GRADING properties a randomly graded ring or necklace gets, and at
//! which grade — the one rule every source uses (#393).
//!
//! THE RULE. The property count is drawn by [`JEWELRY_PROPERTY_COUNT_PERCENT`]
//! (8% one, 17% two, 75% three); the grade among those with that many properties
//! by the APK [`JEWELRY_GRADES`] weights (with three properties: grades 3-6 at
//! 18/16/14/10); the grade fixes the tiers (`_providedBonuses`). The properties
//! themselves are each caller's own uniform draw without repeats from the slot's
//! APK list.
//!
//! WHERE IT APPLIES: every ring and necklace the server grades at random — town
//! merchants, the Sigil shop, Wooden/Silver/Gold treasury
//! chests, the Golden store chest, and dungeon enemy loot. NOT the Elder and
//! Legendary store chests: HauDrauf (#393, 2026-10-15) wants their jewellery as
//! retail paid it, so they keep the recorded shell's grade (5 / 7 / 88%). Never to a fixed
//! (mandatory-property) template, to authored GRADING the server keeps as it is,
//! or to an item the player already owns. CRAFTED jewellery has its own grade
//! split instead (#560, [`CRAFTED_JEWELRY_GRADE_PERCENT`]).
//!
//! This crate holds it because the chest and enemy-loot rolls live here; the
//! server's `jewelry_grade` re-exports it.

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

/// Percent of randomly graded rings and necklaces carrying 1, 2 and 3 GRADING
/// properties: `(property count, percent)`, summing to 100. Every source but the
/// Elder and Legendary chests (see the module doc).
///
/// Per HauDrauf, tracker #393. First for the Sigil shop (2026-10-14: "thousands
/// of rings" bought in retail, one property maybe 5%, two maybe 10%, the vast
/// majority three; a first step of 75% three, the rest split 1 : 2), then, after
/// testing it, for every ring and necklace (2026-10-15): "überall, keine
/// Unterschiede" — everywhere, there were no differences — except the Elder,
/// Legendary and Golden chests, which he asked to keep as they were; then the
/// Golden chest too (same day: the same split as everything else).
///
/// THE RECORD is thin and does not all agree, which is why HauDrauf's word
/// decides it. The 542 Legendary/Elder/Golden chest jewels split 5 / 7 / 88, and
/// the 451 distinct tier-2 arcane jewels in retail imports 5 / 9 / 86, both with
/// him; the APK weights alone give 22 / 20 / 58, and so do the 102 treasury
/// chest jewels (25 / 20 / 57) and the 552 enemy-loot jewels (34 / 30 / 36).
pub const JEWELRY_PROPERTY_COUNT_PERCENT: [(usize, u32); 3] = [(1, 8), (2, 17), (3, 75)];

const _: () = {
    let mut sum = 0;
    let mut i = 0;
    while i < JEWELRY_PROPERTY_COUNT_PERCENT.len() {
        sum += JEWELRY_PROPERTY_COUNT_PERCENT[i].1;
        i += 1;
    }
    assert!(sum == 100, "JEWELRY_PROPERTY_COUNT_PERCENT must sum to 100");
};

/// The GRADING tiers of one randomly graded ring or necklace.
///
/// `count_draw` is uniform in `0..100` and picks the property count by
/// [`JEWELRY_PROPERTY_COUNT_PERCENT`]; `grade_draw(total)` returns a value
/// uniform in `0..total` and picks the grade with that many properties by its
/// APK weight. The grade is the sum of the returned tiers.
pub fn tiers(count_draw: u32, grade_draw: impl FnOnce(u32) -> u32) -> &'static [u64] {
    let mut pick = count_draw % 100;
    let count = JEWELRY_PROPERTY_COUNT_PERCENT
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
    let kept = JEWELRY_GRADES.iter().filter(|g| g.2.len() == count);
    let total: u32 = kept.clone().map(|g| g.1).sum();
    let mut at = grade_draw(total) % total;
    for g in kept {
        if at < g.1 {
            return g.2;
        }
        at -= g.1;
    }
    JEWELRY_GRADES[JEWELRY_GRADES.len() - 1].2
}

/// Percent of CRAFTED rings and necklaces at each grade: `(grade, percent)`,
/// summing to 100. HauDrauf's rule, tracker #560 (corrected 2026-11-19, msg 3600):
/// "Ringe und Ketten: 6 35%, 5 25%, 4 20%, 3 15%, 2 5%, 1 0%, 6 ist dabei die beste
/// Qualität" for the ones you make yourself; grade 1 is never crafted. Every other
/// source keeps [`tiers`]. Not retail. Revert: make the craft path call
/// `jewelry_grade::grade_if_bare` again.
pub const CRAFTED_JEWELRY_GRADE_PERCENT: [(u64, u32); 5] =
    [(2, 5), (3, 15), (4, 20), (5, 25), (6, 35)];

const _: () = {
    let mut sum = 0;
    let mut i = 0;
    while i < CRAFTED_JEWELRY_GRADE_PERCENT.len() {
        sum += CRAFTED_JEWELRY_GRADE_PERCENT[i].1;
        i += 1;
    }
    assert!(sum == 100, "CRAFTED_JEWELRY_GRADE_PERCENT must sum to 100");
};

/// The GRADING tiers of one crafted ring or necklace (#560): the grade by
/// [`CRAFTED_JEWELRY_GRADE_PERCENT`] from `draw` (uniform in `0..100`), its tiers
/// the APK's `providedBonuses` for that grade ([`JEWELRY_GRADES`]).
pub fn crafted_tiers(draw: u32) -> &'static [u64] {
    let mut pick = draw % 100;
    let grade = CRAFTED_JEWELRY_GRADE_PERCENT
        .iter()
        .find(|(_, pct)| {
            if pick < *pct {
                true
            } else {
                pick -= pct;
                false
            }
        })
        .map_or(6, |(g, _)| *g);
    JEWELRY_GRADES
        .iter()
        .find(|g| g.0 == grade)
        .map_or(JEWELRY_GRADES[5].2, |g| g.2)
}

/// [`tiers`] from a 64-bit seed (the chest and enemy-loot rolls).
pub fn tiers_from_seed(seed: u64) -> &'static [u64] {
    use super::store_bundles::mix;
    tiers((mix(seed) % 100) as u32, |total| {
        (mix(seed ^ 0x5851_F42D_4C95_7F2D) % u64::from(total)) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #560: every draw 0..100 maps to grade 2-6 in the stated split, never 1,
    /// and the tiers are that grade's APK `providedBonuses`.
    #[test]
    fn crafted_grades_are_haudraufs_split() {
        let mut by_grade = [0u32; 7];
        for d in 0..100 {
            let t = crafted_tiers(d);
            let g = t.iter().sum::<u64>() as usize;
            assert_eq!(
                t,
                JEWELRY_GRADES.iter().find(|j| j.0 == g as u64).unwrap().2
            );
            by_grade[g] += 1;
        }
        assert_eq!(by_grade, [0, 0, 5, 15, 20, 25, 35]);
    }

    /// Every count and every grade is reachable, at the stated shares.
    #[test]
    fn the_split_and_the_grades_are_the_stated_ones() {
        let n = 100_000u64;
        let mut by_count = [0u64; 4];
        let mut by_grade = [0u64; 7];
        for s in 0..n {
            let t = tiers_from_seed(s);
            by_count[t.len()] += 1;
            by_grade[t.iter().sum::<u64>() as usize] += 1;
        }
        for (count, pct) in JEWELRY_PROPERTY_COUNT_PERCENT {
            let got = by_count[count] as f64 * 100.0 / n as f64;
            assert!(
                (got - f64::from(pct)).abs() < 1.0,
                "{count}: {got:.2}% vs {pct}%"
            );
        }
        // Grades 1 and 2 are the only one- and two-property grades.
        assert_eq!(by_grade[1], by_count[1]);
        assert_eq!(by_grade[2], by_count[2]);
        for (grade, weight, _) in JEWELRY_GRADES.iter().filter(|g| g.2.len() == 3) {
            let got = by_grade[*grade as usize] as f64 / by_count[3] as f64;
            let want = f64::from(*weight) / 58.0;
            assert!(
                (got - want).abs() < 0.01,
                "grade {grade}: {got:.3} vs {want:.3}"
            );
        }
    }

    /// The draws map onto the table edges exactly.
    #[test]
    fn the_draw_edges() {
        assert_eq!(tiers(0, |_| 0), &[1]);
        assert_eq!(tiers(7, |_| 0), &[1]);
        assert_eq!(tiers(8, |_| 0), &[1, 1]);
        assert_eq!(tiers(24, |_| 0), &[1, 1]);
        assert_eq!(tiers(25, |_| 0), &[1, 1, 1]);
        assert_eq!(tiers(99, |t| t - 1), &[2, 2, 2]);
        assert_eq!(tiers(99, |_| 18), &[2, 1, 1]);
    }
}
