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

/// `CalculateAttackTypeFactor` (@0x1BD3DF0): the swing/side multiplier added to 1.
/// Middle (charged) adds the full swing magnitude; Right keeps the source base;
/// Left/None add nothing beyond the source base.
fn attack_type_factor(source_base: f32, active_side: ActiveSide, swing_factor: f32) -> f32 {
    match active_side {
        ActiveSide::Middle => source_base + swing_factor,
        ActiveSide::Right => source_base,
        ActiveSide::Left | ActiveSide::None => 0.0,
    }
}

/// Per-`DamageSource` base contribution to the attack-type factor (game-data
/// constant; defaults are representative until wired from item data).
fn source_base(source: DamageSource) -> f32 {
    match source {
        DamageSource::WeaponManeuver | DamageSource::ShieldManeuver => 0.5,
        DamageSource::Spell | DamageSource::ContinuousSpell => 0.25,
        _ => 0.0, // plain Attack: factor is just the swing
    }
}

/// Enchant magnitude per tier (game-data constant; representative default).
fn enchant_base(tier: u8) -> f32 {
    8.0 * tier as f32
}

/// Block outcome on a hit: `(flags, damage_multiplier)`. Optimal block (same side
/// the attacker is swinging) nearly negates; a block on the wrong side is "late".
fn block_outcome(target: &Fighter, active_side: ActiveSide) -> (u8, f32) {
    use super::state::ActorStateType;
    if target.actor_state != ActorStateType::Blocking || active_side == ActiveSide::None {
        return (0, 1.0);
    }
    if target.blocking_side == active_side {
        (flags::WAS_OPTIMAL_BLOCKING, 0.0) // optimal block fully negates
    } else {
        (flags::WAS_LATE_BLOCKING, 0.5) // mistimed/wrong-side block: half
    }
}

/// The damage model the arena uses. A trait so the RE-derived [`RetailDamageModel`]
/// can be swapped for tuning/tests without touching resolution or the builders.
pub trait DamageModel {
    /// Resolve a weapon swing from `attacker` against `target`.
    fn resolve_attack(
        &self,
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        active_side: ActiveSide,
        swing_factor: f32,
    ) -> ResolvedDamage;

    /// Resolve an ability/spell cast → Spell-source damage on `target`.
    fn resolve_ability(&self, ability_level: u8, target: &Fighter, active_side: ActiveSide) -> ResolvedDamage;
}

/// The RE-derived model (formula structure above). Number-exact once the weapon
/// `base_by_type` / source-base / enchant constants are wired from game data.
pub struct RetailDamageModel;

impl DamageModel for RetailDamageModel {
    fn resolve_attack(
        &self,
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        active_side: ActiveSide,
        swing_factor: f32,
    ) -> ResolvedDamage {
        let factor = attack_type_factor(source_base(source), active_side, swing_factor);
        let scale = 1.0 + factor;

        let mut components: Vec<(DamageType, f32)> = Vec::new();
        // Base weapon damage, per type, scaled by the attack-type factor.
        for (ty, base) in &attacker.weapon.base_by_type {
            components.push((*ty, base * scale));
        }
        // Weapon enchant: an independent damage-type track + an equal Magicka drain
        // (mirrors the captured "Slashing + Shock (+equal Magicka)" shape).
        for (ench_ty, tier) in &attacker.enchants {
            let v = enchant_base(*tier) * scale;
            components.push((*ench_ty, v));
            components.push((DamageType::Magicka, v));
        }

        // Block reduction on the target.
        let (mut hit_flags, mult) = block_outcome(target, active_side);
        hit_flags |= flags::SHOW_DAMAGE | flags::HAS_ATTACKER;
        if mult != 1.0 {
            for c in &mut components {
                c.1 *= mult;
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
            components,
            total,
            most_resisted: DamageType::None,
        }
    }

    fn resolve_ability(&self, ability_level: u8, target: &Fighter, active_side: ActiveSide) -> ResolvedDamage {
        // Representative spell: a Fire component scaled by ability level. The SHAPE
        // (Spell source, a single elemental component) matches the captured fire
        // spells; the exact per-ability magnitude needs ability game-data.
        let base = 20.0 + 10.0 * ability_level.max(1) as f32;
        let mut components = vec![(DamageType::Fire, base)];
        let (mut hit_flags, mult) = block_outcome(target, active_side);
        hit_flags |= flags::SHOW_DAMAGE | flags::HAS_ATTACKER;
        if mult != 1.0 {
            for c in &mut components {
                c.1 *= mult;
            }
        }
        let total: f32 = components
            .iter()
            .filter(|(t, _)| is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        ResolvedDamage {
            source: DamageSource::Spell,
            active_side,
            flags: hit_flags,
            components,
            total,
            most_resisted: DamageType::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::combat::state::{ActorStateType, Loadout, WeaponProfile};
    use std::time::Instant;

    fn shock_blade() -> Loadout {
        Loadout {
            weapon: WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 60.0)],
            },
            enchants: vec![(DamageType::Shock, 3)],
            ..Default::default()
        }
    }

    fn target() -> Fighter {
        Fighter::new(1, 565, Loadout::default(), Instant::now())
    }

    #[test]
    fn middle_swing_exceeds_right_exceeds_left() {
        let m = RetailDamageModel;
        let lo = shock_blade();
        let mid = m
            .resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Middle, 1.0)
            .total;
        let right = m
            .resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0)
            .total;
        let left = m
            .resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Left, 1.0)
            .total;
        assert!(mid > right, "charged Middle swing hits hardest");
        assert!(right >= left, "Right (base factor) ≥ Left (no factor)");
    }

    #[test]
    fn enchant_adds_independent_type_and_excluded_drain() {
        let m = RetailDamageModel;
        let rd = m.resolve_attack(&shock_blade(), &target(), DamageSource::Attack, ActiveSide::Left, 1.0);
        // Components: Slashing (health) + Shock (health) + Magicka (drain).
        assert!(rd.components.iter().any(|(t, _)| *t == DamageType::Shock));
        let magicka: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Magicka).map(|(_, v)| *v).sum();
        let shock: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Shock).map(|(_, v)| *v).sum();
        assert_eq!(magicka, shock, "enchant adds an equal Magicka drain");
        // total = Slashing + Shock, excluding the Magicka drain.
        let slashing: f32 = rd.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum();
        assert!((rd.total - (slashing + shock)).abs() < 1e-3, "drain excluded from total");
    }

    #[test]
    fn optimal_block_negates() {
        let m = RetailDamageModel;
        let mut t = target();
        t.actor_state = ActorStateType::Blocking;
        t.blocking_side = ActiveSide::Right;
        let rd = m.resolve_attack(&shock_blade(), &t, DamageSource::Attack, ActiveSide::Right, 1.0);
        assert_eq!(rd.total, 0.0, "optimal block fully negates");
        assert!(rd.flags & flags::WAS_OPTIMAL_BLOCKING != 0);
    }

    #[test]
    fn late_block_halves() {
        let m = RetailDamageModel;
        let mut t = target();
        t.actor_state = ActorStateType::Blocking;
        t.blocking_side = ActiveSide::Left; // wrong side
        let blocked = m.resolve_attack(&shock_blade(), &t, DamageSource::Attack, ActiveSide::Right, 1.0).total;
        let unblocked = m.resolve_attack(&shock_blade(), &target(), DamageSource::Attack, ActiveSide::Right, 1.0).total;
        assert!((blocked - unblocked * 0.5).abs() < 1e-3, "late block halves damage");
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
