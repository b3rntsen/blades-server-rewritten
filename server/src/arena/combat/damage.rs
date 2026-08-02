//! Damage model — turns a fighter's loadout + a swing/cast into the per-type
//! damage components that go into a `ReceiveDamage`.
//!
//! Structure recovered by RE of `libil2cpp.so` (sha256 9fc19d29…), validated
//! against the captured `ReceiveDamage` frames (s293 / s506):
//!
//! ```text
//! physical[type]  = (weaponBase(item, tempering) − armorCut) × comboFactor(depth)
//! elemental[type] = enchantDamage(family, tier) × elementAmp(conditioning)
//!                   (+ Frost→Stamina / Shock→Magicka mirrored drain)
//! block           = a FRACTION from the defender's Block Rating (phys 1.0 /
//!                   elem 0.6666667); a connected OPTIMAL block additionally
//!                   negates physical outright
//! resistance      = a FLAT subtraction from the defender's Resistance Rating
//! totalDamage     = Σ components of HEALTH-affecting types (Slashing..Poison)
//! ```
//!
//! # Phase 3 — where the numbers come from now
//!
//! | quantity | before | now |
//! |---|---|---|
//! | weapon base | `weapon_base_for_level(level, Light)` | `gamedata::weapon().base_damage` + `tables::tempering_bonus` |
//! | enchant | `13.73 × tier` (linear GUESS) | the family's own convex `_value` curve × [`tables::ENCHANT_DAMAGE_PER_VALUE`] |
//! | enchant drain | *always* an equal **Magicka** drain | `frostDamageToStaminaDamage` / `shockDamageToMagickaDamage` only |
//! | armor | *not modelled* | `tables::armor_reduction` (Phase 3.3) |
//! | resistance | a flat loadout number | a **Resistance Rating** (Phase 3.4) |
//! | block | fixed `÷1.6` / `÷1.23` | `tables::block_reduction` from the item Block Rating (Phase 3.5) |
//!
//! ## Where armor is applied (data-derived, and it is NOT where the RE doc says)
//!
//! `blades-combat-formulae.md` §1 writes `physFinal = physRaw × swingMult −
//! armorReduction`, i.e. armor AFTER the combo roll. The s506 ramp falsifies that:
//! the recorded chain is **exactly proportional** to the combo-0 value
//! (113.82 → 165.07 = ×1.4503, → 469.30 = ×4.123). If a flat armor cut were taken
//! after the multiplier, the ratios would compress as the swing grows. Armor is
//! therefore applied to the **base**, before the swing factor. [Phase 3.3]

use std::time::Instant;

use super::gamedata::combat_params;
use super::state::{ActiveSide, BlockPhase, DamageSource, DamageType, Fighter, Loadout};
use super::tables;

/// A resolved hit: the per-type components (incl. stat drains) + the
/// health-affecting total + flags + most-resisted, ready for `messages::receive_damage`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedDamage {
    pub source: DamageSource,
    pub active_side: ActiveSide,
    pub flags: u8,
    /// All components, including Magicka/Stamina drains (which are excluded from `total`).
    pub components: Vec<(DamageType, f32)>,
    /// Sum of health-affecting components only (matches the wire `totalDamage`).
    pub total: f32,
    pub most_resisted: DamageType,
    /// True iff a Ward/Absorb/Dodge negation pool ate the WHOLE hit.
    pub negated: bool,
    /// HP the negation healed back to the DEFENDER (Absorb only).
    pub heal: f32,
}

/// Damage flags (`ReceiveDamage` propId 7 bitfield).
pub mod flags {
    pub const SHOW_DAMAGE: u8 = 0b0001;
    pub const HAS_ATTACKER: u8 = 0b0010;
    pub const WAS_LATE_BLOCKING: u8 = 0b0100;
    pub const WAS_OPTIMAL_BLOCKING: u8 = 0b1000;
}

/// True for damage types that reduce health (and so count toward `totalDamage`).
pub fn is_health_type(t: DamageType) -> bool {
    matches!(
        t,
        DamageType::Slashing
            | DamageType::Cleaving
            | DamageType::Bashing
            | DamageType::Fire
            | DamageType::Frost
            | DamageType::Shock
            | DamageType::Poison
            | DamageType::Health
    )
}

