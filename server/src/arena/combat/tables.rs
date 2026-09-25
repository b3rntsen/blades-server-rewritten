//! Derived combat surfaces that sit *between* the shipped game data
//! ([`super::gamedata`]) and the damage model.
//!
//! # What lives here now (Phase 3)
//!
//! Everything numeric that the shipped assets define directly was moved OUT of
//! this file and is read from [`super::gamedata`]:
//!
//! | was | now |
//! |---|---|
//! | `ability_cost()` hand-transcribed UUID table | [`ability_cost`] → `gamedata::ability_rank_clamped().stamina_cost/magicka_cost` |
//! | `SPELL_BASE_BY_RANK` / `spell_base_for_rank` | `gamedata::AbilityRank::damage()` |
//! | `Weight::swing_interval()` (guessed per-class) | [`swing_interval`] from the item's `attack_delay + recovery_to_combo_time` |
//! | `weapon_base_for_level` as the *primary* base | `gamedata::weapon().base_damage` (+ [`tempering_bonus`]) |
//!
//! What remains is genuinely *derived*:
//!
//! * (the combo factor is no longer here: it is the shipped per-weapon
//!   `_comboDamageFactor`, read through `Loadout::charge_params`, combat-spec 02 §4.2);
//! * the **rating→reduction** helpers ([`armor_reduction`],
//!   [`resistance_reduction`], [`block_cut`], [`weakness_increase`]),
//!   which apply the shipped `CombatParameters` factors;
//! * the **tempering axis** ([`QUALITY_BONUS`] / [`tempering_bonus`]) — the
//!   shipped `WeaponTemplateList` carries only the *quality-0* cell (verified:
//!   Dragonbone Dagger `base_damage` 99.0 == `heavy_base(10) * Light 0.60`), so
//!   the per-tempering-level bonus still comes from the UESP surface;
//! * the **UESP fallback surface** ([`fallback`]) for bots / starter loadouts
//!   whose items do not resolve to a real template.

use super::gamedata::{self, combat_params};

/// Weapon weight class. Mirrors [`gamedata::WeaponClass`] (`Light 1 / Balanced 2 /
/// Heavy 3`); kept as a separate type because the damage model's combo/crit
/// surfaces are capture-derived rather than shipped data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Light,
    Versatile,
    Heavy,
}

impl Weight {
    /// Map the shipped `WeaponClass` enum onto the model's weight class.
    pub fn from_class(c: gamedata::WeaponClass) -> Self {
        match c {
            gamedata::WeaponClass::Light => Weight::Light,
            gamedata::WeaponClass::Versatile => Weight::Versatile,
            gamedata::WeaponClass::Heavy => Weight::Heavy,
        }
    }

    /// Damage factor relative to Heavy (Versatile = 2H grip 0.92). **Only used by
    /// [`fallback`]** — a real item's `base_damage` already has this baked in
    /// (Dragonbone Dagger 99.0 = Dragonbone heavy base 165 × 0.60).
    pub fn damage_factor(self) -> f32 {
        match self {
            Weight::Light => 0.60,
            Weight::Versatile => 0.92,
            Weight::Heavy => 1.00,
        }
    }
    /// `(crit, combo)` swing multipliers for this weight. [uesp]
    pub fn crit_combo(self) -> (f32, f32) {
        match self {
            Weight::Light => (1.325, 1.540),
            Weight::Versatile => (1.625, 1.250),
            Weight::Heavy => (1.987, 1.186),
        }
    }

}

// ---------------------------------------------------------------------------
// Swing cadence — per ITEM, from the shipped weapon template (Phase 3.12)
// ---------------------------------------------------------------------------

