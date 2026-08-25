//! Perks — resolving `AbilityKind::Perk` into the bonuses the damage model reads.
//!
//! Until this module existed, `resolve.rs` matched `AbilityTag::Perk => {}`: an
//! empty arm. All 20 shipped perks were parsed into the loadout, carried a rank,
//! and then did **nothing**. A player who had spent ability points on Elemental
//! Protection or Augmented Flames got exactly the same fight as one who had not.
//!
//! Five of the 20 are out of arena scope and stay unimplemented on purpose:
//! `AdvancedTempering` (Smithy tempering cap), `LoadBearer` (inventory size), and
//! the three `*AtronachPower` perks (there are no summons in PvP). They are listed
//! explicitly in [`PerkBonuses::resolve`] so the match is exhaustive over shipped
//! data rather than silently defaulting.
//!
//! The remaining 15 are all driven from `AbilityField::BonusValue` on the equipped
//! rank — no magnitude is hard-coded here. Every number in this file's tests comes
//! from `gamedata.rs`, which is generated from the shipped asset dump.

use super::gamedata::{self, AbilityField};
use super::state::{DamageType, EquippedAbility};
use super::tables::Weight;

/// Fraction of MAX health at or below which health counts as "critical", gating
/// [`PerkBonuses::mettle`]. Shipped as `combat_parameters.criticalHealthThreshold`
/// = 35 (a percentage), so 0.35 here.
pub const CRITICAL_HEALTH_THRESHOLD: f32 = 0.35;

/// Resolved perk bonuses for one fighter, computed once when the loadout is built.
///
/// Every field is zero by default, and a zero field is a no-op at its application
/// site — so a fighter with no perks resolves to `Default` and behaves exactly as
/// before this module landed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PerkBonuses {
    /// `Augmented{Flames,Frost,Shock,Poison}` — a FLAT addition to that element's
    /// damage on a direct hit. Sparse: only elements the fighter has a perk for.
    pub element_damage: Vec<(DamageType, f32)>,
    /// `ElementalProtection` — added to Block Rating against elemental damage, and
    /// only while blocking **with a shield**.
    pub elemental_block_rating: f32,
    /// `Scout` / `Armsman` / `Barbarian` — a FLAT addition to weapon damage for
    /// light / versatile / heavy weapons respectively. Indexed by [`Weight`].
    pub weapon_damage: [f32; 3],
    /// `MatchingSet` — added to Armor Rating while all four armour slots come from
    /// one set. Already conditioned at resolve time, so this is zero unless the set
    /// actually matches.
    pub matching_set_armor: f32,
    /// `MaximumPower` — spells are this FRACTION more effective, but only when cast
    /// with magicka full. Ravage the caster's magicka and the perk is void.
    pub max_power: f32,
    /// `Mettle` — abilities are this FRACTION more effective while health is at or
    /// below [`CRITICAL_HEALTH_THRESHOLD`].
    pub mettle: f32,
    /// `CombatFocus` — added to Resistance against all damage while using an ability.
    pub combat_focus: f32,
    /// `Conservationist` ("Willpower") — added to Resistance against all damage
    /// while casting a spell.
    pub conservationist: f32,
    /// `HealingSurge` — up to this much health per second while stamina is high.
    pub healing_surge: f32,
    /// `EnchantmentSynergy` — stacked enchantments are this FRACTION more effective.
    pub enchantment_synergy: f32,
}