/// True for physical damage categories — the ones Armor Rating reduces.
/// **Correction 3:** `1 = Slashing, 2 = Cleaving, 3 = Bashing` are three *swing
/// shapes*, not a physical/elemental split.
pub fn is_physical(t: DamageType) -> bool {
    matches!(t, DamageType::Slashing | DamageType::Cleaving | DamageType::Bashing)
}

/// Elemental damage types (Fire/Frost/Shock/Poison).
pub fn is_elemental(t: DamageType) -> bool {
    matches!(t, DamageType::Fire | DamageType::Frost | DamageType::Shock | DamageType::Poison)
}

/// The **secondary stat drain** an elemental damage track mirrors into, from the
/// shipped `CombatParameters`:
///
/// * `frostDamageToStaminaDamage = 1` → **Frost drains Stamina**;
/// * `shockDamageToMagickaDamage = 1` → **Shock drains Magicka**.
///
/// Fire and Poison have no mirrored drain.
///
/// **Phase 3.6 correction:** the old model gave *every* enchant an equal
/// **Magicka** drain, which is only right for Shock. The captured pairings are
/// `Shock 18.6 + Magicka 18.6` / `Shock 72 + Magicka 72` (s293) — a Shock/Magicka
/// pair; nothing in the capture pairs Fire or Poison with a drain.
pub fn mirrored_drain(ty: DamageType) -> Option<(DamageType, f32)> {
    match ty {
        DamageType::Frost => Some((DamageType::Stamina, combat_params::FROST_DAMAGE_TO_STAMINA_DAMAGE)),
        DamageType::Shock => Some((DamageType::Magicka, combat_params::SHOCK_DAMAGE_TO_MAGICKA_DAMAGE)),
        _ => None,
    }
}

/// The physical-swing multiplier for a hit (`docs/arena-combat-reproduction-spec.md`
/// §4.2). Normal swings are **Left/Right** and combo-driven; **`Middle` is the
/// maneuver/charged-crit lane**.
fn swing_multiplier(weight: tables::Weight, combo_count: u32, active_side: ActiveSide) -> f32 {
    match active_side {
        ActiveSide::Middle => weight.crit_combo().0,
        ActiveSide::Left | ActiveSide::Right => tables::combo_factor(weight, combo_count),
        ActiveSide::None => 1.0,
    }
}

/// Elemental **amplification** as the target's matching-element conditioning stacks
/// (`docs/arena-combat-reproduction-spec.md` §4.3): the recorded Poison track ramped
/// ×1.00 → ×1.50 over the fight. Linear in
/// `recent_element_damage / condition_threshold`, where the threshold is the shipped
/// `healthPercentToCauseStatus × maxHP`.
pub const ELEMENT_AMP_MAX: f32 = 1.5;
pub fn element_amp(recent_element_damage: f32, condition_threshold: f32) -> f32 {
    if condition_threshold <= 0.0 {
        return 1.0;
    }
    let frac = (recent_element_damage / condition_threshold).clamp(0.0, 1.0);
    1.0 + (ELEMENT_AMP_MAX - 1.0) * frac
}

/// The per-CATEGORY block outcome.
///
/// * a connected **OPTIMAL** block: physical **negated** (the parry — the s506
///   per-hit ground truth: Slashing 113.82 → 0.77 across 27 connected blocks,
///   mean 1.17), elemental reduced by the defender's Block Rating read at the
///   optimal weight (`optimalBlockBoost × 2` — uesp "high block").
/// * a **LATE** block (guard held > `BLOCK_OPTIMAL_TIME` 2.0 s, OR re-raised inside
///   `postOptimalBlockResetTime` 1.4 s): both categories take the plain Block-Rating
///   reduction.
///
/// **Phase 3.5:** this replaces the hardcoded `÷1.6` / `÷1.23` divisors with
/// [`tables::block_reduction`]. The old constants remain below only as the
/// documented `PvpDefaultSettings` values they came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockOutcome {
    pub flag: u8,
    pub optimal: bool,
    pub blocking: bool,
    /// Block Rating in effect for this hit (already optimal-weighted).
    pub rating: f32,
}

