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
    /// Attacker's flat block piercing, subtracted from `rating` in
    /// [`BlockOutcome::factor_for`] — physical and elemental respectively.
    pub block_piercing: f32,
    pub elem_block_piercing: f32,
    /// `ElementalProtection` perk — added to `rating` for ELEMENTAL damage only,
    /// and only when the block is made with a shield. Zero for an unperked or
    /// shieldless defender, which makes the elemental branch of `factor_for`
    /// byte-identical to what it was before the perk existed.
    pub elem_rating_bonus: f32,
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
            // Block piercing cuts the rating, exactly as armor piercing cuts armor.
            let pierced = (self.rating - self.block_piercing).max(0.0);
            return 1.0 - tables::block_reduction(pierced, true);
        }
        if is_elemental(ty) {
            let pierced =
                (self.rating + self.elem_rating_bonus - self.elem_block_piercing).max(0.0);
            return 1.0 - tables::block_reduction(pierced, false);
        }
        // Stamina/Magicka drains and raw Health are not blocked. This stays at 1.0
        // ON PURPOSE for the drains: they are derived in [`append_mirrored_drains`]
        // from the element's ALREADY-blocked value, so the reduction is baked in and
        // applying the factor here as well would mitigate the drain twice.
        1.0
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
/// `attacker` supplies the flat block-piercing ratings. **Additive:** they are 0.0 on
/// every loadout unless an ability sets them, so a hit with no piercing produces the
/// same numbers as before this parameter existed — which the s506 block differentials
/// prove.
///
/// STATED LIMIT: piercing reduces the RATING, so it weakens a LATE block. A connected
/// OPTIMAL block still negates physical outright (`factor_for` returns 0.0 before the
/// rating is consulted) — that zero is capture-pinned by
/// `roundtrip_s506_damage::s506_optimal_block_negates_physical_halves_elemental`.
/// Whether retail lets block piercing through an optimal block is NOT pinned by any
/// capture we hold, so this leaves the pinned behaviour alone rather than guessing.
pub fn block_outcome(
    target: &Fighter,
    attacker: &Loadout,
    active_side: ActiveSide,
    now: Instant,
) -> BlockOutcome {
    use super::state::ActorStateType;
    let none = BlockOutcome {
        flag: 0,
        optimal: false,
        blocking: false,
        rating: 0.0,
        block_piercing: 0.0,
        elem_block_piercing: 0.0,
        elem_rating_bonus: 0.0,
    };
    if target.actor_state() != ActorStateType::Blocking || active_side == ActiveSide::None {
        return none;
    }
    let Some(phase) = target.block_phase(now) else {
        return none;
    };
    // Timing only — see the note above. `target.blocking_side` is deliberately NOT
    // consulted: it is the wire-visible facing of the guard animation
    // (`PlayerBlockingState.Parameters.ActiveSide`), not a hit-test.
    let optimal = matches!(phase, BlockPhase::Optimal);
    BlockOutcome {
        flag: if optimal { flags::WAS_OPTIMAL_BLOCKING } else { flags::WAS_LATE_BLOCKING },
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
    /// The attacker's per-type PHYSICAL base **after** the defender's Armor Rating,
    /// before the swing/combo factor. [Phase 3.3 — see the module doc for why armor
    /// lands here and not after the multiplier.]
    fn physical_base_after_armor(attacker: &Loadout, target: &Fighter) -> Vec<(DamageType, f32)> {
        let armor_rating = (target.loadout.armor_rating - attacker.armor_piercing_rating).max(0.0);

        // Scout / Armsman / Barbarian add flat damage for LIGHT / VERSATILE / HEAVY
        // weapons. It rides on the weapon's own damage, so it is added BEFORE armour
        // and mitigated with it — a perk should not be a hole in the armour model.
        // Applied to the FIRST physical component only: the perk is "+{0} Damage
        // with <class> weapons", one bonus per swing, not one per damage type.
        let mut weapon_bonus = attacker.perks.weapon_bonus(attacker.weapon.weight);

        attacker
            .weapon
            .base_by_type
            .iter()
            .map(|(ty, base)| {
                let mut base = *base;
                if is_physical(*ty) && weapon_bonus > 0.0 {
                    base += weapon_bonus;
                    weapon_bonus = 0.0;
                }
                let cut = if is_physical(*ty) {
                    tables::armor_reduction(base, armor_rating)
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
        // §4.3). The magnitude is the family's own shipped curve value.
        //
        // The mirrored stat drain is NOT appended here: it mirrors the element
        // *after* mitigation and is derived in [`append_mirrored_drains`], which
        // `finish_resolved` calls once the block factor has been applied.
        // ENCHANTMENT SYNERGY — "Stacked enchantments are {0}% more effective."
        // "Stacked" is the same element carried by more than one equipped
        // enchantment; a lone enchantment is not stacked and gets nothing.
        let synergy = attacker.perks.enchantment_synergy;
        for (ench_ty, magnitude) in enchant_tracks(attacker) {
            let amp = target.element_amp_for(ench_ty) * (1.0 + fortify_for(attacker, ench_ty));
            let stacked = synergy > 0.0
                && attacker.enchants.iter().filter(|(t, _)| *t == ench_ty).count() > 1;
            let synergy_mult = if stacked { 1.0 + synergy } else { 1.0 };
            components.push((ench_ty, magnitude * amp * synergy_mult));
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
/// The captures say otherwise, and they are the same captures this model was built
/// from: 118 `ContinuousSpell` frames across s615+s616 carry per-tick magnitudes of
/// 39.33 - 45.00, matching `dps x 0.2` with nothing subtracted. Under the old model a
/// defender with a t4 resist could not have produced a 44.997 tick — the ceiling was
/// 2.25.
///
/// So the flat cost is charged ONCE PER CAST, spread across the ticks: a full channel
/// loses exactly `rating`, the same as a single hit of the same total would. This
/// mirrors the rule already applied to flat BONUSES, which are paid only on
/// `single_impact` — the asymmetry was that the code refused to pay a flat bonus per
/// tick while still charging a flat penalty per tick.
///
/// 1.0 for everything else, so every capture-pinned single-hit test is untouched.
fn resistance_scale_for(source: DamageSource, ability_uuid: &str, ability_level: u8) -> f32 {
    if source != DamageSource::ContinuousSpell {
        return 1.0;
    }
    match channel_ticks(ability_uuid, ability_level) {
        Some(t) if t > 1 => 1.0 / t as f32,
        _ => 1.0,
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
    // What share of the defender's flat resistance THIS call should charge. 1.0
    // everywhere except a channelled-spell tick, which charges 1/ticks so the whole
    // channel pays the resistance ONCE. See `resistance_scale_for`.
    resistance_scale: f32,
) -> ResolvedDamage {
    let mut hit_flags = flags::SHOW_DAMAGE | flags::HAS_ATTACKER;
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
    let single_impact = !continuous && source != DamageSource::ContinuousSpell;
    if single_impact && !attacker.perks.element_damage.is_empty() {
        for (ty, v) in components.iter_mut() {
            if is_elemental(*ty) && *v > 0.0 {
                *v += attacker.perks.element_bonus(*ty);
            }
        }
    }

    // 1) BLOCK — a fraction from the defender's Block Rating. NOT de-rated against
    //    continuous damage (`continuousDamageBlockingEffectiveness == 1`).
    let block = block_outcome(target, attacker, active_side, now);
    hit_flags |= block.flag;
    for (ty, v) in components.iter_mut() {
        *v *= block.factor_for(*ty);
    }

    // 1.5) MIRRORED STAT DRAIN — Frost→Stamina / Shock→Magicka, 1:1 with the
    //      element's **post-block** value. This lands HERE, after step 1, because
    //      retail's drain follows the reduced element rather than the raw roll.
    append_mirrored_drains(components);

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
        let resisted =
            tables::resistance_reduction(before, rating * resistance_scale, continuous);
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

    /// An un-armored, un-blocking L100 target.
    pub(super) fn target() -> Fighter {
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
        def.set_actor_state(ActorStateType::Blocking, now);
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

        // LATE: the plain (un-doubled) rating applies to BOTH categories.
        // Forced by TIMING, not by a side mismatch — tracker #31 removed the side gate
        // (high/low is a phase, never a direction), so this re-raises the guard inside
        // the `OPTIMAL_BLOCK_RECOVERY_SECS` cooldown, which is a real way to be LATE.
        let mut late_t = def.clone();
        late_t.last_block_dropped_at = Some(now);
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
                absorb_fraction: 1.0,
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
        tgt.set_actor_state(ActorStateType::Blocking, now);
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
            def.loadout.block_rating = 379.5;
            def.set_actor_state(ActorStateType::Blocking, now);
            // Exactly what `resolve.rs` sets on both block-raise paths.
            def.blocking_side = ActiveSide::Middle;
            def.block_raised_at = Some(now);
            def.blocking_until = Some(now + Duration::from_secs(2));

            let r = m.resolve_attack(&lo, &def, DamageSource::Attack, swing, 1.0, 0, now);
            assert!(
                r.flags & flags::WAS_OPTIMAL_BLOCKING != 0,
                "{swing:?}: a Middle guard inside BLOCK_OPTIMAL_TIME must block HIGH",
            );
            assert_eq!(
                comp(&r, DamageType::Slashing),
                0.0,
                "{swing:?}: a high block negates physical",
            );
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
        def.loadout.block_rating = 379.5;
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
        assert!(held.flags & flags::WAS_LATE_BLOCKING != 0, "held too long → LOW");
        assert!(
            comp(&held, DamageType::Slashing) > 0.0,
            "a low block only reduces, it does not negate",
        );
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