/// Commit-to-commit swing interval for a weapon: `attackDelay + recoveryToComboTime`,
/// floored at `PlayerCombatParameters.globalMinimumAttackDelay` (0.1 s).
///
/// `recoveryTime` is the whole recovery animation, not the gate for a combo input.
/// Treating it as the latter made the Dragonbone Dagger wait 0.783 s although retail
/// releases in the retained pcap are commonly 0.37-0.67 s apart. Its authored combo
/// gate is 0.2333 + 0.1 = 0.3333 s. Every one of the 370 templates carries both
/// values, so the model must not collapse them again.
pub fn swing_interval(attack_delay: f32, recovery_to_combo_time: f32) -> std::time::Duration {
    let secs = (attack_delay + recovery_to_combo_time).max(combat_params::GLOBAL_MINIMUM_ATTACK_DELAY);
    std::time::Duration::from_secs_f32(secs)
}

/// The cadence for a resolved weapon template.
pub fn swing_interval_for_weapon(w: &gamedata::WeaponStats) -> std::time::Duration {
    swing_interval(w.attack_delay, w.recovery_to_combo_time)
}

/// Time from commit until the actor returns to neutral. This is deliberately
/// separate from [`swing_interval_for_weapon`]: retail permits the next alternating
/// combo before the recovery animation reaches neutral.
pub fn neutral_interval_for_weapon(w: &gamedata::WeaponStats) -> std::time::Duration {
    let secs = (w.attack_delay + w.recovery_to_neutral_time)
        .max(combat_params::GLOBAL_MINIMUM_ATTACK_DELAY);
    std::time::Duration::from_secs_f32(secs)
}

/// The cadence used when a fighter's weapon does not resolve to a real template
/// (bot / starter). The global floor times the weight's relative speed
/// [uesp Speed column: Light 2.07, Versatile 1.33, Heavy 1.00] normalised so
/// Heavy keeps the historical ~0.9 s and Light stays fast.
pub fn fallback_swing_interval(weight: Weight) -> std::time::Duration {
    let secs: f32 = match weight {
        Weight::Light => 0.40,
        Weight::Versatile => 0.65,
        Weight::Heavy => 0.90,
    };
    std::time::Duration::from_secs_f32(secs.max(combat_params::GLOBAL_MINIMUM_ATTACK_DELAY))
}

// ---------------------------------------------------------------------------
// Tempering (the axis the shipped WeaponTemplateList does NOT carry)
// ---------------------------------------------------------------------------

/// 11 quality/tempering tiers (base→Mythical): additive bonus on top of the
/// material's **heavy** base, before the weight factor. [uesp — verified exact
/// across all 110 material×quality cells]
///
/// **Why this survives Phase 3.** `gamedata::WEAPONS[].base_damage` is the
/// *quality-0* cell only (Dragonbone Dagger 99.0 == `heavy_base(10) 165 × Light
/// 0.60` — an exact cross-validation of the UESP surface against the shipped
/// asset). A character's `Item.tempering_level` is the orthogonal axis and the
/// shipped `WeaponTemplateList` has no per-temper table, so the bonus still comes
/// from UESP. [Class 2: real mechanism, UESP magnitudes]
pub const QUALITY_BONUS: [f32; 11] =
    [0.0, 1.5, 4.5, 9.0, 15.0, 22.5, 30.0, 37.5, 45.0, 60.0, 75.0];

/// The damage a `tempering_level` adds to a weapon of this `weight`:
/// `QUALITY_BONUS[level] × weight.damage_factor()`. Levels past the table clamp.
///
/// Prefer [`tempering_bonus_in_hand`] — this entry point is grip-blind and so
/// over-credits a one-handed VERSATILE weapon. Kept for the Light/Heavy callers and
/// the fallback profile, where grip makes no difference.
pub fn tempering_bonus(weight: Weight, tempering_level: u64) -> f32 {
    let idx = (tempering_level as usize).min(QUALITY_BONUS.len() - 1);
    QUALITY_BONUS[idx] * weight.damage_factor()
}