/// The dump's `PvpDefaultSettings` LATE-block divisors, kept for provenance. The
/// live model derives its reduction from the Block Rating instead.
pub const PHYSICAL_BLOCK_MULTIPLIER: f32 = 1.6;
pub const ELEMENTAL_BLOCK_MULTIPLIER: f32 = 1.23;

impl BlockOutcome {
    /// The per-component damage multiplier for `ty` under this block outcome.
    /// `continuous` marks DoT damage — note that
    /// `continuousDamageBlockingEffectiveness == 1`, so blocking is at FULL
    /// effectiveness against DoT (correction 1), unlike absorb/fortify/resistance.
    pub fn factor_for(&self, ty: DamageType) -> f32 {
        if !self.blocking {
            return 1.0;
        }
        if is_physical(ty) {
            if self.optimal {
                // The connected optimal block negates physical outright.
                return 0.0;
            }
            return 1.0 - tables::block_reduction(self.rating, true);
        }
        if is_elemental(ty) {
            return 1.0 - tables::block_reduction(self.rating, false);
        }
        // Stamina/Magicka drains and raw Health are not blocked.
        1.0
    }
}

/// Resolve the block outcome for a hit on `target` swung on `active_side`.
///
/// OPTIMAL requires BOTH: the defender is in the `Optimal` phase AND the defending
/// side matches the attacking side. Wrong-side in the Optimal phase is still LATE.
pub fn block_outcome(target: &Fighter, active_side: ActiveSide, now: Instant) -> BlockOutcome {
    use super::state::ActorStateType;
    let none = BlockOutcome { flag: 0, optimal: false, blocking: false, rating: 0.0 };
    if target.actor_state != ActorStateType::Blocking || active_side == ActiveSide::None {
        return none;
    }
    let Some(phase) = target.block_phase(now) else {
        return none;
    };
    let side_matches = target.blocking_side == active_side;
    let optimal = matches!(phase, BlockPhase::Optimal) && side_matches;
    BlockOutcome {
        flag: if optimal { flags::WAS_OPTIMAL_BLOCKING } else { flags::WAS_LATE_BLOCKING },
        optimal,
        blocking: true,
        rating: target.block_rating(optimal),
    }
}

/// The damage model the arena uses.
pub trait DamageModel {
    fn resolve_attack(
        &self,
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        active_side: ActiveSide,
        swing_factor: f32,
        combo_count: u32,
        now: Instant,
    ) -> ResolvedDamage;

    /// Resolve an ability/spell cast → Spell-source damage on `target`. `ability_uuid`
    /// selects the shipped `_damage`/`damage_type`; an unknown uuid falls back to
    /// Fireball's per-rank curve.
    fn resolve_ability(
        &self,
        ability_uuid: &str,
        ability_level: u8,
        target: &Fighter,
        active_side: ActiveSide,
        now: Instant,
    ) -> ResolvedDamage;
}

/// The RE-derived model, now running on the shipped item/ability/enchant data.
pub struct RetailDamageModel;

impl RetailDamageModel {
    /// The attacker's per-type PHYSICAL base **after** the defender's Armor Rating,
    /// before the swing/combo factor. [Phase 3.3 — see the module doc for why armor
    /// lands here and not after the multiplier.]
    fn physical_base_after_armor(attacker: &Loadout, target: &Fighter) -> Vec<(DamageType, f32)> {
        let armor_rating = (target.loadout.armor_rating - attacker.armor_piercing_rating).max(0.0);
        attacker
            .weapon
            .base_by_type
            .iter()
            .map(|(ty, base)| {
                let cut = if is_physical(*ty) {
                    tables::armor_reduction(*base, armor_rating)
                } else {
                    0.0
                };
                (*ty, (base - cut).max(0.0))
            })
            .collect()
    }

