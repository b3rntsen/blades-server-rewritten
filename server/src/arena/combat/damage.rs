//! Damage model — turns a fighter's loadout + a swing/cast into the per-type
//! damage components that go into a `ReceiveDamage`.
//!
//! Structure recovered by RE of `libil2cpp.so` (sha256 9fc19d29…), validated
//! against the 775 captured `ReceiveDamage` frames in session 293:
//!
//! ```text
//! finalDamage[type] = baseWeaponDamage[type] × (1 + factor)        (per health type)
//! factor            = CalculateAttackTypeFactor(source, activeSide, swingFactor)
//!                   = source_base + (swingFactor on Middle | 0 on Left/None | base on Right)
//! enchant           = independent 2nd damage-type track + an EQUAL Magicka/Stamina drain
//!                     (the drain is a component but is EXCLUDED from totalDamage)
//! block             = optimal/late block reduces (or negates) on the matching side
//! totalDamage       = Σ components of HEALTH-affecting types (Slashing..Poison, 1..=7)
//! ```
//!
//! `ResolveAttackDamage` @0x1BD2398, `CalculateAttackTypeFactor` @0x1BD3DF0,
//! `ResolveWeaponAlchemyEffect` @0x1BD3E7C, base weapon DamageList from Item data
//! (`GetTotalEquippedWeaponDamage` @0x1C557A4). The *constants* (per-source base,
//! per-tier enchant magnitude, the weapon's base DamageList) come from the game's
//! item/equipment data — see [`WeaponProfile`] in `state.rs`; the model is exact
//! once those are wired from the imported character (until then, loadout defaults
//! drive the formula and the structure/relationships below are what's verified).

use super::state::{ActiveSide, DamageSource, DamageType, Fighter, Loadout};
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
    /// True iff a Ward/Absorb/Dodge negation pool ate the WHOLE hit (every health
    /// component → 0). The caller emits `DamageNegated`(66) instead of (or alongside)
    /// the `ReceiveDamage` and skips the HP decrement. [status-resistance-spec §4]
    pub negated: bool,
    /// HP the negation healed back to the DEFENDER (Absorb's restoration of what it
    /// negated; 0 for Ward/Dodge). A Health effect → NOT arena-tripled. [§4.1]
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
/// Stamina(8)/Magicka(9) are resource drains and are excluded from the total.
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

/// The physical-swing multiplier for a hit, given the attacker's weapon `weight`, its
/// current `combo_count`, and the `ActiveSide` (`docs/arena-combat-reproduction-spec.md`
/// §4.2). The big fix vs the old fixed `swing_multiplier`: normal swings are **Left/Right
/// and combo-driven** (the factor COMPOUNDS on chained alternating swings, ×1.0 → ×1.45
/// → … → ~×4.12 for a Light dagger), while **`Middle` is the maneuver/charged-crit lane**
/// (the weight's `crit` factor), NOT the normal-swing crit the old model assumed.
fn swing_multiplier(weight: tables::Weight, combo_count: u32, active_side: ActiveSide) -> f32 {
    match active_side {
        // Middle = the charged WeaponManeuver lane: the crit factor (s506's Middle
        // maneuvers landed 201–274 Slashing ≈ base × the Light crit, not the combo ramp).
        ActiveSide::Middle => weight.crit_combo().0,
        // Normal swings: the compounding combo ramp (combo 0 = ×1.0 = the base).
        ActiveSide::Left | ActiveSide::Right => tables::combo_factor(weight, combo_count),
        ActiveSide::None => 1.0,
    }
}

/// Per-enchant base elemental magnitude at `tier`, by enchant `DamageType`
/// (`docs/arena-combat-reproduction-spec.md` §4.3). Recovered for **Weapon Poison
/// Damage** as ≈ **13.7 × tier** (137.32 @ t10 on the recorded match), ~2.2× SMALLER
/// than the old flat `30 × tier`. The other damage families share the generic shape;
/// their exact per-tier number is enchant-specific game-data we don't have, so they use
/// the same 13.7×tier surface — **flagged as a calibration GUESS** until a per-enchant
/// table (keyed by the enchant template id) is wired. [calibration knob]
fn enchant_base(ty: DamageType, tier: u8) -> f32 {
    // Per-element per-tier magnitude. Poison is capture-pinned (s506); the rest reuse
    // it as the representative surface (GUESS — no per-family capture).
    let per_tier = match ty {
        DamageType::Poison => 13.73, // capture-pinned: 137.32 / 10 (s506 Weapon Poison Damage)
        _ => 13.73,                  // GUESS: same surface for Fire/Frost/Shock/Stamina/Magicka
    };
    per_tier * tier as f32
}

