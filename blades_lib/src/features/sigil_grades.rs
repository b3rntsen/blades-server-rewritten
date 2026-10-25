//! The grade retail rolled on Sigil-shop arcane gear.
//!
//! THE BUG (#167). Ninety global-shop offers carry `arcaneTier > 0` and an EMPTY
//! authored `grading`: retail rolled the grade when you bought the item, and the
//! APK records only that a roll happens, never its outcome.
//! `grant_from_offer_contents` refused those rather than hand an ungraded item to
//! someone who paid, so most of the Sigil shop could not be bought at all.
//!
//! WHAT THIS IS, AND IS NOT. It is the distribution of OUTCOMES retail produced,
//! not retail's rule. The rule existed only in the server being replaced and is
//! not recoverable from anything we hold. The reporter was told that explicitly
//! and asked for the distribution anyway, which is the right call: an item whose
//! grade is drawn from what retail really handed out is closer to the game than
//! an item nobody can buy.
//!
//! 243 grants over 19 of the 90 offers, keyed by `arcaneTier` because that is the
//! only feature the outcomes separate on. The ungraded outcome is kept and drawn
//! like any other — 111 of the 235 arcane-tier-2 grants carried no grade at all,
//! so removing it would make every purchase graded, which retail's was not.
//!
//! The GRADING property DEFINITION ids are real, taken from the captured grants.
//! An earlier cut of the corpus stored only the tiers, which would have left this
//! module inventing ids for properties that have to name a specific bonus.
//!
//! WHY THE DRAW IS NOT DETERMINISTIC, unlike dungeon loot. A dungeon is described
//! to the client in advance and must still hold the same thing when opened, so
//! those rolls are seeded. A purchase has no such requirement: the response IS
//! the grant, and retail rolled afresh every time. Buying the same offer twice
//! should be able to give two different grades, which is the whole complaint.

use serde::Deserialize;

use crate::user_data::ItemSingleProperty;

static SIGIL_ARCANE_GRADES_RAW: &str = include_str!("../sigil_arcane_grades.json");

#[derive(Deserialize)]
struct Corpus {
    #[serde(rename = "byArcaneTier")]
    by_arcane_tier: std::collections::HashMap<String, Tier>,
}

#[derive(Deserialize)]
struct Tier {
    observations: u64,
    results: Vec<Outcome>,
}

#[derive(Deserialize, Clone)]
struct Outcome {
    grading: Vec<ItemSingleProperty>,
    n: u64,
}

fn corpus() -> &'static Corpus {
    static TABLE: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(SIGIL_ARCANE_GRADES_RAW).unwrap_or_else(|_| Corpus {
            by_arcane_tier: std::collections::HashMap::new(),
        })
    })
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// How many retail grants back this arcane tier, for tests and diagnostics.
pub fn observations_for(arcane_tier: u64) -> u64 {
    corpus()
        .by_arcane_tier
        .get(&arcane_tier.to_string())
        .map(|t| t.observations)
        .unwrap_or(0)
}

/// Whether a grade can be rolled for this arcane tier at all.
pub fn can_roll(arcane_tier: u64) -> bool {
    corpus()
        .by_arcane_tier
        .get(&arcane_tier.to_string())
        .is_some_and(|t| !t.results.is_empty())
}

/// Roll the GRADING for one purchase of an arcane item.
///
/// `nonce` must differ per purchase. Returns an empty vector when the roll came
/// out ungraded, which is a real outcome and the commonest one. `None` only when
/// the tier is absent from the corpus, which the caller must treat as "still not
/// grantable" rather than "no grade".
pub fn roll_grading(arcane_tier: u64, nonce: u64) -> Option<Vec<ItemSingleProperty>> {
    roll_grading_where(arcane_tier, nonce, |_| true)
}

