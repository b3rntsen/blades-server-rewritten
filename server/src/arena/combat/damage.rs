//! Damage model — turns a fighter's loadout + a swing/cast into the per-type
//! damage components that go into a `ReceiveDamage`.
//!
//! Structure recovered by RE of `libil2cpp.so` (sha256 9fc19d29…), validated
//! against the captured `ReceiveDamage` frames (s293 / s506):
//!
//! ```text
//! physical[type]  = weaponBase(item, tempering) × (1 + f), then block/armor/resist
//!                   f = [combo ≥ 1]·comboDamageFactor + swing   (attack_type_multiplier)
//! elemental[type] = enchantDamage(family, tier) × elementAmp(conditioning)
//!                   (+ Frost→Stamina / Shock→Magicka mirrored drain)
//! block           = a FLAT budget per category, factor · R · 0.1 split by share,
//!                   5 % floor per component (PvP factors: phys 1.6 / elem 0.82);
//!                   an OPTIMAL block uses R × (1 + OptimalBlockBoost)
//! resistance      = a FLAT subtraction from the defender's Resistance Rating
//! totalDamage     = Σ components of HEALTH-affecting types (Slashing..Poison)
//! ```
//!
//! # Phase 3 — where the numbers come from now
//!
//! | quantity | before | now |
//! |---|---|---|
//! | weapon base | `weapon_base_for_level(level, Light)` | `gamedata::weapon().base_damage` + `tables::tempering_bonus` |
//! | enchant | `13.73 × tier` (linear GUESS) | the shipped per-element weapon table for the weapon class |
//! | enchant drain | *always* an equal **Magicka** drain | `frostDamageToStaminaDamage` / `shockDamageToMagickaDamage` only |
//! | armor | *not modelled* | `tables::armor_cut_share` after block and multipliers (01-D1) |
//! | resistance | a flat loadout number | a **Resistance Rating** (Phase 3.4) |
//! | block | fixed `÷1.6` / `÷1.23` | [`BlockOutcome::apply`]: a flat budget from the blocking item's rating (03-D1) |
//!
//! ## Where armor is applied
//!
//! `ResolveResistanceReduction` subtracts armor after defender-side negation/block and
//! after the attack-type multiplier. The budget is flat (`ArmorRating × 0.1`), split
//! by each physical component's share, with the same 5% floor as the other mitigation
//! stages. That means combo hits add the multiplier first, then lose the same armor
//! budget rather than scaling a pre-armored base. [01-D1]

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
    /// Components after attacker-side bonuses/factors but before defender-side
    /// negation, block, armor and resistance. Ward/Absorb/Dodge consume this list:
    /// `ResolveDamageTaken` runs negation first (`Actor$$ResolveDamageTaken@0x1c5389c`).
    pub pre_mitigation_components: Vec<(DamageType, f32)>,
    /// All components, including Magicka/Stamina drains (which are excluded from `total`).
    pub components: Vec<(DamageType, f32)>,
    /// Components after attacker-side bonuses but before defender block/resistance.
    /// Dodge pools drain against these raw values; the mitigated components above
    /// are then reduced proportionally for the damage that actually lands.
    pub raw_components: Vec<(DamageType, f32)>,
    /// Sum of health-affecting components only (matches the wire `totalDamage`).
    pub total: f32,
    pub most_resisted: DamageType,
    /// True iff a Ward/Absorb/Dodge negation pool ate the WHOLE hit.
    pub negated: bool,
    /// HP the negation healed back to the DEFENDER (Absorb only).
    pub heal: f32,
    /// The share of this hit's PHYSICAL damage the block let through: post-block over
    /// pre-block physical total (1.0 unblocked, or with no physical component).
    /// Carried so the caller can scale effects that ride the swing rather than a
    /// damage component — Ravage is the one that does.
    pub block_physical: f32,
    /// True when the defender's guard took this hit (the block stage ran). The wire
    /// flags cannot say this: a low block carries no bit at all (03 §2.7), and bit 3
    /// is defender state, set even on an unblockable frame.
    pub blocked: bool,
    /// What share of the defender's flat resistance this resolution charged. Partial
    /// Absorb re-enters mitigation after shrinking the raw list, and must reuse the
    /// periodic/channel scale instead of falling back to single-hit math.
    pub resistance_scale: f32,
}