/// `tempering_bonus`, with the VERSATILE grip correction.
///
/// `damage_factor()` is documented as the **two-handed** grip figure, and the
/// shipped templates confirm it: across `WeaponTemplateList`,
/// `baseTwoHandedDamage / baseDamage` is exactly **1.15 on 129 of 130 versatile
/// weapons**, and exactly **1.00 on all 111 light and all 126 heavy** ones. So
/// versatile is the only class whose base depends on grip, and its one-handed
/// tempering factor is `0.92 / 1.15 = 0.80`, not 0.92.
///
/// Charging the two-handed factor to a one-handed versatile build over-credited
/// `75 × (0.92 − 0.80) = 9.0` base damage at tempering 10 — on most characters,
/// since 77 of 94 carry a shield.
pub fn tempering_bonus_in_hand(weight: Weight, tempering_level: u64, two_handed: bool) -> f32 {
    let idx = (tempering_level as usize).min(QUALITY_BONUS.len() - 1);
    let factor = match (weight, two_handed) {
        // 0.92 / 1.15 — the exact ratio the templates ship.
        (Weight::Versatile, false) => 0.80,
        _ => weight.damage_factor(),
    };
    QUALITY_BONUS[idx] * factor
}

// ---------------------------------------------------------------------------
// Rating → reduction (Phase 3.3 / 3.4 / 3.5) — shipped CombatParameters
// ---------------------------------------------------------------------------

/// Scale that turns a shipped **rating** into the units
/// `reductionPer*Rating` multiplies.
///
/// Armor / Resistance ratings are *damage points*: `reductionPerArmorRating 0.1`
/// means "0.1 damage removed per point of Armor Rating" — i.e. UESP's
/// `reduction = armorRating / 10` and bladesarena's "10 % of AR is deducted per
/// hit (AR 1000 → −100/hit)", two independent sources agreeing with the shipped
/// constant. `reductionPerResistanceRating 1.0` then makes a Resistance Rating a
/// literal flat damage subtraction, which is exactly how the enemy assets read
/// (`Nascent Flame Atronach` `resistances.Fire = 65.28`).
///
/// Block has the same flat shape — see [`block_cut`].
///
/// `PvpClientManager.PhysicalBlockMultiplier` (`+0x144`), set to 1.6 in
/// `PvpClientManager$$Initialize@0x1adac7c` (0x1adaf38–0x1adaf54).
pub const PVP_PHYSICAL_BLOCK_MULTIPLIER: f32 = 1.6;
/// `PvpClientManager.ElementalBlockMultiplier` (`+0x148`), 1.23.
pub const PVP_ELEMENTAL_BLOCK_MULTIPLIER: f32 = 1.23;

/// The PvP block-rating FACTOR for a damage category:
/// `PvpPlayerActor$$GetBlockRatingFactor@0x1a32348` multiplies the shipped
/// `_physicalBlockRatingFactor` (1.0) / `_elementalBlockRatingFactor` (0.6666667) by
/// the two `PvpClientManager` multipliers — physical **1.6**, elemental **0.82**.
///
/// These are the factors the retail arena server used. Capture test T1
/// (blades-capture `docs/combat-spec/capture-tests.md` §1): of 128 clean physical and
/// 72 clean elemental blocked hits, 19 and 30 sit within 1 % of the PvP value and 0
/// of either within 1 % of the PvE one; the mean factors are 1.5988 and 0.8231.
pub fn pvp_block_rating_factor(physical: bool) -> f32 {
    if physical {
        combat_params::PHYSICAL_BLOCK_RATING_FACTOR * PVP_PHYSICAL_BLOCK_MULTIPLIER
    } else {
        combat_params::ELEMENTAL_BLOCK_RATING_FACTOR * PVP_ELEMENTAL_BLOCK_MULTIPLIER
    }
}

/// Physical damage removed by an Armor Rating: a FLAT
/// `rating × reductionPerArmorRating`, capped so armor can never remove more than
/// `maximumArmorReduction` (95 %) of the incoming amount. [Phase 3.3]
pub fn armor_reduction(incoming: f32, armor_rating: f32) -> f32 {
    if incoming <= 0.0 || armor_rating <= 0.0 {
        return 0.0;
    }
    (armor_rating * combat_params::REDUCTION_PER_ARMOR_RATING)
        .min(incoming * combat_params::MAXIMUM_ARMOR_REDUCTION)
}