    /// Build the per-type damage components for a weapon swing.
    fn swing_components(
        attacker: &Loadout,
        target: &Fighter,
        active_side: ActiveSide,
        swing_factor: f32,
        combo_count: u32,
    ) -> Vec<(DamageType, f32)> {
        let weight = attacker.weapon.weight.unwrap_or(tables::Weight::Light);
        let scale = swing_multiplier(weight, combo_count, active_side) * swing_factor;

        let mut components: Vec<(DamageType, f32)> = Vec::new();
        for (ty, base) in Self::physical_base_after_armor(attacker, target) {
            components.push((ty, base * scale));
        }
        // Enchant tracks: independent of the physical combo roll (capture-validated,
        // §4.3). The magnitude is the family's own shipped curve value; the
        // mirrored stat drain is per-element, NOT a blanket Magicka drain.
        for (ench_ty, magnitude) in enchant_tracks(attacker) {
            let amp = target.element_amp_for(ench_ty) * (1.0 + fortify_for(attacker, ench_ty));
            let v = magnitude * amp;
            components.push((ench_ty, v));
            if let Some((drain_ty, ratio)) = mirrored_drain(ench_ty) {
                components.push((drain_ty, v * ratio));
            }
        }
        components
    }
}

/// The attacker's resolved enchant damage tracks: each `(element, tier)` mapped
/// through that element's shipped `Weapon <Element> Damage` family curve.
///
/// The magnitude is deliberately derived here rather than cached on the loadout —
/// a cached copy silently desyncs whenever caller code sets `enchants` alone
/// (which several engine tests do).
fn enchant_tracks(attacker: &Loadout) -> Vec<(DamageType, f32)> {
    attacker
        .enchants
        .iter()
        .map(|(ty, tier)| (*ty, weapon_damage_family_value(*ty, *tier)))
        .collect()
}

/// The shipped `Weapon <Element> Damage` family for an element.
pub fn weapon_damage_family_value(ty: DamageType, tier: u8) -> f32 {
    let family = match ty {
        DamageType::Fire => "c40ed851-8777-4d09-b169-0223dae8f67d",
        DamageType::Frost => "63b6c73a-af1a-4f95-8ffe-9434b8e68d56",
        DamageType::Shock => "139024a7-3965-4e90-a4c1-60e3d7ca3133",
        DamageType::Poison => "08ea75d0-5cf1-44a9-9816-d3c6740c4191",
        DamageType::Stamina => "9fdbb542-ff37-4199-93a3-d9444cca9090",
        DamageType::Magicka => "5a145cf8-3a20-4b8a-bf6d-8ee1607d3417",
        _ => return 0.0,
    };
    tables::enchant_damage(family, tier).unwrap_or(0.0)
}

fn fortify_for(attacker: &Loadout, ty: DamageType) -> f32 {
    attacker
        .element_fortify
        .iter()
        .filter(|(t, _)| *t == ty)
        .map(|(_, v)| *v)
        .sum()
}

impl DamageModel for RetailDamageModel {
    fn resolve_attack(
        &self,
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        active_side: ActiveSide,
        swing_factor: f32,
        combo_count: u32,
        now: Instant,
    ) -> ResolvedDamage {
        let mut components =
            Self::swing_components(attacker, target, active_side, swing_factor, combo_count);
        finish_resolved(attacker, target, source, active_side, &mut components, now)
    }

    fn resolve_ability(
        &self,
        ability_uuid: &str,
        ability_level: u8,
        target: &Fighter,
        active_side: ActiveSide,
        now: Instant,
    ) -> ResolvedDamage {
        // The shipped per-rank `_damage` + the ability's own `damage_type`.
        let (ty, base) = match super::gamedata::ability(ability_uuid) {
            Some(a) => {
                let dmg = tables::ability_damage(ability_uuid, ability_level).unwrap_or(0.0);
                let ty = a
                    .damage_type
                    .map(super::loadout::map_damage_type)
                    .unwrap_or(DamageType::Fire);
                (ty, dmg)
            }
            // Unknown ability: Fireball's per-rank curve is the representative spell.
            None => (
                DamageType::Fire,
                tables::ability_damage(super::gamedata::ids::FIREBALL, ability_level).unwrap_or(0.0),
            ),
        };
        let mut components = vec![(ty, base)];
        if let Some((drain_ty, ratio)) = mirrored_drain(ty) {
            components.push((drain_ty, base * ratio));
        }
        finish_resolved(&Loadout::default(), target, DamageSource::Spell, active_side, &mut components, now)
    }
}