impl PerkBonuses {
    /// Resolve every equipped perk into its bonus, at the rank the fighter has.
    ///
    /// `armor_set` is the outcome of the matched-set test the caller has already
    /// done over the four armour slots (see [`matched_armor_set`]); passing `false`
    /// zeroes `matching_set_armor` without changing how the perk itself resolves.
    pub fn resolve(abilities: &[EquippedAbility], set_matches: bool) -> Self {
        let mut p = Self::default();

        for a in abilities {
            let Some(ability) = gamedata::ability(&a.instance_uuid) else {
                continue;
            };
            if ability.kind != gamedata::AbilityKind::Perk {
                continue;
            }
            // The rank the player actually owns. `ability_rank_clamped` pins a level
            // above `maximum_level` to the top rank rather than dropping the perk.
            let Some(value) = gamedata::ability_rank_clamped(&a.instance_uuid, a.level as u16)
                .and_then(|r| r.get(AbilityField::BonusValue))
            else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }

            match ability.editor_name {
                "AugmentedFlames" => p.add_element(DamageType::Fire, value),
                "AugmentedFrost" => p.add_element(DamageType::Frost, value),
                "AugmentedShock" => p.add_element(DamageType::Shock, value),
                "AugmentedPoison" => p.add_element(DamageType::Poison, value),

                "ElementalProtection" => p.elemental_block_rating += value,

                "Scout" => p.weapon_damage[Weight::Light as usize] += value,
                "Armsman" => p.weapon_damage[Weight::Versatile as usize] += value,
                "Barbarian" => p.weapon_damage[Weight::Heavy as usize] += value,

                "MatchingSet" => {
                    if set_matches {
                        p.matching_set_armor += value;
                    }
                }

                "MaximumPower" => p.max_power += value,
                "Mettle" => p.mettle += value,
                "CombatFocus" => p.combat_focus += value,
                "Conservationist" => p.conservationist += value,
                "HealingSurge" => p.healing_surge += value,
                "EnchantmentSynergy" => p.enchantment_synergy += value,

                // Deliberately inert in the arena — see the module note.
                "AdvancedTempering" | "LoadBearer" | "FlameAtronachPower"
                | "FrostAtronachPower" | "StormAtronachPower" => {}

                _ => {}
            }
        }

        p
    }

    fn add_element(&mut self, ty: DamageType, value: f32) {
        match self.element_damage.iter_mut().find(|(t, _)| *t == ty) {
            Some((_, v)) => *v += value,
            None => self.element_damage.push((ty, value)),
        }
    }

    /// Flat perk damage added to this element on a direct hit; 0.0 if unperked.
    pub fn element_bonus(&self, ty: DamageType) -> f32 {
        self.element_damage
            .iter()
            .find(|(t, _)| *t == ty)
            .map(|(_, v)| *v)
            .unwrap_or(0.0)
    }

    /// Flat perk damage added to a weapon of this weight class; 0.0 if unperked or
    /// if the weapon's class is unknown (bots and the starter loadout).
    pub fn weapon_bonus(&self, weight: Option<Weight>) -> f32 {
        weight.map(|w| self.weapon_damage[w as usize]).unwrap_or(0.0)
    }

    /// The Healing Surge rate at this stamina fraction.
    ///
    /// The shipped description is *"Increases Health regeneration while Stamina is
    /// high, by up to {0} per second"* — a ceiling ("up to") gated on a condition
    /// ("while Stamina is high"), with neither the ramp nor the threshold shipped as
    /// data. Modelled as a linear ramp from zero at
    /// [`HEALING_SURGE_FLOOR`] to the full rate at full stamina, which is the
    /// reading that satisfies both halves of the sentence. See the PR body: this is
    /// the one perk whose SHAPE is an assumption rather than shipped data.
    pub fn healing_surge_rate(&self, stamina_fraction: f32) -> f32 {
        if self.healing_surge <= 0.0 {
            return 0.0;
        }
        let ramp = ((stamina_fraction - HEALING_SURGE_FLOOR) / (1.0 - HEALING_SURGE_FLOOR))
            .clamp(0.0, 1.0);
        self.healing_surge * ramp
    }

    /// Multiplier on a spell's magnitude for this caster's state.
    ///
    /// Maximum Power and Mettle are independent conditions that can both hold at
    /// once (full magicka, critical health), so they stack additively — two
    /// separate perks each promising "{0}% more effective" should not multiply into
    /// more than the sum of their printed values.
    pub fn spell_multiplier(&self, magicka_full: bool, health_critical: bool) -> f32 {
        let mut m = 1.0;
        if magicka_full {
            m += self.max_power;
        }
        if health_critical {
            m += self.mettle;
        }
        m
    }

    /// Multiplier on a non-spell ability's magnitude. Maximum Power is spell-only —
    /// its shipped text says *"Spells are…"* — so only Mettle applies here.
    pub fn ability_multiplier(&self, health_critical: bool) -> f32 {
        if health_critical {
            1.0 + self.mettle
        } else {
            1.0
        }
    }

    /// True when this fighter has any perk at all, i.e. anything to apply.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Floor on the "while using an ability" window that Combat Focus and Willpower