/// Damage flags (`ReceiveDamage` propId 7 bitfield).
pub mod flags {
    pub const SHOW_DAMAGE: u8 = 0b0001;
    pub const HAS_ATTACKER: u8 = 0b0010;
    /// The guard was pressed but its startup had not finished, so the hit was NOT
    /// mitigated (`CombatHUDHelper$$WasPlayerLateBlockingAttack@0x201c828`). The arena
    /// guard has no startup, so the server never sets it; a low block is not "late".
    pub const WAS_LATE_BLOCKING: u8 = 0b0100;
    /// The defender's `IsOptimalBlocking` STATE, on every frame addressed to it.
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

/// The attack-type factor `f` of a hit, as `1 + f`:
/// `CombatManager$$CalculateAttackTypeFactor@0x1bd3df0`.
///
/// ```text
/// f = [comboCount >= 1] * comboDamageFactor     (Attack and WeaponManeuver)
///   + swing                                     (Attack only)
/// ```
///
/// It is applied once, in the physical bonus pass, as `x(1 + f + ...)` (combat-spec
/// 01 step C/D3). The two terms ADD. A Shield bash (source 11) gets neither, and a
/// maneuver gets no swing term: the `swing = 1.0` that `ResolveManeuverDamage` passes
/// is never read (05 §2.6).
///
/// `swing_factor` is `1 + swing`, where `swing` is the charge plateau's
/// `maxDamageFactor` or 0 (see `ChargeParams::swing_factor`).
pub fn attack_type_multiplier(
    source: DamageSource,
    combo_damage_factor: f32,
    combo_count: u32,
    swing_factor: f32,
) -> f32 {
    let combo = if combo_count >= 1 { combo_damage_factor } else { 0.0 };
    match source {
        DamageSource::Attack => 1.0 + combo + (swing_factor - 1.0),
        DamageSource::WeaponManeuver => 1.0 + combo,
        _ => 1.0,
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

/// The block applied to one hit.
///
/// A block removes a **flat budget** per damage category, `factor · R · 0.1`,
/// split over the category's components by share, with a 5 % floor per component
/// ([`tables::block_cut`]). There is no ×0 and no fixed fraction: an optimal block
/// doubles `R` (for a boost of 1.0), and a big enough budget drives a component to
/// its floor. That is what the s506 "optimal zeroes physical" captures were — the
/// budget exceeding the hit, then armor taking most of the 5 % that was left
/// (03 V1). `Actor$$ResolveBlocking@0x1c54284`, `b__1@0x1fd06f0`.
///
/// In PvP the category factors are physical **1.6** and elemental **0.82**
/// ([`tables::pvp_block_rating_factor`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockOutcome {
    pub optimal: bool,
    pub blocking: bool,
    /// `R` for this hit before piercing: the blocking item's rating, boosted when
    /// optimal, plus the flat Block Reduction enchants ([`Fighter::block_rating`]).
    pub rating: f32,
    /// Attacker's flat block piercing, subtracted from `R` in
    /// [`BlockOutcome::rating_for`] — physical and elemental respectively. The client
    /// subtracts it from the already-boosted `R` for every blocked component, so it
    /// weakens an optimal block exactly as much as a low one (03 V2).
    pub block_piercing: f32,
    pub elem_block_piercing: f32,
    /// `ElementalProtection` perk — added to `R` for ELEMENTAL damage only, after the
    /// optimal boost, and only when the block is made with a shield (10 §3; capture
    /// T1: a weapon-blocker with the perk matches only without it). Zero for an
    /// unperked or shieldless defender.
    pub elem_rating_bonus: f32,
}

/// The dump's `PvpDefaultSettings` values, kept as aliases of the live factors'
/// inputs: they multiply the block-rating FACTOR (03 V3), they are not divisors.
pub const PHYSICAL_BLOCK_MULTIPLIER: f32 = tables::PVP_PHYSICAL_BLOCK_MULTIPLIER;
pub const ELEMENTAL_BLOCK_MULTIPLIER: f32 = tables::PVP_ELEMENTAL_BLOCK_MULTIPLIER;

impl BlockOutcome {
    const NONE: BlockOutcome = BlockOutcome {
        optimal: false,
        blocking: false,
        rating: 0.0,
        block_piercing: 0.0,
        elem_block_piercing: 0.0,
        elem_rating_bonus: 0.0,
    };

    /// `R` for a component of this category: the additives, then
    /// `max(0, R − attacker piercing)` (`b__1@0x1fd06f0` 0x1fd08f0–0x1fd09c4).
    pub fn rating_for(&self, physical: bool) -> f32 {
        if physical {
            (self.rating - self.block_piercing).max(0.0)
        } else {
            (self.rating + self.elem_rating_bonus - self.elem_block_piercing).max(0.0)
        }
    }

    /// Apply the block to `components` in place.
    ///
    /// * physical and elemental health components take [`tables::block_cut`] against
    ///   their own category's pre-block total;
    /// * a **Stamina or Magicka** component is set to 0 outright
    ///   (0x1fd0790–0x1fd0824 zeroes non-health damage). The Frost→Stamina /
    ///   Shock→Magicka mirror is not in the list yet — it is derived afterwards from
    ///   the element's post-block value ([`append_mirrored_drains`]) — so it still
    ///   lands, as it does in the client;
    /// * `periodic` (ContinuousSpell 8 / ContinuousAttack 10): `R` is scaled by the
    ///   tick interval × `continuousDamageBlockingEffectiveness` (1.0) after piercing
    ///   (0x1fd09d0–0x1fd0a40). The client code reads `_globalTickInterval` (0.1);
    ///   capture test T2 showed the retail server substituting the 0.2 s PvP tick in
    ///   the periodic resistance scale, so the PvP tick is used here too. INFERRED:
    ///   T2 measured the resistance path, not the block path.
    pub fn apply(&self, components: &mut [(DamageType, f32)], periodic: bool) {
        if !self.blocking {
            return;
        }
        let total = |pick: fn(DamageType) -> bool, cs: &[(DamageType, f32)]| -> f32 {
            cs.iter().filter(|(t, v)| pick(*t) && *v > 0.0).map(|(_, v)| *v).sum()
        };
        let phys_total = total(is_physical, components);
        let elem_total = total(is_elemental, components);
        let scale = if periodic {
            combat_params::GLOBAL_PVP_TICK_INTERVAL
                * combat_params::CONTINUOUS_DAMAGE_BLOCKING_EFFECTIVENESS
        } else {
            1.0
        };
        let phys_r = self.rating_for(true) * scale;
        let elem_r = self.rating_for(false) * scale;
        for (ty, v) in components.iter_mut() {
            *v = match *ty {
                DamageType::Stamina | DamageType::Magicka => 0.0,
                t if is_physical(t) => {
                    tables::block_cut(*v, phys_total, phys_r, tables::pvp_block_rating_factor(true))
                }
                t if is_elemental(t) => {
                    tables::block_cut(*v, elem_total, elem_r, tables::pvp_block_rating_factor(false))
                }
                // Raw Health / None: not a category the block budget covers.
                _ => *v,
            };
        }
    }
}

/// Resolve the block outcome for a hit on `target` swung on `active_side`.
///
/// OPTIMAL is a **TIMING PHASE, NOT A DIRECTION** (tracker #31). "High" vs "low" is
/// how long the guard has been up — `UI.Help.Blocking.Description`: *"At first, for a
/// short time, you will block high, then lower your shield to block low."* That is
/// `PvpDefaultSettings.BLOCK_OPTIMAL_TIME` (2.0 s, `dump.cs:427014`), which
/// [`Fighter::block_phase`] already models. Retail's `PlayerBlockingState`
/// (`dump.cs:597057-597068`) decides `IsOptimalBlocking` from `_blockOptimalTime` /
/// `_couldBeOptimalBlocking` / `_consumedOptimalBlock` and compares no sides at all.
///
/// This function used to ALSO require `target.blocking_side == active_side`. That gate
/// was ours, not retail's, and it made the optimal phase **unreachable for a weapon
/// swing**: a guard is always raised on `ActiveSide::Middle` (propId 9 == 1 in 578/578
/// recorded blocking-state frames, `resolve.rs`), while an auto-attack is always `Left`
/// or `Right` (`classify_side_from_x` never returns `Middle`; 0 of 6 595 recorded
/// attack hits carried Middle). So `side_matches` was false on every weapon hit and
/// true on every maneuver — both halves wrong. The client also transmits no block
/// direction: `PlayerCombatInputActivateMessage` (`dump.cs:589516-589526`) carries only
/// `_held`, `_clientChargeTime`, `_isWithinBlockZone`.
///
/// `attacker` supplies the flat block-piercing ratings (0.0 unless an ability sets
/// them).
pub fn block_outcome(
    target: &Fighter,
    attacker: &Loadout,
    active_side: ActiveSide,
    now: Instant,
) -> BlockOutcome {
    use super::state::ActorStateType;
    if target.actor_state() != ActorStateType::Blocking || active_side == ActiveSide::None {
        return BlockOutcome::NONE;
    }
    let Some(phase) = target.block_phase(now) else {
        return BlockOutcome::NONE;
    };
    // Timing only — see the note above. `target.blocking_side` is deliberately NOT
    // consulted: it is the wire-visible facing of the guard animation
    // (`PlayerBlockingState.Parameters.ActiveSide`), not a hit-test.
    let optimal = matches!(phase, BlockPhase::Optimal);
    BlockOutcome {
        optimal,
        blocking: true,
        rating: target.block_rating(optimal),
        block_piercing: attacker.block_piercing_rating,
        elem_block_piercing: attacker.elem_block_piercing_rating,
        // "while blocking with a shield" — a two-handed guard gets nothing.
        elem_rating_bonus: if target.loadout.has_shield {
            target.loadout.perks.elemental_block_rating
        } else {
            0.0
        },
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
        caster: &super::perks::CasterPerks<'_>,
        target: &Fighter,
        active_side: ActiveSide,
        now: Instant,
    ) -> ResolvedDamage;
}

/// The RE-derived model, now running on the shipped item/ability/enchant data.
/// Does this ability's rank actually ship a damage number?
///
/// `_damage` or `_damagePerSecond`. An ability with neither is a buff, and routing it
/// through the damage model produces a hit of exactly 0.0 — which `emit_damage` then
/// puts on the wire as an op50 over the OPPONENT, rendering a floating `0`. Reported
/// after the first human-vs-human match: "RE shows a 0 damage effect on the opponent."
///
/// Two ways an ability reaches the damage arm without shipping damage:
///   - it is a buff whose tag is `Generic` (`MagickaSurge`, `EchoWeapon` — both
///     `kind: Spell`, `damage_type: None`, so `ability_tag_for_template` gives them
///     `Generic`), or
///   - the cast uuid missed the equipped-loadout lookup and fell back to `Generic`.
///
/// `false` for an ability gamedata does not know at all: we have no number for it, and
/// a fabricated 0 is worse than silence.
///
/// This predicate was previously inlined in `every_cast_does_something`'s sweep. It is
/// shared now so the test and the runtime cannot drift apart — the test asserting a
/// class of abilities is exempt while the runtime happily fires at them for zero is
/// exactly how this shipped.
pub fn ships_damage(ability_uuid: &str, level: u8) -> bool {
    super::gamedata::ability_rank_clamped(ability_uuid, u16::from(level.max(1)))
        .map(|r| r.damage().is_some() || r.damage_per_second().is_some())
        .unwrap_or(false)
}

pub struct RetailDamageModel;

impl RetailDamageModel {
    /// Resolve a flat generic damage event through the shared mitigation pipeline.
    ///
    /// Echo Weapon and Wall of Fire ship already-computed flat magnitudes; they still
    /// need the same block/resistance/negation-facing shape as ordinary damage.
    pub(super) fn resolve_flat(
        &self,
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        active_side: ActiveSide,
        damage_type: DamageType,
        amount: f32,
        now: Instant,
    ) -> ResolvedDamage {
        let mut components = vec![(damage_type, amount.max(0.0))];
        finish_resolved(attacker, target, source, active_side, &mut components, now, 1.0)
    }

    /// The attacker's per-type PHYSICAL base before the swing/combo factor.
    ///
    /// Armor is not applied here. The client applies it later in
    /// `ResolveResistanceReduction` after negation, block and the attack-type
    /// multiplier, share-weighted across the post-block physical total.
    fn physical_base_components(
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        now: Instant,
    ) -> Vec<(DamageType, f32)> {
        // Scout / Armsman / Barbarian add flat damage for LIGHT / VERSATILE / HEAVY
        // weapons. It rides on the weapon's own damage, so it is added BEFORE armour
        // and mitigated with it — a perk should not be a hole in the armour model.
        // Applied to the FIRST physical component only: the perk is "+{0} Damage
        // with <class> weapons", one bonus per swing, not one per damage type.
        //
        // Not on a shield bash: the class perks are weapon-based bonuses, and
        // `Damage$$IsWeaponBased@0x1bd3e6c` is false for source 11.
        let bash = source == DamageSource::ShieldManeuver;
        let mut weapon_bonus = if bash {
            0.0
        } else {
            attacker.perks.weapon_bonus(attacker.weapon.weight)
        };

        // PDOC — `Opportunist Physical`, "Increases physical damage by {0} against
        // targets suffering a condition". Added HERE, to the base, so the combo and
        // crit multipliers in `swing_components` multiply it. That placement is the
        // whole point: retail accumulates situational ADDITIVES into the base and then
        // applies the factors, so on a deep combo a flat 25.2 is worth ~104. Adding it
        // in `finish_resolved` step 0 instead — the obvious spot, next to the other
        // flat bonuses — would land it AFTER the multiplier and discard most of it.
        //
        // One bonus per swing, not one per damage type: same rule as `weapon_bonus`.
        let mut pdoc = if attacker.opportunist_physical > 0.0 && target.is_conditioned(now) {
            attacker.opportunist_physical
        } else {
            0.0
        };

        // The maneuver's own `parameters.bonusDamage × grip multiplier`. Treated like
        // `weapon_bonus`: it rides on the weapon's damage, so it is added before armour
        // and mitigated with it. One bonus per swing, first physical component only.
        //
        // It is multiplied by the maneuver's attack-type factor like the rest of the
        // physical total (05 §2.6: `(weapon + bonus*g) * (1 + [combo>=1]*comboDF)`).
        let mut maneuver_bonus = attacker.maneuver_bonus_damage.max(0.0);

        // A shield bash hits with the SHIELD, not the weapon: `ResolveManeuverDamage
        // @0x1bd2d88` takes `owner.ShieldDamage` for source 11, and
        // `ManeuverParameters$$DistributeBonusDamage@0x1a21844` folds every physical
        // type into one Bashing entry plus the bonus (combat-spec 03 §5.1, 05 §2.5).
        let shield_base = [(DamageType::Bashing, attacker.shield_damage.max(0.0))];
        let base_list: &[(DamageType, f32)] =
            if bash { &shield_base } else { &attacker.weapon.base_by_type };

        base_list
            .iter()
            .map(|(ty, base)| {
                let mut base = *base;
                if is_physical(*ty) && weapon_bonus > 0.0 {
                    base += weapon_bonus;
                    weapon_bonus = 0.0;
                }
                if is_physical(*ty) && pdoc > 0.0 {
                    base += pdoc;
                    pdoc = 0.0;
                }
                if is_physical(*ty) && maneuver_bonus > 0.0 {
                    base += maneuver_bonus;
                    maneuver_bonus = 0.0;
                }
                (*ty, base.max(0.0))
            })
            .collect()
    }

    /// Build the per-type damage components for a weapon swing.
    fn swing_components(
        attacker: &Loadout,
        target: &Fighter,
        source: DamageSource,
        swing_factor: f32,
        combo_count: u32,
        now: Instant,
    ) -> Vec<(DamageType, f32)> {
        // The attack-type factor (combat-spec 01 step C, 02 §4.2). The shipped
        // `_comboDamageFactor` and the charge plateau ADD to one multiplier. This
        // replaces two fitted rules: `max(combo, charge)` on Left/Right and
        // `crit x charge` on Middle. On a standard Light weapon the multiplier is now
        // 1.0 fresh, 1.54 chained, 1.325 crit and 1.865 both (02 §7).
        let scale = attack_type_multiplier(
            source,
            attacker.charge_params().combo_damage_factor,
            combo_count,
            swing_factor,
        );

        let mut components: Vec<(DamageType, f32)> = Vec::new();
        for (ty, base) in Self::physical_base_components(attacker, target, source, now) {
            components.push((ty, base * scale));
        }
        // A shield bash carries no weapon enchantment damage: the enchant tracks are
        // weapon-based (`Damage$$IsWeaponBased@0x1bd3e6c` excludes source 11), and
        // the weapon's alchemy poison does not ride a bash either (05 §3.11).
        if source == DamageSource::ShieldManeuver {
            return components;
        }
        // Enchant tracks: independent of the physical combo roll (capture-validated,
        // §4.3). The magnitude is the family's own shipped curve value.
        //
        // The mirrored stat drain is NOT appended here: it mirrors the element
        // *after* mitigation and is derived in [`append_mirrored_drains`], which
        // `finish_resolved` calls once the block factor has been applied.
        // ENCHANTMENT SYNERGY — "Stacked enchantments are {0}% more effective."
        // "Stacked" is the same element carried by more than one equipped
        // enchantment; a lone enchantment is not stacked and gets nothing.
        let synergy = attacker.perks.enchantment_synergy;
        // EDOC — `Opportunist Elemental`. Same gate as PDOC, but it lands on the
        // enchant track, which sits OUTSIDE `scale`. So elemental damage does not
        // scale with crit/combo while physical does — the asymmetry the community
        // guides describe, and it falls out of the existing structure rather than
        // being imposed.
        let edoc = if attacker.opportunist_elemental > 0.0 && target.is_conditioned(now) {
            attacker.opportunist_elemental
        } else {
            0.0
        };
        let mut edoc_left = edoc;
        for (idx, (ench_ty, magnitude)) in enchant_tracks(attacker).into_iter().enumerate() {
            // Fortify is a FLAT add and is paid once per hit in `finish_resolved`
            // step 0, alongside the Augmented* perks it shares its shape with — not
            // as a multiplier here, and not once per enchant track.
            let amp = target.element_amp_for(ench_ty);
            // ENCHANTMENT SYNERGY — "Stacked enchantments are {0}% more effective."
            //
            // "Stacked" means TWO OR MORE COPIES OF THE SAME SPECIFIC PROPERTY,
            // counted across every equipped item. Disassembled:
            // `ItemPropertyBonusInstance::GetXValueMultiplier` gates the bonus on
            // `ActorBonusHandler.HasMatchingItemBonus(propertyId)`, which walks
            // `_bonusesByItem` over all items and ends `cmp w22, 1; cset w0, gt` —
            // i.e. `matchCount > 1`. The perk itself is an 8-byte field read that
            // counts nothing, so every "stacked" semantic lives here.
            //
            // We used to count enchants of the same DAMAGE TYPE on the weapon alone,
            // which was wrong twice over: "Weapon Fire Damage" and "Fortify Fire
            // Damage" are different properties and were counting as stacked, while
            // two identical properties on a helmet and a cuirass counted as nothing.
            //
            // It is also a BINARY gate, not a scaling one — two copies and five copies
            // both give `1 + bonusValue`.
            let stacked = synergy > 0.0
                && attacker
                    .enchant_property_ids
                    .get(idx)
                    .is_some_and(|id| {
                        attacker.property_ids.iter().filter(|p| *p == id).count() > 1
                    });
            let synergy_mult = if stacked { 1.0 + synergy } else { 1.0 };
            let mut v = magnitude * amp * synergy_mult;
            // Once per swing, on the first elemental track, mirroring PDOC.
            if edoc_left > 0.0 {
                v += edoc_left;
                edoc_left = 0.0;
            }
            components.push((ench_ty, v));
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
    let Some(weight) = attacker.weapon.weight else {
        return Vec::new();
    };
    attacker
        .enchants
        .iter()
        .map(|(ty, tier)| (*ty, weapon_damage_family_value_for_weight(*ty, *tier, weight)))
        .collect()
}

/// The shipped `Weapon <Element> Damage` family for an element.
pub fn weapon_damage_family_value(ty: DamageType, tier: u8) -> f32 {
    weapon_damage_family_value_for_weight(ty, tier, tables::Weight::Light)
}

pub fn weapon_damage_family_value_for_weight(ty: DamageType, tier: u8, weight: tables::Weight) -> f32 {
    let family = match ty {
        DamageType::Fire => "c40ed851-8777-4d09-b169-0223dae8f67d",
        DamageType::Frost => "63b6c73a-af1a-4f95-8ffe-9434b8e68d56",
        DamageType::Shock => "139024a7-3965-4e90-a4c1-60e3d7ca3133",
        DamageType::Poison => "08ea75d0-5cf1-44a9-9816-d3c6740c4191",
        DamageType::Stamina => "9fdbb542-ff37-4199-93a3-d9444cca9090",
        DamageType::Magicka => "5a145cf8-3a20-4b8a-bf6d-8ee1607d3417",
        _ => return 0.0,
    };
    super::gamedata::enchant_magnitude_for_weight(family, tier, weight).unwrap_or(0.0)
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
            Self::swing_components(attacker, target, source, swing_factor, combo_count, now);
        finish_resolved(attacker, target, source, active_side, &mut components, now, 1.0)
    }

    fn resolve_ability(
        &self,
        ability_uuid: &str,
        ability_level: u8,
        caster: &super::perks::CasterPerks<'_>,
        target: &Fighter,
        active_side: ActiveSide,
        now: Instant,
    ) -> ResolvedDamage {
        // The shipped per-rank `_damage` + the ability's own `damage_type`.
        let mut source = DamageSource::Spell;
        let (ty, base) = match super::gamedata::ability(ability_uuid) {
            Some(a) => {
                // Some spells are DAMAGE-OVER-TIME and ship no direct `_damage` at all —
                // only `_damagePerSecond`. Frostbite is one: rank 1 carries
                // dps = 35.51 and no `damage`, so reading `_damage` alone returned None
                // and `unwrap_or(0.0)` made a 280-magicka spell deal NOTHING. Measured on
                // prod 2026-08-03, where 87 of 160 casts dealt exactly 0.0.
                //
                // The total is `dps × the rank's OWN channel/effect length`, not the
                // 5 s elemental-condition duration this used to reach for. Frostbite
                // ships `channelMaxLength = 3`; billing it for 5 s inflated every rank
                // by 5/3 (R4: 479.0 instead of 287.4 — the exact figure arena-server
                // logged for the reporter's cast in prod session 911 on 2026-08-18).
                // `ConsumingInferno` is the same shape; `PoisonCloud` ships
                // `duration = 5`, so it is unchanged, and `ELEMENTAL_STATUS_DURATION`
                // survives only as the last-resort default.
                //
                // Retail TICKS this over the channel rather than landing it in one
                // hit, so this returns ONE TICK: `dps × CHANNEL_TICK_INTERVAL_SECS`.
                // `resolve.rs` schedules the rest, calling back here once per tick, so
                // block and resistance are re-read as the channel runs. The total over
                // a full channel is still `dps × channelMaxLength` — see
                // [`channel_ticks`] for the count and the capture behind the interval.
                let dmg = tables::ability_damage(ability_uuid, ability_level).unwrap_or_else(|| {
                    super::gamedata::ability_rank_clamped(ability_uuid, ability_level.max(1) as u16)
                        .and_then(|r| r.damage_per_second())
                        .map(|dps| {
                            // A CHANNELLED spell is `ContinuousSpell` on the wire, not
                            // `Spell` — s615 #4394011 carries `damageSource = 8`. The
                            // client keys its channelled-damage presentation off this
                            // discriminator, so sending 2 renders a channel as nothing.
                            source = DamageSource::ContinuousSpell;
                            dps * CHANNEL_TICK_INTERVAL_SECS
                        })
                        .unwrap_or(0.0)
                });
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
        // MAXIMUM POWER / METTLE — the two magnitude perks. Maximum Power is
        // spell-only ("Spells are {0}% more effective when cast while Magicka is
        // full"); Mettle applies to any ability while health is critical. Applied to
        // the BASE, so block and resistance still bite afterwards — a perk makes the
        // spell bigger, it does not bypass the defender.
        let is_spell = super::gamedata::ability(ability_uuid)
            .map(|a| a.kind == super::gamedata::AbilityKind::Spell)
            .unwrap_or(true);
        let base = base * caster.magnitude_multiplier(is_spell);

        // The caster's PERKS **and elemental-resistance piercing**.
        //
        // The piercing ratings used to be left at `Default` — the note here said
        // switching them on for spells was "a real change to the damage model, and
        // it is not this one's to make". The consequence is that
        // `Elemental Damage Ignores Resistance` gear did NOTHING on elemental SPELLS,
        // which is the one place its own text promises it works. A player with four
        // EDIR pieces on a frost build got zero benefit from all four; the weapon path
        // (`resolve.rs`, which clones the real loadout) honoured them the whole time.
        //
        // `finish_resolved` reads exactly three things from `attacker` —
        // `perks.element_bonus`, `perks.element_damage`, and these two piercing fields
        // — so copying them is the complete fix and touches nothing else.
        let mut caster_loadout = Loadout::default();
        caster_loadout.perks = caster.perks.clone();
        caster_loadout.element_fortify = caster.element_fortify.to_vec();
        caster_loadout.elem_resist_piercing = caster.elem_resist_piercing;
        caster_loadout.elem_resist_piercing_rating = caster.elem_resist_piercing_rating;
        // The ABILITY's own `_elementalResistancePiercing`, the same field the weapon
        // path adds in `resolve.rs`. A spell that ships one was ignoring it too.
        if let Some(erp) = super::gamedata::ability_rank_clamped(ability_uuid, ability_level.max(1) as u16)
            .and_then(|r| r.elemental_resistance_piercing())
        {
            caster_loadout.elem_resist_piercing_rating += erp;
        }

        // The mirrored stat drain is appended by `finish_resolved` from the
        // post-block value — see [`append_mirrored_drains`].
        let mut components = vec![(ty, base)];
        finish_resolved(
            &caster_loadout,
            target,
            source,
            active_side,
            &mut components,
            now,
            resistance_scale_for(source, ability_uuid, ability_level),
        )
    }
}

/// How long a `_damagePerSecond` ability applies for, in seconds — the rank's OWN
/// shipped span, in the order the assets define it:
///
/// | field                | who ships it                        | value |
/// |----------------------|-------------------------------------|-------|
/// | `_channelMaxLength`  | Frostbite, ConsumingInferno         | 3     |
/// | `_duration`          | PoisonCloud, FlameBreath, FrostBreath | 5 / 3 |
///
/// The fallback is `ELEMENTAL_STATUS_DURATION`, which is the *elemental-condition*
/// DoT length and has nothing to do with a spell's channel — it was standing in for
/// both, which is why Frostbite billed 5 s of damage for a 3 s channel.
/// The interval between ticks of a channelled (`_damagePerSecond`) spell.
///
/// This is the shipped `GLOBAL_PVP_TICK_INTERVAL` (0.2 s, 5 Hz) — not a number
/// chosen here. Measured against every channelled tick in the two fully-decrypted
/// sessions (s615 + s616, 118 `DamageSource::ContinuousSpell` frames across 74 cast
/// runs, all Frost + Stamina 1:1):
///
/// * **Span.** No cast run spans more than 3 wall-clock seconds, matching the shipped
///   `channelMaxLength = 3`.
/// * **Tick count.** The longest run observed is **13** ticks. That alone excludes a
///   0.25 s interval, which could not produce more than 12.
/// * **Magnitude.** `per_tick = dps × interval`, so each candidate interval implies a
///   ceiling of `max_dps × interval`. Frostbite's top rank ships `dps = 231.01`. At
///   1/6 s (6 Hz) the ceiling is **38.50**, but six observed per-tick values exceed it
///   (39.33, 39.62, 40.86, 41.24, 42.48, 45.00) — damage is *reduced* by resistance,
///   never inflated, so 6 Hz is refuted outright. At 0.2 s the ceiling is 46.20 and
///   nothing exceeds it; two magnitudes land within 0.05 of a shipped rank
///   (42.477 vs r13 42.498, 44.997 vs r15 44.948).
///
/// 0.2 s is therefore the only candidate consistent with both the tick count and the
/// magnitudes, and it is a shipped constant rather than a fitted one.
pub const CHANNEL_TICK_INTERVAL_SECS: f32 = super::gamedata::combat_params::GLOBAL_PVP_TICK_INTERVAL;

/// The interval between ticks of continuous gear damage (Ebony Mail, Rimelink).
///
/// Retail computes it as `rate x deltaTime` (`CombatManager.ApplyContinuousDamage`,
/// libil2cpp `0x1BD3864`: `damage += sum(callbacks) * _cachedDeltaTime`), so the
/// interval sets the tick SIZE, never the damage per second. The PvP delta is the
/// shipped `GLOBAL_PVP_TICK_INTERVAL`. Retail's AreaEffect (7) frames arrive at that
/// rate: s167's first Poison Cloud burst carries 19 unique ticks in about 4 s.
pub const CONTINUOUS_AREA_TICK_SECS: f32 = combat_params::GLOBAL_PVP_TICK_INTERVAL;

/// One tick of continuous gear damage from `attacker`'s gear onto `target`:
/// `rate_per_sec x CONTINUOUS_AREA_TICK_SECS` of `ty`, as `DamageSource::AreaEffect`
/// on `ActiveSide::None`, through the defender's mitigation only (see [`mitigate`]).
///
/// The flat resistance rating is charged at `CONTINUOUS_AREA_TICK_SECS` per tick, so
/// one second of the effect pays the rating once. Charging the whole rating on each
/// 0.2 s tick would cut every tick to the 95% floor for any defender with a few points
/// of resistance. That is the per-tick behaviour the channel captures ruled out for
/// `ContinuousSpell` (see `resistance_scale_for`). No capture shows this effect, so
/// that is an inference.
pub fn resolve_continuous_area_tick(
    attacker: &Loadout,
    target: &Fighter,
    ty: DamageType,
    rate_per_sec: f32,
    now: Instant,
) -> ResolvedDamage {
    let mut components = vec![(ty, rate_per_sec * CONTINUOUS_AREA_TICK_SECS)];
    mitigate(
        attacker,
        target,
        DamageSource::AreaEffect,
        ActiveSide::None,
        ActiveSide::Middle,
        &mut components,
        now,
        CONTINUOUS_AREA_TICK_SECS,
    )
}

/// How many ticks a channelled cast of `ability_uuid` at `ability_level` delivers,
/// or `None` when the ability is not channelled (no `_damagePerSecond`).
///
/// `channelMaxLength / CHANNEL_TICK_INTERVAL_SECS` — 15 for a 3 s channel. The caster
/// can release early, so this is the maximum, not a promise; the capture's 1..13 spread
/// is exactly that (a full 15 also needs no frame to be dropped, and these are UDP
/// captures).
/// The share of the defender's flat resistance one call should charge.
///
/// **A channelled spell was paying the full resistance on EVERY tick.**
/// `resistance_reduction` subtracts a flat `rating x REDUCTION_PER_RESISTANCE_RATING`,
/// and a channel re-enters the whole pipeline once per `CHANNEL_TICK_INTERVAL_SECS`
/// — 15 times for a 3 s Frostbite. So a defender with one Resist Frost affix
/// (~35 rating) slammed every tick into the 95% cap and took **14.4 damage from a
/// full channel**, 0.4% of a 3240 HP bar. Reported as "Frostbite and Ice spike quite
/// surely damage too little".
///
/// Capture-test T2 then pinned the periodic scale: a 0.2 s server tick charges
/// `0.2` of the resistance budget, and the mitigation loop applies the shipped
/// `continuousDamageResistanceEffectiveness` (`0.75`) on top. A Frostbite tick
/// therefore subtracts `rating x 0.2 x 0.75 = rating x 0.15`.
///
/// 1.0 for everything else, so every capture-pinned single-hit test is untouched.
fn resistance_scale_for(source: DamageSource, _ability_uuid: &str, _ability_level: u8) -> f32 {
    if source == DamageSource::ContinuousSpell {
        CHANNEL_TICK_INTERVAL_SECS
    } else {
        1.0
    }
}

pub fn channel_ticks(ability_uuid: &str, ability_level: u8) -> Option<u32> {
    let r = super::gamedata::ability_rank_clamped(ability_uuid, ability_level.max(1) as u16)?;
    r.damage_per_second()?;
    let span = dot_span_secs(&r);
    if span <= 0.0 {
        return None;
    }
    Some((span / CHANNEL_TICK_INTERVAL_SECS).round().max(1.0) as u32)
}

fn dot_span_secs(r: &super::gamedata::AbilityRank) -> f32 {
    r.get(super::gamedata::AbilityField::ChannelMaxLength)
        .or_else(|| r.duration())
        .filter(|s| *s > 0.0)
        .unwrap_or(super::gamedata::combat_params::ELEMENTAL_STATUS_DURATION)
}

/// Append each element's mirrored stat drain, 1:1 with the value the element carries
/// **at the moment of the call** — i.e. after [`BlockOutcome::factor_for`] has run.
///
/// # Why the drain is derived here and not with the element
///
/// The drain used to be pushed alongside its element while the components were still
/// being built, from the PRE-block magnitude, and `factor_for`'s Stamina/Magicka
/// fall-through then returned `1.0` for it. That made the drain **doubly**
/// unmitigated: never scaled by block, and computed from the unreduced element. On
/// the s506 fixture with a Frost tier-10 enchant against a connected optimal block
/// the elemental landed for 67.84 while the drain still took 137.32 — from a quantity
/// documented as a 1:1 mirror.
///
/// Retail disagrees. Capture session **s293**, decoded over a full ENet walk (241
/// distinct hits after deduping the 5x live-ingest inflation by `sequenceId`; 40
/// optimal-block, 71 mirrored-drain, **9 carrying both**), shows the drain still
/// landing on an optimally-blocked hit and equal to the already-reduced element:
/// seq 40 `232.93 Slashing + 105.87 Shock + 105.87 Magicka`, seq 164 `71.75 Shock +
/// 71.75 Magicka`, seq 348 `44.09 Slashing + 10.26 Shock + 10.26 Magicka`. The flags
/// are `0xb`, bit 3 being `wasOptimalBlocking` — corroborated by the signature
/// `ReceiveDamage(DamageList, DamageSource, ActiveSide, Actor attacker, bool fxOnly,
/// bool wasOptimalBlocking)` at `reference/il2cpp/dump.cs:338156`. And the pool really
/// moved: obj#65 seq 468 → 474, both optimal-blocked, magicka −25/1023 across the
/// pair. Magicka *falls* across consecutive optimal blocks, so the drain is reduced
/// with the element rather than suppressed.
///
/// `factor_for` deliberately keeps its `1.0` fall-through for Stamina/Magicka: the
/// mitigation is already baked into the value being mirrored, and routing the drain
/// through the block factor as well would apply it twice.
///
/// The drain is inserted immediately after its own element so the component ORDER on
/// the wire is exactly what it was before, and it is appended before step 2 so the
/// resistance/weakness pass still sees it — together those make the no-block case
/// (block factor 1.0) byte-identical. Pinned by
/// `roundtrip_s506_damage::mirrored_drain_is_byte_identical_without_a_block`.
fn append_mirrored_drains(components: &mut Vec<(DamageType, f32)>) {
    if !components.iter().any(|(ty, _)| mirrored_drain(*ty).is_some()) {
        return;
    }
    let mut out: Vec<(DamageType, f32)> = Vec::with_capacity(components.len() + 1);
    for (ty, v) in components.iter() {
        out.push((*ty, *v));
        if let Some((drain_ty, ratio)) = mirrored_drain(*ty) {
            out.push((drain_ty, *v * ratio));
        }
    }
    *components = out;
}

/// Apply the post-roll mitigation pipeline and assemble the [`ResolvedDamage`]:
///   block (a FLAT budget, per category) → resistance (a FLAT rating subtraction) →
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
    // What share of the defender's flat resistance THIS call should charge. 1.0
    // everywhere except a channelled-spell tick, which charges one PvP tick's worth
    // of the periodic resistance budget. See `resistance_scale_for`.
    resistance_scale: f32,
) -> ResolvedDamage {
    // A channelled spell IS continuous damage, so the shipped
    // CONTINUOUS_DAMAGE_RESISTANCE_EFFECTIVENESS (0.75) applies to it too. Only
    // `StatusEffect` used to qualify, which left `ContinuousSpell` paying full
    // effectiveness on every one of its ticks.
    let continuous = matches!(
        source,
        DamageSource::StatusEffect | DamageSource::ContinuousSpell
    );

    // 0) AUGMENTED ELEMENTS — `Augmented{Flames,Frost,Shock,Poison}` add a FLAT
    //    amount to that element ("Increases fire damage by {0}").
    //
    //    Direct hits only. A burning/poison DoT ticks several times a second, so
    //    adding the full flat bonus to every tick would multiply the perk by the
    //    tick count and dwarf its printed value. The bonus is added to an element
    //    the attack ALREADY deals — the perk augments fire damage, it does not
    //    grant it — so a component at zero stays at zero.
    //    A CHANNELLED spell (`ContinuousSpell`) is excluded for the same reason as
    //    a DoT: it re-enters here once per 0.2 s tick, so a per-tick flat bonus would
    //    pay the perk 15 times for one cast.
    // A CONTINUOUS tick DOES pay the Augmented* perks and Fortify gear — it is simply
    // scaled down hard. Walked in the client:
    //   AbilityContinuousDamage.Tick -> CombatManager.ResolveGenericDamage
    //     -> Actor.ResolveDamageBonuses -> ResolvePermanentDamageBonuses
    // and `ResolvePermanentDamageBonuses` never compares `damageSource` at all
    // (control: `Damage.CalculateAttackTypeFactor`, which DOES branch on it, shows the
    // `cmp` instructions this one lacks). The `PermanentFortifyDamage` list runs for
    // every source.
    //
    // The scaling is in `<ResolvePermanentDamageBonuses>b__0`: for a PERIODIC source
    // (`IsPeriodic` is true for StatusEffect, ContinuousAttack and ContinuousSpell)
    // the accumulated bonus is multiplied by `_continuousDamageFortifyEffectiveness`
    // (0.75) AND by `_globalTickInterval` (0.1) before it is added.
    //
    // So a tick pays `bonusValue * 0.075`, not the full value. Over a 15-tick 3 s
    // channel that is 22.5 -> 25.3 total at Augmented Flames rank 11, about 10% of
    // Consuming Inferno's own output — not the 337.5 that paying it in full per tick
    // would give. Paying ZERO, which is what we did, was also wrong.
    const PERIODIC_FORTIFY_SCALE: f32 = combat_params::CONTINUOUS_DAMAGE_FORTIFY_EFFECTIVENESS
        * combat_params::GLOBAL_TICK_INTERVAL;
    let fortify_scale = if continuous { PERIODIC_FORTIFY_SCALE } else { 1.0 };
    // Kept only to preserve the existing name at the use site below.
    let single_impact = true;
    // `Fortify <Element> Damage` gear joins the Augmented* perks here. Same shipped
    // shape ("Increases frost damage by {0}", no percent), so same treatment: a flat
    // add, once per hit, on the SHARED path — which is the fix. It used to be read in
    // exactly one place, inside `swing_components`, reachable only from the weapon
    // path. Every damaging spell is elemental, so a frost build's Fortify Frost was
    // discarded on 100% of its spells. A tier-10 suffix is 137.32 against
    // AugmentedFrost's 18.60 — 7.4x the perk, applied to nothing.
    //
    // `single_impact` keeps a 15-tick channel from paying it 15 times, exactly as it
    // already does for the perks.
    // Venom Strikes boosts weapon-alchemy poison status effectiveness/duration, not
    // Poison damage components (`VenomStrikesAbility` feeds `PoisonAlchemy`, 05 §3.5).
    for (ty, v) in components.iter_mut() {
        if *v > 0.0 {
            *v *= attacker.innate_damage_multiplier(*ty, source);
        }
    }
    if single_impact {
        for (ty, v) in components.iter_mut() {
            if is_elemental(*ty) && *v > 0.0 {
                *v += (attacker.perks.element_bonus(*ty) + fortify_for(attacker, *ty))
                    * fortify_scale;
            }
        }
    }

    mitigate(
        attacker,
        target,
        source,
        active_side,
        active_side,
        components,
        now,
        resistance_scale,
    )
}

/// Steps 1-2 of [`finish_resolved`] — the DEFENDER's side of a hit: block →
/// mirrored drain → resistance/weakness → total. No attacker bonus is added here.
///
/// Split out for continuous gear damage (Ebony Mail), which retail sends through the
/// defender's mitigation and nothing else: `CombatManager.ResolveContinuousAreaEffectDamage`
/// (libil2cpp `0x1BD34C4`) calls `target.ResolveDamageTaken(source, list, AreaEffect,
/// unblockable: false, NotSimulated)` then `ApplyDamage(..., ActiveSide.None)`, and
/// never `ResolveDamageBonuses`, the path the Fortify gear and Augmented perks ride.
///
/// `block_side` is the side the block test sees and `active_side` the one written to
/// the wire. They differ only for that damage: it carries `ActiveSide.None`. Retail
/// passes `unblockable = false` for it, but that does not make it blockable —
/// `Damage$$IsBlockable` has no bit for AreaEffect (7), and [`source_is_blockable`]
/// keeps it out of the block stage.
#[allow(clippy::too_many_arguments)]
pub fn mitigate_components(
    attacker: &Loadout,
    target: &Fighter,
    source: DamageSource,
    active_side: ActiveSide,
    block_side: ActiveSide,
    components: &mut Vec<(DamageType, f32)>,
    now: Instant,
    resistance_scale: f32,
) -> ResolvedDamage {
    let pre_mitigation_components = components.clone();
    let mut hit_flags = flags::SHOW_DAMAGE | flags::HAS_ATTACKER;
    let raw_components = components.clone();
    let continuous = matches!(
        source,
        DamageSource::StatusEffect | DamageSource::ContinuousSpell
    );

    // Bit 3 is the defender's `IsOptimalBlocking`, for every source (03-D19). Bit 2
    // (`WAS_LATE_BLOCKING`) is never set: in the client it marks a hit that landed
    // before the guard's startup finished, and the arena opponent's guard has no
    // startup (`PvpOpponentActor$$get_TimeToBlock@0x1963c98` returns 0 under
    // `InstantShieldBlock`). A LOW block is an ordinary block with no flag (03-D5).
    hit_flags |= target.optimal_block_flag(now);

    // 1) BLOCK — a flat per-category budget from the defender's Block Rating
    //    (`BlockOutcome::apply`). Only blockable sources reach it
    //    (`Damage$$IsBlockable@0x1bd4cc8`): an AreaEffect (7) tick is never blocked.
    let block = if source_is_blockable(source) {
        block_outcome(target, attacker, block_side, now)
    } else {
        BlockOutcome::NONE
    };
    let phys_before: f32 = components
        .iter()
        .filter(|(t, v)| is_physical(*t) && *v > 0.0)
        .map(|(_, v)| *v)
        .sum();
    block.apply(
        components,
        matches!(source, DamageSource::ContinuousSpell | DamageSource::ContinuousAttack),
    );
    // Kept for effects that ride the SWING rather than a component (Ravage): the
    // share of the physical track the block let through.
    let block_physical = if block.blocking && phys_before > 0.0 {
        let after: f32 = components
            .iter()
            .filter(|(t, v)| is_physical(*t) && *v > 0.0)
            .map(|(_, v)| *v)
            .sum();
        after / phys_before
    } else {
        1.0
    };

    // 2) ARMOR — a flat, share-weighted cut after block and after the attack-type
    //    multiplier. `DisplayClass328_0.b__0@0x1fd1944` subtracts
    //    `(v / Σphys) * max(0, A - piercing) * 0.1`, with the same 5% floor as the
    //    other mitigation stages.
    let phys_total: f32 = components
        .iter()
        .filter(|(t, v)| is_physical(*t) && *v > 0.0)
        .map(|(_, v)| *v)
        .sum();
    let armor_rating = (target.loadout.armor_rating_with_innates() - attacker.armor_piercing_rating).max(0.0);
    if phys_total > 0.0 && armor_rating > 0.0 {
        for (ty, v) in components.iter_mut() {
            if is_physical(*ty) && *v > 0.0 {
                *v = tables::armor_cut_share(*v, phys_total, armor_rating);
            }
        }
    }

    // 3) RESISTANCE — a FLAT subtraction driven by the defender's Resistance Rating,
    //    capped at `maximumResistanceReduction`, with elemental resistance first
    //    pierced by the attacker's Elemental-Resistance-Piercing rating/fraction.
    //    Transient Resist-Elements amounts are ratings too and add in.
    let mut most_resisted = DamageType::None;
    let mut most_resisted_frac = MOST_RESISTED_FLOOR;
    for (ty, v) in components.iter_mut() {
        if matches!(*ty, DamageType::Stamina | DamageType::Magicka) {
            continue;
        }
        *v *= target.loadout.innate_base_resistance_multiplier(*ty, source);
        let before = *v;
        if before <= 0.0 {
            continue;
        }
        let rating = target.resistance_rating_against(
            *ty,
            attacker.elem_resist_piercing,
            attacker.elem_resist_piercing_rating,
        ) + target.transient_resistance_against(*ty, now);
        let weakness = target.weakness_rating_against(*ty, now);
        let piercing = if is_elemental(*ty) {
            attacker.elem_resist_piercing_rating
        } else {
            0.0
        };
        let effectiveness_scale = if continuous {
            combat_params::CONTINUOUS_DAMAGE_RESISTANCE_EFFECTIVENESS * resistance_scale
        } else {
            resistance_scale
        };
        let mitigated =
            tables::apply_resistance_and_weakness(before, rating, weakness, effectiveness_scale, piercing);
        let resisted = (before - mitigated).max(0.0);
        *v = mitigated;
        if resisted > 0.0 && is_elemental(*ty) {
            let frac = resisted.min(before) / before;
            if frac > most_resisted_frac {
                most_resisted_frac = frac;
                most_resisted = *ty;
            }
        }
    }

    // 3.5) MIRRORED STAT DRAIN — Frost→Stamina / Shock→Magicka, 1:1 with the final
    //      health-affecting element. The drain itself does not run through armor,
    //      base resistance, resistance or weakness a second time.
    append_mirrored_drains(components);

    let total: f32 = components
        .iter()
        .filter(|(t, _)| is_health_type(*t))
        .map(|(_, v)| *v)
        .sum();

    ResolvedDamage {
        source,
        active_side,
        flags: hit_flags,
        pre_mitigation_components,
        components: std::mem::take(components),
        raw_components,
        total,
        most_resisted,
        negated: false,
        heal: 0.0,
        block_physical,
        blocked: block.blocking,
        resistance_scale,
    }
}

fn mitigate(
    attacker: &Loadout,
    target: &Fighter,
    source: DamageSource,
    active_side: ActiveSide,
    block_side: ActiveSide,
    components: &mut Vec<(DamageType, f32)>,
    now: Instant,
    resistance_scale: f32,
) -> ResolvedDamage {
    mitigate_components(
        attacker,
        target,
        source,
        active_side,
        block_side,
        components,
        now,
        resistance_scale,
    )
}

/// `Damage$$IsBlockable@0x1bd4cc8`: the mask `0x90e` (Attack 1, Spell 2,
/// WeaponManeuver 3, ShieldManeuver 11) plus ContinuousSpell 8, EchoWeapon 9 and
/// ContinuousAttack 10. StatusEffect 4, Trap 5, Revenge 6 and AreaEffect 7 are
/// never blocked.
pub fn source_is_blockable(source: DamageSource) -> bool {
    matches!(
        source,
        DamageSource::Attack
            | DamageSource::Spell
            | DamageSource::WeaponManeuver
            | DamageSource::ContinuousSpell
            | DamageSource::EchoWeapon
            | DamageSource::ContinuousAttack
            | DamageSource::ShieldManeuver
    )
}

/// Minimum resisted fraction for an element to be reported as `mostResisted`
/// (`CombatHUDHelper.DetermineMostResistedElementalDamageType`). The shipped
/// `resistMessagingThreshold` is 0.2 — kept at the lower 0.05 wire floor because the
/// capture reports `mostResisted` well below the *messaging* threshold.
const MOST_RESISTED_FLOOR: f32 = 0.05;

#[cfg(test)]
mod report31_span_tests {
    use super::*;
    use crate::arena::combat::gamedata::{self, AbilityField};

    /// The blast radius of reading a `_damagePerSecond` ability's OWN span instead
    /// of the 5 s elemental-status duration. Exactly three abilities ship
    /// `damagePerSecond` with no `_damage` and are routed to `resolve_ability`:
    ///
    /// | ability          | span field          | span | was | now |
    /// |------------------|---------------------|------|-----|-----|
    /// | Frostbite        | `channelMaxLength`  | 3    | ×5  | ×3  |
    /// | ConsumingInferno | `channelMaxLength`  | 3    | ×5  | ×3  |
    /// | PoisonCloud      | `duration`          | 5    | ×5  | ×5  |
    ///
    /// PoisonCloud is the control: its shipped `duration` IS 5, so this change
    /// must leave it byte-identical. If the span lookup were wrong, it would move.
    #[test]
    fn dot_span_comes_from_the_ability_not_the_status_duration() {
        const FROSTBITE: &str = "4be1d681-c35d-4540-b255-c2910ac80664";
        const CONSUMING_INFERNO: &str = "e07f9b1a-64db-44ef-ba25-0e4378789ddc";
        const POISON_CLOUD: &str = "66bdc017-30c5-4b5e-9753-215c45056f6a";

        for (uuid, want) in [(FROSTBITE, 3.0), (CONSUMING_INFERNO, 3.0), (POISON_CLOUD, 5.0)] {
            let r = gamedata::ability_rank_clamped(uuid, 1).expect("shipped rank 1");
            assert!(r.damage_per_second().is_some(), "{uuid} ships damagePerSecond");
            assert!(r.damage().is_none(), "{uuid} ships no flat _damage");
            assert_eq!(dot_span_secs(&r), want, "{uuid} span");
        }

        // Frostbite's span is its channel, and it is NOT the elemental-status
        // duration — the two were conflated.
        let fb = gamedata::ability_rank_clamped(FROSTBITE, 4).unwrap();
        assert_eq!(fb.get(AbilityField::ChannelMaxLength), Some(3.0));
        assert_ne!(dot_span_secs(&fb), gamedata::combat_params::ELEMENTAL_STATUS_DURATION);
        // Rank 4 is the reporter's rank (magickaCost 235, logged by arena-server on
        // 2026-08-18 05:46:47): 95.80 dps × 3 s = 287.4, not the 479.0 prod emitted.
        assert!((fb.damage_per_second().unwrap() * dot_span_secs(&fb) - 287.4).abs() < 0.1);
    }
}

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

    #[test]
    fn weapon_damage_enchants_use_the_weight_specific_base_table() {
        let light = weapon_damage_family_value_for_weight(DamageType::Fire, 10, Weight::Light);
        let versatile = weapon_damage_family_value_for_weight(DamageType::Fire, 10, Weight::Versatile);
        let heavy = weapon_damage_family_value_for_weight(DamageType::Fire, 10, Weight::Heavy);
        assert!((light - 57.25).abs() < 0.01, "Fire t10 light = 57.25, got {light}");
        assert!((versatile - 67.2).abs() < 0.01, "Fire t10 versatile = 67.2, got {versatile}");
        assert!((heavy - 78.49).abs() < 0.01, "Fire t10 heavy = 78.49, got {heavy}");
    }

    #[test]
    fn weapon_damage_enchants_pay_nothing_without_a_weapon_class() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let mut unarmed = plain_blade(Weight::Light);
        unarmed.weapon.weight = None;
        unarmed.enchants = vec![(DamageType::Fire, 10)];

        let hit = m.resolve_attack(&unarmed, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);

        assert_eq!(comp(&hit, DamageType::Fire), 0.0);
    }

    #[test]
    fn racial_damage_factor_and_base_resistance_apply_before_flat_mitigation() {
        let m = RetailDamageModel;
        let now = Instant::now();

        let mut argonian = plain_blade(Weight::Light);
        argonian.innate_damage_factors.push(super::super::state::InnateDamageFactor {
            damage_types: Vec::new(),
            damage_sources: Vec::new(),
            weapon_class: Some(Weight::Light),
            factor: 0.05,
        });
        let hit = m.resolve_attack(&argonian, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&hit, DamageType::Slashing) - 105.0).abs() < 0.01);

        let mut frost = plain_blade(Weight::Light);
        frost.weapon.base_by_type = vec![(DamageType::Frost, 100.0)];
        let mut nord = target();
        nord.loadout.innate_base_resistances.push(super::super::state::InnateBaseResistance {
            damage_types: vec![DamageType::Frost],
            damage_sources: Vec::new(),
            factor: 0.15,
        });
        let resisted = m.resolve_attack(&frost, &nord, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&resisted, DamageType::Frost) - 85.0).abs() < 0.01);

        let mut fire = plain_blade(Weight::Light);
        fire.weapon.base_by_type = vec![(DamageType::Fire, 100.0)];
        fire.perks.element_damage.push((DamageType::Fire, 10.0));
        fire.innate_damage_factors.push(super::super::state::InnateDamageFactor {
            damage_types: vec![DamageType::Fire],
            damage_sources: Vec::new(),
            weapon_class: None,
            factor: 0.05,
        });
        let ordered = m.resolve_attack(&fire, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&ordered, DamageType::Fire) - 115.0).abs() < 0.01);
    }

    /// An un-armored, un-blocking L100 target.
    pub(super) fn target() -> Fighter {
        Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, Instant::now())
    }

    /// A target suffering an elemental condition — the PDOC/EDOC gate.
    pub(super) fn conditioned_target(now: Instant) -> Fighter {
        use crate::arena::combat::state::{ActiveEffect, StatusEffectType};
        let mut f = target();
        f.effects.push(ActiveEffect {
            effect: StatusEffectType::Poisoned,
            damage_type: DamageType::Poison,
            value: 0.0,
            per_tick_damage: 0.0,
            expires_at: now + Duration::from_secs(5),
            last_tick: now,
            is_transient_resist: false,
        });
        f
    }

    /// THE PDOC PLACEMENT — the thing the owner flagged: "There is a special thing
    /// about PDOC in how it applies to critical hits which really makes for a lot of
    /// damage."
    ///
    /// PDOC is added to the physical BASE, so the combo and crit multipliers multiply
    /// it. Adding it in `finish_resolved` step 0 instead — the obvious spot, beside the
    /// other flat bonuses — would land it AFTER the multiplier and discard most of it.
    ///
    /// A single hit cannot tell the two apart. Comparing TWO combo depths can: if PDOC
    /// is pre-multiplier the gain scales with the combo factor; if post-multiplier the
    /// gain is the same flat number at every depth.
    #[test]
    fn pdoc_scales_with_the_combo_multiplier() {
        let m = RetailDamageModel;
        let now = Instant::now();
        const PDOC: f32 = 25.20; // the SHIPPED OpportunistPhysical t10

        let gain_at = |combo: u32| -> f32 {
            let mut with = poison_dagger();
            with.opportunist_physical = PDOC;
            let without = poison_dagger();
            let a = comp(&m.resolve_attack(&with, &conditioned_target(now),
                DamageSource::Attack, ActiveSide::Left, 1.0, combo, now), DamageType::Slashing);
            let b = comp(&m.resolve_attack(&without, &conditioned_target(now),
                DamageSource::Attack, ActiveSide::Left, 1.0, combo, now), DamageType::Slashing);
            a - b
        };

        let shallow = gain_at(0);
        let deep = gain_at(3);
        assert!(shallow > 0.0, "PDOC must contribute at all, got {shallow}");
        assert!(
            deep > shallow * 1.5,
            "PDOC must be MULTIPLIED by the combo: depth-0 gain {shallow:.1} vs \
             depth-3 gain {deep:.1}. Equal gains mean it landed after the multiplier."
        );
    }

    /// The gate. `_triggerStatusEffects = [4,5,6,7]` in the shipped asset — Burning,
    /// Frozen, Enervated, Poisoned. An unconditioned target pays nothing.
    #[test]
    fn pdoc_pays_nothing_against_an_unconditioned_target() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let mut a = poison_dagger();
        a.opportunist_physical = 25.20;
        let with = comp(&m.resolve_attack(&a, &target(),
            DamageSource::Attack, ActiveSide::Left, 1.0, 0, now), DamageType::Slashing);
        let without = comp(&m.resolve_attack(&poison_dagger(), &target(),
            DamageSource::Attack, ActiveSide::Left, 1.0, 0, now), DamageType::Slashing);
        assert!((with - without).abs() < 0.01, "no condition, no PDOC");
    }

    /// Staggered / Blind / Paralyzed are NOT in `_triggerStatusEffects`, so they must
    /// not arm it — a distinction that would be invisible without the extracted asset.
    #[test]
    fn pdoc_is_not_armed_by_stagger_or_paralysis() {
        use crate::arena::combat::state::{ActiveEffect, StatusEffectType};
        let m = RetailDamageModel;
        let now = Instant::now();
        for effect in [StatusEffectType::Staggered, StatusEffectType::Paralyzed, StatusEffectType::Blind] {
            let mut t = target();
            t.effects.push(ActiveEffect {
                effect,
                damage_type: DamageType::None,
                value: 0.0,
                per_tick_damage: 0.0,
                expires_at: now + Duration::from_secs(5),
                last_tick: now,
                is_transient_resist: false,
            });
            let mut a = poison_dagger();
            a.opportunist_physical = 25.20;
            let with = comp(&m.resolve_attack(&a, &t,
                DamageSource::Attack, ActiveSide::Left, 1.0, 0, now), DamageType::Slashing);
            let without = comp(&m.resolve_attack(&poison_dagger(), &t,
                DamageSource::Attack, ActiveSide::Left, 1.0, 0, now), DamageType::Slashing);
            assert!(
                (with - without).abs() < 0.01,
                "{effect:?} is not one of the shipped triggers [4,5,6,7] — it must not arm PDOC"
            );
        }
    }

    /// EDOC lands on the enchant track, which sits OUTSIDE the combo/crit multiplier.
    /// So elemental damage does NOT scale with crit while physical does — the
    /// asymmetry the community guides describe, falling out of the existing structure.
    #[test]
    fn edoc_does_not_scale_with_the_combo_multiplier() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let gain_at = |combo: u32| -> f32 {
            let mut with = poison_dagger();
            with.opportunist_elemental = 25.20;
            let a = comp(&m.resolve_attack(&with, &conditioned_target(now),
                DamageSource::Attack, ActiveSide::Left, 1.0, combo, now), DamageType::Poison);
            let b = comp(&m.resolve_attack(&poison_dagger(), &conditioned_target(now),
                DamageSource::Attack, ActiveSide::Left, 1.0, combo, now), DamageType::Poison);
            a - b
        };
        let shallow = gain_at(0);
        let deep = gain_at(3);
        assert!(shallow > 0.0, "EDOC must contribute, got {shallow}");
        assert!(
            (deep - shallow).abs() < 0.5,
            "EDOC must NOT be multiplied by the combo: {shallow:.1} vs {deep:.1}"
        );
    }

    fn comp(rd: &ResolvedDamage, ty: DamageType) -> f32 {
        rd.components.iter().filter(|(t, _)| *t == ty).map(|(_, v)| *v).sum()
    }

    /// 01 D3 / 02 X3: the attack-type factor is ONE sum, `1 + [combo>=1]*comboDF +
    /// swing`, over combo in {0, 1} and swing in {0, 0.1625, 0.325} at AR 0. The
    /// dagger ships comboDF 0.54 and its 144 tempered base is unarmored here, so the
    /// Slashing component must be `144 * (1 + 0.54c + s)` exactly. The fork used to
    /// take `max(combo, charge)`, which is 1.99 at every chained cell.
    ///
    /// Controls: a maneuver takes the combo but never the swing, a shield bash takes
    /// neither (05 §2.6), and the enchant track takes none of it (01 D4).
    #[test]
    fn combo_and_charge_add_into_one_factor() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let now = Instant::now();
        let fresh_poison = comp(
            &m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now),
            DamageType::Poison,
        );
        for c in [0u32, 1] {
            for swing in [0.0f32, 0.1625, 0.325] {
                let rd = m.resolve_attack(
                    &lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0 + swing, c, now,
                );
                let want = 144.0 * (1.0 + 0.54 * c as f32 + swing);
                let got = comp(&rd, DamageType::Slashing);
                assert!((got - want).abs() < 0.05, "combo {c}, swing {swing}: {got} != {want}");
                assert!((comp(&rd, DamageType::Poison) - fresh_poison).abs() < 1e-3);

                let man = m.resolve_attack(
                    &lo, &target(), DamageSource::WeaponManeuver, ActiveSide::Middle, 1.0 + swing, c, now,
                );
                let want = 144.0 * (1.0 + 0.54 * c as f32);
                let got = comp(&man, DamageType::Slashing);
                assert!((got - want).abs() < 0.05, "maneuver, combo {c}, swing {swing}: {got} != {want}");
            }
        }
    }

    #[test]
    fn armor_is_applied_after_the_attack_multiplier() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let attacker = plain_blade(Weight::Light);
        let mut defender = target();
        defender.loadout.armor_rating = 300.0;

        let rd = m.resolve_attack(
            &attacker,
            &defender,
            DamageSource::Attack,
            ActiveSide::Right,
            1.0,
            1,
            now,
        );
        let slash = comp(&rd, DamageType::Slashing);
        assert!((slash - 124.0).abs() < 0.05, "100 * 1.54 - 30, got {slash}");

        let control = m.resolve_attack(
            &attacker,
            &target(),
            DamageSource::Attack,
            ActiveSide::Right,
            1.0,
            1,
            now,
        );
        assert!((comp(&control, DamageType::Slashing) - 154.0).abs() < 0.05);
    }

    #[test]
    fn armor_budget_is_shared_across_physical_components() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let attacker = Loadout {
            weapon: WeaponProfile {
                primary_type: Some(DamageType::Slashing),
                base_by_type: vec![(DamageType::Slashing, 100.0), (DamageType::Bashing, 300.0)],
                weight: Some(Weight::Heavy),
            },
            ..Default::default()
        };
        let mut defender = target();
        defender.loadout.armor_rating = 400.0;

        let rd = m.resolve_attack(
            &attacker,
            &defender,
            DamageSource::Attack,
            ActiveSide::Right,
            1.0,
            0,
            now,
        );
        assert!((comp(&rd, DamageType::Slashing) - 90.0).abs() < 0.05);
        assert!((comp(&rd, DamageType::Bashing) - 270.0).abs() < 0.05);
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
        // One step of the dagger's shipped `_comboDamageFactor` 0.54 (02 §4.2): the
        // chained swing is ×1.54 and a deeper one is no more.
        assert!((comp(&c1, DamageType::Slashing) - 144.0 * 1.54).abs() < 0.5);
        assert!(
            (comp(&c4, DamageType::Slashing) - 144.0 * 1.54).abs() < 0.5,
            "depth 4 is the same one step as depth 1"
        );
        // The enchant track is combo-independent.
        assert!((comp(&c0, DamageType::Poison) - comp(&c1, DamageType::Poison)).abs() < 1e-3);
        assert!((comp(&c4, DamageType::Poison) - comp(&c0, DamageType::Poison)).abs() < 1e-3);
    }

    /// ARMOR (01-D1): a Rating removes `rating × 0.1` after the attack-type
    /// multiplier, so the flat cut does NOT scale with combo.
    #[test]
    fn armor_rating_cuts_after_the_attack_multiplier() {
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
        assert!((c1 - 191.58).abs() < 0.05, "144 × 1.54 − 30.18 = 191.58, got {c1}");
        // Armor does NOT touch the elemental track.
        let poison = comp(
            &m.resolve_attack(&lo, &armored, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now),
            DamageType::Poison,
        );
        assert!((poison - 57.25).abs() < 0.5, "armor is physical-only, got {poison}");
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
        assert!((comp(&rd, DamageType::Poison) - 57.25).abs() < 0.5);
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

    #[test]
    fn mirrored_drain_is_based_on_final_element_after_resistance() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let mut shock = plain_blade(Weight::Light);
        shock.weapon.base_by_type = vec![(DamageType::Shock, 100.0)];
        let mut resisted = target();
        resisted.loadout.resistances = vec![(DamageType::Shock, 20.0)];

        let hit = m.resolve_attack(&shock, &resisted, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);

        assert!((comp(&hit, DamageType::Shock) - 80.0).abs() < 0.01);
        assert!((comp(&hit, DamageType::Magicka) - 80.0).abs() < 0.01);
    }

    /// A defender guarding with `item` at `tempering`, the guard raised at `now`
    /// (optimal) — or inside the post-release cooldown when `low` (a low block).
    fn guarding(
        now: Instant,
        shield: Option<(&str, u64)>,
        weapon: Option<(&str, u64)>,
        low: bool,
    ) -> Fighter {
        let mut def = target();
        if let Some((u, t)) = weapon {
            let w = gamedata::weapon(u).expect("weapon template");
            def.loadout.weapon_optimal_block_boost = w.optimal_block_boost;
            def.loadout.block_rating = loadout::blocking_item_rating(u, w.block_base, t);
        }
        if let Some((u, t)) = shield {
            let sh = gamedata::shield(u).expect("shield template");
            def.loadout.has_shield = true;
            def.loadout.shield_optimal_block_boost = sh.optimal_block_boost;
            def.loadout.block_rating = loadout::blocking_item_rating(u, sh.block_base, t);
        }
        def.set_actor_state(ActorStateType::Blocking, now);
        def.blocking_side = ActiveSide::Middle;
        def.block_raised_at = Some(now);
        def.blocking_until = Some(now + Duration::from_secs(8));
        if low {
            // Released 0.1 s before this raise: inside the 0.8 s cooldown, so the
            // latch says no optimal for this whole guard.
            def.last_block_dropped_at = Some(now - Duration::from_millis(100));
        }
        def
    }

    /// Blocking-item templates behind the capture-test T1 fixtures
    /// (blades-capture `docs/combat-spec/capture-tests.md` §1).
    const LEATHER_SHIELD_247: &str = "aa4ffedf-ad74-4aa1-bd82-18a041100f79"; // T10 → 360
    const DAEDRIC_SHIELD_300: &str = "fc5ef036-e045-4920-b95f-b32067b00163"; // T10 → 450
    const EBONY_SHIELD_330: &str = "1d248608-7347-4122-8b42-840b6304c203"; // untempered 330
    const IRON_WARHAMMER_148: &str = "905ee635-200e-445a-89de-db5ecbb17989"; // T10 → 216
    const DEATHSTING: &str = "28000fc8-4208-4036-90d4-5e698b680d96"; // untempered 147

    fn apply(block: &BlockOutcome, comps: &[(DamageType, f32)]) -> Vec<(DamageType, f32)> {
        let mut c = comps.to_vec();
        block.apply(&mut c, false);
        c
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    /// The T1 golden cuts, each one a hit observed on the retail corpus to the cent,
    /// with the prediction from `factor · R · 0.1` (PvP factors 1.6 / 0.82).
    #[test]
    fn t1_golden_block_cuts() {
        let now = Instant::now();
        let none = Loadout::default();
        let cut = |def: &Fighter, ty: DamageType| {
            let b = block_outcome(def, &none, ActiveSide::Right, now);
            let d = 1000.0;
            d - apply(&b, &[(ty, d)])[0].1
        };

        // Elven etc., Leather/Hide Shield T10 (R0 360), optimal, physical: 115.20.
        let leather = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        assert_eq!(leather.loadout.block_rating, 360.0);
        assert!(close(cut(&leather, DamageType::Slashing), 115.20));
        // Tristan, Daedric Shield T10 (R0 450): low 72.00, optimal 144.00.
        let daedric_low = guarding(now, Some((DAEDRIC_SHIELD_300, 10)), None, true);
        let daedric_opt = guarding(now, Some((DAEDRIC_SHIELD_300, 10)), None, false);
        assert!(close(cut(&daedric_low, DamageType::Cleaving), 72.00));
        assert!(close(cut(&daedric_opt, DamageType::Cleaving), 144.00));
        // Morgoth, Iron Warhammer T10 (R0 216, a WEAPON block), optimal:
        // physical 69.12 and poison 35.42.
        let mut morgoth = guarding(now, None, Some((IRON_WARHAMMER_148, 10)), false);
        assert_eq!(morgoth.loadout.block_rating, 216.0);
        // He carries Elemental Protection r1 — and matches only WITHOUT it, because
        // the perk needs a shield (10 §3).
        morgoth.loadout.perks.elemental_block_rating = 32.5;
        assert!(close(cut(&morgoth, DamageType::Bashing), 69.12));
        assert!(close(cut(&morgoth, DamageType::Poison), 35.42));
        // Ava, Deathsting (R0 147, weapon), low, fire: 12.05.
        let ava = guarding(now, None, Some((DEATHSTING, 0)), true);
        assert!(close(cut(&ava, DamageType::Fire), 12.05));
        // Galadriel etc., R0 360 shield + EP rank 6 (117.5), optimal, elemental:
        // (720 + 117.5) · 0.082 = 68.68 — EP adds AFTER the doubling.
        let mut gal = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        gal.loadout.perks.elemental_block_rating = 117.5;
        assert!(close(cut(&gal, DamageType::Frost), 68.675));
        // Black Betty, same shield + EP 117.5, LOW, poison: (360 + 117.5) · 0.082 =
        // 39.155 at the block stage. (The observed 33.28 is that × Redguard 0.85,
        // which is applied later in the client and is PR-14's, not modelled here.)
        let mut betty = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, true);
        betty.loadout.perks.elemental_block_rating = 117.5;
        assert!(close(cut(&betty, DamageType::Poison), 39.155));
        assert!(close(cut(&betty, DamageType::Poison) * 0.85, 33.28));
    }

    /// 03-D1 / M-block-shape: the SAME guard takes the SAME amount off a 50, 150 or
    /// 450 hit — the old fraction took more off a bigger hit. Ebony Shield (330),
    /// low block, 200 Slashing: 330 · 1.6 · 0.1 = 52.8 → 147.2.
    #[test]
    fn a_block_is_a_constant_cut_not_a_fraction() {
        let now = Instant::now();
        let def = guarding(now, Some((EBONY_SHIELD_330, 0)), None, true);
        let b = block_outcome(&def, &Loadout::default(), ActiveSide::Right, now);
        assert!(b.blocking && !b.optimal);
        assert!(close(apply(&b, &[(DamageType::Slashing, 200.0)])[0].1, 147.2));
        for d in [150.0_f32, 450.0] {
            assert!(close(d - apply(&b, &[(DamageType::Slashing, d)])[0].1, 52.8), "d={d}");
        }
        // 50 is under the budget: the 5 % floor holds it at 2.5, never 0.
        assert!(close(apply(&b, &[(DamageType::Slashing, 50.0)])[0].1, 2.5));
        // Control: an unguarded defender takes the hit unchanged.
        let open = block_outcome(&target(), &Loadout::default(), ActiveSide::Right, now);
        assert!(!open.blocking);
        assert_eq!(apply(&open, &[(DamageType::Slashing, 200.0)])[0].1, 200.0);
    }

    /// The owner's match (gsid 45149845, 2026-09-25): every physical hit into an
    /// optimal block did exactly 0.0. There is no ×0: the same hit keeps what the
    /// budget leaves. Poison dagger (Slashing 144 + tier-10 light Poison, both from
    /// shipped data) into a Leather Shield T10 (R 720 optimal): 144 − 115.2 = 28.8
    /// and the elemental track takes the PvP elemental block budget.
    #[test]
    fn an_optimal_block_does_not_zero_physical() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let lo = poison_dagger();
        let open = m.resolve_attack(&lo, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(close(comp(&open, DamageType::Slashing), 144.0));
        assert!(close(comp(&open, DamageType::Poison), 57.25));
        assert!(!open.blocked);

        let def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        let opt = m.resolve_attack(&lo, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(opt.blocked);
        assert_ne!(opt.flags & flags::WAS_OPTIMAL_BLOCKING, 0);
        assert!(close(comp(&opt, DamageType::Slashing), 28.8), "{:?}", opt.components);
        assert!(close(comp(&opt, DamageType::Poison), 2.8625), "{:?}", opt.components);
        assert!(close(opt.block_physical, 28.8 / 144.0));

        // The same guard, low: 144 − 57.6 plus the low elemental budget; no wire flag.
        let low_def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, true);
        let low = m.resolve_attack(&lo, &low_def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(low.blocked);
        assert_eq!(low.flags & (flags::WAS_OPTIMAL_BLOCKING | flags::WAS_LATE_BLOCKING), 0);
        assert!(close(comp(&low, DamageType::Slashing), 86.4));
        assert!(close(comp(&low, DamageType::Poison), 27.73));
    }

    /// A block sets a Stamina or Magicka component to 0 outright
    /// (`b__1@0x1fd06f0` 0x1fd0790). The Frost→Stamina mirror still lands, equal to
    /// the post-block Frost, because it is derived afterwards.
    #[test]
    fn a_block_zeroes_stamina_and_magicka_but_the_mirror_still_lands() {
        let now = Instant::now();
        let def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        let b = block_outcome(&def, &Loadout::default(), ActiveSide::Right, now);
        let out = apply(&b, &[(DamageType::Magicka, 255.83), (DamageType::Stamina, 40.0)]);
        assert_eq!(out, vec![(DamageType::Magicka, 0.0), (DamageType::Stamina, 0.0)]);
        // Control: unblocked, they pass.
        let open = block_outcome(&target(), &Loadout::default(), ActiveSide::Right, now);
        assert_eq!(apply(&open, &[(DamageType::Magicka, 255.83)])[0].1, 255.83);

        let mut frost = poison_dagger();
        frost.enchants = vec![(DamageType::Frost, 10)];
        let r = RetailDamageModel.resolve_attack(&frost, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(comp(&r, DamageType::Frost) > 0.0);
        assert_eq!(comp(&r, DamageType::Stamina), comp(&r, DamageType::Frost));
    }

    /// Block piercing is subtracted from the BOOSTED R — an optimal block is not
    /// immune to it (03 V2). R 720 − Skullcrusher's 60 = 660 → 105.6 physical.
    /// Elemental piercing touches only the elemental R.
    #[test]
    fn block_piercing_comes_off_the_boosted_rating() {
        let now = Instant::now();
        let def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        let mut pierce = Loadout::default();
        pierce.block_piercing_rating = 60.0;
        let b = block_outcome(&def, &pierce, ActiveSide::Right, now);
        assert!(b.optimal);
        assert!(close(1000.0 - apply(&b, &[(DamageType::Slashing, 1000.0)])[0].1, 105.6));
        assert!(close(1000.0 - apply(&b, &[(DamageType::Fire, 1000.0)])[0].1, 59.04));
        // Piercing can never make the rating negative.
        pierce.block_piercing_rating = 10_000.0;
        let b = block_outcome(&def, &pierce, ActiveSide::Right, now);
        assert_eq!(apply(&b, &[(DamageType::Slashing, 200.0)])[0].1, 200.0);
    }

    /// The budget is shared by SHARE within a category, and each category has its
    /// own. Two physical components 100 + 300 against the 115.2 budget lose 28.8 and
    /// 86.4; an elemental component beside them draws on the elemental budget only.
    #[test]
    fn the_budget_is_split_by_share_within_its_category() {
        let now = Instant::now();
        let def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        let b = block_outcome(&def, &Loadout::default(), ActiveSide::Right, now);
        let out = apply(
            &b,
            &[(DamageType::Slashing, 100.0), (DamageType::Bashing, 300.0), (DamageType::Fire, 200.0)],
        );
        assert!(close(out[0].1, 71.2));
        assert!(close(out[1].1, 213.6));
        assert!(close(out[2].1, 200.0 - 59.04));
    }

    /// 03-D18: the optimal boost is `×(1 + boost)` of the blocking item — the Pestle
    /// ships 0.5 (×1.5), the Initial Dagger 2.0 (×3). The fork used `max(boost, 1) ×
    /// 2`. Control: a boost of 1.0 is ×2 either way.
    #[test]
    fn the_optimal_boost_is_one_plus_the_items_own_boost() {
        let now = Instant::now();
        let pestle = guarding(now, None, Some(("12bc6bad-15df-4fa1-8335-22fbb4eb8d9f", 0)), false);
        assert_eq!(pestle.loadout.block_rating, 5.0);
        assert!(close(pestle.block_rating(true), 7.5));
        let initial = guarding(now, None, Some(("d0386ef9-9e0f-47a8-b091-976def4d63b2", 0)), false);
        assert!(close(initial.block_rating(true), 15.0));
        let steel = guarding(now, None, Some(("68577fab-83f5-4bd1-983c-f396963ac14b", 0)), false);
        assert_eq!(steel.block_rating(false), 116.0, "ceil(115.5)");
        assert_eq!(steel.block_rating(true), 232.0);
        // Block Reduction enchants add AFTER the boost, so they are not doubled.
        let mut enchanted = steel.clone();
        enchanted.loadout.block_rating_bonus = 50.0;
        assert_eq!(enchanted.block_rating(true), 282.0);
    }

    /// Optimal eligibility is LATCHED when the guard goes up
    /// (`ChangeToBlockingState@0x1d5beec`). Drop at t0, raise at t0+0.5 (inside the
    /// 0.8 s cooldown), hit at t0+1.0 (past it): the client says LOW for the whole
    /// guard; the fork used to re-check at the hit and say optimal. Control: a raise
    /// at t0+0.9 is optimal.
    #[test]
    fn optimal_eligibility_is_latched_at_the_raise() {
        let t0 = Instant::now();
        let mut def = guarding(t0, Some((EBONY_SHIELD_330, 0)), None, false);
        def.last_block_dropped_at = Some(t0);
        def.block_raised_at = Some(t0 + Duration::from_millis(500));
        let hit_at = t0 + Duration::from_millis(1000);
        assert_eq!(def.block_phase(hit_at), Some(BlockPhase::Late));
        assert_eq!(def.optimal_block_flag(hit_at), 0);
        def.block_raised_at = Some(t0 + Duration::from_millis(900));
        assert_eq!(def.block_phase(hit_at), Some(BlockPhase::Optimal));
        assert_eq!(def.optimal_block_flag(hit_at), flags::WAS_OPTIMAL_BLOCKING);
    }

    /// 03-D5: bit 2 means "the guard was not up yet"; a guard HELD for 3 s and hit
    /// is a low block with flags & 0x4 == 0 (and no bit 3 either).
    #[test]
    fn a_held_guard_never_sends_the_late_bit() {
        let now = Instant::now();
        let def = guarding(now, Some((EBONY_SHIELD_330, 0)), None, false);
        let at = now + Duration::from_secs(3);
        let r = RetailDamageModel.resolve_attack(&poison_dagger(), &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, at);
        assert!(r.blocked);
        assert_eq!(r.flags & flags::WAS_LATE_BLOCKING, 0);
        assert_eq!(r.flags & flags::WAS_OPTIMAL_BLOCKING, 0);
    }

    /// `Damage$$IsBlockable@0x1bd4cc8`: an AreaEffect (7) tick is never blocked, but
    /// bit 3 is still the defender's state on its frame (03-D19).
    #[test]
    fn an_area_effect_tick_is_not_blocked_but_carries_the_optimal_bit() {
        let now = Instant::now();
        let def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        let tick = resolve_continuous_area_tick(&Loadout::default(), &def, DamageType::Poison, 20.0, now);
        assert!(!tick.blocked);
        assert!(close(comp(&tick, DamageType::Poison), 20.0 * CONTINUOUS_AREA_TICK_SECS));
        assert_ne!(tick.flags & flags::WAS_OPTIMAL_BLOCKING, 0);
        // Control: the same tick on an unguarded defender, same value, no bit.
        let open = resolve_continuous_area_tick(&Loadout::default(), &target(), DamageType::Poison, 20.0, now);
        assert_eq!(comp(&open, DamageType::Poison), comp(&tick, DamageType::Poison));
        assert_eq!(open.flags & flags::WAS_OPTIMAL_BLOCKING, 0);
    }

    /// A channelled (ContinuousSpell) tick is blocked with `R` scaled by the 0.2 s
    /// PvP tick (INFERRED from capture T2, see `BlockOutcome::apply`), so a full
    /// 15-tick channel loses ~3× one hit's budget rather than 15×.
    #[test]
    fn a_periodic_tick_is_blocked_at_the_tick_scaled_rating() {
        let now = Instant::now();
        let def = guarding(now, Some((LEATHER_SHIELD_247, 10)), None, false);
        let b = block_outcome(&def, &Loadout::default(), ActiveSide::Middle, now);
        let mut c = vec![(DamageType::Frost, 40.0_f32)];
        b.apply(&mut c, true);
        // 720 · 0.2 · 0.82 · 0.1 = 11.808
        assert!(close(40.0 - c[0].1, 11.808), "{c:?}");
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
        assert!((comp(&rd, DamageType::Poison) - 17.25).abs() < 0.5);
        assert_eq!(rd.most_resisted, DamageType::Poison);

        let mut piercer = poison_dagger();
        piercer.elem_resist_piercing_rating = 25.0;
        let rd2 = m.resolve_attack(&piercer, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(
            (comp(&rd2, DamageType::Poison) - 42.25).abs() < 0.5,
            "piercing 25 of the 40 rating leaves 15, got {}",
            comp(&rd2, DamageType::Poison)
        );
        // Resistance can never remove more than 95 % of the component.
        let mut wall = target();
        wall.loadout.resistances = vec![(DamageType::Poison, 100_000.0)];
        let rd3 = m.resolve_attack(&poison_dagger(), &wall, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd3, DamageType::Poison) - 2.8625).abs() < 0.5);
    }

    /// THE FORTIFY BUG — EDIR's twin, on the same frost build.
    ///
    /// `Fortify <Element> Damage` was read in exactly ONE place: inside
    /// `swing_components`, reachable only from `resolve_attack`. `resolve_ability`
    /// never calls it, and the `caster_loadout` it built left `element_fortify` empty
    /// — two independent reasons a spell could never see it. Every damaging spell in
    /// the shipped table is elemental, so a frost build's Fortify Frost was discarded
    /// on 100% of its spells.
    ///
    /// Scale: a tier-10 suffix is 137.32 against AugmentedFrost's 18.60 — 7.4x the
    /// perk, applied to nothing.
    #[test]
    fn fortify_element_applies_to_spells_not_only_weapons() {
        let m = RetailDamageModel;
        let now = Instant::now();
        // Fireball is the module's standing reference spell; the bug is path-shaped,
        // not element-specific, so any elemental spell exercises it.
        let spell = gamedata::ids::FIREBALL;

        let bare = m.resolve_ability(
            spell, 1, &crate::arena::combat::perks::CasterPerks::none(), &target(), ActiveSide::Middle, now);

        let fortify = vec![(DamageType::Fire, 137.32_f32)];
        let perks = crate::arena::combat::perks::PerkBonuses::default();
        let fortified_caster = crate::arena::combat::perks::CasterPerks {
            perks: &perks,
            magicka_full: false,
            health_critical: false,
            elem_resist_piercing: 0.0,
            elem_resist_piercing_rating: 0.0,
            element_fortify: &fortify,
        };
        let fortified = m.resolve_ability(
            spell, 1, &fortified_caster, &target(), ActiveSide::Middle, now);

        let gain = comp(&fortified, DamageType::Fire) - comp(&bare, DamageType::Fire);
        assert!(
            (gain - 137.32).abs() < 0.5,
            "Fortify must add its flat 137.32 to a SPELL, got {gain}"
        );
    }

    /// It is a FLAT add, not a multiplier. The shipped text is "Increases frost damage
    /// by {0}." with no percent sign, where `Haste` beside it reads "{0}%".
    ///
    /// The old code stored a `curve_fraction` and applied `(1.0 + f) x`. At tier 10
    /// that coincides exactly with the flat value — the weapon-enchant base and the
    /// fortify magnitude share one curve — which is why the bug survived. At tier 8 it
    /// under-paid by about a third, and with no matching weapon enchant it paid zero.
    #[test]
    fn fortify_element_is_flat_not_a_multiplier() {
        let m = RetailDamageModel;
        let now = Instant::now();
        // Two attackers differing ONLY in the size of their poison enchant track.
        let mk = |ench_tier: u8| {
            let mut a = poison_dagger();
            a.element_fortify = vec![(DamageType::Poison, 89.74)]; // tier 8
            a.enchants = vec![(DamageType::Poison, ench_tier)];
            a
        };
        let small = m.resolve_attack(
            &mk(4), &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        let large = m.resolve_attack(
            &mk(10), &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);

        // A flat add contributes the SAME amount to both. A multiplier would scale
        // with the track it multiplies, so the gap would differ.
        let base_small = comp(&small, DamageType::Poison);
        let base_large = comp(&large, DamageType::Poison);
        let mut nofort4 = mk(4);
        nofort4.element_fortify.clear();
        let mut nofort10 = mk(10);
        nofort10.element_fortify.clear();
        let plain_small = comp(&m.resolve_attack(
            &nofort4, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now), DamageType::Poison);
        let plain_large = comp(&m.resolve_attack(
            &nofort10, &target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now), DamageType::Poison);

        let gain_small = base_small - plain_small;
        let gain_large = base_large - plain_large;
        assert!(
            (gain_small - gain_large).abs() < 0.5,
            "a flat add pays the same regardless of the track it sits on: \
             {gain_small} vs {gain_large}"
        );
        assert!(
            (gain_small - 89.74).abs() < 0.5,
            "and it pays its own magnitude, got {gain_small}"
        );
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
        assert!((comp(&rd, DamageType::Poison) - 107.25).abs() < 0.5);
        let mut huge = target();
        huge.loadout.weaknesses = vec![(DamageType::Poison, 100_000.0)];
        let rd2 = m.resolve_attack(&poison_dagger(), &huge, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!((comp(&rd2, DamageType::Poison) - 114.5).abs() < 0.5, "capped at ×2");
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
        let expect = 278.85;
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
            absorb_fraction: 1.0,
            elemental_only: false,
            consumes_overflow: false,
            on_absorb_restore: (0.0, 0.0, 0.0),
            dodge_started_at: None,
            dodge_status_expires_at: None,
            dodge_effectiveness: 1.0,
            bypass_types: &[],
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
        let r1 = m.resolve_ability(gamedata::ids::FIREBALL, 1, &crate::arena::combat::perks::CasterPerks::none(), &target(), ActiveSide::Middle, now);
        let r3 = m.resolve_ability(gamedata::ids::FIREBALL, 3, &crate::arena::combat::perks::CasterPerks::none(), &target(), ActiveSide::Middle, now);
        assert_eq!(r1.source, DamageSource::Spell);
        assert!((comp(&r1, DamageType::Fire) - 73.89).abs() < 0.01, "Fireball R1 = 73.89");
        assert!((comp(&r3, DamageType::Fire) - 150.24).abs() < 0.01, "Fireball R3 = 150.24");
        // A Shock spell drains Magicka as well.
        let bolt = m.resolve_ability("7fc15804-1637-40a9-8dcc-3ea1eb0f778d", 1, &crate::arena::combat::perks::CasterPerks::none(), &target(), ActiveSide::Middle, now);
        assert!(comp(&bolt, DamageType::Shock) > 0.0);
        assert!((comp(&bolt, DamageType::Magicka) - comp(&bolt, DamageType::Shock)).abs() < 1e-3);
        // Paralyze deals Poison at its shipped 88.7 @ R1.
        let par = m.resolve_ability(gamedata::ids::PARALYZE, 1, &crate::arena::combat::perks::CasterPerks::none(), &target(), ActiveSide::Middle, now);
        assert!((comp(&par, DamageType::Poison) - 88.7).abs() < 0.01);
    }

    /// `continuousDamageBlockingEffectiveness == 1`, while resistance IS de-rated to
    /// 0.75 for continuous damage. And a StatusEffect (4) DoT is never blocked at all
    /// (`Damage$$IsBlockable@0x1bd4cc8`): it keeps the poison tick outside the
    /// guard budget, with resistance de-rated to continuous-damage effectiveness,
    /// where the direct hit loses the block budget and the full 40.
    #[test]
    fn a_status_effect_dot_is_not_blocked_and_its_resistance_is_derated() {
        assert_eq!(combat_params::CONTINUOUS_DAMAGE_BLOCKING_EFFECTIVENESS, 1.0);
        assert_eq!(combat_params::CONTINUOUS_DAMAGE_RESISTANCE_EFFECTIVENESS, 0.75);
        let m = RetailDamageModel;
        let now = Instant::now();
        let mut tgt = target();
        tgt.loadout.resistances = vec![(DamageType::Poison, 40.0)];
        tgt.loadout.has_shield = true;
        tgt.loadout.block_rating = 360.0;
        tgt.loadout.shield_optimal_block_boost = 1.0;
        tgt.set_actor_state(ActorStateType::Blocking, now);
        tgt.blocking_side = ActiveSide::Right;
        tgt.block_raised_at = Some(now);
        tgt.blocking_until = Some(now + Duration::from_secs(2));
        let dot = m.resolve_attack(&poison_dagger(), &tgt, DamageSource::StatusEffect, ActiveSide::Right, 1.0, 0, now);
        let hit = m.resolve_attack(&poison_dagger(), &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(!dot.blocked);
        assert!((comp(&dot, DamageType::Poison) - 27.25).abs() < 0.01);
        assert!(hit.blocked);
        assert!((comp(&hit, DamageType::Poison) - 0.143125).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // tracker #31: "high block" is a TIMING PHASE, not a direction
    // -----------------------------------------------------------------------

    /// A guard raised the way the wire actually raises one — `ActiveSide::Middle`,
    /// which is what propId 9 carries in 578 of 578 recorded blocking-state frames —
    /// must block a LEFT or RIGHT weapon swing HIGH.
    ///
    /// Before tracker #31 `block_outcome` also required `blocking_side == active_side`.
    /// `classify_side_from_x` never produces `Middle` for an auto-attack (0 of 6 595
    /// recorded attack hits carried it), so that gate made the optimal phase
    /// UNREACHABLE for every weapon swing in the game: no high block, no damage
    /// negation, and nothing for the attacker-stun to hang off.
    #[test]
    fn a_high_block_is_side_independent_for_a_weapon_swing() {
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let now = Instant::now();

        for swing in [ActiveSide::Left, ActiveSide::Right, ActiveSide::Middle] {
            let mut def = target();
            def.loadout.has_shield = true;
            def.loadout.shield_optimal_block_boost = 1.0;
            def.loadout.block_rating = 360.0;
            def.set_actor_state(ActorStateType::Blocking, now);
            // Exactly what `resolve.rs` sets on both block-raise paths.
            def.blocking_side = ActiveSide::Middle;
            def.block_raised_at = Some(now);
            def.blocking_until = Some(now + Duration::from_secs(2));

            let r = m.resolve_attack(&lo, &def, DamageSource::Attack, swing, 1.0, 0, now);
            let open = m.resolve_attack(&lo, &target(), DamageSource::Attack, swing, 1.0, 0, now);
            assert!(
                r.flags & flags::WAS_OPTIMAL_BLOCKING != 0,
                "{swing:?}: a Middle guard inside BLOCK_OPTIMAL_TIME must block HIGH",
            );
            // The optimal budget 360 · 2 · 1.6 · 0.1 = 115.2 comes off, not a ×0
            // (a Left/Right swing: 144 → 28.8).
            let cut = comp(&open, DamageType::Slashing) - comp(&r, DamageType::Slashing);
            assert!((cut - 115.2).abs() < 0.01, "{swing:?}: cut {cut}");
        }
    }

    /// The phase, and only the phase, decides high vs low. Same Middle guard, same
    /// Right swing — held past `BLOCK_OPTIMAL_TIME_SECS` it is LOW.
    #[test]
    fn the_same_guard_goes_low_purely_by_holding_it() {
        use crate::arena::combat::state::BLOCK_OPTIMAL_TIME_SECS;
        let m = RetailDamageModel;
        let lo = poison_dagger();
        let now = Instant::now();
        let mut def = target();
        def.loadout.has_shield = true;
        def.loadout.shield_optimal_block_boost = 1.0;
        def.loadout.block_rating = 360.0;
        def.set_actor_state(ActorStateType::Blocking, now);
        def.blocking_side = ActiveSide::Middle;
        def.block_raised_at = Some(now);
        def.blocking_until = Some(now + Duration::from_secs(8));

        let early =
            m.resolve_attack(&lo, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
        assert!(early.flags & flags::WAS_OPTIMAL_BLOCKING != 0, "held briefly → HIGH");

        let late_at = now + Duration::from_secs_f32(BLOCK_OPTIMAL_TIME_SECS + 0.1);
        let held = m.resolve_attack(
            &lo,
            &def,
            DamageSource::Attack,
            ActiveSide::Right,
            1.0,
            0,
            late_at,
        );
        // LOW: still a block, at ×1 R, and with NO wire flag (03-D5).
        assert!(held.blocked, "held too long → LOW, still blocked");
        assert_eq!(held.flags & (flags::WAS_LATE_BLOCKING | flags::WAS_OPTIMAL_BLOCKING), 0);
        assert!((comp(&held, DamageType::Slashing) - 86.4).abs() < 0.01, "144 − 57.6");
    }
}

#[cfg(test)]
mod every_cast_does_something {
    use super::*;
    use crate::arena::combat::gamedata;
    use crate::arena::combat::loadout::ability_tag_for_template;
    use crate::arena::combat::state::AbilityTag;
    use super::tests::target;

    /// **No ability a player can equip may cost a resource and do nothing.**
    ///
    /// Reported 2026-08-03 ("magic doesn't cause damage, neither do abilities") and
    /// measured on prod the same day: of 160 spell casts, **87 dealt exactly 0.0**.
    /// Every one of the six abilities players actually cast, and what it ships:
    ///
    /// | ability | tag | rank-1 data | before |
    /// |---|---|---|---|
    /// | Fireball | Damage | `damage=73.89` | worked |
    /// | IceSpike | Damage | `damage=108.83` | worked |
    /// | Frostbite | Damage | **`dps=35.51`, no `damage`** | **0.0** |
    /// | ResistElements | ResistElements | `resistance_amount=48.54` | 0 damage, correctly — it is a buff |
    /// | QuickStrikes | Maneuver | **nothing at all** | **0.0** for 150 stamina |
    /// | PiercingStrikes | Maneuver | **nothing at all** | **0.0** for 180 stamina |
    ///
    /// So this walks the WHOLE shipped ability table rather than those six, and asserts
    /// that anything routed to the direct-damage path produces a positive number. A
    /// spell that ships neither `_damage` nor `_damagePerSecond` would still be caught.
    ///
    /// Buffs (Ward / Absorb / ResistElements) and Perks are excluded — dealing zero is
    /// the correct answer for them. Maneuvers are excluded because they no longer use
    /// this path at all; they take the weapon path, which
    /// `roundtrip_s506_damage::s506_middle_maneuver_lands_in_recorded_band` pins against
    /// recorded s506 values.
    ///
    /// **A KNOWN GAP this test deliberately does NOT cover.** Sweeping the table turned
    /// up six more abilities that resolve to zero, and every one of them ships no damage
    /// number at all — they are buffs whose tag says otherwise:
    ///
    /// ```text
    ///   SnakeBite       (tag Damage)   MagickaSurge  (tag Generic)
    ///   EchoWeapon      (tag Generic)
    /// ```
    ///
    /// Plus the three `*Armor` spells — FirestormArmor, BlizzardArmor, TempestArmor —
    /// which DO ship a dps but are damage-shield AURAS: the dps burns whoever attacks
    /// the caster, for the buff's duration. Resolving that as a direct hit on the target
    /// would turn a defensive buff into a nuke, so they are skipped here and belong with
    /// the buffs above.
    ///
    /// Zero damage is arguably the RIGHT answer for a buff. What is wrong is that their
    /// buff does nothing either, and fixing that means implementing each effect — not
    /// giving them a damage number we would have to invent. Hence the `ships_damage`
    /// filter: this test pins the bug class where the data exists and the code ignored
    /// it, and does not pretend to cover the class where the data is absent.
    #[test]
    fn no_damage_ability_resolves_to_zero() {
        let m = RetailDamageModel;
        let now = Instant::now();
        let mut dead: Vec<String> = Vec::new();

        for a in gamedata::ABILITIES.iter() {
            let tag = ability_tag_for_template(a.uuid);
            if !matches!(tag, AbilityTag::Damage | AbilityTag::Paralyze | AbilityTag::Generic) {
                continue;
            }
            // Only abilities that SHIP a damage number. An ability with neither
            // `_damage` nor `_damagePerSecond` is almost always a buff whose tag is
            // wrong (see the note below) — asserting it deals damage would demand a
            // number we would have to invent.
            let ships_damage = gamedata::ability_rank_clamped(a.uuid, 1)
                .map(|r| r.damage().is_some() || r.damage_per_second().is_some())
                .unwrap_or(false);
            if !ships_damage {
                continue;
            }
            // `*Armor` spells (Firestorm / Blizzard / Tempest) are damage-shield AURAS:
            // their dps burns whoever attacks the caster, over the buff's duration. They
            // ship a damage number, so the filter above lets them through, but resolving
            // it as a direct hit on the TARGET is the wrong model entirely — it would
            // make a defensive buff a nuke. They belong with the unimplemented buffs
            // listed above, not here.
            if a.editor_name.ends_with("Armor") {
                continue;
            }
            // Rank 1 is what a freshly-equipped ability resolves at, so it is the
            // floor that matters.
            let r = m.resolve_ability(a.uuid, 1, &crate::arena::combat::perks::CasterPerks::none(), &target(), ActiveSide::Middle, now);
            if r.total <= 0.0 {
                dead.push(format!("{} ({}, tag {tag:?})", a.editor_name, a.uuid));
            }
        }

        assert!(
            dead.is_empty(),
            "these abilities are routed to the damage path but deal NOTHING — a player \
             spends the resource and sees no effect:\n  {}",
            dead.join("\n  "),
        );
    }
}
#[cfg(test)]
mod periodic_fortify_tests {
    use super::*;

    /// A channelled tick pays the Augmented perks and Fortify gear, but scaled by
    /// `continuousDamageFortifyEffectiveness * globalTickInterval` = 0.075. Paying it
    /// in full per tick would be ~13x too much; paying zero — which is what we did —
    /// was also wrong.
    #[test]
    fn a_periodic_tick_pays_a_scaled_fortify_bonus() {
        let scale = combat_params::CONTINUOUS_DAMAGE_FORTIFY_EFFECTIVENESS
            * combat_params::GLOBAL_TICK_INTERVAL;
        assert!((scale - 0.075).abs() < 1e-6, "0.75 x 0.1 = 0.075, got {scale}");

        // Augmented Flames rank 11 over a 15-tick, 3 s channel.
        let bonus = 22.5_f32;
        let per_tick = bonus * scale;
        assert!((per_tick - 1.6875).abs() < 1e-4);
        let whole_channel = per_tick * 15.0;
        assert!(
            (whole_channel - 25.3125).abs() < 1e-3,
            "a full channel adds ~25, not the 337.5 an unscaled per-tick payout gives"
        );
        assert!(whole_channel < bonus * 2.0, "and nowhere near 15x the printed value");
    }

    /// The scaling uses `_globalTickInterval` (0.1), NOT `_globalPvPTickInterval`
    /// (0.2), even though PvP ticks at 0.2. That is what the binary does and it is
    /// deliberately reproduced rather than "corrected" — see the note at the use site.
    #[test]
    fn the_scale_uses_the_global_tick_not_the_pvp_tick() {
        assert!(
            (combat_params::GLOBAL_TICK_INTERVAL - 0.1).abs() < 1e-6,
            "the fortify scale is pinned to the global tick"
        );
        assert!(
            combat_params::GLOBAL_TICK_INTERVAL < combat_params::GLOBAL_PVP_TICK_INTERVAL,
            "the two differ; using the PvP one would double the bonus"
        );
    }
}