/// Apply the post-roll mitigation pipeline and assemble the [`ResolvedDamage`]:
///   block (a FRACTION, per category) → resistance (a FLAT rating subtraction) →
///   weakness (a FLAT rating increase) → Σ health = total.
///
/// Negation pools (Ward/Absorb/Dodge) are drained later by `resolve::emit_damage`
/// (they mutate the defender).
fn finish_resolved(
    attacker: &Loadout,
    target: &Fighter,
    source: DamageSource,
    active_side: ActiveSide,
    components: &mut Vec<(DamageType, f32)>,
    now: Instant,
) -> ResolvedDamage {
    let mut hit_flags = flags::SHOW_DAMAGE | flags::HAS_ATTACKER;
    let continuous = source == DamageSource::StatusEffect;

    // 1) BLOCK — a fraction from the defender's Block Rating. NOT de-rated against
    //    continuous damage (`continuousDamageBlockingEffectiveness == 1`).
    let block = block_outcome(target, active_side, now);
    hit_flags |= block.flag;
    for (ty, v) in components.iter_mut() {
        *v *= block.factor_for(*ty);
    }

    // 2) RESISTANCE — a FLAT subtraction driven by the defender's Resistance Rating,
    //    capped at `maximumResistanceReduction`, with elemental resistance first
    //    pierced by the attacker's Elemental-Resistance-Piercing rating/fraction.
    //    Transient Resist-Elements amounts are ratings too and add in.
    let mut most_resisted = DamageType::None;
    let mut most_resisted_frac = MOST_RESISTED_FLOOR;
    for (ty, v) in components.iter_mut() {
        let before = *v;
        if before <= 0.0 {
            continue;
        }
        let rating = target.resistance_rating_against(
            *ty,
            attacker.elem_resist_piercing,
            attacker.elem_resist_piercing_rating,
        ) + target.transient_resistance_against(*ty, now);
        let resisted = tables::resistance_reduction(before, rating, continuous);
        let gained = tables::weakness_increase(before, target.weakness_rating_against(*ty), continuous);
        *v = (before - resisted + gained).max(0.0);
        if resisted > 0.0 && is_elemental(*ty) {
            let frac = resisted.min(before) / before;
            if frac > most_resisted_frac {
                most_resisted_frac = frac;
                most_resisted = *ty;
            }
        }
    }

    let total: f32 = components
        .iter()
        .filter(|(t, _)| is_health_type(*t))
        .map(|(_, v)| *v)
        .sum();

    ResolvedDamage {
        source,
        active_side,
        flags: hit_flags,
        components: std::mem::take(components),
        total,
        most_resisted,
        negated: false,
        heal: 0.0,
    }
}

