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

/// Swing multiplier by `ActiveSide`, using the representative Heavy-weapon crit/combo
/// factors (UESP, `tables::Weight`): a charged centre (Middle) swing lands the **crit**
/// tier, an alternating side (Right) the **combo** tier, Left/None the base. (Real
/// per-weapon weight comes with equipped-item data; Heavy matches the captured heavy
/// hits. `CalculateAttackTypeFactor` @0x1BD3DF0 is the in-game source.)
fn swing_multiplier(active_side: ActiveSide) -> f32 {
    let (crit, combo) = tables::Weight::Heavy.crit_combo();
    match active_side {
        ActiveSide::Middle => crit,
        ActiveSide::Right => combo,
        ActiveSide::Left | ActiveSide::None => 1.0,
    }
}

/// Enchant base magnitude per tier — calibrated so a crit-tier hit reproduces the
/// captured weapon-enchant tracks (~90–160 Shock/Poison in s223/s257). Additive and
/// independent of the physical roll (validated: per-family CV 4–10%).
fn enchant_base(tier: u8) -> f32 {
    30.0 * tier as f32
}

/// Maximum fraction of the target's **max** HP a SINGLE resolved hit may take —
/// the anti-one-shot clamp. The structural model (per-type components, enchant
/// tracks, block, the captured `totalDamage` invariant) is unchanged; this only
/// bounds the *health total* a single swing/cast can subtract so a round is always
/// a multi-hit fight, never a 3-second one-shot.
///
/// **Why a clamp, not just a smaller base:** the per-hit physical roll is already a
/// sane ~14–17% of the ×3-arena pool (`docs/blades-combat-formulae.md` §10), but a
/// high-end character's WEAPON ENCHANT track is unbounded — `from_character`
/// (`loadout.rs`) sums one `enchant_base(tier)` per damage-enchanted equipped piece,
/// so a multi-enchant endgame loadout's elemental total alone can exceed the whole
/// pool (the observed real-2-player ONE-SHOT: gsid afed5428, a single Flappety swing
/// killed WolfWalker in 3s). Clamping the *resolved* total makes the fix robust to
/// ANY loadout's enchant pathology, independent of the exact per-tier magnitudes.
///
/// 0.25 → a basic swing takes at most ~¼ of max HP, so a round lasts ≥4 landed hits
/// (more with block / sub-crit sides / armor), matching retail's multi-hit rounds.
pub const MAX_HIT_FRACTION_OF_MAX_HP: f32 = 0.25;

/// Clamp a resolved hit's health `total` to [`MAX_HIT_FRACTION_OF_MAX_HP`] of
/// `target_max_health`, scaling every health-affecting component by the same factor
/// so the wire `totalDamage` (Σ health components) stays consistent with the
/// per-type breakdown the client renders. Stat-drain components (Stamina/Magicka),
/// which are excluded from `total`, are left untouched. Returns the (possibly
/// reduced) total. No-op when the hit is already under the cap.
fn clamp_hit_to_max_hp(components: &mut [(DamageType, f32)], total: f32, target_max_health: u32) -> f32 {
    if target_max_health == 0 || total <= 0.0 {
        return total;
    }
    let cap = MAX_HIT_FRACTION_OF_MAX_HP * target_max_health as f32;
    if total <= cap {
        return total;
    }
    let factor = cap / total;
    for c in components.iter_mut() {
        if is_health_type(c.0) {
            c.1 *= factor;
        }
    }
    cap
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
        let scale = swing_multiplier(active_side) * swing_factor;

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

        let raw_total: f32 = components
            .iter()
            .filter(|(t, _)| is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        // Anti-one-shot clamp: bound the health total to a fraction of the target's
        // max HP (scales the components in place to keep the wire breakdown consistent).
        let total = clamp_hit_to_max_hp(&mut components, raw_total, target.max_health);

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
        let base = tables::spell_base_for_rank(ability_level);
        let mut components = vec![(DamageType::Fire, base)];
        let (mut hit_flags, mult) = block_outcome(target, active_side);
        hit_flags |= flags::SHOW_DAMAGE | flags::HAS_ATTACKER;
        if mult != 1.0 {
            for c in &mut components {
                c.1 *= mult;
            }
        }
        let raw_total: f32 = components
            .iter()
            .filter(|(t, _)| is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        let total = clamp_hit_to_max_hp(&mut components, raw_total, target.max_health);
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

    /// A target with a large max-HP pool so the structural assertions below
    /// (side ordering, enchant track, block) exercise the RAW model, not the
    /// anti-one-shot clamp (which only bites when a single hit exceeds 25% of max
    /// HP — see `single_hit_never_one_shots`). A L100 fighter ×3 ≈ 3170 HP.
    fn target() -> Fighter {
        Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, Instant::now())
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

    /// Anti-one-shot guarantee: even a pathological multi-enchant endgame loadout
    /// (the kind that produced the real-2-player one-shot) can take AT MOST
    /// `MAX_HIT_FRACTION_OF_MAX_HP` of the target's max HP in a single hit — so a
    /// round always lasts several hits. The clamp scales the health components so
    /// the wire `totalDamage` stays == Σ health components (capture invariant).
    #[test]
    fn single_hit_never_one_shots() {
        let m = RetailDamageModel;
        // A loadout whose RAW total would massively exceed the pool: a Dragonbone
        // heavy + FOUR tier-10 damage enchants (≈ the worst real `from_character` case).
        let lethal = Loadout {
            level: 100,
            weapon: WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 240.0)],
            },
            enchants: vec![
                (DamageType::Shock, 10),
                (DamageType::Fire, 10),
                (DamageType::Frost, 10),
                (DamageType::Poison, 10),
            ],
            ..Default::default()
        };
        let tgt = target(); // L100 ×3 ≈ 3170 HP
        let cap = MAX_HIT_FRACTION_OF_MAX_HP * tgt.max_health as f32;
        let rd = m.resolve_attack(&lethal, &tgt, DamageSource::Attack, ActiveSide::Middle, 1.0);
        assert!(
            rd.total <= cap + 1e-3,
            "a single hit must not exceed {cap:.0} ({}% of {} max HP); got {:.0}",
            (MAX_HIT_FRACTION_OF_MAX_HP * 100.0) as u32,
            tgt.max_health,
            rd.total,
        );
        assert!(rd.total < tgt.max_health as f32, "and certainly not a full-HP one-shot");
        // The clamped components still sum (health types) to the clamped total.
        let health_sum: f32 = rd.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert!((health_sum - rd.total).abs() < 1e-2, "clamped components Σ == wire total");
        // Several such hits are needed to kill (the multi-hit fight): ceil(maxHP/cap) ≥ 4.
        let hits_to_kill = (tgt.max_health as f32 / rd.total).ceil() as u32;
        assert!(hits_to_kill >= 4, "a round should last ≥4 hits, got {hits_to_kill}");
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
