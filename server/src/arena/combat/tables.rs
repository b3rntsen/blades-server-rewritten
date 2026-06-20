//! UESP-derived combat constants (Blades level-50-cap era; see
//! `docs/blades-combat-formulae.md`). The weapon-damage surface is **additive**
//! (verified exact across all 110 material×quality cells); spell magnitudes are
//! per-rank. Used by `loadout`/`damage` to produce level-appropriate numbers until
//! real equipped-item game-data is wired.
//!
//! ⚠️ These are L50-era reference magnitudes; our build is L100. Treat the
//! *formulae/ratios* as solid and the *absolute magnitudes* as calibrated against
//! captured s293 damage (the level→quality/weight choices below are the tunable
//! calibration knobs).

/// Weapon weight class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Light,
    Versatile,
    Heavy,
}

impl Weight {
    /// Damage factor relative to Heavy (Versatile = 2H grip 0.92).
    pub fn damage_factor(self) -> f32 {
        match self {
            Weight::Light => 0.60,
            Weight::Versatile => 0.92,
            Weight::Heavy => 1.00,
        }
    }
    /// `(crit, combo)` swing multipliers for this weight.
    pub fn crit_combo(self) -> (f32, f32) {
        match self {
            Weight::Light => (1.325, 1.540),
            Weight::Versatile => (1.625, 1.250),
            Weight::Heavy => (1.987, 1.186),
        }
    }

    /// Per-step combo multiplier (the factor each *chained alternating* side-swing
    /// COMPOUNDS by) and the combo ceiling, for [`combo_factor`].
    ///
    /// **Light is capture-calibrated to s506** (`docs/arena-combat-reproduction-spec.md`
    /// §4.2): the recorded Dragonbone-dagger combo ramped Slashing ×1.00 → ×1.45 →
    /// (deep) ×2.65 → ×4.12 against a combo-0 base of 113.82. `1.45^count` reproduces
    /// that within the spec's stated charge/swingFactor variation (1.45^{0,1,2,3,4} =
    /// {1.00, 1.45, 2.10, 3.05, 4.42}); capped at **4.12** (the recorded ceiling). The
    /// first step 1.45 is the EMPIRICAL value (the table `combo` factor 1.540 is the
    /// nominal one — the spec notes "×1.45 ≈ the Light combo factor 1.540").
    ///
    /// **Versatile / Heavy steps + caps are GUESSES** (those weights aren't in the
    /// recorded match): step = the weight's nominal `combo` factor, cap = step^4. A
    /// Heavy weapon combos slowly (1.186/step) and leans on charged `Middle` crits
    /// instead — flagged for calibration when a heavy-weapon match is captured.
    pub fn combo_step_cap(self) -> (f32, f32) {
        match self {
            Weight::Light => (1.45, 4.12),                 // capture-calibrated (s506)
            Weight::Versatile => (1.250, 1.250_f32.powi(4)), // GUESS (no capture)
            Weight::Heavy => (1.186, 1.186_f32.powi(4)),     // GUESS (no capture)
        }
    }
}

/// The combo multiplier for a normal swing at chain depth `count` (0 = the fresh,
/// post-reset swing). Compounds `combo_step_cap().0` per chained alternating swing,
/// capped at `combo_step_cap().1`. `combo_factor(_, 0) == 1.0` for every weight
/// (a fresh swing is the un-combo'd base). [`docs/arena-combat-reproduction-spec.md` §4.2]
pub fn combo_factor(weight: Weight, count: u32) -> f32 {
    let (step, cap) = weight.combo_step_cap();
    (step.powi(count as i32)).min(cap)
}

/// 11 quality tiers (base→Mythical): additive bonus on top of the material base.
pub const QUALITY_BONUS: [f32; 11] =
    [0.0, 1.5, 4.5, 9.0, 15.0, 22.5, 30.0, 37.5, 45.0, 60.0, 75.0];

/// Heavy (1.0×) base damage for a smithy level (1 = Iron … 10 = Dragonbone):
/// `15 × (smithy_level + 1)`.
pub fn heavy_base(smithy_level: u8) -> f32 {
    15.0 * (smithy_level as f32 + 1.0)
}

