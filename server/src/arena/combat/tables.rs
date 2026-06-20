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
    /// COMPOUNDS by) and the combo ceiling, for the GEOMETRIC fallback in
    /// [`combo_factor`] (used for weights WITHOUT a capture-pinned per-depth table).
    ///
    /// **Versatile / Heavy steps + caps are GUESSES** (those weights aren't in the
    /// recorded match): step = the weight's nominal `combo` factor, cap = step^4. A
    /// Heavy weapon combos slowly (1.186/step) and leans on charged `Middle` crits
    /// instead — flagged for calibration when a heavy-weapon match is captured.
    /// **Light** does NOT use this geometric model — it uses the capture-pinned
    /// per-depth anchor table [`LIGHT_COMBO_RAMP`] (the recorded ramp is irregular, not
    /// geometric — see [`combo_factor`]).
    pub fn combo_step_cap(self) -> (f32, f32) {
        match self {
            // Light's geometric params are kept only as the >ceiling fallback; the
            // measured ramp is the explicit LIGHT_COMBO_RAMP table.
            Weight::Light => (1.45, 4.12),                 // capture-calibrated (s506); table-driven
            Weight::Versatile => (1.250, 1.250_f32.powi(4)), // GUESS (no capture)
            Weight::Heavy => (1.186, 1.186_f32.powi(4)),     // GUESS (no capture)
        }
    }
}

/// The **capture-pinned** Light-weapon combo ramp, indexed by chain depth (0 = the
/// fresh post-reset swing). These are the s506 recorded per-depth Slashing factors
/// against the combo-0 base of 113.82 (`docs/arena-combat-reproduction-spec.md` §2a/§4.2):
/// the recorded chain ramped ×1.00 → ×1.45 → ~×1.50 → ×2.65 → ×4.12 (seq 277/287 →
/// 375/420 → 436 → 452). The ramp is **irregular** (NOT a clean `1.45^n`: the
/// step-to-step ratios are 1.45 / 1.03 / 1.77 / 1.55), so it is reproduced as an
/// explicit table rather than a geometric series — `1.45^3 = 3.05` overshot the
/// recorded ×2.65 deep step by ~15%. Depths past the table HOLD at the ×4.12 ceiling
/// (`LIGHT_COMBO_CAP`). [calibration: the four magnitudes are capture-pinned to s506.]
pub const LIGHT_COMBO_RAMP: [f32; 5] = [1.00, 1.45, 1.50, 2.65, 4.12];
/// The recorded Light combo ceiling (×4.12, seq 452) — depths beyond [`LIGHT_COMBO_RAMP`]
/// stay capped here (a runaway chain can't exceed the recorded maximum).
pub const LIGHT_COMBO_CAP: f32 = 4.12;

/// The combo multiplier for a normal swing at chain depth `count` (0 = the fresh,
/// post-reset swing). `combo_factor(_, 0) == 1.0` for every weight (a fresh swing is
/// the un-combo'd base). For **Light** (the only capture-calibrated weight) this reads
/// the explicit s506 [`LIGHT_COMBO_RAMP`] anchor table (holding at [`LIGHT_COMBO_CAP`]
/// beyond it) — the recorded ramp is irregular, not geometric. Other weights compound
/// `combo_step_cap().0` per chained swing, capped at `combo_step_cap().1` (uncaptured
/// GUESS). [`docs/arena-combat-reproduction-spec.md` §4.2]
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