/// Elemental **amplification** factor as the target's matching-element conditioning
/// stacks (`docs/arena-combat-reproduction-spec.md` §4.3). In the recorded match the
/// Poison track ramped ×1.00 → ×1.50 over the fight as Fortify-Poisoned / Fortify-Poison-
/// Damage + the target's poison-weakness conditioning + Elemental-Resistance-Piercing
/// stacked. Modeled here as a linear function of the target's accumulated [element]
/// damage in the sliding window, from ×1.0 (fresh) to a +50% ceiling (`ELEMENT_AMP_MAX`)
/// once the accumulation reaches the conditioning threshold (25% of max HP).
///
/// `recent_element_damage` = Σ of the matching element's `damage_history` window;
/// `condition_threshold` = `healthPercentToCauseStatus × max_health` (§5). Returns 1.0
/// when the target has no max HP or no accumulation. [calibration knob: the +50% endpoint
/// is capture-pinned; the linear shape is the simplest faithful interpolation.]
pub const ELEMENT_AMP_MAX: f32 = 1.5;
pub fn element_amp(recent_element_damage: f32, condition_threshold: f32) -> f32 {
    if condition_threshold <= 0.0 {
        return 1.0;
    }
    let frac = (recent_element_damage / condition_threshold).clamp(0.0, 1.0);
    1.0 + (ELEMENT_AMP_MAX - 1.0) * frac
}

/// True for physical damage categories (Slashing/Cleaving/Bashing) — blocked harder
/// than elemental (the asymmetric block, §4.4).
pub fn is_physical(t: DamageType) -> bool {
    matches!(t, DamageType::Slashing | DamageType::Cleaving | DamageType::Bashing)
}

/// **LATE / imperfect** block divisors (`PvpDefaultSettings`, dump.cs 427019-427020):
/// a held guard that catches the swing off-angle reduces physical ÷1.6, elemental
/// ÷1.23. These are the LATE tier, **not** the optimal one (the long-standing
/// cross-spec error this corrects — see `block_outcome`). [reproduction-spec §4.4]
pub const PHYSICAL_BLOCK_MULTIPLIER: f32 = 1.6;
pub const ELEMENTAL_BLOCK_MULTIPLIER: f32 = 1.23;

/// The corrected, per-CATEGORY block outcome (`docs/arena-combat-reproduction-spec.md`
/// §4.4 — the AUTHORITATIVE per-hit ground truth, which **supersedes** the
/// ÷1.6/÷1.23-for-optimal model in `arena-status-resistance-spec.md` §3 and
/// `blades-combat-formulae.md` §3):
///
///   - a CONNECTED **optimal** block (correct side, in the window) **negates physical
///     (×0.0)** but only **halves elemental (×0.5)** — phys≈0 / elem×0.5, capture-pinned
///     on s506 (seq 323: Slashing 113.82 → 0.77 ≈ 0, Poison 137.32 → 68.65 = ÷2.0);
///   - a **late / wrong-side** block is a *partial* reduction: physical ÷1.6,
///     elemental ÷1.23 (the dump's real `PvpDefaultSettings` constants — but the LATE
///     tier, not optimal).
///
/// `wasOptimalBlocking` is a defender-STATE bit (the server decides absorption from its
/// own side/timing, then sets the flag) — it is set here only when we applied the
/// *optimal* reduction. A non-blocking target ⇒ no reduction (factor 1.0, no flag).
#[derive(Debug, Clone, Copy, PartialEq)]
struct BlockOutcome {
    flag: u8,
    optimal: bool,
    blocking: bool,
}

impl BlockOutcome {
    /// The per-component damage multiplier for `ty` under this block outcome.
    fn factor_for(&self, ty: DamageType) -> f32 {
        if !self.blocking {
            return 1.0;
        }
        if self.optimal {
            // Optimal: physical NEGATED, elemental HALVED. (Stamina/Magicka drains and
            // raw Health pass at ×1.0 — only Physical/Elemental categories are blocked.)
            if is_physical(ty) {
                0.0
            } else if is_elemental(ty) {
                0.5
            } else {
                1.0
            }
        } else {
            // Late / wrong-side: the dump's partial divisors.
            if is_physical(ty) {
                1.0 / PHYSICAL_BLOCK_MULTIPLIER
            } else if is_elemental(ty) {
                1.0 / ELEMENTAL_BLOCK_MULTIPLIER
            } else {
                1.0
            }
        }
    }
}