/// Damage removed by a Resistance Rating: a FLAT
/// `rating × reductionPerResistanceRating`, capped at `maximumResistanceReduction`
/// (95 %) of the incoming amount. `continuous` applies
/// `continuousDamageResistanceEffectiveness` (0.75) for DoT ticks. [Phase 3.4]
pub fn resistance_reduction(incoming: f32, resistance_rating: f32, continuous: bool) -> f32 {
    if incoming <= 0.0 || resistance_rating <= 0.0 {
        return 0.0;
    }
    let eff = if continuous {
        combat_params::CONTINUOUS_DAMAGE_RESISTANCE_EFFECTIVENESS
    } else {
        1.0
    };
    (resistance_rating * combat_params::REDUCTION_PER_RESISTANCE_RATING * eff)
        .min(incoming * combat_params::MAXIMUM_RESISTANCE_REDUCTION)
}

/// Extra damage added by a Weakness Rating: FLAT
/// `rating × increasePerWeaknessRating`, capped at `maximumWeaknessEffect` (×1.0,
/// i.e. at most doubling). `continuous` applies
/// `continuousDamageWeaknessEffectiveness` (0.75).
pub fn weakness_increase(
    incoming: f32,
    weakness_rating: f32,
    resistance_rating: f32,
    continuous: bool,
) -> f32 {
    if incoming <= 0.0 || weakness_rating <= 0.0 {
        return 0.0;
    }
    let eff = if continuous {
        combat_params::CONTINUOUS_DAMAGE_WEAKNESS_EFFECTIVENESS
    } else {
        1.0
    };
    // The client nets the weakness against the victim's resistance for that type
    // BEFORE applying it: `amount += min(amount, max(0, w - resistance))`. A target
    // with the matching Resist up therefore takes a reduced amplification, or none —
    // observed in a capture where a victim holding all four Resist statuses took a
    // visibly smaller delta than the enchantment's tier would otherwise give.
    let net = (weakness_rating * combat_params::INCREASE_PER_WEAKNESS_RATING
        - resistance_rating.max(0.0))
    .max(0.0);
    (net * eff).min(incoming * combat_params::MAXIMUM_WEAKNESS_EFFECT)
}

/// One component's value after a block: a FLAT cut, not a fraction.
///
/// ```text
/// cut = factor · (d / D_category) · R · reductionPerBlockRating (0.1)
/// d'  = max(d · (1 − maximumBlockReduction 0.95), d − cut)
/// ```
///
/// `DisplayClass305_0$$<ResolveBlocking>b__1@0x1fd06f0` (the final `Max` at
/// 0x1fd0bf8–0x1fd0c38). `share = d / D_category` spreads ONE budget of
/// `factor · R · 0.1` over the category's components, so the block removes the same
/// amount from a 50 hit as from a 450 one until the 5 % floor bites. `rating` is
/// the per-hit `R` after the optimal boost, the additives and piercing.
///
/// The fork used to take a FRACTION, `clamp(R / 100 · 0.1 · factor)`, so the cut
/// grew with the hit; that is 03-D1 / M-block-shape.
pub fn block_cut(d: f32, category_total: f32, rating: f32, factor: f32) -> f32 {
    if d <= 0.0 {
        return 0.0;
    }
    if rating <= 0.0 || category_total <= 0.0 {
        return d;
    }
    let share = d / category_total;
    let cut = factor * share * rating * combat_params::REDUCTION_PER_BLOCK_RATING;
    (d - cut).max(d * (1.0 - combat_params::MAXIMUM_BLOCK_REDUCTION))
}

// ---------------------------------------------------------------------------
// Enchantment magnitude (Phase 3.6/3.7)
// ---------------------------------------------------------------------------

