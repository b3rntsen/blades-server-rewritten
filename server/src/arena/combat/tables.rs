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
//! | `Weight::swing_interval()` (guessed per-class) | [`swing_interval`] from the item's `attack_delay + recovery_time` |
//! | `weapon_base_for_level` as the *primary* base | `gamedata::weapon().base_damage` (+ [`tempering_bonus`]) |
//!
//! What remains is genuinely *derived*:
//!
//! * the **capture-pinned combo ramp** ([`LIGHT_COMBO_RAMP`]) — a wire
//!   measurement, not an asset value;
//! * the **rating→reduction** helpers ([`armor_reduction`],
//!   [`resistance_reduction`], [`block_reduction`], [`weakness_increase`]),
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

    /// Per-step combo multiplier and ceiling for the GEOMETRIC fallback in
    /// [`combo_factor`] (weights WITHOUT a capture-pinned per-depth table).
    ///
    /// **Versatile / Heavy steps + caps are GUESSES** (those weights aren't in the
    /// recorded match). **Light** does NOT use this — it uses [`LIGHT_COMBO_RAMP`].
    pub fn combo_step_cap(self) -> (f32, f32) {
        match self {
            Weight::Light => (1.45, 4.12), // capture-calibrated (s506); table-driven
            Weight::Versatile => (1.250, 1.250_f32.powi(4)), // GUESS (no capture)
            Weight::Heavy => (1.186, 1.186_f32.powi(4)),     // GUESS (no capture)
        }
    }
}

// ---------------------------------------------------------------------------
// Swing cadence — per ITEM, from the shipped weapon template (Phase 3.12)
// ---------------------------------------------------------------------------

/// Commit-to-commit swing interval for a weapon: `attackDelay + recoveryTime`,
/// floored at `PlayerCombatParameters.globalMinimumAttackDelay` (0.1 s).
///
/// This **replaces** the old guessed `Weight::swing_interval()` (a flat
/// 400/650/900 ms per weight class). The Dragonbone Dagger's shipped numbers
/// (0.2333 + 0.55 = 0.7833 s) are now used verbatim, and every one of the 370
/// templates carries its own pair.
pub fn swing_interval(attack_delay: f32, recovery_time: f32) -> std::time::Duration {
    let secs = (attack_delay + recovery_time).max(combat_params::GLOBAL_MINIMUM_ATTACK_DELAY);
    std::time::Duration::from_secs_f32(secs)
}