/// Resolve the block outcome for a hit on `target` swung on `active_side`. Optimal =
/// the defender is guarding the MATCHING side within the block window; otherwise the
/// guard is up but off-angle (late). See [`BlockOutcome`] for the corrected factors.
fn block_outcome(target: &Fighter, active_side: ActiveSide) -> BlockOutcome {
    use super::state::ActorStateType;
    if target.actor_state != ActorStateType::Blocking || active_side == ActiveSide::None {
        return BlockOutcome { flag: 0, optimal: false, blocking: false };
    }
    let optimal = target.blocking_side == active_side;
    BlockOutcome {
        flag: if optimal { flags::WAS_OPTIMAL_BLOCKING } else { flags::WAS_LATE_BLOCKING },
        optimal,
        blocking: true,
    }
}

/// The damage model the arena uses. A trait so the RE-derived [`RetailDamageModel`]
/// can be swapped for tuning/tests without touching resolution or the builders.
pub trait DamageModel {
    /// Resolve a weapon swing from `attacker` against `target`, at the attacker's
    /// current `combo_count` (the chain depth driving the combo ramp — see
    /// [`swing_multiplier`]). `swing_factor` is the per-swing charge multiplier
    /// (1.0 = a normal commit).
    fn resolve_attack(
        &self,
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        active_side: ActiveSide,
        swing_factor: f32,
        combo_count: u32,
    ) -> ResolvedDamage;

    /// Resolve an ability/spell cast → Spell-source damage on `target`.
    fn resolve_ability(&self, ability_level: u8, target: &Fighter, active_side: ActiveSide) -> ResolvedDamage;
}

/// The RE-derived model (formula structure above). Number-exact once the weapon
/// `base_by_type` / source-base / enchant constants are wired from game data.
pub struct RetailDamageModel;

impl RetailDamageModel {
    /// Build the per-type damage components for a weapon swing (physical track ×
    /// combo/maneuver factor; per-enchant elemental track × element amplification + an
    /// equal Magicka drain). Pure of block/resist/negate — those are applied by the
    /// caller's pipeline. `weight` is the attacker's weapon class (drives the combo
    /// ramp); the elemental amplification reads the target's per-element conditioning.
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
        // Physical track: base weapon damage per type, scaled by the combo/maneuver factor.
        for (ty, base) in &attacker.weapon.base_by_type {
            components.push((*ty, base * scale));
        }
        // Enchant track: an independent damage-type component (amplified as the target's
        // matching-element conditioning stacks) + an equal Magicka drain (excluded from
        // total). The per-enchant base is per-element (§4.3), NOT the old flat 30×tier.
        // NOTE: the enchant track is ADDITIVE and independent of the physical combo roll
        // (capture-validated, §4.3) — it is NOT multiplied by the combo factor.
        for (ench_ty, tier) in &attacker.enchants {
            let amp = target.element_amp_for(*ench_ty);
            let v = enchant_base(*ench_ty, *tier) * amp;
            components.push((*ench_ty, v));
            components.push((DamageType::Magicka, v));
        }
        components
    }
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
    ) -> ResolvedDamage {
        let mut components =
            Self::swing_components(attacker, target, active_side, swing_factor, combo_count);
        finish_resolved(attacker, target, source, active_side, &mut components)
    }

    fn resolve_ability(&self, ability_level: u8, target: &Fighter, active_side: ActiveSide) -> ResolvedDamage {
        // Representative spell: a Fire component scaled by ability level. The SHAPE
        // (Spell source, a single elemental component) matches the captured fire
        // spells; the exact per-ability magnitude needs ability game-data.
        let base = tables::spell_base_for_rank(ability_level);
        let mut components = vec![(DamageType::Fire, base)];
        // A bare attacker loadout (no enchant-piercing) for the spell mitigation path.
        finish_resolved(&Loadout::default(), target, DamageSource::Spell, active_side, &mut components)
    }
}