/// Converts an [`gamedata::EnchantTier::value`] into damage points.
///
/// The shipped `_value` is a **shared power curve** (`268 / 736 / 1318 / 1941 /
/// 2566 / 3209 / 4008 / 4961 / 6137 / 7591` — 32 of the 116 families use exactly
/// this one), i.e. a raw magnitude in the logic class's own units, not damage.
/// The scale that maps it onto wire damage is pinned by capture:
///
/// * s506 `Weapon Poison Damage` **tier 10** (`value 7591`) landed **137.32**
///   → `137.32 / 7591 = 0.018090`;
/// * s293 `Weapon Shock Damage` **tier 7** (`value 4008`) landed **72.0**
///   → `72.0 / 4008 = 0.017964`.
///
/// Two different families, two different sessions, agreeing to **0.7 %**. This
/// replaces the old `13.73 × tier` guess, which was only right at tier 10 and
/// linear where the real curve is convex (`268 → 7591` is ×28 over 10 tiers, not
/// ×10). [Class 2: real curve, capture-pinned scale]
pub const ENCHANT_DAMAGE_PER_VALUE: f32 = 0.018090;

/// Damage contributed by one enchantment family at `tier`, or `None` when the
/// family/tier is not in the shipped data.
pub fn enchant_damage(family_uuid: &str, tier: u8) -> Option<f32> {
    gamedata::enchant_value(family_uuid, tier).map(|v| v * ENCHANT_DAMAGE_PER_VALUE)
}

// ---------------------------------------------------------------------------
// Ability costs / cooldowns — straight from the shipped ranks
// ---------------------------------------------------------------------------

/// `(stamina_cost, magicka_cost)` for an ability at `rank`, from the shipped
/// `<Name>Rank<N>` asset. Unknown ability or perk rank → `(0, 0)`.
///
/// **Replaces** the hand-transcribed table that used to live here — 11 of whose
/// 33 UUIDs were fabricated tails (e.g. Thunderstorm was
/// `2ab06506-c9e5-4d12-…`, the real id is `2ab06506-2114-4738-…`), so those
/// abilities silently cost nothing.
pub fn ability_cost(ability_uuid: &str, rank: u8) -> (u32, u32) {
    match gamedata::ability_rank_clamped(ability_uuid, rank.max(1) as u16) {
        Some(r) => (
            r.stamina_cost.unwrap_or(0.0).round().max(0.0) as u32,
            r.magicka_cost.unwrap_or(0.0).round().max(0.0) as u32,
        ),
        None => (0, 0),
    }
}

/// The shipped cooldown (seconds) for an ability at `rank`, if any.
pub fn ability_cooldown_secs(ability_uuid: &str, rank: u8) -> Option<f32> {
    gamedata::ability_rank_clamped(ability_uuid, rank.max(1) as u16)?.cooldown
}

/// The shipped **initial** cooldown (seconds) for an ability at `rank`, if any.
///
/// `ActiveAbility._initialCooldown` ("cooldown charged at the start of a fight",
/// `dump.cs:607776`) is the first-use delay charged when a round goes live — it is
/// NOT the between-cast `_cooldown`. 43 arena abilities ship one, from 0.5 s
/// (Lightning Bolt) to 10.5 s (Reckless Fury); see
/// `docs/arena-cooldowns-authoritative.md`.
pub fn ability_initial_cooldown_secs(ability_uuid: &str, rank: u8) -> Option<f32> {
    gamedata::ability_rank_clamped(ability_uuid, rank.max(1) as u16)?.initial_cooldown
}

/// Direct-hit damage for an ability rank, from the shipped `_damage`.
/// `None` when the rank defines no `_damage` (buffs, wards, perks).
pub fn ability_damage(ability_uuid: &str, rank: u8) -> Option<f32> {
    gamedata::ability_rank_clamped(ability_uuid, rank.max(1) as u16)?.damage()
}

// ---------------------------------------------------------------------------
// UESP fallback surface — bots / starter only
// ---------------------------------------------------------------------------

/// The level→material→damage surface, kept **only** for fighters whose equipped
/// weapon does not resolve to a real `gamedata::WEAPONS` template (bots, the
/// starter loadout, characters imported without inventory).
pub mod fallback {
    use super::{Weight, QUALITY_BONUS};

    /// Heavy (1.0×) base damage for a smithy level (1 = Iron … 10 = Dragonbone):
    /// `15 × (smithy_level + 1)`. Cross-validated against the shipped assets:
    /// `heavy_base(10) × Light 0.60 == Dragonbone Dagger base_damage 99.0`.
    pub fn heavy_base(smithy_level: u8) -> f32 {
        15.0 * (smithy_level as f32 + 1.0)
    }