/// Highest usable material's smithy level at a character level (the req-level
/// table: Iron/Steel L1 → Dragonbone L45).
pub fn smithy_level_for_char_level(level: u16) -> u8 {
    match level {
        0..=7 => 2,    // Steel (best base usable at L1)
        8..=12 => 3,   // Silver
        13..=17 => 4,  // Orcish
        18..=22 => 5,  // Dwarven
        23..=27 => 6,  // Elven
        28..=32 => 7,  // Glass
        33..=38 => 8,  // Ebony
        39..=44 => 9,  // Daedric
        _ => 10,       // Dragonbone (L45+)
    }
}

/// A representative quality tier (0-10) for a character level — gear quality trends
/// up with level. Tunable calibration knob.
pub fn quality_tier_for_level(level: u16) -> usize {
    ((level as usize) / 9).min(QUALITY_BONUS.len() - 1)
}

/// Level-appropriate weapon base damage for a weight class (additive surface +
/// representative material/quality for the level).
pub fn weapon_base_for_level(level: u16, weight: Weight) -> f32 {
    let heavy = heavy_base(smithy_level_for_char_level(level)) + QUALITY_BONUS[quality_tier_for_level(level)];
    heavy * weight.damage_factor()
}

/// Representative spell base magnitude by rank (Fireball-class direct damage, UESP
/// R1..R6). Used as a grounded stand-in when an ability's exact spell is unknown
/// (we lack ability definitions). Index by rank (1-based; clamped).
pub const SPELL_BASE_BY_RANK: [f32; 7] = [73.89, 73.89, 108.42, 150.24, 182.81, 213.75, 245.53];

pub fn spell_base_for_rank(rank: u8) -> f32 {
    SPELL_BASE_BY_RANK[(rank as usize).clamp(1, SPELL_BASE_BY_RANK.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additive_weapon_surface_matches_uesp() {
        // Dragonbone (smithy 10) base = 165; Mythical (+75) = 240 (UESP anchor).
        assert_eq!(heavy_base(10), 165.0);
        assert_eq!(heavy_base(10) + QUALITY_BONUS[10], 240.0);
        // Iron (smithy 1) base = 30; Mythical = 105.
        assert_eq!(heavy_base(1), 30.0);
        assert_eq!(heavy_base(1) + QUALITY_BONUS[10], 105.0);
        // Versatile 2H = 0.92× heavy.
        assert!((weapon_base_for_level(45, Weight::Versatile)
            - (heavy_base(10) + QUALITY_BONUS[quality_tier_for_level(45)]) * 0.92)
            .abs()
            < 1e-3);
    }

    #[test]
    fn level_picks_material_tier() {
        assert_eq!(smithy_level_for_char_level(1), 2); // Steel
        assert_eq!(smithy_level_for_char_level(30), 7); // Glass
        assert_eq!(smithy_level_for_char_level(86), 10); // Dragonbone
    }

    /// The Light combo ramp reproduces the s506 anchors (combo 0→1.00, 1→1.45,
    /// deep→~2.65/4.12) and is capped at 4.12 — `docs/arena-combat-reproduction-spec.md` §4.2.
    #[test]
    fn light_combo_ramp_matches_s506() {
        assert_eq!(combo_factor(Weight::Light, 0), 1.0, "fresh swing = un-combo'd base");
        assert!((combo_factor(Weight::Light, 1) - 1.45).abs() < 1e-3, "first chained step ≈1.45 (165.1/113.8)");
        // Deep combos land in the recorded 2.65 / 4.12 band (count 2→2.10, 3→3.05, 4→cap 4.12).
        assert!(combo_factor(Weight::Light, 3) > 2.65, "deep combo exceeds the ×2.65 recorded mid-step");
        assert_eq!(combo_factor(Weight::Light, 4), 4.12, "combo is capped at the recorded ×4.12 ceiling");
        assert_eq!(combo_factor(Weight::Light, 9), 4.12, "and stays capped past the ceiling");
        // Monotonic non-decreasing ramp.
        for c in 0..6 {
            assert!(combo_factor(Weight::Light, c + 1) >= combo_factor(Weight::Light, c));
        }
    }
}
