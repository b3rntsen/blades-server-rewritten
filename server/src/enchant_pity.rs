//! The double-Ravage pity timer (tracker #415, owner decision d17). NOT retail: the
//! APK's enchant tables carry no counter or guarantee, and this is an owner-approved
//! Newblades rule on top of them.
//!
//! A weapon gets both Ravage Magicka and Ravage Stamina only when it rolls two
//! secondaries and those two are the pair. From `enchanting.json` (every weapon table
//! is the same): P(two secondaries) is 0.15 for a plain weapon and 1 for an arcane-2
//! one, and the pair is 2 x 0.07 x 0.07 / 0.93 = 1.054% of two-secondary draws, so
//!
//! | odds class | P(pair) per enchant | expected tries |
//! |------------|---------------------|----------------|
//! | `normal`   | 0.15 x 1.054% = 0.158% | 632.7 (owner: 630) |
//! | `arcane1`  | 0 (exactly one secondary) | never; no timer |
//! | `arcane2`  | 1.054%              | 94.9 (owner: 95) |
//!
//! The soul gem's grade only sets the TIER of the roll, never the count or the pool,
//! so it does not change these odds; the counter is kept per odds class
//! ([`EnchantingData::odds_class`]) instead.
//!
//! Each counted enchant either produces the pair naturally (the class's counter is
//! cleared), or misses: once a class has `tries` misses in a row, the next miss is
//! replaced by primary + Ravage Magicka + Ravage Stamina and the counter is cleared.
//! The counter lives in `server_state.enchantPityMisses`, which the client never sees.

use std::collections::BTreeMap;

use blades_lib::static_data::EnchantingData;
use blades_lib::user_data::{Item, ItemSingleProperty};
use uuid::Uuid;

/// `Weapon Ravage Magicka` (`WeaponRavageMagickaPropertyLogic`).
pub const RAVAGE_MAGICKA: Uuid = Uuid::from_u128(0x7cdb7179_4cd4_466e_8b77_4caf2fddb268);
/// `Weapon Ravage Stamina` (`WeaponRavageStaminaPropertyLogic`).
pub const RAVAGE_STAMINA: Uuid = Uuid::from_u128(0x9be2e7e9_5ef5_4ee8_aeec_b864bca07f07);
/// `BGS.Game.Inventory.ItemType.Weapon`.
const WEAPON_ITEM_TYPE: i64 = 2;

/// What the pity timer did to one enchant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PityOutcome {
    /// Not a counted enchant: timer off, not a weapon, a recipe outside the APK
    /// tables, a class that cannot roll two secondaries, or no threshold for it.
    NotCounted,
    /// The roll produced the pair by itself; the class's counter was cleared.
    NaturalHit { class: String },
    /// No pair; the class's counter is now `misses`.
    Miss { class: String, misses: u32 },
    /// No pair after `misses` misses (>= `threshold`): the pair was forced and the
    /// counter cleared.
    Forced {
        class: String,
        misses: u32,
        threshold: u32,
    },
}

/// The odds class and threshold when this enchant counts toward the timer.
fn counted_class(
    enchanting: &EnchantingData,
    before: &Item,
    recipe_id: Uuid,
) -> Option<(String, u32)> {
    let pity = &enchanting.tuning.double_ravage_pity;
    if !pity.enabled {
        return None;
    }
    // Only a roll from the APK tables (`roll_enchant`); the captured-outcome
    // fallback is not a roll this timer can reason about.
    let recipe = enchanting.recipes.get(&recipe_id)?;
    if recipe.property == RAVAGE_MAGICKA || recipe.property == RAVAGE_STAMINA {
        return None;
    }
    let table = enchanting.table_for(&before.item_template_id)?;
    if table.item_type != Some(WEAPON_ITEM_TYPE) {
        return None;
    }
    let in_pool = |id: Uuid| {
        table
            .properties
            .iter()
            .any(|p| p.id == id && p.weight > 0.0)
    };
    if !(in_pool(RAVAGE_MAGICKA) && in_pool(RAVAGE_STAMINA)) {
        return None;
    }
    // A class that can never draw two secondaries (arcane 1) cannot carry the pair;
    // forcing it would break its fixed count.
    let odds = enchanting.roll_count_odds(table, before.arcane_tier);
    if odds.iter().skip(2).all(|p| *p <= 0.0) {
        return None;
    }
    let class = enchanting.odds_class(before.arcane_tier);
    let threshold = *pity.tries.get(&class)?;
    (threshold > 0).then_some((class, threshold))
}