    /// Highest usable material's smithy level at a character level.
    pub fn smithy_level_for_char_level(level: u16) -> u8 {
        match level {
            0..=7 => 2,   // Steel
            8..=12 => 3,  // Silver
            13..=17 => 4, // Orcish
            18..=22 => 5, // Dwarven
            23..=27 => 6, // Elven
            28..=32 => 7, // Glass
            33..=38 => 8, // Ebony
            39..=44 => 9, // Daedric
            _ => 10,      // Dragonbone (L45+)
        }
    }

    /// A representative tempering tier (0-10) for a character level.
    pub fn quality_tier_for_level(level: u16) -> usize {
        ((level as usize) / 9).min(QUALITY_BONUS.len() - 1)
    }

    /// Level-appropriate weapon base damage for a weight class.
    pub fn weapon_base_for_level(level: u16, weight: Weight) -> f32 {
        let heavy =
            heavy_base(smithy_level_for_char_level(level)) + QUALITY_BONUS[quality_tier_for_level(level)];
        heavy * weight.damage_factor()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped weapon template's `base_damage` IS the UESP quality-0 cell —
    /// which is why [`QUALITY_BONUS`] survives as the tempering axis rather than
    /// being deleted.
    #[test]
    fn shipped_base_damage_equals_uesp_quality_zero_cell() {
        let w = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).expect("dagger");
        let uesp_q0 = fallback::heavy_base(10) * Weight::Light.damage_factor();
        assert!(
            (w.base_damage - uesp_q0).abs() < 1e-3,
            "shipped {} vs UESP quality-0 {}",
            w.base_damage,
            uesp_q0
        );
        // Mythical (tempering 10) adds 75 at heavy scale → 45 at Light scale.
        assert!((tempering_bonus(Weight::Light, 10) - 45.0).abs() < 1e-3);
        assert!((w.base_damage + tempering_bonus(Weight::Light, 10) - 144.0).abs() < 1e-3);
    }

    /// Ability costs now come from the shipped ranks — including the abilities the
    /// old hand-written table had wrong UUIDs for.
    #[test]
    fn ability_costs_come_from_shipped_ranks() {
        // Fireball R1/R2 magicka = 90 / 105 (the old table's "R6 = 150" was a
        // linear extrapolation; the real ramp is per-rank).
        assert_eq!(ability_cost(gamedata::ids::FIREBALL, 1), (0, 90));
        assert_eq!(ability_cost(gamedata::ids::FIREBALL, 2), (0, 105));
        // Ward R1 magicka 205.
        assert_eq!(ability_cost(gamedata::ids::WARD, 1), (0, 205));
        // Thunderstorm's REAL uuid resolves (the old table's did not).
        assert_ne!(ability_cost("2ab06506-2114-4738-bd87-f6f402d3ce2e", 1), (0, 0));
        // Unknown uuid → no cost.
        assert_eq!(ability_cost("unknown-uuid", 1), (0, 0));
        // Cooldowns are per-ability, from the asset.
        assert!((ability_cooldown_secs(gamedata::ids::FIREBALL, 1).unwrap() - 3.54).abs() < 1e-3);
        assert!((ability_cooldown_secs(gamedata::ids::WARD, 1).unwrap() - 7.5).abs() < 1e-3);
    }

