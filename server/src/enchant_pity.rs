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
//!
//! (APK numbers. The shipped `enchant_tuning.json` overrides them: plain weapons at
//! P(2) 0.25 -> 380, and Arcane I weapons at P(2) 0.5 -> 190, HauDrauf's rule on
//! tracker #493. A class only counts when it can roll two secondaries; a forced
//! one-secondary roll becomes primary + the pair.)
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
//!
//! A class with a `pityAtRange` (tracker #645, HauDrauf 2026-11-28: Arcane II weapons,
//! 45..=55) has no fixed threshold: the enchant number the pair is due on is drawn from
//! the range per character, kept in `server_state.enchantPityDraws`, and re-drawn
//! every time the class's counter restarts, i.e. after every "hit" (the pair, forced by
//! the timer or rolled naturally). The draw is seeded from the character, the class and
//! how many draws that class has made, so it is reproducible and changes every time.

use std::collections::BTreeMap;

use blades_lib::server_state::EnchantPityDraw;
use blades_lib::static_data::EnchantingData;
use blades_lib::user_data::{Item, ItemSingleProperty};
use rand::RngExt;
use uuid::Uuid;

/// `Weapon Ravage Magicka` (`WeaponRavageMagickaPropertyLogic`).
pub const RAVAGE_MAGICKA: Uuid = Uuid::from_u128(0x7cdb7179_4cd4_466e_8b77_4caf2fddb268);
/// `Weapon Ravage Stamina` (`WeaponRavageStaminaPropertyLogic`).
pub const RAVAGE_STAMINA: Uuid = Uuid::from_u128(0x9be2e7e9_5ef5_4ee8_aeec_b864bca07f07);
/// `Shield Ravage Magicka` (`ShieldRavageMagickaPropertyLogic`).
pub const SHIELD_RAVAGE_MAGICKA: Uuid = Uuid::from_u128(0x2ac1e2f6_8f97_444e_97af_7efd46c71cca);
/// `Shield Ravage Stamina` (`ShieldRavageStaminaPropertyLogic`).
pub const SHIELD_RAVAGE_STAMINA: Uuid = Uuid::from_u128(0x9701ded5_e1d1_4436_8b14_d5d726ff4434);
/// `BGS.Game.Inventory.ItemType.Weapon`.
const WEAPON_ITEM_TYPE: i64 = 2;
/// `BGS.Game.Inventory.ItemType.Shield`.
const SHIELD_ITEM_TYPE: i64 = 9;

/// The Ravage pair an item type's timer watches, and the prefix its counters carry
/// in `tries` / `enchantPityMisses`. Weapons keep the bare class names they always
/// had (`normal`, `arcane2`), so no stored counter moves; shields (tracker #463,
/// HauDrauf, 2026-11-04) count under `shield:<class>` with their own pair, because
/// a shield's Ravage properties are different ids from a weapon's.
fn ravage_pair(item_type: Option<i64>) -> Option<(Uuid, Uuid, &'static str)> {
    match item_type? {
        WEAPON_ITEM_TYPE => Some((RAVAGE_MAGICKA, RAVAGE_STAMINA, "")),
        SHIELD_ITEM_TYPE => Some((SHIELD_RAVAGE_MAGICKA, SHIELD_RAVAGE_STAMINA, "shield:")),
        _ => None,
    }
}

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

/// When a class's pair is forced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Threshold {
    /// After `tries` misses (`doubleRavagePity.tries`).
    Fixed(u32),
    /// On enchant number N, N drawn from `lo..=hi` (`doubleRavagePity.pityAtRange`).
    Drawn { lo: u32, hi: u32 },
}

/// The enchant number the pair is due on for a drawn class: draw number `nonce` of
/// `class` for this character, uniform over `lo..=hi`.
pub fn draw_pity_at(character_id: Uuid, class: &str, nonce: u32, lo: u32, hi: u32) -> u32 {
    crate::jewelry_roll::seeded(
        &[
            character_id.as_bytes(),
            class.as_bytes(),
            b"enchant-pity-645",
        ],
        u64::from(nonce),
    )
    .random_range(lo..=hi)
}