/// [`roll_grading`] over only the outcomes `keep` accepts, weighted as before.
///
/// The corpus mixes every arcane item retail sold: at tier 2, 111 of 235 grants
/// are ungraded (the weapons and armour, which are never graded — #220), 92 are
/// necklaces and 32 are rings. Drawing a NECKLACE's grade from all of them hands
/// it no grade half the time and a ring's properties one time in seven (#386), so
/// the caller narrows the draw to the outcomes its item could have had. `None`
/// when no outcome of this tier survives the filter.
pub fn roll_grading_where(
    arcane_tier: u64,
    nonce: u64,
    keep: impl Fn(&[ItemSingleProperty]) -> bool,
) -> Option<Vec<ItemSingleProperty>> {
    let tier = corpus().by_arcane_tier.get(&arcane_tier.to_string())?;
    let kept: Vec<&Outcome> = tier.results.iter().filter(|o| keep(&o.grading)).collect();
    let total: u64 = kept.iter().map(|r| r.n).sum();
    if total == 0 {
        return None;
    }
    let mut pick = mix(nonce ^ arcane_tier.rotate_left(29)) % total;
    for outcome in &kept {
        if pick < outcome.n {
            return Some(outcome.grading.clone());
        }
        pick -= outcome.n;
    }
    kept.last().map(|o| o.grading.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus must be compiled in and hold what was mined. Parsed explicitly
    /// so a deserialization failure names itself rather than silently degrading
    /// to "nothing is grantable", which looks exactly like the bug.
    #[test]
    fn the_grade_corpus_loads() {
        if let Err(e) = serde_json::from_str::<Corpus>(SIGIL_ARCANE_GRADES_RAW) {
            panic!("the corpus failed to parse: {e}");
        }
        assert_eq!(observations_for(2), 235, "arcane tier 2 grants");
        assert_eq!(observations_for(1), 8, "arcane tier 1 grants");
        assert!(can_roll(1) && can_roll(2));
        assert!(
            !can_roll(9),
            "a tier retail never showed must not be rollable"
        );
    }

    /// THE BUG: these offers could not be bought at all.
    #[test]
    fn an_arcane_item_can_now_be_graded() {
        assert!(roll_grading(2, 1).is_some(), "arcane tier 2 must roll");
        assert!(
            roll_grading(9, 1).is_none(),
            "an unknown tier must stay ungrantable"
        );
    }

    /// Ungraded is a real outcome — 111 of the 235 tier-2 grants carried no
    /// grade. A build where every purchase is graded is as wrong as one where
    /// none can be bought.
    #[test]
    fn ungraded_remains_the_commonest_outcome() {
        let mut ungraded = 0;
        let mut graded = 0;
        for nonce in 0..400u64 {
            match roll_grading(2, nonce).unwrap().len() {
                0 => ungraded += 1,
                _ => graded += 1,
            }
        }
        assert!(
            ungraded > 0,
            "no purchase came out ungraded; retail's mostly did"
        );
        assert!(graded > 0, "no purchase came out graded at all");
        // retail: 111 of 235 ungraded, 47%
        let share = ungraded as f64 / (ungraded + graded) as f64;
        assert!(
            (0.30..0.65).contains(&share),
            "{:.0}% ungraded against retail's 47%",
            share * 100.0
        );
    }

    /// Buying twice must be able to give two different grades — that is the
    /// behaviour being restored.
    #[test]
    fn two_purchases_can_differ() {
        let mut seen = std::collections::HashSet::new();
        for nonce in 0..200u64 {
            let g = roll_grading(2, nonce).unwrap();
            let mut key: Vec<_> = g.iter().map(|p| (p.id, p.tier)).collect();
            key.sort();
            seen.insert(format!("{key:?}"));
        }
        assert!(
            seen.len() > 5,
            "200 purchases produced only {} outcomes",
            seen.len()
        );
    }

    /// Every property must name a real definition id and a sane tier. An
    /// invented id would be a property the client cannot resolve.
    #[test]
    fn every_rolled_property_is_a_real_one() {
        let mut checked = 0;
        for nonce in 0..300u64 {
            for p in roll_grading(2, nonce).unwrap() {
                assert!(!p.id.is_nil(), "a grading property with a nil id");
                assert!(
                    (1..=5).contains(&p.tier),
                    "implausible grading tier {}",
                    p.tier
                );
                checked += 1;
            }
        }
        assert!(checked > 100, "only {checked} properties checked");
    }
}