/// Authoritative Stamina/Magicka cost per ability, keyed by the ability *definition*
/// UUID and the ability RANK (1-based).  Values are `_staminaCost` / `_magickaCost`
/// from the APK's `ActiveAbility` ScriptableObjects (extracted via UnityPy; see
/// docs/arena-combat-fixes-spec.md §1).
///
/// Returns `(stamina_cost, magicka_cost)` for the given UUID and rank.  Both are 0
/// for purely passive / unrecognised abilities; maneuvers have stamina cost only,
/// spells have magicka cost only.  Costs scale LINEARLY across ranks; rank is
/// clamped to [1, max_rank] at the call site.  Unknown UUIDs return `(0, 0)` —
/// the cooldown gate remains but no resource is deducted.
///
/// Per-rank formula: `cost(rank) = r1_cost + (r6_cost - r1_cost) / 5 * (rank - 1)`
/// (linear between R1 and R6 anchors; integer rounds to nearest).
pub fn ability_cost(ability_uuid: &str, rank: u8) -> (u32, u32) {
    // (r1_stamina, r6_stamina, r1_magicka, r6_magicka)
    let (r1s, r6s, r1m, r6m): (u32, u32, u32, u32) = match ability_uuid {
        // --- Spells (magicka cost, staminaCost=0) ---
        "4e760726-b012-4b25-bc92-0cd6312d6601" => (0, 0, 185, 355), // Absorb
        "c4b48518-e847-4f3d-81a2-2856bdb4ed98" => (0, 0, 290, 420), // Blizzard Armor
        "85596d85-93d9-4c74-a3db-5e5e11cde30e" => (0, 0, 140, 215), // Blind
        "e07f9b1a-64db-44ef-ba25-0e4378789ddc" => (0, 0, 160, 255), // Consuming Inferno
        "dfb8d247-1333-42eb-9730-a1c16d10584f" => (0, 0, 145, 220), // Delayed Lightning Bolt
        "f60f69d4-7b5c-4c1d-b0e4-99df3d49e52c" => (0, 0, 425, 570), // Echo Weapon
        "d07a8d30-9a1c-49b0-866d-97a8aa1534cf" => (0, 0, 90, 150),  // Fireball
        "4be1d681-c35d-4540-b255-c2910ac80664" => (0, 0, 170, 280), // Frostbite
        "cfee0b02-6d91-4d34-869c-a7e54329060d" => (0, 0, 130, 190), // Ice Spike
        "7fc15804-1637-40a9-8dcc-3ea1eb0f778d" => (0, 0, 80, 125),  // Lightning Bolt
        "1c836287-3df5-4b54-b05a-2e0a43cece5a" => (0, 0, 425, 570), // Magicka Surge
        "9fdc4d52-ce90-44f8-9b5d-21f31e27dbda" => (0, 0, 185, 265), // Paralyze
        "66bdc017-30c5-4b5e-9753-215c45056f6a" => (0, 0, 110, 175), // Poison Cloud
        "91078132-ef5c-492a-97f2-ac69be5140a8" => (0, 0, 200, 335), // Resist Elements
        "2ab06506-c9e5-4d12-8d5b-1d6a3b3e7e9c" => (0, 0, 190, 270), // Thunderstorm
        "65ede044-d68a-4b2b-8f0c-02075ad133cc" => (0, 0, 205, 305), // Ward
        // --- Maneuvers (stamina cost, magickaCost=0) ---
        "be56c560-a4ba-47ad-8513-f24c342ca594" => (180, 280, 0, 0), // Adrenaline Dodge
        "1e7f0dd6-6015-4f65-b811-3246e407e330" => (145, 265, 0, 0), // Dodging Strike
        "e685e88f-4e3f-4b9c-8f1a-2c3d5e6f7a8b" => (195, 275, 0, 0), // Focusing Dodge
        "cc768bae-a063-4885-8207-f39c6542fb36" => (215, 320, 0, 0), // Guardbreaker
        "69ffa3fd-deb7-4824-bab6-ac6450f19676" => (190, 300, 0, 0), // Harrying Bash
        "66610227-d1e2-4f3a-b4c5-6d7e8f9a0b1c" => (265, 380, 0, 0), // Indomitable Smash
        "cdab44fb-6ff6-4701-a4ec-d19cce79e49f" => (180, 265, 0, 0), // Piercing Strikes
        "ce6b63e9-9f18-49c4-aee0-51f7985f9892" => (145, 230, 0, 0), // Power Attack
        "eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13" => (150, 240, 0, 0), // Quick Strikes
        "0cfe29cd-5e6f-4a7b-8c9d-0e1f2a3b4c5d" => (425, 570, 0, 0), // Reckless Fury
        "e08f95de-85bb-4829-ba7e-cf45bc6fb422" => (250, 345, 0, 0), // Recovery Strikes
        "ba61ce46-163f-4a61-8ede-f5b7ae365e40" => (290, 425, 0, 0), // Reflecting Bash
        "7f78d342-9a0b-4c1d-8e2f-3a4b5c6d7e8f" => (205, 265, 0, 0), // Renewing Dodge
        "f9a2373b-a84f-4716-90ce-165baa2dd6ed" => (155, 260, 0, 0), // Shield Bash
        "c112c956-7d8e-4f0a-b1c2-3d4e5f6a7b8c" => (175, 275, 0, 0), // Skullcrusher
        "9b915ec3-c63b-4b62-b417-4c5436d45fc1" => (235, 360, 0, 0), // Staggering Bash
        "e14eedd5-2f3a-4b5c-9d0e-1f2a3b4c5d6e" => (210, 305, 0, 0), // Venom Strikes
        _ => return (0, 0), // unknown ability: no cost (cooldown gate still fires)
    };
    // Linear interpolation between R1 and R6 anchors (cost ∝ rank − 1).
    // rank is 1-based; clamp to avoid underflow.
    let r = rank.max(1) as u32;
    let stam = r1s + (r6s.saturating_sub(r1s)) * r.saturating_sub(1) / 5;
    let mag  = r1m + (r6m.saturating_sub(r1m)) * r.saturating_sub(1) / 5;
    (stam, mag)
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

    /// APK-authoritative costs (spec §1): spot-check R1 and R6 anchors for a spell
    /// (Fireball, magicka) and a maneuver (Quick Strikes, stamina); unknown UUID → (0,0).
    #[test]
    fn ability_cost_r1_r6_and_linear_ramp() {
        // Fireball (spell): R1=90 mag, R6=150 mag, stam=0.
        assert_eq!(ability_cost("d07a8d30-9a1c-49b0-866d-97a8aa1534cf", 1), (0, 90));
        assert_eq!(ability_cost("d07a8d30-9a1c-49b0-866d-97a8aa1534cf", 6), (0, 150));
        // Quick Strikes (maneuver): R1=150 stam, R6=240 stam, mag=0.
        assert_eq!(ability_cost("eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13", 1), (150, 0));
        assert_eq!(ability_cost("eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13", 6), (240, 0));
        // Linear ramp: R3 is between R1 and R6.
        let (s3, _) = ability_cost("eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13", 3);
        assert!(s3 > 150 && s3 < 240, "R3 Quick Strikes stam cost must be between R1 and R6");
        // Unknown UUID: zero cost (no gate, no deduction).
        assert_eq!(ability_cost("unknown-uuid", 1), (0, 0));
    }

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

    /// The Light combo ramp reproduces the s506 recorded per-depth anchors EXACTLY
    /// (combo 0→1.00, 1→1.45, 2→1.50, 3→2.65, 4→4.12) and holds at the ×4.12 ceiling
    /// beyond — `docs/arena-combat-reproduction-spec.md` §2a/§4.2. (The earlier
    /// geometric `1.45^n` overshot the recorded ×2.65 deep step; the ramp is irregular.)
    #[test]
    fn light_combo_ramp_matches_s506() {
        assert_eq!(combo_factor(Weight::Light, 0), 1.0, "fresh swing = un-combo'd base");
        assert!((combo_factor(Weight::Light, 1) - 1.45).abs() < 1e-3, "first chained step 1.45 (165.1/113.8)");
        assert!((combo_factor(Weight::Light, 2) - 1.50).abs() < 1e-3, "second step ~1.50 (171.8/113.8)");
        // The recorded deep steps are EXACT now (table-driven, not geometric).
        assert!((combo_factor(Weight::Light, 3) - 2.65).abs() < 1e-3, "deep combo = recorded ×2.65 (301.8/113.8)");
        assert!((combo_factor(Weight::Light, 4) - 4.12).abs() < 1e-3, "deeper combo = recorded ×4.12 (469.3/113.8)");
        assert_eq!(combo_factor(Weight::Light, 4), 4.12, "combo is capped at the recorded ×4.12 ceiling");
        assert_eq!(combo_factor(Weight::Light, 9), 4.12, "and stays capped past the ceiling");
        // Monotonic non-decreasing ramp.
        for c in 0..8 {
            assert!(combo_factor(Weight::Light, c + 1) >= combo_factor(Weight::Light, c));
        }
    }
}