/// Whether an enchant (primary first, then secondaries) carries both Ravages.
pub fn has_double_ravage(enchanting: &[ItemSingleProperty]) -> bool {
    let secs = || enchanting.iter().skip(1);
    secs().any(|p| p.id == RAVAGE_MAGICKA) && secs().any(|p| p.id == RAVAGE_STAMINA)
}

/// Run the timer on one enchant. `before` is the item as it went in, `rolled` the item
/// [`crate::craft`]'s roll produced (rewritten in place when the pair is forced), and
/// `misses` the character's `server_state.enchant_pity_misses`.
pub fn apply(
    enchanting: &EnchantingData,
    before: &Item,
    recipe_id: Uuid,
    rolled: &mut Item,
    misses: &mut BTreeMap<String, u32>,
) -> PityOutcome {
    let Some((class, threshold)) = counted_class(enchanting, before, recipe_id) else {
        return PityOutcome::NotCounted;
    };
    let so_far = misses.get(&class).copied().unwrap_or(0);
    if has_double_ravage(&rolled.properties.enchanting) {
        misses.remove(&class);
        return PityOutcome::NaturalHit { class };
    }
    if so_far >= threshold {
        let tier = enchanting.recipes[&recipe_id].tier;
        let e = &mut rolled.properties.enchanting;
        e.truncate(1);
        e.extend([RAVAGE_MAGICKA, RAVAGE_STAMINA].map(|id| ItemSingleProperty { id, tier }));
        misses.remove(&class);
        return PityOutcome::Forced {
            class,
            misses: so_far,
            threshold,
        };
    }
    misses.insert(class.clone(), so_far + 1);
    PityOutcome::Miss {
        class,
        misses: so_far + 1,
    }
}