/// Apply the post-roll mitigation pipeline to raw `components` and assemble the
/// [`ResolvedDamage`] (`docs/arena-status-resistance-spec.md` §1, mitigation order):
///   block (per-category, §3) → resistance (flat per-type, §2) → negation pools (§4)
///   → Σ health = total.
/// Sets the block flag, `most_resisted`, the `negated`/`heal` outputs, and the
/// per-type components the client renders. The 25%-of-maxHP one-shot clamp is GONE for
/// arena (`docs/arena-combat-reproduction-spec.md` §4.5) — deep-combo hits are *earned*.
fn finish_resolved(
    attacker: &Loadout,
    target: &Fighter,
    source: DamageSource,
    active_side: ActiveSide,
    components: &mut Vec<(DamageType, f32)>,
) -> ResolvedDamage {
    let mut hit_flags = flags::SHOW_DAMAGE | flags::HAS_ATTACKER;

    // 1) BLOCK — per-category (physical vs elemental differ; see `block_outcome`).
    let block = block_outcome(target, active_side);
    hit_flags |= block.flag;
    for (ty, v) in components.iter_mut() {
        *v *= block.factor_for(*ty);
    }

    // 2) RESISTANCE — flat per-type subtraction (elemental scaled by the attacker's
    //    Elemental-Resistance-Piercing), summed from the defender's resist sources.
    //    Track the most-resisted ELEMENT (largest resisted fraction ≥ a small floor).
    let mut most_resisted = DamageType::None;
    let mut most_resisted_frac = MOST_RESISTED_FLOOR;
    for (ty, v) in components.iter_mut() {
        let before = *v;
        let resisted = target.resistance_against(*ty, attacker.elem_resist_piercing);
        if resisted > 0.0 && before > 0.0 {
            *v = (before - resisted).max(0.0);
            if is_elemental(*ty) {
                let frac = (resisted.min(before)) / before;
                if frac > most_resisted_frac {
                    most_resisted_frac = frac;
                    most_resisted = *ty;
                }
            }
        }
    }

    // 3) NEGATION POOLS (Ward/Absorb/Dodge) are drained in `resolve::emit_damage` — they
    //    MUTATE the defender's pool, so they can't run in this read-only (`&Fighter`)
    //    model. The components here are post-block/post-resist; `emit_damage` finishes
    //    the pipeline (drain pools → set `negated`/`heal` → recompute `total`).

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
        negated: false, // set by emit_damage after draining the defender's negation pools
        heal: 0.0,
    }
}

/// Minimum resisted fraction for an element to be reported as `mostResisted`
/// (`CombatHUDHelper.DetermineMostResistedElementalDamageType`'s `resistThreshold`).
const MOST_RESISTED_FLOOR: f32 = 0.05;