/// cover, in seconds.
///
/// The window itself is the ability's shipped `_castingDelay` + `_channelDuration`.
/// Many abilities ship neither — they resolve instantly — and a zero-length window
/// would make both perks unreachable for exactly the abilities a player spams. This
/// floor stands in for the attack animation, which is not shipped as data. It is an
/// ASSUMPTION; see the PR body.
pub const ABILITY_USE_MIN_WINDOW_SECS: f32 = 0.5;

/// Stamina fraction below which Healing Surge contributes nothing. See
/// [`PerkBonuses::healing_surge_rate`] — an assumption, not shipped data.
pub const HEALING_SURGE_FLOOR: f32 = 0.5;

/// `equipment_slot` codes for the four armour pieces Matching Set requires:
/// 1 helmet, 3 armor, 4 gauntlets, 7 boots. Verified against all 254 shipped
/// `ArmorStats` entries — those are the only four codes that occur.
pub const MATCHING_SET_SLOTS: [u8; 4] = [1, 3, 4, 7];

/// Does this collection of equipped armour form a matched set?
///
/// Retail's condition is *"while wearing a matched set of armor (armor, helmet,
/// gauntlets, and boots)"* — so all four slots must be filled AND agree on
/// `armor_set`. `armor_set == 0` is the "belongs to no set" marker (farmer clothes
/// and other one-offs carry it), so four unset pieces are NOT a matched set.
pub fn matched_armor_set(pieces: &[(u8, u8)]) -> bool {
    let mut set_id: Option<u8> = None;
    for slot in MATCHING_SET_SLOTS {
        let Some((_, s)) = pieces.iter().find(|(sl, _)| *sl == slot) else {
            return false;
        };
        if *s == 0 {
            return false;
        }
        match set_id {
            None => set_id = Some(*s),
            Some(prev) if prev == *s => {}
            Some(_) => return false,
        }
    }
    set_id.is_some()
}

/// A perkless fighter, for call sites with no caster (tests, unknown attackers).
/// Every application site treats this exactly as the pre-perk engine behaved.
pub static NO_PERKS: PerkBonuses = PerkBonuses {
    element_damage: Vec::new(),
    elemental_block_rating: 0.0,
    weapon_damage: [0.0; 3],
    matching_set_armor: 0.0,
    max_power: 0.0,
    mettle: 0.0,
    combat_focus: 0.0,
    conservationist: 0.0,
    healing_surge: 0.0,
    enchantment_synergy: 0.0,
};

/// The caster-side state a magnitude perk is conditioned on.
///
/// `resolve_ability` used to take no attacker at all — it passed
/// `&Loadout::default()` into `finish_resolved`, so a spell could not see who cast
/// it. Rather than thread the whole loadout (which would silently switch on the
/// caster's piercing ratings for spells too, a much larger change than perks),
/// this carries only what the perks need.
#[derive(Debug, Clone, Copy)]
pub struct CasterPerks<'a> {
    pub perks: &'a PerkBonuses,
    /// Magicka is at maximum — the Maximum Power condition.
    pub magicka_full: bool,
    /// Health is at or below [`CRITICAL_HEALTH_THRESHOLD`] — the Mettle condition.
    pub health_critical: bool,
    /// The caster's **Elemental Damage Ignores Resistance** gear, carried here because
    /// the spell damage path has no other view of the caster's loadout.
    ///
    /// It used to have none, so EDIR did nothing on elemental SPELLS — the one place
    /// its own text promises it works. The weapon path honoured it the whole time.
    pub elem_resist_piercing: f32,
    pub elem_resist_piercing_rating: f32,
    /// The caster's `Fortify <Element> Damage` gear. A slice, not a Vec, because
    /// `CasterPerks` is `Copy`.
    pub element_fortify: &'a [(super::state::DamageType, f32)],
}

impl CasterPerks<'static> {
    /// No caster, no perks, no conditions met.
    pub fn none() -> Self {
        CasterPerks {
            perks: &NO_PERKS,
            magicka_full: false,
            health_critical: false,
            elem_resist_piercing: 0.0,
            elem_resist_piercing_rating: 0.0,
            element_fortify: &[],
        }
    }
}