/// The cadence for a resolved weapon template.
pub fn swing_interval_for_weapon(w: &gamedata::WeaponStats) -> std::time::Duration {
    swing_interval(w.attack_delay, w.recovery_time)
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
// Combo ramp (capture-pinned — NOT shipped data)
// ---------------------------------------------------------------------------

/// The **capture-pinned** Light-weapon combo ramp, indexed by chain depth (0 = the
/// fresh post-reset swing): the s506 recorded per-depth Slashing factors against the
/// combo-0 base (`docs/arena-combat-reproduction-spec.md` §2a/§4.2). The ramp is
/// **irregular** (step ratios 1.45 / 1.03 / 1.77 / 1.55), so it is an explicit table
/// rather than a geometric series.
pub const LIGHT_COMBO_RAMP: [f32; 5] = [1.00, 1.45, 1.50, 2.65, 4.12];
/// The recorded Light combo ceiling (×4.12, seq 452).
pub const LIGHT_COMBO_CAP: f32 = 4.12;

/// The combo multiplier for a normal swing at chain depth `count` (0 = fresh).
pub fn combo_factor(weight: Weight, count: u32) -> f32 {
    if weight == Weight::Light {
        return LIGHT_COMBO_RAMP
            .get(count as usize)
            .copied()
            .unwrap_or(LIGHT_COMBO_CAP);
    }
    let (step, cap) = weight.combo_step_cap();
    (step.powi(count as i32)).min(cap)
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
pub fn tempering_bonus(weight: Weight, tempering_level: u64) -> f32 {
    let idx = (tempering_level as usize).min(QUALITY_BONUS.len() - 1);
    QUALITY_BONUS[idx] * weight.damage_factor()
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
/// Block is different: `maximumBlockReduction`/`minimumBlockReduction` bound a
/// **fraction**, so a Block Rating is scaled into 0..1. `BLOCK_RATING_SCALE`
/// is the percentage-point divisor that puts real shield ratings (150–330) in a
/// sane band instead of saturating instantly. [Class 3: bridge constant]
pub const BLOCK_RATING_SCALE: f32 = 100.0;

/// A connected **optimal** block reads the rating at double weight.
/// [uesp: "blockRating/10 (low block) / blockRating/5 (high block — 2×)"]
pub const OPTIMAL_BLOCK_RATING_MULTIPLIER: f32 = 2.0;

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
pub fn weakness_increase(incoming: f32, weakness_rating: f32, continuous: bool) -> f32 {
    if incoming <= 0.0 || weakness_rating <= 0.0 {
        return 0.0;
    }
    let eff = if continuous {
        combat_params::CONTINUOUS_DAMAGE_WEAKNESS_EFFECTIVENESS
    } else {
        1.0
    };
    (weakness_rating * combat_params::INCREASE_PER_WEAKNESS_RATING * eff)
        .min(incoming * combat_params::MAXIMUM_WEAKNESS_EFFECT)
}

/// The FRACTION of a hit a Block Rating removes.
///
/// `clamp(rating / BLOCK_RATING_SCALE × reductionPerBlockRating × categoryFactor,
/// minimumBlockReduction, maximumBlockReduction)` with `categoryFactor` =
/// `physicalBlockRatingFactor` (1.0) or `elementalBlockRatingFactor` (0.6666667).
///
/// **Blocking is NOT de-rated against continuous damage** —
/// `continuousDamageBlockingEffectiveness == 1`, unlike absorb / fortify /
/// resistance / revenge / weakness which are all 0.75. [Phase 3.5, correction 1]
pub fn block_reduction(block_rating: f32, physical: bool) -> f32 {
    if block_rating <= 0.0 {
        return combat_params::MINIMUM_BLOCK_REDUCTION;
    }
    let factor = if physical {
        combat_params::PHYSICAL_BLOCK_RATING_FACTOR
    } else {
        combat_params::ELEMENTAL_BLOCK_RATING_FACTOR
    };
    let raw = block_rating / BLOCK_RATING_SCALE * combat_params::REDUCTION_PER_BLOCK_RATING * factor;
    raw.clamp(
        combat_params::MINIMUM_BLOCK_REDUCTION,
        combat_params::MAXIMUM_BLOCK_REDUCTION,
    )
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
        // 0.233333 + 0.55 = 0.783333 s.
        assert!((got.as_secs_f32() - 0.783333).abs() < 1e-4, "got {got:?}");
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

    /// Block is a FRACTION, and it is **not** de-rated against continuous damage
    /// (`continuousDamageBlockingEffectiveness == 1`).
    #[test]
    fn block_is_a_fraction_with_elemental_two_thirds_of_physical() {
        let rating = 750.0;
        let phys = block_reduction(rating, true);
        let elem = block_reduction(rating, false);
        assert!((phys - 0.75).abs() < 1e-4, "phys {phys}");
        assert!((elem - 0.5).abs() < 1e-4, "elem {elem}");
        assert!((elem / phys - combat_params::ELEMENTAL_BLOCK_RATING_FACTOR).abs() < 1e-4);
        // Caps.
        assert!((block_reduction(100_000.0, true) - combat_params::MAXIMUM_BLOCK_REDUCTION).abs() < 1e-6);
        assert_eq!(block_reduction(0.0, true), combat_params::MINIMUM_BLOCK_REDUCTION);
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

    /// The Light combo ramp reproduces the s506 recorded per-depth anchors.
    #[test]
    fn light_combo_ramp_matches_s506() {
        assert_eq!(combo_factor(Weight::Light, 0), 1.0);
        assert!((combo_factor(Weight::Light, 1) - 1.45).abs() < 1e-3);
        assert!((combo_factor(Weight::Light, 2) - 1.50).abs() < 1e-3);
        assert!((combo_factor(Weight::Light, 3) - 2.65).abs() < 1e-3);
        assert!((combo_factor(Weight::Light, 4) - 4.12).abs() < 1e-3);
        assert_eq!(combo_factor(Weight::Light, 9), 4.12);
        for c in 0..8 {
            assert!(combo_factor(Weight::Light, c + 1) >= combo_factor(Weight::Light, c));
        }
    }

    #[test]
    fn fallback_surface_still_available_for_bots() {
        assert_eq!(fallback::heavy_base(10), 165.0);
        assert_eq!(fallback::smithy_level_for_char_level(86), 10);
        assert!(fallback::weapon_base_for_level(30, Weight::Light) > 0.0);
    }
}