    /// Per-ITEM swing cadence replaces the guessed per-weight-class constants.
    #[test]
    fn swing_cadence_is_per_item() {
        let dagger = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).unwrap();
        let got = swing_interval_for_weapon(dagger);
        // Combo input unlocks before neutral: 0.233333 + 0.10 = 0.333333 s.
        assert!((got.as_secs_f32() - 0.333333).abs() < 1e-4, "got {got:?}");
        let neutral = neutral_interval_for_weapon(dagger);
        assert!((neutral.as_secs_f32() - 0.633333).abs() < 1e-4, "got {neutral:?}");
        // The global floor is respected for a pathologically fast template.
        assert_eq!(
            swing_interval(0.0, 0.0),
            std::time::Duration::from_secs_f32(combat_params::GLOBAL_MINIMUM_ATTACK_DELAY)
        );
    }

    /// Armor is a FLAT subtraction (`rating × 0.1`), capped at 95 % of the hit.
    #[test]
    fn armor_is_flat_capped_at_95_percent() {
        assert!((armor_reduction(144.0, 301.8) - 30.18).abs() < 1e-3);
        // A huge rating cannot remove more than 95 % of the hit.
        assert!((armor_reduction(100.0, 100_000.0) - 95.0).abs() < 1e-3);
        assert_eq!(armor_reduction(0.0, 500.0), 0.0);
    }

    /// Resistance is a FLAT subtraction at `reductionPerResistanceRating = 1.0`,
    /// de-rated to 0.75 for continuous (DoT) damage.
    #[test]
    fn resistance_is_flat_and_derated_for_dot() {
        assert!((resistance_reduction(200.0, 65.28, false) - 65.28).abs() < 1e-3);
        assert!((resistance_reduction(200.0, 65.28, true) - 65.28 * 0.75).abs() < 1e-3);
        assert!((resistance_reduction(10.0, 1000.0, false) - 9.5).abs() < 1e-3);
    }

    /// Block is a FLAT per-category budget with a 5 % floor per component (03 §2.5,
    /// `b__1@0x1fd06f0`). Expected values are hand arithmetic from the spec.
    #[test]
    fn block_is_a_flat_budget_not_a_fraction() {
        let phys = pvp_block_rating_factor(true);
        let elem = pvp_block_rating_factor(false);
        assert!((phys - 1.6).abs() < 1e-6, "PvP physical factor {phys}");
        assert!((elem - 0.82).abs() < 1e-6, "PvP elemental factor {elem}");

        // Ebony Shield (R 330), low block: the physical budget is 330·1.6·0.1 = 52.8,
        // the SAME for a 50, 150 or 450 hit until the 5 % floor bites.
        for (d, want) in [(150.0_f32, 97.2_f32), (450.0, 397.2), (200.0, 147.2)] {
            let got = block_cut(d, d, 330.0, phys);
            assert!((got - want).abs() < 1e-3, "d={d}: got {got}, want {want}");
        }
        // 50 − 52.8 is below the 5 % floor, so 2.5 remains.
        assert!((block_cut(50.0, 50.0, 330.0, phys) - 2.5).abs() < 1e-4);
        // The budget is split by share: two physical components of 100 and 300 lose
        // 13.2 and 39.6 (52.8 × ¼, × ¾).
        assert!((block_cut(100.0, 400.0, 330.0, phys) - 86.8).abs() < 1e-3);
        assert!((block_cut(300.0, 400.0, 330.0, phys) - 260.4).abs() < 1e-3);
        // Control: no rating, no cut; a non-positive component stays at 0.
        assert_eq!(block_cut(200.0, 200.0, 0.0, phys), 200.0);
        assert_eq!(block_cut(0.0, 200.0, 330.0, phys), 0.0);
        assert_eq!(
            combat_params::CONTINUOUS_DAMAGE_BLOCKING_EFFECTIVENESS,
            1.0,
            "blocking is FULL effectiveness vs DoT — the 0.75 figure is absorb/fortify/resist/revenge/weakness"
        );
    }

    /// Enchant magnitude follows the shipped per-family curve, not `13.73 × tier`.
    #[test]
    fn enchant_damage_follows_the_shipped_curve() {
        const POISON: &str = "08ea75d0-5cf1-44a9-9816-d3c6740c4191";
        let t10 = enchant_damage(POISON, 10).expect("poison tier 10");
        assert!((t10 - 137.32).abs() < 0.5, "s506 poison base {t10} != 137.32");
        // The curve is convex: tier 2 is ~1/10 of tier 10, not 1/5 as a linear
        // `13.73 × tier` model would say.
        let t2 = enchant_damage(POISON, 2).expect("poison tier 2");
        assert!((t2 - 736.0 * ENCHANT_DAMAGE_PER_VALUE).abs() < 1e-3);
        assert!(t2 < t10 / 5.0, "convex curve: t2 {t2} << t10/5 {}", t10 / 5.0);
        // Odd tiers do not exist for this family.
        assert_eq!(enchant_damage(POISON, 3), None);
    }

    /// The weight-class fallback for an unresolved weapon must be the standard shipped
    /// template of that class, field for field: 363 of the 370 templates share one of
    /// three signatures (combat-spec 02 §2.3). Checked against the most common tuple in
    /// the shipped table, so the fallback cannot drift from the data.
    #[test]
    fn the_charge_fallback_is_the_modal_shipped_template_per_class() {
        use crate::arena::combat::state::{Loadout, WeaponProfile};
        for (class, weight) in [
            (gamedata::WeaponClass::Light, Weight::Light),
            (gamedata::WeaponClass::Versatile, Weight::Versatile),
            (gamedata::WeaponClass::Heavy, Weight::Heavy),
        ] {
            let key = |w: &gamedata::WeaponStats| {
                [w.attack_delay, w.backswing_time, w.max_damage_time, w.max_damage_factor,
                 w.combo_damage_factor, w.recovery_time]
                    .map(|v| (v * 1000.0).round() as i64)
            };
            let mut counts: std::collections::HashMap<[i64; 6], usize> = Default::default();
            for w in gamedata::WEAPONS.iter().filter(|w| w.weapon_class == class) {
                *counts.entry(key(w)).or_default() += 1;
            }
            let (modal, n) = counts.iter().max_by_key(|(_, n)| **n).unwrap();
            assert!(*n >= 100, "{weight:?}: the standard template covers most of the class");
            let lo = Loadout {
                weapon: WeaponProfile { weight: Some(weight), ..Default::default() },
                ..Default::default()
            };
            let cp = lo.charge_params();
            let got = [cp.attack_delay, cp.backswing_time, cp.max_damage_time,
                       cp.max_damage_factor, cp.combo_damage_factor, cp.recovery_time]
                .map(|v| (v * 1000.0).round() as i64);
            assert_eq!(&got, modal, "{weight:?} fallback must equal the shipped standard");
        }
    }

    #[test]
    fn fallback_surface_still_available_for_bots() {
        assert_eq!(fallback::heavy_base(10), 165.0);
        assert_eq!(fallback::smithy_level_for_char_level(86), 10);
        assert!(fallback::weapon_base_for_level(30, Weight::Light) > 0.0);
    }
}
#[cfg(test)]
mod weakness_tests {
    use super::*;