/// The journald line for a forced pair. Deliberately NOT an `enchant rolled:` line:
/// the odds tally reads those, and a forced pair is not a draw from the tables.
pub fn forced_log_line(
    character_id: Uuid,
    item_template_id: Uuid,
    recipe_id: Uuid,
    outcome: &PityOutcome,
    rolled: &[ItemSingleProperty],
) -> Option<String> {
    let PityOutcome::Forced {
        class,
        misses,
        threshold,
    } = outcome
    else {
        return None;
    };
    let primary = rolled.first()?;
    let secondaries: Vec<String> = rolled.iter().skip(1).map(|p| p.id.to_string()).collect();
    Some(format!(
        "enchant pity forced: character={character_id} template={item_template_id} \
         recipe={recipe_id} class={class} misses={misses} threshold={threshold} \
         primary={} tier={} secondaries=[{}]",
        primary.id,
        primary.tier,
        secondaries.join(","),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::static_data::{DoubleRavagePity, EnchantTuning};
    use blades_lib::user_data::ItemPropertiesAll;

    const MADNESS_BATTLEAXE: Uuid = Uuid::from_u128(0x80344fa2_2c4a_4ef3_a5e3_7533b712a740);
    const GOLD_EMERALD_RING: Uuid = Uuid::from_u128(0xd408a912_3a4f_4b2a_bf4d_a55b78910f08);
    const DRAGONSCALE_ARMOR: Uuid = Uuid::from_u128(0x659dd496_f71e_4cf8_aaee_8f0c4723410e);
    /// `Enchant.Recipe.DamageMagickaT10`.
    const MAGICKA_DAMAGE_T10: Uuid = Uuid::from_u128(0xe0d48d1a_8d8e_4c76_bfeb_970d80f9b838);
    /// A 0.18-weight weapon secondary (not a Ravage).
    const FORTIFY_A: Uuid = Uuid::from_u128(0xbf107d7d_8777_4411_b07a_56819a58a709);

    fn enchanting() -> EnchantingData {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/enchanting.json");
        let mut e: EnchantingData =
            serde_json::from_slice(&std::fs::read(p).expect("enchanting.json")).unwrap();
        e.tuning = EnchantTuning::default();
        e
    }

    fn item(template: Uuid, arcane_tier: Option<u64>) -> Item {
        Item {
            item_template_id: template,
            tempering_level: 10,
            durability: 325.0,
            grade: None,
            arcane_tier,
            properties: ItemPropertiesAll::default(),
        }
    }

    fn with_enchant(mut it: Item, secondaries: &[Uuid]) -> Item {
        let primary = enchanting().recipes[&MAGICKA_DAMAGE_T10].property;
        it.properties.enchanting = std::iter::once(primary)
            .chain(secondaries.iter().copied())
            .map(|id| ItemSingleProperty { id, tier: 10 })
            .collect();
        it
    }

    fn misses(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn the_defaults_are_the_owners_numbers_and_on() {
        let d = DoubleRavagePity::default();
        assert!(d.enabled);
        assert_eq!(d.tries, misses(&[("normal", 630), ("arcane2", 95)]));
        // The expected-tries formula the thresholds come from, against the shipped
        // tables: 1 / (P(two secondaries) x P(the pair | two draws)).
        let e = enchanting();
        let t = e.table_for(&MADNESS_BATTLEAXE).unwrap();
        let w = |id| t.properties.iter().find(|p| p.id == id).unwrap().weight;
        let total: f64 = t.properties.iter().map(|p| p.weight).sum();
        let (m, s) = (w(RAVAGE_MAGICKA), w(RAVAGE_STAMINA));
        let pair = m / total * s / (total - m) + s / total * m / (total - s);
        let normal = 1.0 / (t.count_odds[2] * pair);
        let arcane2 = 1.0 / pair;
        assert_eq!(normal.round() as u32, 633, "{normal}");
        assert_eq!(arcane2.round() as u32, 95, "{arcane2}");
    }

    /// The decision itself: after `tries` misses the next miss is the pair. Red
    /// without the timer (the roll is left alone and the count keeps climbing).
    #[test]
    fn the_pair_is_forced_after_the_threshold_of_misses() {
        let e = enchanting();
        let before = item(MADNESS_BATTLEAXE, None);
        let mut m = misses(&[("normal", 630)]);
        let mut rolled = with_enchant(before.clone(), &[FORTIFY_A]);
        let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Forced {
                class: "normal".into(),
                misses: 630,
                threshold: 630
            }
        );
        let primary = e.recipes[&MAGICKA_DAMAGE_T10].property;
        assert_eq!(
            rolled.properties.enchanting,
            vec![
                ItemSingleProperty {
                    id: primary,
                    tier: 10
                },
                ItemSingleProperty {
                    id: RAVAGE_MAGICKA,
                    tier: 10
                },
                ItemSingleProperty {
                    id: RAVAGE_STAMINA,
                    tier: 10
                },
            ]
        );
        assert!(
            m.is_empty(),
            "the counter restarts after a forced pair: {m:?}"
        );

        // One short of the threshold is still an ordinary miss.
        let mut m = misses(&[("normal", 629)]);
        let mut rolled = with_enchant(before.clone(), &[FORTIFY_A]);
        let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Miss {
                class: "normal".into(),
                misses: 630
            }
        );
        assert_eq!(
            rolled.properties.enchanting.len(),
            2,
            "the roll is untouched"
        );
    }

    /// Statistically: 3,000 real arcane-2 rolls never go more than 95 enchants
    /// without the pair. Without the timer a run of 96+ misses is near-certain in
    /// 3,000 tries at 1 in 95 (each run of misses reaches 96 with p = 0.36).
    #[test]
    fn real_rolls_never_go_past_the_threshold() {
        use rand::SeedableRng;
        let e = enchanting();
        let before = item(MADNESS_BATTLEAXE, Some(2));
        let mut rng = rand::rngs::StdRng::seed_from_u64(415);
        let mut m = BTreeMap::new();
        let (mut streak, mut longest, mut forced, mut natural) = (0u32, 0u32, 0, 0);
        for _ in 0..3_000 {
            let mut rolled =
                crate::craft::roll_item_for_test(&before, MAGICKA_DAMAGE_T10, &e, &mut rng);
            match apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m) {
                PityOutcome::Forced { .. } => forced += 1,
                PityOutcome::NaturalHit { .. } => natural += 1,
                PityOutcome::Miss { .. } => {}
                PityOutcome::NotCounted => panic!("an arcane-2 battleaxe enchant counts"),
            }
            if has_double_ravage(&rolled.properties.enchanting) {
                streak = 0;
            } else {
                streak += 1;
                longest = longest.max(streak);
            }
        }
        assert!(longest <= 95, "longest run without the pair: {longest}");
        assert!(
            forced > 0 && natural > 0,
            "forced {forced}, natural {natural}"
        );
    }

    #[test]
    fn a_natural_pair_resets_the_counter() {
        let e = enchanting();
        let before = item(MADNESS_BATTLEAXE, None);
        let mut m = misses(&[("normal", 400), ("arcane2", 50)]);
        let mut rolled = with_enchant(before.clone(), &[RAVAGE_STAMINA, RAVAGE_MAGICKA]);
        let kept = rolled.clone();
        let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::NaturalHit {
                class: "normal".into()
            }
        );
        assert_eq!(rolled, kept, "a natural pair is left as rolled");
        assert_eq!(m, misses(&[("arcane2", 50)]), "only its own class resets");
    }

    #[test]
    fn each_odds_class_keeps_its_own_counter() {
        let e = enchanting();
        // The normal counter is at its threshold, but an arcane-2 enchant only
        // advances the arcane-2 one ...
        let arcane = item(MADNESS_BATTLEAXE, Some(2));
        let mut m = misses(&[("normal", 630), ("arcane2", 10)]);
        let mut rolled = with_enchant(arcane.clone(), &[FORTIFY_A, RAVAGE_MAGICKA]);
        let out = apply(&e, &arcane, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Miss {
                class: "arcane2".into(),
                misses: 11
            }
        );
        assert_eq!(m, misses(&[("normal", 630), ("arcane2", 11)]));
        // ... and the arcane-2 one at ITS threshold forces only arcane-2 enchants.
        let plain = item(MADNESS_BATTLEAXE, None);
        let mut m = misses(&[("normal", 3), ("arcane2", 95)]);
        let mut rolled = with_enchant(plain.clone(), &[]);
        let out = apply(&e, &plain, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Miss {
                class: "normal".into(),
                misses: 4
            }
        );
        let mut rolled = with_enchant(arcane.clone(), &[FORTIFY_A, RAVAGE_MAGICKA]);
        assert!(matches!(
            apply(&e, &arcane, MAGICKA_DAMAGE_T10, &mut rolled, &mut m),
            PityOutcome::Forced { ref class, .. } if class == "arcane2"
        ));
        assert_eq!(m, misses(&[("normal", 4)]));
    }

    #[test]
    fn non_weapons_and_arcane_one_are_never_counted_or_forced() {
        let e = enchanting();
        for (what, before) in [
            ("ring", item(GOLD_EMERALD_RING, None)),
            ("body armor", item(DRAGONSCALE_ARMOR, None)),
            ("arcane-1 weapon", item(MADNESS_BATTLEAXE, Some(1))),
        ] {
            let mut m = misses(&[("normal", 10_000), ("arcane1", 10_000), ("arcane2", 10_000)]);
            let start = m.clone();
            let mut rolled = with_enchant(before.clone(), &[FORTIFY_A]);
            let kept = rolled.clone();
            let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
            assert_eq!(out, PityOutcome::NotCounted, "{what}");
            assert_eq!(rolled, kept, "{what}: enchant untouched");
            assert_eq!(m, start, "{what}: no counter moves");
        }
    }

    #[test]
    fn switched_off_by_config_nothing_is_counted_or_forced() {
        let mut e = enchanting();
        e.tuning.double_ravage_pity.enabled = false;
        let before = item(MADNESS_BATTLEAXE, None);
        let mut m = misses(&[("normal", 10_000)]);
        let mut rolled = with_enchant(before.clone(), &[]);
        let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(out, PityOutcome::NotCounted);
        assert_eq!(rolled.properties.enchanting.len(), 1);
        assert_eq!(m, misses(&[("normal", 10_000)]));
    }

    #[test]
    fn the_forced_line_is_distinct_from_the_roll_tally_line() {
        let e = enchanting();
        let before = item(MADNESS_BATTLEAXE, None);
        let mut m = misses(&[("normal", 630)]);
        let mut rolled = with_enchant(before.clone(), &[]);
        let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        let c = Uuid::from_u128(0xc0ffee);
        let line = forced_log_line(
            c,
            MADNESS_BATTLEAXE,
            MAGICKA_DAMAGE_T10,
            &out,
            &rolled.properties.enchanting,
        )
        .expect("a forced outcome logs");
        assert_eq!(
            line,
            format!(
                "enchant pity forced: character=00000000-0000-0000-0000-000000c0ffee \
                 template={MADNESS_BATTLEAXE} recipe={MAGICKA_DAMAGE_T10} class=normal \
                 misses=630 threshold=630 primary={} tier=10 \
                 secondaries=[{RAVAGE_MAGICKA},{RAVAGE_STAMINA}]",
                e.recipes[&MAGICKA_DAMAGE_T10].property
            )
        );
        assert!(!line.contains("enchant rolled"));
        assert_eq!(
            forced_log_line(
                c,
                MADNESS_BATTLEAXE,
                MAGICKA_DAMAGE_T10,
                &PityOutcome::NotCounted,
                &[]
            ),
            None
        );
    }
}
