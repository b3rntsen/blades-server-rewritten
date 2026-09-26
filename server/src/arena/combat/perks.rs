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

/// Fraction of the **un-multiplied** max health at or below which health counts as
/// "critical", gating [`PerkBonuses::mettle`]. Shipped as
/// `combat_parameters.criticalHealthThreshold` = 35 (a percentage), so 0.35 here.
///
/// The arena's ×3 health bar does NOT scale this window: see [`health_is_critical`].
pub const CRITICAL_HEALTH_THRESHOLD: f32 =
    super::gamedata::combat_params::CRITICAL_HEALTH_THRESHOLD / 100.0;

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
    /// `Mettle` — a maneuver's flat bonus damage is this FRACTION larger while
    /// health is critical (see [`health_is_critical`]).
    pub mettle: f32,
    /// `CombatFocus` — added to Resistance against all damage while the fighter is
    /// performing a MANEUVER or has Reckless Fury up. Never during a spell.
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
    /// Resolve every perk in `abilities` into its bonus, at the rank given.
    ///
    /// For a real character the caller passes the LEARNED perks
    /// ([`super::loadout`]'s `learned_perks`), not the six equip slots: the client
    /// registers every learned perk (`LearnedAbilitiesHandler$$RegisterAbility`
    /// @0x1A89724, iff `_abilityType == Perk`) and its ability menu has no perk slot
    /// at all. Enemy-only perks (the three Atronach Powers) are skipped here too.
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
            if ability.kind != gamedata::AbilityKind::Perk || ability.enemy_only {
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
    /// *"Increases Health regeneration while Stamina is high, by up to {0} per
    /// second"*. The shape is read from the client, not assumed:
    /// `HealingSurgePerk$$GetRegenerationBonus@0x1A7DFBC` returns
    /// `_bonusValue × powf(Stamina.BoundedPercent, 7.0)` (`bl powf` at 0x1A7E06C
    /// with `s1 = 7.0`) — a 7th-power curve with no floor. 100 % stamina pays 1.0×,
    /// 90 % 0.48×, 75 % 0.13×, 50 % 0.008×.
    ///
    /// This replaces a linear ramp from 50 % that was a guess (it paid 0.50× at 75 %
    /// stamina, nearly 4× the client).
    pub fn healing_surge_rate(&self, stamina_fraction: f32) -> f32 {
        if self.healing_surge <= 0.0 {
            return 0.0;
        }
        self.healing_surge * stamina_fraction.clamp(0.0, 1.0).powi(7)
    }

    /// Multiplier on a spell's magnitude for this caster's state.
    ///
    /// Only **Maximum Power** applies to a spell. Its shipped text is *"Spells are
    /// {0}% more effective when cast while Magicka is full"*, and
    /// `MaximumPowerPerk.GetSpellBonus` returns 0 unless `AbilityType == Spell(1)`.
    ///
    /// **Mettle used to be added here and that was wrong.** Its description says
    /// "Abilities", which reads as everything, but `MettlePerk.GetManeuverBonus`
    /// opens with `cmp w2, 2` — `AbilityType.Maneuver` — and returns 0.0 for anything
    /// else. The loose description is copy; the code is maneuver-only. Disassembled
    /// from `libil2cpp.so` (RVA 0x1A22F74) rather than inferred.
    ///
    /// `health_critical` is kept in the signature so every caller still states the
    /// condition it evaluated; it simply no longer affects a SPELL.
    pub fn spell_multiplier(&self, magicka_full: bool, _health_critical: bool) -> f32 {
        if magicka_full {
            1.0 + self.max_power
        } else {
            1.0
        }
    }

    /// Mettle's effectiveness multiplier for a **maneuver** — Mettle's actual scope.
    ///
    /// The client stores this as `ManeuverParameters._effectivenessMultiplier`, and
    /// it multiplies only `_bonusDamage × grip` (`get_OneHandedMultiplier` /
    /// `get_TwoHandedMultiplier` → `DistributeBonusDamage@0x1A21844`), never the
    /// weapon's base damage. The caller applies it to the maneuver's flat bonus.
    ///
    /// Maximum Power is spell-only and never applies here.
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
            // Maximum Power needs a FULL magicka pool, and "full" means the pool's
            // TRUE ceiling — `max_magicka + ravaged_magicka`, not the ceiling ravage
            // has left behind. Once Ravage Magicka lands, the pool cannot reach it
            // again inside that round, so the perk is void for the rest of the round.
            //
            // Comparing against the ravaged ceiling instead would let the victim
            // refill to the reduced maximum and keep the perk, which is backwards:
            // ravaging an opponent's magicka is precisely how Maximum Power is denied
            // (a Max-Power Ice Spike can stun through a Stahlrim shield, and ravage is
            // the counter). Not measurable from captures — ravage is absent from the
            // wire entirely (docs/arena-ravage.md) — so this follows the owner's
            // reading, which the perk's own shipped text already agreed with.
            magicka_full: magicka_full_for_maximum_power(f),
            health_critical: fighter_health_is_critical(f),
            elem_resist_piercing: f.loadout.elem_resist_piercing,
            elem_resist_piercing_rating: f.loadout.elem_resist_piercing_rating,
            element_fortify: &f.loadout.element_fortify,
        }
    }
}