/// Elemental damage types (Fire/Frost/Shock/Poison) — the ones `mostResisted` ranges
/// over and that carry conditioning/status.
pub fn is_elemental(t: DamageType) -> bool {
    matches!(t, DamageType::Fire | DamageType::Frost | DamageType::Shock | DamageType::Poison)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::combat::state::{ActorStateType, DamageNegationSource, Loadout, NegationPool, WeaponProfile};
    use crate::arena::combat::tables::Weight;
    use std::time::{Duration, Instant};

    /// A Light poison dagger ≈ the recorded s506 weapon: base Slashing + a Poison
    /// enchant track (the calibration target).
    fn poison_dagger() -> Loadout {
        Loadout {
            weapon: WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 113.82)], // the s506 combo-0 post-armor base
                weight: Some(Weight::Light),
            },
            enchants: vec![(DamageType::Poison, 10)],
            ..Default::default()
        }
    }

    /// A plain Light slashing blade (no enchant) — for isolating the physical combo ramp.
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

    /// A high-HP target (L100 ×3 ≈ 3170 HP) so structural assertions exercise the model.
    fn target() -> Fighter {
        Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, Instant::now())
    }

    /// COMBO RAMP (§4.2): a normal Left/Right swing's physical scales with the combo
    /// count — ×1.0 fresh, ×1.45 at one chained step, climbing to the ×4.12 ceiling on a
    /// deep combo. The Poison enchant track is ADDITIVE and does NOT scale with combo.
    #[test]
    fn combo_ramp_drives_physical_not_enchant() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let slash = |rd: &ResolvedDamage| -> f32 {
            rd.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum()
        };
        let poison = |rd: &ResolvedDamage| -> f32 {
            rd.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum()
        };

        let c0 = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let c1 = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Left, 1.0, 1);
        let c4 = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 4);

        // Physical reproduces the s506 anchors: combo-0 Right ≈ 113.8, +1 chained ≈ 165.
        assert!((slash(&c0) - 113.82).abs() < 0.5, "combo-0 Slashing ≈ 113.8, got {}", slash(&c0));
        assert!((slash(&c1) - 113.82 * 1.45).abs() < 0.5, "combo-1 Slashing ≈ 165.0, got {}", slash(&c1));
        // Deep combo ramps hard (×4.12 ceiling) — and is much bigger than the fresh hit.
        assert!(slash(&c4) > slash(&c1), "deeper combo hits harder");
        assert!((slash(&c4) - 113.82 * 4.12).abs() < 1.0, "combo-4 Slashing ≈ 469, got {}", slash(&c4));
        // The Poison enchant base is INDEPENDENT of combo (same at every depth).
        assert!((poison(&c0) - poison(&c1)).abs() < 1e-3, "poison track is additive, combo-independent");
        assert!((poison(&c4) - poison(&c0)).abs() < 1e-3, "poison track is additive, combo-independent");
    }

    /// ENCHANT BASE (§4.3): Weapon Poison Damage ≈ 13.7 × tier (137.3 @ t10), ~2.2×
    /// SMALLER than the old 30×tier (=300). The enchant adds an independent damage-type
    /// component + an equal Magicka drain (excluded from the total).
    #[test]
    fn enchant_base_is_poison_calibrated_with_excluded_drain() {
        let m = RetailDamageModel;
        let rd = m.resolve_attack(&poison_dagger(), &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let poison: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum();
        let magicka: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Magicka).map(|(_, v)| *v).sum();
        // Fresh (no conditioning) → amp ×1.0 → base ≈ 137.3, NOT the old 300.
        assert!((poison - 137.3).abs() < 1.0, "Weapon Poison Damage base ≈ 137.3 @ t10, got {poison}");
        assert!(poison < 200.0, "the enchant base is the SMALL (13.7×tier) magnitude, not 30×tier=300");
        assert_eq!(magicka, poison, "the enchant adds an equal Magicka drain");
        // total = Slashing + Poison, the Magicka drain excluded.
        let slashing: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum();
        assert!((rd.total - (slashing + poison)).abs() < 1e-3, "drain excluded from total");
    }

    /// THE CORRECTED BLOCK MODEL (§4.4): a CONNECTED OPTIMAL block NEGATES physical
    /// (×0.0) but only HALVES elemental (×0.5) — NOT the ÷1.6/÷1.23-for-optimal the
    /// status-resistance spec wrongly prescribed. A LATE/wrong-side block is the partial
    /// ÷1.6 physical / ÷1.23 elemental tier.
    #[test]
    fn optimal_block_negates_physical_halves_elemental() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        // Baseline (no block).
        let open = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let open_slash: f32 = open.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum();
        let open_poison: f32 = open.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum();

        // OPTIMAL: target guards the SAME side (Right).
        let mut tgt = target();
        tgt.actor_state = ActorStateType::Blocking;
        tgt.blocking_side = ActiveSide::Right;
        let opt = m.resolve_attack(&lo, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let opt_slash: f32 = opt.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum();
        let opt_poison: f32 = opt.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum();
        assert!(opt.flags & flags::WAS_OPTIMAL_BLOCKING != 0, "optimal flag set");
        assert_eq!(opt_slash, 0.0, "optimal block NEGATES physical (×0), not ÷1.6");
        assert!((opt_poison - open_poison * 0.5).abs() < 1e-2, "optimal block HALVES elemental (×0.5), not ÷1.23");

        // LATE / wrong-side: target guards the OTHER side (Left) vs a Right swing.
        let mut late_t = target();
        late_t.actor_state = ActorStateType::Blocking;
        late_t.blocking_side = ActiveSide::Left;
        let late = m.resolve_attack(&lo, &late_t, DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let late_slash: f32 = late.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum();
        let late_poison: f32 = late.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum();
        assert!(late.flags & flags::WAS_LATE_BLOCKING != 0, "late flag set");
        assert!((late_slash - open_slash / PHYSICAL_BLOCK_MULTIPLIER).abs() < 1e-2, "late block ÷1.6 physical");
        assert!((late_poison - open_poison / ELEMENTAL_BLOCK_MULTIPLIER).abs() < 1e-2, "late block ÷1.23 elemental");
    }

    /// NO 25% CLAMP for arena (§4.5): a deep-combo hit is *earned* and can legitimately
    /// exceed 25% of max HP (the old clamp would corrupt the faithful damage). With the
    /// real (small) poison base + combo, a deep hit lands large but un-clamped.
    #[test]
    fn deep_combo_hit_is_not_clamped() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let tgt = target(); // ×3 ≈ 3170 HP; 25% ≈ 792
        let rd = m.resolve_attack(&lo, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 4);
        // combo-4 Slashing ≈ 469 + Poison ≈ 137 = ~606. The total = Σ health components
        // EXACTLY (no clamp scaling), and it is NOT capped at the old 25% (≈792) ceiling
        // by a clamp — prove the clamp is gone by checking the total == raw Σ.
        let health_sum: f32 = rd.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert!((rd.total - health_sum).abs() < 1e-3, "total == Σ health components (no clamp scaling)");
        // A pathological 4-enchant loadout's deep hit is now allowed to exceed 25% maxHP.
        let lethal = Loadout {
            level: 100,
            weapon: WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 240.0)],
                weight: Some(Weight::Light),
            },
            enchants: vec![(DamageType::Poison, 10), (DamageType::Fire, 10)],
            ..Default::default()
        };
        let big = m.resolve_attack(&lethal, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 4);
        let cap_25 = 0.25 * tgt.max_health as f32;
        assert!(big.total > cap_25, "a deep-combo big hit is NO LONGER clamped to 25% of max HP");
    }

    /// RESISTANCE (§2): a flat per-type subtraction applied after block, with elemental
    /// resist scaled by the attacker's Elemental-Resistance-Piercing, and `most_resisted`
    /// = the most-resisted element.
    #[test]
    fn resistance_subtracts_flat_and_sets_most_resisted() {
        let m = RetailDamageModel;
        let attacker = poison_dagger(); // no piercing
        let mut tgt = target();
        tgt.loadout.resistances = vec![(DamageType::Poison, 40.0)];
        let rd = m.resolve_attack(&attacker, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let poison: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum();
        // 137.3 − 40 = 97.3.
        assert!((poison - 97.3).abs() < 1.0, "poison reduced by the flat 40 resist, got {poison}");
        assert_eq!(rd.most_resisted, DamageType::Poison, "most_resisted = the resisted element");

        // With attacker Elemental-Resistance-Piercing 0.5, only half the resist applies.
        let mut piercer = poison_dagger();
        piercer.elem_resist_piercing = 0.5;
        let rd2 = m.resolve_attack(&piercer, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0);
        let poison2: f32 = rd2.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum();
        assert!((poison2 - (137.3 - 20.0)).abs() < 1.0, "piercing halves the effective resist (−20), got {poison2}");
    }

    /// NEGATION POOL (§4): a Ward/Absorb pool eats incoming damage. A pool that covers
    /// the whole hit returns `negated`; Absorb heals back its restoration factor. (The
    /// pool drain itself is `Fighter::apply_negation_pools`; here we exercise it directly
    /// since the model leaves negation to `emit_damage`.)
    #[test]
    fn negation_pool_eats_hit_and_absorb_heals() {
        let now = Instant::now();
        let mut tgt = target();
        tgt.negation_pools.push(NegationPool {
            source: DamageNegationSource::Absorb,
            remaining: 10_000.0, // larger than any single hit
            expires_at: now + Duration::from_secs(5),
            restoration_factor: 1.0,
        });
        let mut components = vec![(DamageType::Slashing, 200.0), (DamageType::Poison, 137.3), (DamageType::Magicka, 137.3)];
        let res = tgt.apply_negation_pools(&mut components);
        assert!(res.negated, "the pool ate the whole hit");
        assert!((res.heal - (200.0 + 137.3)).abs() < 1e-2, "Absorb heals back the negated HEALTH damage (drain excluded)");
        // Health components are zeroed.
        let health: f32 = components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert_eq!(health, 0.0, "negated → 0 health damage");
    }

    #[test]
    fn ability_deals_spell_damage_scaling_with_level() {
        let m = RetailDamageModel;
        let l1 = m.resolve_ability(1, &target(), ActiveSide::Middle);
        let l3 = m.resolve_ability(3, &target(), ActiveSide::Middle);
        assert_eq!(l1.source, DamageSource::Spell);
        assert!(l1.components.iter().any(|(t, _)| *t == DamageType::Fire));
        assert!(l3.total > l1.total, "higher ability level → more damage");
    }
}