    /// Flat, capped at +100% of the hit, and netted against the victim's resistance
    /// first — `amount += min(amount, max(0, w - resistance))`.
    #[test]
    fn weakness_is_flat_capped_and_net_of_resistance() {
        // Plain: the full rating lands.
        assert!((weakness_increase(100.0, 50.4, 0.0, false) - 50.4).abs() < 1e-3);
        // Capped at +100% of the incoming hit, never more.
        assert!((weakness_increase(20.0, 50.4, 0.0, false) - 20.0).abs() < 1e-3);
        // Resistance is subtracted from the WEAKNESS before it is applied.
        assert!((weakness_increase(100.0, 50.4, 20.0, false) - 30.4).abs() < 1e-3);
        // Enough resistance cancels it outright — never negative.
        assert_eq!(weakness_increase(100.0, 50.4, 80.0, false), 0.0);
        // No rating, no effect.
        assert_eq!(weakness_increase(100.0, 0.0, 0.0, false), 0.0);
    }

    /// A periodic tick pays the shipped `continuousDamageWeaknessEffectiveness`.
    #[test]
    fn a_continuous_tick_pays_the_reduced_effectiveness() {
        let one = weakness_increase(1000.0, 50.4, 0.0, false);
        let tick = weakness_increase(1000.0, 50.4, 0.0, true);
        assert!(tick < one);
        assert!(
            (tick - one * combat_params::CONTINUOUS_DAMAGE_WEAKNESS_EFFECTIVENESS).abs() < 1e-3
        );
    }
}