/// Mettle's critical-health test, as the client computes it.
///
/// `Actor$$get_IsAtCriticalHealth@0x1C5C2BC`:
/// `health.BoundedPercent × 100 ≤ criticalHealthThreshold / GetMultiplierForStat(Health)`.
/// In the arena that multiplier is the PvP health multiplier
/// (`PvpOpponentActor$$InitStats` → `SetMaximumHealthMultiplier`), i.e.
/// [`ARENA_HEALTH_MULTIPLIER`](super::state::ARENA_HEALTH_MULTIPLIER). So
/// "critical" is 35 % of the UN-multiplied maximum — ≈ 11.7 % of the tripled bar —
/// not 35 % of the tripled bar, which opened Mettle's window at 3× the HP.
///
/// `max_health` is the pool's FULL maximum (the multiplied bar, ravage not
/// subtracted), because `BoundedPercent` reads against the full `Maximum`.
/// Compared without dividing by it so a zero-max fighter cannot produce a NaN.
pub fn health_is_critical(health: u32, max_health: u32) -> bool {
    let multiplier = super::state::ARENA_HEALTH_MULTIPLIER.max(1) as f32;
    max_health > 0
        && (health as f32) * multiplier <= max_health as f32 * CRITICAL_HEALTH_THRESHOLD
}

/// [`health_is_critical`] for a fighter, against the pool's full maximum
/// (`max_health`; ravage is tracked as a destroyed portion, not a smaller maximum).
pub fn fighter_health_is_critical(f: &super::state::Fighter) -> bool {
    health_is_critical(f.health, f.max_health)
}

/// Maximum Power's condition: the magicka pool reads FULL.
///
/// `MaximumPowerPerk$$GetSpellBonus@0x1A22908` requires
/// `Magicka.BoundedPercent ≥ 1.0`, and `BoundedPercent` is measured against the full
/// `Maximum` while Ravage only raises `DestroyedPortion` and caps the value at
/// `DamagedMaximum` (combat-spec ch. 11 §1.1, read). So a ravaged pool can never
/// read full: the perk is void while ravaged.
///
/// This is the ONE place that condition lives. `resolve_ability_cast` used to
/// compute its own `magicka >= max_magicka` against the ravage-LOWERED ceiling and
/// override [`CasterPerks::of`], so the ravage-aware test here was dead for casts.
pub fn magicka_full_for_maximum_power(f: &super::state::Fighter) -> bool {
    f.magicka >= f.max_magicka
}