/// Minimum resisted fraction for an element to be reported as `mostResisted`
/// (`CombatHUDHelper.DetermineMostResistedElementalDamageType`). The shipped
/// `resistMessagingThreshold` is 0.2 — kept at the lower 0.05 wire floor because the
/// capture reports `mostResisted` well below the *messaging* threshold.
const MOST_RESISTED_FLOOR: f32 = 0.05;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::combat::gamedata;
    use crate::arena::combat::loadout;
    use crate::arena::combat::state::{
        ActorStateType, DamageNegationSource, Loadout, NegationPool, WeaponProfile,
    };
    use crate::arena::combat::tables::Weight;
    use std::time::{Duration, Instant};

    /// The real s506 weapon: a Dragonbone Dagger at tempering 10 with a tier-10
    /// `Weapon Poison Damage` enchant.
    fn poison_dagger() -> Loadout {
        let w = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).expect("dagger");
        Loadout {
            level: 86,
            weapon: loadout::weapon_profile(w, 10),
            weapon_template: Some(w),
            enchants: vec![(DamageType::Poison, 10)],
            ..Default::default()
        }
    }

    fn plain_blade(weight: Weight) -> Loadout {
        Loadout {
            weapon: WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 100.0)],
                weight: Some(weight),
            },
            enchants: vec![],
            ..Default::default()
        }
    }

    /// An un-armored, un-blocking L100 target.
    fn target() -> Fighter {
        Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, Instant::now())
    }

    fn comp(rd: &ResolvedDamage, ty: DamageType) -> f32 {
        rd.components.iter().filter(|(t, _)| *t == ty).map(|(_, v)| *v).sum()
    }

    #[test]
    fn combo_ramp_drives_physical_not_enchant() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let now = Instant::now();
        let c0 = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        let c1 = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Left, 1.0, 1, now);
        let c4 = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 4, now);

        // Un-armored: the raw tempered base of 144.0.
        assert!((comp(&c0, DamageType::Slashing) - 144.0).abs() < 0.5);
        assert!((comp(&c1, DamageType::Slashing) - 144.0 * 1.45).abs() < 0.5);
        assert!((comp(&c4, DamageType::Slashing) - 144.0 * 4.12).abs() < 1.0);
        // The enchant track is combo-independent.
        assert!((comp(&c0, DamageType::Poison) - comp(&c1, DamageType::Poison)).abs() < 1e-3);
        assert!((comp(&c4, DamageType::Poison) - comp(&c0, DamageType::Poison)).abs() < 1e-3);
    }

    /// ARMOR (Phase 3.3): a Rating removes `rating × 0.1` from the BASE, so the
    /// combo ramp stays exactly proportional to the post-armor value.
    #[test]
    fn armor_rating_cuts_the_base_and_preserves_the_ramp() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let mut armored = target();
        armored.loadout.armor_rating = 301.8; // → 30.18 flat
        let now = Instant::now();
        let c0 = comp(
            &m.resolve_attack(&lo, &armored, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now),
            DamageType::Slashing,
        );
        let c1 = comp(
            &m.resolve_attack(&lo, &armored, DamageSource::Attack, ActiveSide::Left, 1.0, 1, now),
            DamageType::Slashing,
        );
        assert!((c0 - 113.82).abs() < 0.05, "144 − 30.18 = 113.82, got {c0}");
        assert!((c1 / c0 - 1.45).abs() < 1e-3, "the ramp stays proportional, got {}", c1 / c0);
        // Armor does NOT touch the elemental track.
        let poison = comp(
            &m.resolve_attack(&lo, &armored, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now),
            DamageType::Poison,
        );
        assert!((poison - 137.32).abs() < 0.5, "armor is physical-only, got {poison}");
        // Armor Piercing eats the rating.
        let mut piercer = poison_dagger();
        piercer.armor_piercing_rating = 301.8;
        let pierced = comp(
            &m.resolve_attack(&piercer, &armored, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now),
            DamageType::Slashing,
        );
        assert!((pierced - 144.0).abs() < 0.05, "full piercing removes the armor cut");
    }

    /// ENCHANT (Phase 3.6): the magnitude follows the family's convex curve, and
    /// Poison has **no** mirrored stat drain (only Frost→Stamina, Shock→Magicka).
    #[test]
    fn enchant_uses_the_family_curve_and_the_right_drain() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let rd = m.resolve_attack(&poison_dagger(), &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd, DamageType::Poison) - 137.32).abs() < 0.5);
        assert_eq!(comp(&rd, DamageType::Magicka), 0.0, "Poison does NOT drain Magicka");
        assert_eq!(comp(&rd, DamageType::Stamina), 0.0, "Poison does NOT drain Stamina");

        // Shock DOES drain Magicka 1:1; Frost drains Stamina 1:1.
        let mut shock = plain_blade(Weight::Light);
        shock.enchants = vec![(DamageType::Shock, 7)];
        let s = m.resolve_attack(&shock, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(comp(&s, DamageType::Shock) > 0.0);
        assert!((comp(&s, DamageType::Magicka) - comp(&s, DamageType::Shock)).abs() < 1e-3);

        let mut frost = plain_blade(Weight::Light);
        frost.enchants = vec![(DamageType::Frost, 4)];
        let f = m.resolve_attack(&frost, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(comp(&f, DamageType::Frost) > 0.0);
        assert!((comp(&f, DamageType::Stamina) - comp(&f, DamageType::Frost)).abs() < 1e-3);
        assert_eq!(comp(&f, DamageType::Magicka), 0.0, "Frost drains STAMINA, not Magicka");
        // Drains never count toward the wire total.
        assert!((s.total - comp(&s, DamageType::Slashing) - comp(&s, DamageType::Shock)).abs() < 1e-3);
    }

    /// BLOCK (Phase 3.5): the reduction is a FRACTION from the defender's Block
    /// Rating; a connected optimal block negates physical outright.
    #[test]
    fn block_is_rating_driven() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let now = Instant::now();
        let open = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);

        // A shield-bearing defender: Ebony Shield 330 + Dragonbone Dagger 49.5.
        let mut def = target();
        def.loadout.block_rating = 330.0 + 49.5;
        def.loadout.shield_optimal_block_boost = 1.0;
        def.actor_state = ActorStateType::Blocking;
        def.blocking_side = ActiveSide::Right;
        def.block_raised_at = Some(now);
        def.blocking_until = Some(now + Duration::from_secs(2));

        let opt = m.resolve_attack(&lo, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(opt.flags & flags::WAS_OPTIMAL_BLOCKING != 0);
        assert_eq!(comp(&opt, DamageType::Slashing), 0.0, "connected optimal block negates physical");
        // Elemental takes the rating reduction at the optimal (×2) weight.
        let expect_elem = comp(&open, DamageType::Poison) * (1.0 - tables::block_reduction(759.0, false));
        assert!(
            (comp(&opt, DamageType::Poison) - expect_elem).abs() < 0.5,
            "elem {} vs {}",
            comp(&opt, DamageType::Poison),
            expect_elem
        );

        // LATE / wrong-side: the plain (un-doubled) rating applies to BOTH categories.
        let mut late_t = def.clone();
        late_t.blocking_side = ActiveSide::Left;
        let late = m.resolve_attack(&lo, &late_t, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(late.flags & flags::WAS_LATE_BLOCKING != 0);
        let expect_phys = comp(&open, DamageType::Slashing) * (1.0 - tables::block_reduction(379.5, true));
        assert!((comp(&late, DamageType::Slashing) - expect_phys).abs() < 0.5);
        assert!(comp(&late, DamageType::Slashing) > 0.0, "a late block does NOT negate");
    }

    /// RESISTANCE (Phase 3.4): a Resistance Rating is a flat subtraction; the
    /// attacker's Elemental-Resistance-Piercing RATING eats it first.
    #[test]
    fn resistance_is_a_rating_pierced_by_a_rating() {
        let m = RetailDamageModel;
        let mut tgt = target();
        tgt.loadout.resistances = vec![(DamageType::Poison, 40.0)];
        let now = Instant::now();
        let rd = m.resolve_attack(&poison_dagger(), &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd, DamageType::Poison) - 97.32).abs() < 0.5);
        assert_eq!(rd.most_resisted, DamageType::Poison);

        let mut piercer = poison_dagger();
        piercer.elem_resist_piercing_rating = 25.0;
        let rd2 = m.resolve_attack(&piercer, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(
            (comp(&rd2, DamageType::Poison) - (137.32 - 15.0)).abs() < 0.5,
            "piercing 25 of the 40 rating leaves 15, got {}",
            comp(&rd2, DamageType::Poison)
        );
        // Resistance can never remove more than 95 % of the component.
        let mut wall = target();
        wall.loadout.resistances = vec![(DamageType::Poison, 100_000.0)];
        let rd3 = m.resolve_attack(&poison_dagger(), &wall, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd3, DamageType::Poison) - 137.32 * 0.05).abs() < 0.5);
    }

    /// WEAKNESS is a separate flat INCREASE, capped at ×1 of the component — it no
    /// longer silently cancels a resistance.
    #[test]
    fn weakness_increases_and_is_capped() {
        let m = RetailDamageModel;
        let mut tgt = target();
        tgt.loadout.weaknesses = vec![(DamageType::Poison, 50.0)];
        let now = Instant::now();
        let rd = m.resolve_attack(&poison_dagger(), &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd, DamageType::Poison) - 187.32).abs() < 0.5);
        let mut huge = target();
        huge.loadout.weaknesses = vec![(DamageType::Poison, 100_000.0)];
        let rd2 = m.resolve_attack(&poison_dagger(), &huge, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd2, DamageType::Poison) - 137.32 * 2.0).abs() < 0.5, "capped at ×2");
    }

    #[test]
    fn deep_combo_hit_is_not_clamped() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let tgt = target();
        let now = Instant::now();
        let rd = m.resolve_attack(&lo, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 4, now);
        let health_sum: f32 = rd.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert!((rd.total - health_sum).abs() < 1e-3);
        // The total is the exact Σ of components — no clamp scaling anywhere.
        let expect = 144.0 * tables::combo_factor(Weight::Light, 4) + 137.32;
        assert!((rd.total - expect).abs() < 1.0, "unclamped total {} != {expect}", rd.total);
    }

    #[test]
    fn negation_pool_eats_hit_and_absorb_heals() {
        let now = Instant::now();
        let mut tgt = target();
        tgt.negation_pools.push(NegationPool {
            source: DamageNegationSource::Absorb,
            remaining: 10_000.0,
            expires_at: now + Duration::from_secs(5),
            restoration_factor: 1.0,
        });
        let mut components = vec![(DamageType::Slashing, 200.0), (DamageType::Poison, 137.3), (DamageType::Magicka, 137.3)];
        let res = tgt.apply_negation_pools(&mut components);
        assert!(res.negated);
        assert!((res.heal - (200.0 + 137.3)).abs() < 1e-2);
        let health: f32 = components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert_eq!(health, 0.0);
    }

    /// Abilities now deal their SHIPPED per-rank `_damage` in their own damage type.
    #[test]
    fn ability_damage_comes_from_the_shipped_rank() {
        let now = Instant::now();
        let m = RetailDamageModel;
        let r1 = m.resolve_ability(gamedata::ids::FIREBALL, 1, &target(), ActiveSide::Middle, now);
        let r3 = m.resolve_ability(gamedata::ids::FIREBALL, 3, &target(), ActiveSide::Middle, now);
        assert_eq!(r1.source, DamageSource::Spell);
        assert!((comp(&r1, DamageType::Fire) - 73.89).abs() < 0.01, "Fireball R1 = 73.89");
        assert!((comp(&r3, DamageType::Fire) - 150.24).abs() < 0.01, "Fireball R3 = 150.24");
        // A Shock spell drains Magicka as well.
        let bolt = m.resolve_ability("7fc15804-1637-40a9-8dcc-3ea1eb0f778d", 1, &target(), ActiveSide::Middle, now);
        assert!(comp(&bolt, DamageType::Shock) > 0.0);
        assert!((comp(&bolt, DamageType::Magicka) - comp(&bolt, DamageType::Shock)).abs() < 1e-3);
        // Paralyze deals Poison at its shipped 88.7 @ R1.
        let par = m.resolve_ability(gamedata::ids::PARALYZE, 1, &target(), ActiveSide::Middle, now);
        assert!((comp(&par, DamageType::Poison) - 88.7).abs() < 0.01);
    }

    /// Blocking is at FULL effectiveness against continuous (DoT) damage —
    /// `continuousDamageBlockingEffectiveness == 1` (correction 1) — while
    /// resistance IS de-rated to 0.75.
    #[test]
    fn blocking_is_not_derated_vs_dot_but_resistance_is() {
        assert_eq!(combat_params::CONTINUOUS_DAMAGE_BLOCKING_EFFECTIVENESS, 1.0);
        assert_eq!(combat_params::CONTINUOUS_DAMAGE_RESISTANCE_EFFECTIVENESS, 0.75);
        let m = RetailDamageModel;
        let now = Instant::now();
        let mut tgt = target();
        tgt.loadout.resistances = vec![(DamageType::Poison, 40.0)];
        tgt.loadout.block_rating = 379.5;
        tgt.actor_state = ActorStateType::Blocking;
        tgt.blocking_side = ActiveSide::Right;
        tgt.block_raised_at = Some(now);
        tgt.blocking_until = Some(now + Duration::from_secs(2));
        let dot = m.resolve_attack(&poison_dagger(), &tgt, DamageSource::StatusEffect, ActiveSide::Right, 1.0, 0, now);
        let hit = m.resolve_attack(&poison_dagger(), &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        // Same block factor, but the DoT keeps MORE damage because its resistance
        // is de-rated (0.75 × 40 = 30 instead of 40).
        assert!(
            comp(&dot, DamageType::Poison) > comp(&hit, DamageType::Poison),
            "DoT {} should exceed the direct hit {} (resistance de-rated, block not)",
            comp(&dot, DamageType::Poison),
            comp(&hit, DamageType::Poison)
        );
    }
}