/// The counter key, threshold and Ravage pair when this enchant counts toward the
/// timer.
fn counted_class(
    enchanting: &EnchantingData,
    before: &Item,
    recipe_id: Uuid,
) -> Option<(String, Threshold, [Uuid; 2])> {
    let pity = &enchanting.tuning.double_ravage_pity;
    if !pity.enabled {
        return None;
    }
    // Only a roll from the APK tables (`roll_enchant`); the captured-outcome
    // fallback is not a roll this timer can reason about.
    let recipe = enchanting.recipes.get(&recipe_id)?;
    let table = enchanting.table_for(&before.item_template_id)?;
    let (magicka, stamina, prefix) = ravage_pair(table.item_type)?;
    if recipe.property == magicka || recipe.property == stamina {
        return None;
    }
    let in_pool = |id: Uuid| {
        table
            .properties
            .iter()
            .any(|p| p.id == id && p.weight > 0.0)
    };
    if !(in_pool(magicka) && in_pool(stamina)) {
        return None;
    }
    // A class that can never draw two secondaries (arcane 1) cannot carry the pair;
    // forcing it would break its fixed count.
    let odds = enchanting.roll_count_odds(table, before.arcane_tier);
    if odds.iter().skip(2).all(|p| *p <= 0.0) {
        return None;
    }
    let class = format!("{prefix}{}", enchanting.odds_class(before.arcane_tier));
    if let Some(&[lo, hi]) = pity.pity_at_range.get(&class)
        && (1..=hi).contains(&lo)
    {
        return Some((class, Threshold::Drawn { lo, hi }, [magicka, stamina]));
    }
    let threshold = *pity.tries.get(&class)?;
    (threshold > 0).then_some((class, Threshold::Fixed(threshold), [magicka, stamina]))
}

/// Whether an enchant (primary first, then secondaries) carries both WEAPON Ravages.
pub fn has_double_ravage(enchanting: &[ItemSingleProperty]) -> bool {
    has_pair(enchanting, [RAVAGE_MAGICKA, RAVAGE_STAMINA])
}

/// Whether an enchant's secondaries carry both properties of `pair`.
fn has_pair(enchanting: &[ItemSingleProperty], pair: [Uuid; 2]) -> bool {
    let secs = || enchanting.iter().skip(1);
    pair.iter().all(|id| secs().any(|p| p.id == *id))
}