impl<'a> CasterPerks<'a> {
    /// Read the caster's perks and live conditions off the fighter.
    pub fn of(f: &'a super::state::Fighter) -> Self {
        CasterPerks {
            perks: &f.loadout.perks,
            magicka_full: f.magicka >= f.max_magicka,
            health_critical: health_is_critical(f.health, f.max_health),
            elem_resist_piercing: f.loadout.elem_resist_piercing,
            elem_resist_piercing_rating: f.loadout.elem_resist_piercing_rating,
            element_fortify: &f.loadout.element_fortify,
        }
    }
}

/// Health at or below [`CRITICAL_HEALTH_THRESHOLD`] of maximum. Compared without
/// dividing so a zero-max fighter cannot produce a NaN.
pub fn health_is_critical(health: u32, max_health: u32) -> bool {
    max_health > 0 && (health as f32) <= max_health as f32 * CRITICAL_HEALTH_THRESHOLD
}

impl CasterPerks<'_> {
    /// Magnitude multiplier for an ability of this kind, given the caster's state.
    pub fn magnitude_multiplier(&self, is_spell: bool) -> f32 {
        if is_spell {
            self.perks.spell_multiplier(self.magicka_full, self.health_critical)
        } else {
            self.perks.ability_multiplier(self.health_critical)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::combat::state::AbilityTag;

    fn perk(uuid: &str, level: u8) -> EquippedAbility {
        EquippedAbility {
            instance_uuid: uuid.to_string(),
            level,
            tag: AbilityTag::Perk,
        }
    }

    const AUGMENTED_FLAMES: &str = "ed235f8d-0648-4aee-b955-a951562f549d";
    const ELEMENTAL_PROTECTION: &str = "788aa75e-4796-4d57-bbab-b1b901623f16";
    const BARBARIAN: &str = "64a6a981-0dc8-4fc1-b043-a75d052b00f5";
    const MAXIMUM_POWER: &str = "83784ade-533e-4965-a540-05bfd4f056d8";
    const METTLE: &str = "d6d7ad89-0c41-410f-8a19-c4850ab9fe4f";
    const HEALING_SURGE: &str = "09aa3390-8f42-4cd5-a88c-5c94d5e1dd29";
    const MATCHING_SET: &str = "3dcb91c5-2279-4003-b6a6-53eac6fb86c8";
    const LOAD_BEARER: &str = "7f0c9202-2130-4376-aa17-890c25040e7b";

    /// The values below are the shipped rank tables. If `gamedata.rs` is regenerated
    /// and a magnitude moves, this test is SUPPOSED to fail — it is the pin between
    /// the asset dump and every application site.
    #[test]
    fn perk_ranks_resolve_to_their_shipped_values() {
        let p = PerkBonuses::resolve(&[perk(AUGMENTED_FLAMES, 1)], false);
        assert_eq!(p.element_bonus(DamageType::Fire), 5.04);

        let p = PerkBonuses::resolve(&[perk(AUGMENTED_FLAMES, 11)], false);
        assert_eq!(p.element_bonus(DamageType::Fire), 22.5);

        let p = PerkBonuses::resolve(&[perk(ELEMENTAL_PROTECTION, 11)], false);
        assert_eq!(p.elemental_block_rating, 171.0);

        let p = PerkBonuses::resolve(&[perk(BARBARIAN, 11)], false);
        assert_eq!(p.weapon_bonus(Some(Weight::Heavy)), 28.34);
        // …and nothing for the classes Barbarian does not cover.
        assert_eq!(p.weapon_bonus(Some(Weight::Light)), 0.0);
        assert_eq!(p.weapon_bonus(Some(Weight::Versatile)), 0.0);
        assert_eq!(p.weapon_bonus(None), 0.0);
    }

    /// A level above the perk's `maximum_level` pins to the top rank. Ranks come from
    /// gear bonuses that can exceed the purchasable ceiling, so this is reachable.
    #[test]
    fn an_over_max_rank_pins_to_the_top_rank_instead_of_dropping_the_perk() {
        let top = PerkBonuses::resolve(&[perk(MAXIMUM_POWER, 6)], false);
        let over = PerkBonuses::resolve(&[perk(MAXIMUM_POWER, 99)], false);
        assert_eq!(top.max_power, 0.4);
        assert_eq!(over.max_power, top.max_power);
    }

    #[test]
    fn out_of_scope_perks_resolve_to_nothing() {
        let p = PerkBonuses::resolve(&[perk(LOAD_BEARER, 9)], true);
        assert!(p.is_empty(), "LoadBearer is inventory size, not combat: {p:?}");
    }

    /// Maximum Power is CONDITIONAL on full magicka — the owner's recollection was
    /// "if ravaged, then MP is void", and the shipped text agrees.
    #[test]
    fn maximum_power_is_void_unless_magicka_is_full() {
        let p = PerkBonuses::resolve(&[perk(MAXIMUM_POWER, 6)], false);
        assert_eq!(p.spell_multiplier(true, false), 1.4);
        assert_eq!(p.spell_multiplier(false, false), 1.0);
    }

    /// Mettle applies to abilities generally; Maximum Power does not — it is
    /// spell-only. This is the test that separates the two paths.
    #[test]
    fn mettle_applies_to_abilities_but_maximum_power_does_not() {
        let p = PerkBonuses::resolve(&[perk(MAXIMUM_POWER, 6), perk(METTLE, 6)], false);
        // A maneuver at critical health: Mettle only.
        assert_eq!(p.ability_multiplier(true), 1.45);
        assert_eq!(p.ability_multiplier(false), 1.0);
        // A spell at full magicka AND critical health: both, added not multiplied.
        assert!((p.spell_multiplier(true, true) - 1.85).abs() < 1e-5);
    }

    #[test]
    fn healing_surge_ramps_with_stamina_and_is_zero_when_low() {
        let p = PerkBonuses::resolve(&[perk(HEALING_SURGE, 8)], false);
        assert_eq!(p.healing_surge, 15.4);
        assert_eq!(p.healing_surge_rate(1.0), 15.4);
        assert_eq!(p.healing_surge_rate(0.5), 0.0);
        assert_eq!(p.healing_surge_rate(0.0), 0.0);
        assert!((p.healing_surge_rate(0.75) - 7.7).abs() < 1e-4);
        // Without the perk there is no regen contribution at any stamina.
        let none = PerkBonuses::default();
        assert_eq!(none.healing_surge_rate(1.0), 0.0);
    }

    #[test]
    fn matching_set_needs_all_four_slots_from_one_set() {
        // armor_set 5 in every one of the four slots.
        let full: Vec<(u8, u8)> = MATCHING_SET_SLOTS.iter().map(|s| (*s, 5u8)).collect();
        assert!(matched_armor_set(&full));

        // One piece from a different set breaks it.
        let mut mixed = full.clone();
        mixed[2].1 = 6;
        assert!(!matched_armor_set(&mixed));

        // A missing slot breaks it.
        assert!(!matched_armor_set(&full[..3]));

        // Four setless pieces are not a set.
        let setless: Vec<(u8, u8)> = MATCHING_SET_SLOTS.iter().map(|s| (*s, 0u8)).collect();
        assert!(!matched_armor_set(&setless));
    }

    /// The perk only pays out when the set actually matches — the condition is
    /// applied at resolve time, so no application site has to re-check it.
    #[test]
    fn matching_set_pays_nothing_without_a_matched_set() {
        assert_eq!(
            PerkBonuses::resolve(&[perk(MATCHING_SET, 9)], true).matching_set_armor,
            141.0
        );
        assert_eq!(
            PerkBonuses::resolve(&[perk(MATCHING_SET, 9)], false).matching_set_armor,
            0.0
        );
    }

    /// Two ranks of the same perk from different sources add. Guards against a
    /// `=` typo where `+=` is meant, which no single-perk test would catch.
    #[test]
    fn duplicate_perk_entries_accumulate() {
        let p = PerkBonuses::resolve(&[perk(AUGMENTED_FLAMES, 1), perk(AUGMENTED_FLAMES, 1)], false);
        assert_eq!(p.element_bonus(DamageType::Fire), 10.08);
    }

    /// A fighter with no perks must resolve to exactly `Default`, so that every
    /// application site is a provable no-op for unperked players.
    #[test]
    fn no_perks_is_the_default() {
        assert!(PerkBonuses::resolve(&[], true).is_empty());
    }
}