impl CasterPerks<'_> {
    /// Magnitude multiplier on an ability's WHOLE base damage, given the caster's
    /// state.
    ///
    /// Only Maximum Power scales a whole base, and only a spell's. Mettle is not
    /// applied here: it scales a maneuver's flat `_bonusDamage × grip` alone
    /// (combat-spec ch. 07 §7, D9), which `resolve.rs`'s maneuver arm folds into
    /// `Loadout::maneuver_bonus_damage` before the hit resolves. Maneuvers do not
    /// come through `resolve_ability` at all, so a non-spell here has no bonus term
    /// for Mettle to scale.
    pub fn magnitude_multiplier(&self, is_spell: bool) -> f32 {
        if is_spell {
            self.perks.spell_multiplier(self.magicka_full, self.health_critical)
        } else {
            1.0
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

    /// The two perks are DISJOINT: Mettle is maneuvers-only, Maximum Power is
    /// spells-only. Neither ever applies to the other's ability type.
    ///
    /// This test previously asserted that a spell at full magicka AND critical health
    /// got BOTH (×1.85). That was wrong. Mettle's description says "Abilities", which
    /// reads as everything, but `MettlePerk.GetManeuverBonus` opens with
    /// `cmp w2, 2` — `AbilityType.Maneuver` — and returns 0.0 for anything else
    /// (disassembled from libil2cpp.so, RVA 0x1A22F74). The description is loose copy.
    #[test]
    fn mettle_is_maneuver_only_and_maximum_power_is_spell_only() {
        let p = PerkBonuses::resolve(&[perk(MAXIMUM_POWER, 6), perk(METTLE, 6)], false);
        // A maneuver at critical health: Mettle only.
        assert_eq!(p.ability_multiplier(true), 1.45);
        assert_eq!(p.ability_multiplier(false), 1.0);
        // A spell at full magicka AND critical health: Maximum Power ONLY — Mettle
        // contributes nothing to a spell however critical the caster's health is.
        assert!((p.spell_multiplier(true, true) - 1.40).abs() < 1e-5);
        assert!((p.spell_multiplier(true, false) - 1.40).abs() < 1e-5);
        assert_eq!(
            p.spell_multiplier(false, true),
            1.0,
            "critical health alone must do nothing to a spell"
        );
    }

    /// `HealingSurgePerk$$GetRegenerationBonus@0x1A7DFBC`: `bonus × stamina^7`.
    ///
    /// Expected values are the client's curve evaluated by hand
    /// (15.4 × 0.75^7 = 15.4 × 0.13348 = 2.0556; 15.4 × 0.5^7 = 15.4 / 128 = 0.1203),
    /// not by calling the code under test.
    #[test]
    fn healing_surge_follows_the_clients_seventh_power_stamina_curve() {
        let p = PerkBonuses::resolve(&[perk(HEALING_SURGE, 8)], false);
        assert_eq!(p.healing_surge, 15.4);
        assert_eq!(p.healing_surge_rate(1.0), 15.4);
        assert!((p.healing_surge_rate(0.75) - 2.0556).abs() < 1e-3);
        // No floor: half stamina still pays a sliver (the old linear ramp paid 0 here
        // and 7.7 at 75 %).
        assert!((p.healing_surge_rate(0.5) - 0.1203).abs() < 1e-3);
        assert!(p.healing_surge_rate(0.5) > 0.0);
        assert_eq!(p.healing_surge_rate(0.0), 0.0);
        // Out-of-range fractions clamp rather than extrapolate.
        assert_eq!(p.healing_surge_rate(1.5), 15.4);
        // Control: without the perk there is no regen contribution at any stamina.
        let none = PerkBonuses::default();
        assert_eq!(none.healing_surge_rate(1.0), 0.0);
    }

    /// `get_IsAtCriticalHealth@0x1C5C2BC`: `hp% x 100 <= 35 / healthMultiplier`.
    /// On a tripled 3000-HP bar (1000 un-multiplied) critical is <= 350 HP, i.e.
    /// 35 % of the un-multiplied 1000 — not <= 1050 (35 % of the tripled bar).
    #[test]
    fn critical_health_is_35_percent_of_the_unmultiplied_bar() {
        assert_eq!(crate::arena::combat::state::ARENA_HEALTH_MULTIPLIER, 3, "fixture assumes x3");
        assert!(health_is_critical(350, 3000));
        assert!(!health_is_critical(351, 3000));
        // The old gate: 1050 of 3000 counted as critical. It must not now.
        assert!(!health_is_critical(1050, 3000));
        assert!(!health_is_critical(0, 0), "a zero-max pool is never critical");
    }

    /// The Atronach Powers are `enemy_only` and must not resolve for a player even
    /// if one appears in a learned map.
    #[test]
    fn enemy_only_perks_are_skipped() {
        const FLAME_ATRONACH_POWER: &str = "9bc43c3d-4eb7-4d9c-b507-b3288f3b9ea1";
        assert!(gamedata::ability(FLAME_ATRONACH_POWER).unwrap().enemy_only);
        assert!(PerkBonuses::resolve(&[perk(FLAME_ATRONACH_POWER, 1)], true).is_empty());
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