/// Run the timer on one enchant. `before` is the item as it went in, `rolled` the item
/// [`crate::craft`]'s roll produced (rewritten in place when the pair is forced),
/// `misses` the character's `server_state.enchant_pity_misses` and `draws` its
/// `server_state.enchant_pity_draws` (only read and written for a drawn class).
pub fn apply(
    enchanting: &EnchantingData,
    character_id: Uuid,
    before: &Item,
    recipe_id: Uuid,
    rolled: &mut Item,
    misses: &mut BTreeMap<String, u32>,
    draws: &mut BTreeMap<String, EnchantPityDraw>,
) -> PityOutcome {
    let Some((class, threshold, pair)) = counted_class(enchanting, before, recipe_id) else {
        return PityOutcome::NotCounted;
    };
    // The pair is forced once `threshold` misses are on record: `tries` for a fixed
    // class, the drawn enchant number minus one for a drawn class (drawn now if this
    // character has none yet, or the stored one fell outside a changed range).
    let threshold = match threshold {
        Threshold::Fixed(t) => t,
        Threshold::Drawn { lo, hi } => {
            let d = match draws.get(&class).copied() {
                Some(d) if (lo..=hi).contains(&d.at) => d,
                old => {
                    let nonce = old.map_or(0, |d| d.draws);
                    let d = EnchantPityDraw {
                        at: draw_pity_at(character_id, &class, nonce, lo, hi),
                        draws: nonce + 1,
                    };
                    draws.insert(class.clone(), d);
                    d
                }
            };
            d.at - 1
        }
    };
    // A hit (the pair, natural or forced) restarts the counter, and a drawn class
    // draws its next point at the same time.
    let restart = |misses: &mut BTreeMap<String, u32>,
                   draws: &mut BTreeMap<String, EnchantPityDraw>| {
        misses.remove(&class);
        if let Some(&[lo, hi]) = enchanting
            .tuning
            .double_ravage_pity
            .pity_at_range
            .get(&class)
            && let Some(d) = draws.get_mut(&class)
        {
            d.at = draw_pity_at(character_id, &class, d.draws, lo, hi);
            d.draws += 1;
        }
    };
    let so_far = misses.get(&class).copied().unwrap_or(0);
    if has_pair(&rolled.properties.enchanting, pair) {
        restart(misses, draws);
        return PityOutcome::NaturalHit { class };
    }
    if so_far >= threshold {
        let tier = enchanting.recipes[&recipe_id].tier;
        let e = &mut rolled.properties.enchanting;
        e.truncate(1);
        e.extend(pair.map(|id| ItemSingleProperty { id, tier }));
        restart(misses, draws);
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

    /// The fixed-threshold tests' entry point: no character, no drawn points kept.
    fn apply(
        e: &EnchantingData,
        before: &Item,
        recipe_id: Uuid,
        rolled: &mut Item,
        m: &mut BTreeMap<String, u32>,
    ) -> PityOutcome {
        super::apply(
            e,
            Uuid::nil(),
            before,
            recipe_id,
            rolled,
            m,
            &mut BTreeMap::new(),
        )
    }

    const CHARACTER: Uuid = Uuid::from_u128(0x645_0000_0000_0000_0000_0000_0000_0645);

    /// The shipped tuning (`deploy/static/enchant_tuning.json`).
    fn shipped() -> EnchantingData {
        let mut e = enchanting();
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/enchant_tuning.json");
        e.tuning = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        e
    }

    fn drawn(at: u32, draws: u32) -> EnchantPityDraw {
        EnchantPityDraw { at, draws }
    }

    /// HauDrauf's rule (tracker #645): on an Arcane II weapon the pair is forced on
    /// exactly the drawn enchant number (here 47: after 46 misses), and the hit draws
    /// the next number (draw #4 of this character's arcane2 sequence). Red before: the
    /// threshold was the fixed 49 and nothing was drawn.
    #[test]
    fn an_arcane_two_weapon_is_forced_on_its_drawn_enchant_and_redraws() {
        let e = shipped();
        let before = item(MADNESS_BATTLEAXE, Some(2));
        let mut d = BTreeMap::from([("arcane2".to_string(), drawn(47, 3))]);
        // 45 misses: the 46th enchant is an ordinary miss.
        let mut m = misses(&[("arcane2", 45)]);
        let mut rolled = with_enchant(before.clone(), &[FORTIFY_A, RAVAGE_MAGICKA]);
        let out = super::apply(
            &e,
            CHARACTER,
            &before,
            MAGICKA_DAMAGE_T10,
            &mut rolled,
            &mut m,
            &mut d,
        );
        assert_eq!(
            out,
            PityOutcome::Miss {
                class: "arcane2".into(),
                misses: 46
            }
        );
        assert_eq!(d["arcane2"], drawn(47, 3), "a miss keeps the drawn point");
        // 46 misses: the 47th enchant gets the pair.
        let mut rolled = with_enchant(before.clone(), &[FORTIFY_A, RAVAGE_MAGICKA]);
        let out = super::apply(
            &e,
            CHARACTER,
            &before,
            MAGICKA_DAMAGE_T10,
            &mut rolled,
            &mut m,
            &mut d,
        );
        assert_eq!(
            out,
            PityOutcome::Forced {
                class: "arcane2".into(),
                misses: 46,
                threshold: 46
            }
        );
        assert!(has_double_ravage(&rolled.properties.enchanting));
        assert!(m.is_empty());
        let next = d["arcane2"];
        assert_eq!(next.draws, 4);
        assert_eq!(next.at, draw_pity_at(CHARACTER, "arcane2", 3, 45, 55));
        assert!((45..=55).contains(&next.at));
    }

    /// A natural pair is a hit too: the counter restarts AND a new point is drawn.
    #[test]
    fn a_natural_pair_on_arcane_two_also_draws_a_new_point() {
        let e = shipped();
        let before = item(MADNESS_BATTLEAXE, Some(2));
        let mut d = BTreeMap::from([("arcane2".to_string(), drawn(52, 7))]);
        let mut m = misses(&[("arcane2", 20)]);
        let mut rolled = with_enchant(before.clone(), &[RAVAGE_STAMINA, RAVAGE_MAGICKA]);
        let out = super::apply(
            &e,
            CHARACTER,
            &before,
            MAGICKA_DAMAGE_T10,
            &mut rolled,
            &mut m,
            &mut d,
        );
        assert_eq!(
            out,
            PityOutcome::NaturalHit {
                class: "arcane2".into()
            }
        );
        assert!(m.is_empty());
        assert_eq!(
            d["arcane2"],
            drawn(draw_pity_at(CHARACTER, "arcane2", 7, 45, 55), 8)
        );
    }

    /// A character with no drawn point yet (every existing one at deploy) draws its
    /// first on its next Arcane II weapon enchant; a stored point outside the range
    /// (the range was edited) is replaced the same way.
    #[test]
    fn the_first_point_is_drawn_on_the_first_counted_enchant() {
        let e = shipped();
        let before = item(MADNESS_BATTLEAXE, Some(2));
        for (start, want) in [
            (
                BTreeMap::new(),
                drawn(draw_pity_at(CHARACTER, "arcane2", 0, 45, 55), 1),
            ),
            (
                BTreeMap::from([("arcane2".to_string(), drawn(95, 2))]),
                drawn(draw_pity_at(CHARACTER, "arcane2", 2, 45, 55), 3),
            ),
        ] {
            let mut d = start;
            let mut m = misses(&[("arcane2", 3)]);
            let mut rolled = with_enchant(before.clone(), &[FORTIFY_A, RAVAGE_MAGICKA]);
            super::apply(
                &e,
                CHARACTER,
                &before,
                MAGICKA_DAMAGE_T10,
                &mut rolled,
                &mut m,
                &mut d,
            );
            assert_eq!(d["arcane2"], want);
            assert_eq!(m, misses(&[("arcane2", 4)]));
        }
    }

    /// The draw is uniform over 45..=55, never outside it, reproducible, and its own
    /// sequence per character.
    #[test]
    fn draws_are_within_45_to_55_reproducible_and_per_character() {
        let mut seen = BTreeMap::<u32, u32>::new();
        for nonce in 0..11_000 {
            let at = draw_pity_at(CHARACTER, "arcane2", nonce, 45, 55);
            assert!((45..=55).contains(&at), "draw {nonce}: {at}");
            assert_eq!(at, draw_pity_at(CHARACTER, "arcane2", nonce, 45, 55));
            *seen.entry(at).or_default() += 1;
        }
        assert_eq!(seen.len(), 11, "every value 45..=55 is drawn: {seen:?}");
        assert!(
            seen.values().all(|n| (800..=1200).contains(n)),
            "about 1,000 each: {seen:?}"
        );
        let seq = |c: Uuid| -> Vec<u32> {
            (0..20)
                .map(|n| draw_pity_at(c, "arcane2", n, 45, 55))
                .collect()
        };
        assert_ne!(seq(CHARACTER), seq(Uuid::from_u128(0xBEEF)));
    }

    /// Real Arcane II weapon rolls with the shipped tuning and the state persisting
    /// between enchants: every stored point stays within 45..=55, each pair forced by
    /// the timer lands on exactly the stored point, every hit draws again, and no run
    /// goes past 54 misses.
    #[test]
    fn real_arcane_two_weapon_rolls_follow_the_drawn_points() {
        use rand::SeedableRng;
        let e = shipped();
        let before = item(MADNESS_BATTLEAXE, Some(2));
        let mut rng = rand::rngs::StdRng::seed_from_u64(645);
        let (mut m, mut d) = (BTreeMap::new(), BTreeMap::new());
        let (mut streak, mut longest, mut forced, mut hits) = (0u32, 0u32, 0, 0);
        let mut points = std::collections::BTreeSet::new();
        for _ in 0..6_000 {
            let mut rolled =
                crate::craft::roll_item_for_test(&before, MAGICKA_DAMAGE_T10, &e, &mut rng);
            let point = d.get("arcane2").copied();
            let out = super::apply(
                &e,
                CHARACTER,
                &before,
                MAGICKA_DAMAGE_T10,
                &mut rolled,
                &mut m,
                &mut d,
            );
            let now = d["arcane2"];
            assert!((45..=55).contains(&now.at), "{now:?}");
            points.insert(now.at);
            match out {
                PityOutcome::Forced { misses, .. } => {
                    forced += 1;
                    hits += 1;
                    assert_eq!(misses + 1, point.unwrap().at, "forced on the drawn enchant");
                    assert_eq!(now.draws, point.unwrap().draws + 1, "a hit redraws");
                }
                PityOutcome::NaturalHit { .. } => {
                    hits += 1;
                    assert_eq!(now.draws, point.map_or(1, |p| p.draws) + 1, "a hit redraws");
                }
                PityOutcome::Miss { .. } => {
                    if let Some(p) = point {
                        assert_eq!(now, p, "a miss keeps the point");
                    }
                }
                PityOutcome::NotCounted => panic!("an arcane-2 weapon counts"),
            }
            if has_double_ravage(&rolled.properties.enchanting) {
                streak = 0;
            } else {
                streak += 1;
                longest = longest.max(streak);
            }
        }
        assert!(longest <= 54, "longest run without the pair: {longest}");
        assert!(forced > 30 && hits > forced, "forced {forced}, hits {hits}");
        assert!(points.len() >= 8, "the point moves around: {points:?}");
    }

    /// Negative control (#645 is Arcane II WEAPONS only): Arcane I and plain weapons and
    /// Arcane II shields keep their fixed thresholds and never draw a point.
    #[test]
    fn arcane_one_plain_weapons_and_shields_keep_their_fixed_thresholds() {
        let e = shipped();
        for (before, recipe, class, threshold, secondaries) in [
            (
                item(MADNESS_BATTLEAXE, Some(1)),
                MAGICKA_DAMAGE_T10,
                "arcane1",
                190,
                vec![FORTIFY_A],
            ),
            (
                item(MADNESS_BATTLEAXE, None),
                MAGICKA_DAMAGE_T10,
                "normal",
                380,
                vec![FORTIFY_A],
            ),
            (
                item(DIVINE_DAEDRIC_LIGHT_SHIELD, Some(2)),
                SHIELD_FIRE_DAMAGE_T10,
                "shield:arcane2",
                49,
                vec![FORTIFY_A, SHIELD_RAVAGE_MAGICKA],
            ),
        ] {
            let mut d = BTreeMap::new();
            let mut m = misses(&[(class, threshold - 1)]);
            let mut rolled = before.clone();
            let primary = e.recipes[&recipe].property;
            rolled.properties.enchanting = std::iter::once(primary)
                .chain(secondaries.iter().copied())
                .map(|id| ItemSingleProperty { id, tier: 10 })
                .collect();
            let again = rolled.clone();
            assert_eq!(
                super::apply(&e, CHARACTER, &before, recipe, &mut rolled, &mut m, &mut d),
                PityOutcome::Miss {
                    class: class.into(),
                    misses: threshold
                },
                "{class}"
            );
            let mut rolled = again;
            assert_eq!(
                super::apply(&e, CHARACTER, &before, recipe, &mut rolled, &mut m, &mut d),
                PityOutcome::Forced {
                    class: class.into(),
                    misses: threshold,
                    threshold
                },
                "{class}"
            );
            assert!(d.is_empty(), "{class}: no point drawn");
        }
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

    const DIVINE_DAEDRIC_LIGHT_SHIELD: Uuid =
        Uuid::from_u128(0x08a302c9_fc5b_4a03_bd6c_e7974a81009f);
    /// `Enchant.Recipe.ShieldFireDamageT10`.
    const SHIELD_FIRE_DAMAGE_T10: Uuid = Uuid::from_u128(0xb1db39b2_2bf1_49bb_962a_e55f58e447ff);

    fn shield_enchant(arcane_tier: Option<u64>, secondaries: &[Uuid]) -> Item {
        let mut it = item(DIVINE_DAEDRIC_LIGHT_SHIELD, arcane_tier);
        let primary = enchanting().recipes[&SHIELD_FIRE_DAMAGE_T10].property;
        it.properties.enchanting = std::iter::once(primary)
            .chain(secondaries.iter().copied())
            .map(|id| ItemSingleProperty { id, tier: 10 })
            .collect();
        it
    }

    /// HauDrauf's rule (tracker #463): an Arcane II shield gets the SHIELD Ravage
    /// pair on the 50th enchant in a row without it, on its own counter. Red before:
    /// shields were never counted (`NotCounted`, roll untouched).
    #[test]
    fn arcane_two_shields_are_forced_their_own_pair_on_their_own_counter() {
        let mut e = enchanting();
        e.tuning.double_ravage_pity.tries =
            misses(&[("normal", 380), ("arcane2", 49), ("shield:arcane2", 49)]);
        let before = item(DIVINE_DAEDRIC_LIGHT_SHIELD, Some(2));
        // 49 misses so far: the 50th miss is replaced by the pair.
        let mut m = misses(&[("arcane2", 7), ("shield:arcane2", 49)]);
        let mut rolled = shield_enchant(Some(2), &[FORTIFY_A, SHIELD_RAVAGE_MAGICKA]);
        let out = apply(&e, &before, SHIELD_FIRE_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Forced {
                class: "shield:arcane2".into(),
                misses: 49,
                threshold: 49
            }
        );
        let ids: Vec<Uuid> = rolled.properties.enchanting.iter().map(|p| p.id).collect();
        assert_eq!(&ids[1..], &[SHIELD_RAVAGE_MAGICKA, SHIELD_RAVAGE_STAMINA]);
        assert_eq!(
            m,
            misses(&[("arcane2", 7)]),
            "the weapon counter is not touched"
        );

        // 48 misses: an ordinary miss, counted under the shield key.
        let mut m = misses(&[("shield:arcane2", 48)]);
        let mut rolled = shield_enchant(Some(2), &[FORTIFY_A, SHIELD_RAVAGE_MAGICKA]);
        let kept = rolled.clone();
        let out = apply(&e, &before, SHIELD_FIRE_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Miss {
                class: "shield:arcane2".into(),
                misses: 49
            }
        );
        assert_eq!(rolled, kept);

        // A natural shield pair restarts the count; WEAPON Ravages on a shield are
        // not its pair (they are not even in its pool, but the check is by id).
        let mut m = misses(&[("shield:arcane2", 30)]);
        let mut rolled = shield_enchant(Some(2), &[SHIELD_RAVAGE_STAMINA, SHIELD_RAVAGE_MAGICKA]);
        let out = apply(&e, &before, SHIELD_FIRE_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::NaturalHit {
                class: "shield:arcane2".into()
            }
        );
        assert!(m.is_empty());

        // The code default has no shield key: shields are not counted.
        let d = enchanting();
        let mut m = misses(&[("shield:arcane2", 1_000)]);
        let mut rolled = shield_enchant(Some(2), &[FORTIFY_A, SHIELD_RAVAGE_MAGICKA]);
        assert_eq!(
            apply(&d, &before, SHIELD_FIRE_DAMAGE_T10, &mut rolled, &mut m),
            PityOutcome::NotCounted
        );
    }

    /// Real Arcane II shield and weapon rolls with the shipped thresholds never go
    /// past 50 enchants without their pair (#463). Weapons now draw their point from
    /// 45..=55 (#645), so theirs is capped at 55.
    #[test]
    fn shipped_thresholds_cap_arcane_two_runs_at_fifty() {
        use rand::SeedableRng;
        let mut e = enchanting();
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/enchant_tuning.json");
        e.tuning = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(463);
        for (before, recipe, pair) in [
            (
                item(MADNESS_BATTLEAXE, Some(2)),
                MAGICKA_DAMAGE_T10,
                [RAVAGE_MAGICKA, RAVAGE_STAMINA],
            ),
            (
                item(DIVINE_DAEDRIC_LIGHT_SHIELD, Some(2)),
                SHIELD_FIRE_DAMAGE_T10,
                [SHIELD_RAVAGE_MAGICKA, SHIELD_RAVAGE_STAMINA],
            ),
        ] {
            let (mut m, mut d) = (BTreeMap::new(), BTreeMap::new());
            let (mut streak, mut longest, mut forced) = (0u32, 0u32, 0);
            for _ in 0..3_000 {
                let mut rolled = crate::craft::roll_item_for_test(&before, recipe, &e, &mut rng);
                match super::apply(&e, CHARACTER, &before, recipe, &mut rolled, &mut m, &mut d) {
                    PityOutcome::Forced { .. } => forced += 1,
                    PityOutcome::NotCounted => panic!("arcane II counts"),
                    _ => {}
                }
                if has_pair(&rolled.properties.enchanting, pair) {
                    streak = 0;
                } else {
                    streak += 1;
                    longest = longest.max(streak);
                }
            }
            let cap = if pair[0] == RAVAGE_MAGICKA { 55 } else { 50 };
            assert!(longest < cap, "longest run without the pair: {longest}");
            if pair[0] == RAVAGE_MAGICKA {
                assert!(forced > 0, "weapons at 1.05% hit the timer in 3,000 tries");
            }
        }
    }

    /// Shields like weapons (#493): a plain shield counts under `shield:normal` with the
    /// shipped threshold, so real rolls never go past it without the shield Ravage pair.
    #[test]
    fn shipped_thresholds_count_plain_shields() {
        use rand::SeedableRng;
        let mut e = enchanting();
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/enchant_tuning.json");
        e.tuning = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        let threshold = e.tuning.double_ravage_pity.tries["shield:normal"];
        let before = item(DIVINE_DAEDRIC_LIGHT_SHIELD, None);
        let pair = [SHIELD_RAVAGE_MAGICKA, SHIELD_RAVAGE_STAMINA];
        let mut rng = rand::rngs::StdRng::seed_from_u64(493);
        let mut m = BTreeMap::new();
        let (mut streak, mut longest, mut forced) = (0u32, 0u32, 0);
        for _ in 0..5_000 {
            let mut rolled =
                crate::craft::roll_item_for_test(&before, SHIELD_FIRE_DAMAGE_T10, &e, &mut rng);
            match apply(&e, &before, SHIELD_FIRE_DAMAGE_T10, &mut rolled, &mut m) {
                PityOutcome::Forced { class, .. } => {
                    assert_eq!(class, "shield:normal");
                    forced += 1;
                }
                PityOutcome::NotCounted => panic!("a plain shield counts"),
                _ => {}
            }
            if has_pair(&rolled.properties.enchanting, pair) {
                streak = 0;
            } else {
                streak += 1;
                longest = longest.max(streak);
            }
        }
        assert!(longest <= threshold, "longest run {longest} > {threshold}");
        assert!(forced > 0, "5,000 plain shields at ~1.5% hit the timer");
    }

    /// HauDrauf's rule (tracker #493, 2026-11-12): an Arcane I weapon gets the pair on
    /// the 191st enchant in a row without it (190 misses), and an Arcane I shield after
    /// 33, each on its own counter. Red before: the shipped file had no `arcane1` key,
    /// so Arcane I was `NotCounted` and the roll left alone.
    #[test]
    fn shipped_arcane_one_timer_fires_at_190_for_weapons_and_33_for_shields() {
        use rand::SeedableRng;
        let mut e = enchanting();
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/enchant_tuning.json");
        e.tuning = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        assert_eq!(e.tuning.double_ravage_pity.tries["arcane1"], 190);
        assert_eq!(e.tuning.double_ravage_pity.tries["shield:arcane1"], 33);

        // A ONE-secondary Arcane I roll at the threshold is rewritten to the pair.
        let before = item(MADNESS_BATTLEAXE, Some(1));
        let mut m = misses(&[("normal", 5), ("arcane1", 190)]);
        let mut rolled = with_enchant(before.clone(), &[FORTIFY_A]);
        let out = apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m);
        assert_eq!(
            out,
            PityOutcome::Forced {
                class: "arcane1".into(),
                misses: 190,
                threshold: 190
            }
        );
        let ids: Vec<Uuid> = rolled.properties.enchanting.iter().map(|p| p.id).collect();
        assert_eq!(&ids[1..], &[RAVAGE_MAGICKA, RAVAGE_STAMINA]);
        assert_eq!(m, misses(&[("normal", 5)]), "only arcane1 restarts");
        // One short is an ordinary miss.
        let mut m = misses(&[("arcane1", 189)]);
        let mut rolled = with_enchant(before.clone(), &[FORTIFY_A]);
        assert_eq!(
            apply(&e, &before, MAGICKA_DAMAGE_T10, &mut rolled, &mut m),
            PityOutcome::Miss {
                class: "arcane1".into(),
                misses: 190
            }
        );

        // Real rolls: no run past the threshold, for weapons and shields.
        let mut rng = rand::rngs::StdRng::seed_from_u64(4931);
        for (before, recipe, pair, class, n) in [
            (
                item(MADNESS_BATTLEAXE, Some(1)),
                MAGICKA_DAMAGE_T10,
                [RAVAGE_MAGICKA, RAVAGE_STAMINA],
                "arcane1",
                6_000,
            ),
            (
                item(DIVINE_DAEDRIC_LIGHT_SHIELD, Some(1)),
                SHIELD_FIRE_DAMAGE_T10,
                [SHIELD_RAVAGE_MAGICKA, SHIELD_RAVAGE_STAMINA],
                "shield:arcane1",
                3_000,
            ),
        ] {
            let threshold = e.tuning.double_ravage_pity.tries[class];
            let mut m = BTreeMap::new();
            let (mut streak, mut longest, mut forced) = (0u32, 0u32, 0);
            for _ in 0..n {
                let mut rolled = crate::craft::roll_item_for_test(&before, recipe, &e, &mut rng);
                match apply(&e, &before, recipe, &mut rolled, &mut m) {
                    PityOutcome::Forced { class: c, .. } => {
                        assert_eq!(c, class);
                        forced += 1;
                    }
                    PityOutcome::NotCounted => panic!("{class} counts"),
                    _ => {}
                }
                if has_pair(&rolled.properties.enchanting, pair) {
                    streak = 0;
                } else {
                    streak += 1;
                    longest = longest.max(streak);
                }
            }
            assert!(longest <= threshold, "{class}: run {longest} > {threshold}");
            assert!(
                forced > 0,
                "{class}: {n} rolls at ~1/threshold hit the timer"
            );
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
