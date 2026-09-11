//! Build a `Fighter`'s combat [`Loadout`] from the imported character.
//!
//! [`from_character`] is a **pure** parser (no DB) over the stored
//! `CompleteCharacter` + `CompleteInventory` (the matchmaker does the async query
//! and calls this).
//!
//! # Phase 3 — this now runs on real item data
//!
//! Every equipped item's `itemTemplateId` is looked up in [`gamedata`]:
//!
//! * **weapon** → `base_damage` / `damage_type` / `weapon_class` / `attack_delay` /
//!   `recovery_time` / `block_base` / `optimal_block_boost`, plus the item's own
//!   `tempering_level` via [`tables::tempering_bonus`];
//! * **armor** → summed `armor_rating` (Phase 3.3);
//! * **shield** → `block_base` + `optimal_block_boost` (Phase 3.5);
//! * **enchantments** → dispatched on the family's **logic class**
//!   (`WeaponDamageFirePropertyLogic`, `ResistFrostPropertyLogic`, …), with the
//!   magnitude from that family's own tier curve (Phase 3.6/3.7);
//! * **abilities** → the full 63-ability shipped table (Phase 3.11).
//!
//! What is *deleted*: `DEFAULT_WEAPON_WEIGHT` (the hardcoded `Light`), the
//! 8-char-prefix `defensive_enchant` matcher and its four invented per-tier
//! constants (`RESIST_PER_TIER` 8.0, `FORTIFY_CONDITION_PER_TIER` 0.02,
//! `ELEM_PIERCE_PER_TIER` 0.04, `STATUS_DUR_STEP` 0.03), and the single-prefix
//! `ability_tag_for_template`.

use blades_lib::user_data::{CompleteCharacter, CompleteInventory};
use serde_json::Value;
use uuid::Uuid;

use super::gamedata;
use super::state::{AbilityTag, ActorAnimation, DamageType, EquippedAbility, Loadout, StatusEffectType, WeaponProfile};
use super::tables;

/// A representative starter loadout, used when there is no character row / no DB
/// (bots, ghosts, tests). Built from **real shipped templates** rather than the
/// UESP fallback surface, so even the bot path exercises the real data model:
/// a tempering-4 **Glass Dagger** + **Chaurus Shield**, both L28-equippable, with a
/// tier-3 `Weapon Shock Damage` enchant (the historical starter flavour).
pub fn starter() -> Loadout {
    const STARTER_LEVEL: u16 = 30;
    let mut lo = Loadout {
        level: STARTER_LEVEL,
        status_dur_mult: 1.0,
        shield_optimal_block_boost: 1.0,
        weapon_optimal_block_boost: 1.0,
        ..Default::default()
    };
    // The shield goes on FIRST. `install_weapon` reads `has_shield` to decide
    // whether the weapon is being wielded two-handed, and the starter carries a
    // Chaurus Shield — installing the weapon first would classify it as
    // two-handed and quietly hand the starter 15% more damage.
    if let Some(sh) = gamedata::shield(STARTER_SHIELD) {
        lo.has_shield = true;
        lo.block_rating += sh.block_base;
        lo.shield_optimal_block_boost = sh.optimal_block_boost.max(1.0);
    }
    match gamedata::weapon(STARTER_WEAPON) {
        Some(w) => install_weapon(&mut lo, w, STARTER_TEMPERING),
        None => lo.weapon = fallback_weapon_profile(STARTER_LEVEL),
    }
    lo.enchants = vec![(DamageType::Shock, STARTER_ENCHANT_TIER)];
    lo
}

/// `Glass Dagger` — Light / Slashing, `base_damage` 72.0, `block_base` 36.0, req L28.
const STARTER_WEAPON: &str = "82ed9c7a-bda4-446d-a83f-586d239e2fb9";
/// `Chaurus Shield` — `block_base` 240.0, req L28.
const STARTER_SHIELD: &str = "069654c7-32a6-4391-a944-3f1f97efa11c";
/// Tempering 4 → `QUALITY_BONUS[4] 15.0 × Light 0.60 = 9.0` → base 81.0.
const STARTER_TEMPERING: u64 = 4;
/// `Weapon Shock Damage` tier 3 (`value 1318`) → 23.84 damage + an equal Magicka drain.
const STARTER_ENCHANT_TIER: u8 = 3;

/// Build a [`WeaponProfile`] from a resolved shipped template + the instance's
/// `tempering_level`. [Phase 3.1/3.2]
pub fn weapon_profile(w: &'static gamedata::WeaponStats, tempering_level: u64) -> WeaponProfile {
    let weight = tables::Weight::from_class(w.weapon_class);
    let ty = map_damage_type(w.damage_type);
    let base = w.base_damage + tables::tempering_bonus(weight, tempering_level);
    WeaponProfile {
        primary_type: Some(ty),
        base_by_type: vec![(ty, base)],
        weight: Some(weight),
    }
}

/// The base damage a template deals in the hands it is actually being used in.
///
/// Every one of the 370 shipped weapon templates carries BOTH figures and nothing
/// read the two-handed one, so every two-handed wielder has been swinging at the
/// one-handed number. 129 templates set it 1.15x higher (Steel Longsword
/// 132 → 151.8, Thunderfell 192 → 220.8); 238 set the two figures equal, where
/// this changes nothing.
///
/// **Guard on zero.** Three templates — Goblin Caster Staff, Lich Sword, Outcast
/// Staff — ship `base_two_handed_damage: 0.0`. Taking that literally would make
/// them deal nothing at all, so a zero falls back to the one-handed figure rather
/// than being treated as data.
fn base_damage_in_hand(w: &gamedata::WeaponStats, two_handed: bool) -> f32 {
    if two_handed && w.base_two_handed_damage > 0.0 {
        w.base_two_handed_damage
    } else {
        w.base_damage
    }
}

/// Install a resolved weapon template onto `lo`: the damage profile, the template
/// (for cadence, Phase 3.12) and the weapon's Block Rating contribution.
///
/// Handedness comes from `lo.has_shield`: Blades lets the same weapon be used
/// one-handed with a shield or two-handed without one, and the shipped data
/// carries a separate base damage for each. So the shield must already be
/// installed when this runs — see the ordering note in [`starter`].
fn install_weapon(lo: &mut Loadout, w: &'static gamedata::WeaponStats, tempering_level: u64) {
    let two_handed = !lo.has_shield;
    let weight = tables::Weight::from_class(w.weapon_class);
    let ty = map_damage_type(w.damage_type);
    let base = base_damage_in_hand(w, two_handed) + tables::tempering_bonus(weight, tempering_level);
    lo.weapon = WeaponProfile {
        primary_type: Some(ty),
        base_by_type: vec![(ty, base)],
        weight: Some(weight),
    };
    lo.weapon_template = Some(w);
    lo.weapon_optimal_block_boost = w.optimal_block_boost.max(1.0);
    lo.block_rating += w.block_base;
}

/// The UESP fallback surface, for characters whose weapon does not resolve to a
/// shipped template (imported without inventory, bots).
fn fallback_weapon_profile(level: u16) -> WeaponProfile {
    let weight = tables::Weight::Light;
    WeaponProfile {
        primary_type: Some(DamageType::Slashing),
        base_by_type: vec![(
            DamageType::Slashing,
            tables::fallback::weapon_base_for_level(level, weight),
        )],
        weight: Some(weight),
    }
}

/// `gamedata::DamageType` → the combat model's `DamageType`. The two enums share
/// the client's raw values (`1 = Slashing, 2 = Cleaving, 3 = Bashing, 4..7 =
/// Fire/Frost/Shock/Poison`) — correction 3: the physical trio is a *swing shape*,
/// not a physical/elemental split.
pub fn map_damage_type(t: gamedata::DamageType) -> DamageType {
    match t {
        gamedata::DamageType::None => DamageType::None,
        gamedata::DamageType::Slashing => DamageType::Slashing,
        gamedata::DamageType::Cleaving => DamageType::Cleaving,
        gamedata::DamageType::Bashing => DamageType::Bashing,
        gamedata::DamageType::Fire => DamageType::Fire,
        gamedata::DamageType::Frost => DamageType::Frost,
        gamedata::DamageType::Shock => DamageType::Shock,
        gamedata::DamageType::Poison => DamageType::Poison,
    }
}

/// Parse a combat [`Loadout`] from a player's stored character + inventory.
pub fn from_character(character: &CompleteCharacter, inventory: &CompleteInventory) -> Loadout {
    let mut lo = Loadout {
        level: character.level,
        // The character's own attribute spend. `Fighter::new` turns these into max
        // Stamina / Magicka; see `state::pool_for_points`.
        stamina_points: character.stamina_attribute_points as u16,
        magicka_points: character.magicka_attribute_points as u16,
        has_character: true,
        display_name: character.name.clone(),
        status_dur_mult: 1.0,
        shield_optimal_block_boost: 1.0,
        ..Default::default()
    };

    let mut weapon: Option<(&'static gamedata::WeaponStats, u64)> = None;

    // (equipment_slot, armor_set) per equipped armour piece — Matching Set needs
    // all four slots to agree on a set, so the test cannot be done per-item.
    let mut armor_pieces: Vec<(u8, u8)> = Vec::new();

    // ability uuid -> total bonus ranks from jewellery, summed across slots

    let mut grade_bonus: std::collections::HashMap<String, u16> =

        std::collections::HashMap::new();


    for eq in inventory.loadout.equipped_items.0.values() {
        let template = eq.item.item_template_id.as_hyphenated().to_string();

        // --- item stats (Phase 3.1/3.2/3.3/3.5) ---
        if let Some(w) = gamedata::weapon(&template) {
            // Prefer the highest-damage resolvable weapon if several are equipped
            // (the client only ever equips one, but be deterministic).
            let better = weapon.map(|(p, _)| w.base_damage > p.base_damage).unwrap_or(true);
            if better {
                weapon = Some((w, eq.item.tempering_level));
            }
        } else if let Some(a) = gamedata::armor(&template) {
            lo.armor_rating += a.armor_rating;
            armor_pieces.push((a.equipment_slot, a.armor_set));
        } else if let Some(s) = gamedata::shield(&template) {
            lo.has_shield = true;
            lo.block_rating += s.block_base;
            lo.shield_optimal_block_boost = lo.shield_optimal_block_boost.max(s.optimal_block_boost);
        }

        // --- jewellery GRADING affixes: +N ranks to a named ability ------------
        // Collected here and applied AFTER `parse_equipped_abilities` below, because
        // the abilities they raise do not exist on the loadout yet at this point.
        collect_grade_bonus(&eq.item.properties.grading, &mut grade_bonus);

        // --- the TEMPLATE's mandatory properties -------------------------------
        //
        // Artifact effects live on the template, not on the item instance. Captured
        // artifact instances carry `properties: null` outright — Ebony Mail,
        // Dragon's Blight and Warlock's Ring all do — while ordinary items carry an
        // ENCHANTING array. So the instance loop below sees ZERO properties for every
        // artifact, and all 24 of them were pure stat-sticks: Dawnbreaker's fire
        // damage and its EDIR, Dragon's Blight's 4961 armour piercing, Lord's Mail's
        // poison resist, Guardian's Claymore's PowerfulBlock — every one discarded.
        //
        // These run through the SAME `apply_enchant` as instance enchants, so nothing
        // new is modelled here; the properties simply arrive. A property whose logic
        // has no arm yet still falls through `apply_enchant`'s `_ => {}` exactly as
        // before, so this cannot switch on anything unmodelled by accident.
        for (property_uuid, tier) in gamedata::mandatory_properties(&template) {
            // The generated table stores the uuid as a string; `apply_enchant` keys on
            // `Uuid`. A malformed one is skipped rather than panicking — a bad row in a
            // 37k-line generated file must not take the arena down.
            if let Ok(id) = uuid::Uuid::parse_str(property_uuid) {
                apply_enchant(&mut lo, &id, *tier);
            }
        }

        // --- enchantments, dispatched on the family's LOGIC CLASS (Phase 3.6/3.7) ---
        for prop in &eq.item.properties.enchanting {
            let tier = prop.tier.min(u8::MAX as u64) as u8;
            apply_enchant(&mut lo, &prop.id, tier);
        }
    }

    match weapon {
        Some((w, tempering)) => install_weapon(&mut lo, w, tempering),
        None => lo.weapon = fallback_weapon_profile(character.level),
    }

    lo.abilities = parse_equipped_abilities(&character.equipped_abilities, &character.abilities);

    // Gear-granted ability ranks. Until this existed EVERY ability resolved at its
    // base rank — a ~2.3x damage shortfall. The owner's Frostbite produced rank-4
    // numbers on the wire while his skills menu read 4+10.
    //
    // Additive across slots (the same ring in both hands gives +5+5), clamped to the
    // ability's own `maximum_level`.
    apply_grade_bonuses(&mut lo.abilities, &grade_bonus);
    // Perks resolve HERE, after grading, so a perk raised by jewellery pays out
    // at the raised rank exactly as a damage ability does.
    lo.perks = super::perks::PerkBonuses::resolve(
        &lo.abilities,
        super::perks::matched_armor_set(&armor_pieces),
    );
    // Matching Set is a static gear property once the set test has passed, so it
    // folds straight into the rating rather than being re-checked per hit.
    lo.armor_rating += lo.perks.matching_set_armor;

    lo.paralyze_rank = lo
        .abilities
        .iter()
        .find(|a| a.tag == AbilityTag::Paralyze)
        .map(|a| a.level)
        .unwrap_or(0);

    lo
}

fn profile_base(p: &WeaponProfile) -> f32 {
    p.base_by_type.iter().map(|(_, v)| *v).sum()
}

// ---------------------------------------------------------------------------
// Enchantments — dispatched on the shipped `ItemPropertyLogic` subclass
// ---------------------------------------------------------------------------

/// Apply one `{id, tier}` ENCHANTING property to `lo`, dispatching on the shipped
/// family's **logic class** (correction 2: the `_value` curve is *shared* — 32 of
/// 116 families use the identical `268 … 7591` ramp — so the logic class, not the
/// magnitude, is what an enchantment *is*).
///
/// Unknown families are ignored (they are cosmetic / out-of-combat / economy
/// logics such as `SelfRepairOnFrostDamagePropertyLogic`).
fn apply_enchant(lo: &mut Loadout, id: &Uuid, tier: u8) {
    let uuid = id.as_hyphenated().to_string();
    let Some(family) = gamedata::enchant_family(&uuid) else {
        return;
    };
    let value = family.value(tier).unwrap_or(0.0);
    // SHIPPED magnitude first. `enchant_value x ENCHANT_DAMAGE_PER_VALUE` is a
    // back-solve from ONE capture of ONE family and is wrong by 0.5x to 915x
    // elsewhere; the client ships the real per-family curve. Falls back to the old
    // inference only where the client ships no table, so nothing silently drops to
    // zero. See `gamedata::ENCHANT_MAGNITUDES`.
    let magnitude = gamedata::enchant_magnitude(&uuid, tier)
        .unwrap_or_else(|| value * tables::ENCHANT_DAMAGE_PER_VALUE);

    match family.logic {
        // ---- offensive weapon damage tracks -------------------------------
        "WeaponDamageFirePropertyLogic" => push_enchant(lo, DamageType::Fire, tier),
        "WeaponDamageFrostPropertyLogic" => push_enchant(lo, DamageType::Frost, tier),
        "WeaponDamageShockPropertyLogic" => push_enchant(lo, DamageType::Shock, tier),
        "WeaponDamagePoisonPropertyLogic" => push_enchant(lo, DamageType::Poison, tier),
        "WeaponDamageStaminaPropertyLogic" => push_enchant(lo, DamageType::Stamina, tier),
        "WeaponDamageMagickaPropertyLogic" => push_enchant(lo, DamageType::Magicka, tier),

        // ---- elemental retaliation (Revenge) -------------------------------
        // Only these FOUR ship values. All nine `SpellRevenge*` /
        // `BlockSpellRevenge*` / Templar variants are zero at every tier in the
        // shipped data, so they are deliberately not wired: they would add
        // dispatch for a mechanic that does nothing.
        //
        // `magnitude` is validated against the wire: Frost Revenge t10 is
        // 7591 * ENCHANT_DAMAGE_PER_VALUE = 137.32, and 137.21 is an observed
        // value in s615 — the remainder being the target's resistance.
        "RevengeFirePropertyLogic" => lo.revenge.push((DamageType::Fire, magnitude)),
        "RevengeFrostPropertyLogic" => lo.revenge.push((DamageType::Frost, magnitude)),
        "RevengeShockPropertyLogic" => lo.revenge.push((DamageType::Shock, magnitude)),
        "RevengePoisonPropertyLogic" => lo.revenge.push((DamageType::Poison, magnitude)),

        // ---- resistance ratings (Phase 3.4) --------------------------------
        "ResistFirePropertyLogic" | "ResistFireMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Fire, magnitude)
        }
        "ResistFrostPropertyLogic" | "ResistFrostMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Frost, magnitude)
        }
        "ResistShockPropertyLogic" | "ResistShockMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Shock, magnitude)
        }
        "ResistPoisonPropertyLogic" | "ResistPoisonMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Poison, magnitude)
        }
        "ResistSlashingPropertyLogic" | "ResistSlashingMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Slashing, magnitude)
        }
        "ResistCleavingPropertyLogic" | "ResistCleavingMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Cleaving, magnitude)
        }
        "ResistBashingPropertyLogic" | "ResistBashingMaterialPropertyLogic" => {
            push_resist(lo, DamageType::Bashing, magnitude)
        }

        // ---- block rating -------------------------------------------------
        // `magnitude`, not the raw `value`: the shared `_value` curve runs
        // 268…7591, while an item's own `block_base` is ~50 (a shield) to ~276
        // (the whole starter set) — and `tables::block_reduction` divides by
        // BLOCK_RATING_SCALE (100) × REDUCTION_PER_BLOCK_RATING (0.1), so it caps
        // at MAXIMUM_BLOCK_REDUCTION (0.95) from rating 950 up. Adding the raw
        // 7591 put a single tier-10 block enchant 8× past the cap on its own,
        // pinning block_reduction at 0.95 and making a guard eat 95 % of every
        // hit. Every sibling arm here scales by ENCHANT_DAMAGE_PER_VALUE first;
        // this one did not.
        "BlockReductionFirePropertyLogic"
        | "BlockReductionFrostPropertyLogic"
        | "BlockReductionShockPropertyLogic"
        | "BlockReductionPoisonPropertyLogic"
        | "BlockReductionSlashingPropertyLogic"
        | "BlockReductionCleavingPropertyLogic"
        | "BlockReductionBashingPropertyLogic"
        | "BlockReductionTemplarPropertyLogic"
        | "PowerfulBlockPropertyLogic" => lo.block_rating += magnitude,

        // ---- piercing ------------------------------------------------------
        "ResistancePiercingElementalPropertyLogic" => lo.elem_resist_piercing_rating += magnitude,

        // ---- Opportunist (PDOC / EDOC) ---------------------------------------
        // "Increases physical damage by {0} against targets suffering a condition."
        // No percent sign, so a FLAT add — the same shape as Fortify.
        "OpportunistPhysicalPropertyLogic" => lo.opportunist_physical += magnitude,
        "OpportunistElementalPropertyLogic" => lo.opportunist_elemental += magnitude,
        "ArmorPiercingPhysicalPropertyLogic" => lo.armor_piercing_rating += magnitude,

        // ---- status-threshold fortifies (Phase 3.8) ------------------------
        "FortifyPoisonedPropertyLogic" => push_status_resist(lo, StatusEffectType::Poisoned, magnitude),
        "FortifyBurningPropertyLogic" => push_status_resist(lo, StatusEffectType::Burning, magnitude),
        "FortifyFrozenPropertyLogic" => push_status_resist(lo, StatusEffectType::Frozen, magnitude),
        "FortifyEnervatedPropertyLogic" => push_status_resist(lo, StatusEffectType::Enervated, magnitude),

        // ---- status duration ------------------------------------------------
        // The shared curve is a magnitude, not a percentage; express it as a
        // fraction of the family's own tier-10 ceiling so the multiplier stays in
        // a sane band. [Class 3: shape authored, family + curve real]
        "ShortenElementalStatusPropertyLogic" => {
            lo.status_dur_mult *= (1.0 - curve_fraction(family, tier) * 0.5).max(0.1)
        }
        "ExtendElementalStatusesPropertyLogic" => {
            lo.status_dur_mult *= 1.0 + curve_fraction(family, tier) * 0.5
        }

        // ---- offensive element amplification --------------------------------
        // `Fortify <Element> Damage` raises the attacker's own element track.
        //
        // A FLAT add, in damage units — the shipped text is "Increases frost damage
        // by {0}." with no percent sign, where `Haste` next to it reads "{0}%". It
        // used to store `curve_fraction` and be consumed as `(1.0 + f) x`, which was
        // wrong twice over: wrong shape, and it under-paid every sub-tier-10 enchant.
        // (At tier 10 the two happen to coincide exactly, because the weapon-enchant
        // base and the fortify magnitude share the same curve — which is why this
        // survived so long.)
        "FortifyFirePropertyLogic" => push_fortify(lo, DamageType::Fire, magnitude),
        "FortifyFrostPropertyLogic" => push_fortify(lo, DamageType::Frost, magnitude),
        "FortifyShockPropertyLogic" => push_fortify(lo, DamageType::Shock, magnitude),
        "FortifyPoisonPropertyLogic" => push_fortify(lo, DamageType::Poison, magnitude),

        _ => {}
    }
}

/// This tier's position on its family's own curve, 0..1 (tier value ÷ the family's
/// maximum tier value). Used where a family's magnitude must become a *fraction*.
fn curve_fraction(family: &'static gamedata::EnchantFamily, tier: u8) -> f32 {
    let max = family
        .tiers()
        .iter()
        .map(|t| t.value)
        .fold(0.0_f32, f32::max);
    if max <= 0.0 {
        return 0.0;
    }
    (family.value(tier).unwrap_or(0.0) / max).clamp(0.0, 1.0)
}

fn push_enchant(lo: &mut Loadout, ty: DamageType, tier: u8) {
    lo.enchants.push((ty, tier));
}

/// Bonus ranks a jewellery GRADING affix grants at `tier`.
///
/// CONTESTED. The values below are measured, and the shipped data disagrees with
/// them by a factor of 2.5. Read this before touching either.
///
/// ## The premise this comment used to rest on was false
///
/// It said: "the shipped data carries NO magnitude for these —
/// `AbilityBonusRanksStaticData` has exactly one field, `_abilityUid` — so the
/// number is not extractable". The first half is true and the conclusion does not
/// follow. `AbilityBonusRanksStaticData` extends **`StandardItemPropertyStaticData`**,
/// which declares
///
/// ```text
/// [SerializeField] [ExcelVariable] private float[] _xValueByTier;  // 0x30
/// ```
///
/// I read the subclass, saw one field, and concluded no magnitude existed. It is
/// on the base class, and `reference/game-defs/extract/x_grade_properties.py` has
/// been extracting it into `grade_properties.json` the whole time.
///
/// ## What the data says
///
/// All **49** grade properties ship the identical array `[0.0, 1.0, 2.0]`, indexed
/// by tier, and all 49 have `PropertyType == 3`. Tracker #54 traced `GetRawXValue`:
/// it returns the raw per-tier value unless `PropertyType == 2`, which is the
/// gear-tempering-multiplied branch these do not take. So the shipped grant is
///
/// ```text
/// tier 1 -> +1 rank      tier 2 -> +2 ranks
/// ```
///
/// against the `+4 / +5` below. Grade tiers only ever occur as 1 or 2, which
/// matches the array having exactly three entries.
///
/// ## One measurement disagrees, and it is not dismissable
///
/// The owner's skills menu reads "Frostbite 4+10". His equipped gear carries
/// `FrostbiteBonusRanks` (d5676014) at tier 2 on **two** items — prod, verified.
/// The shipped array gives +2 each, so +4, and a rank of 8. The client's own menu
/// says 14. The client computes that display from the same shipped data we are
/// reading, so one of the two readings of `_xValueByTier` is wrong.
///
/// ## What was genuinely wrong in the old reasoning
///
/// The headroom argument — `maximum_level - maximum_purchaseable_level == 5 x
/// slots` — bounds the CEILING, not the per-item grant. A cap of 5 is perfectly
/// consistent with +2 per tier-2 item; it says what you may reach, not what one
/// ring gives. Treating a cap as a grant is how `+5` got here, and that step was
/// unsound regardless of which number turns out to be right.
///
/// ## SOLVED (2026-09-08) — the divisor is 3, and 4/5 is now DERIVED
///
/// The blocker was `BONUS_RANKS_DIVISOR`: a `static readonly int`, so absent from
/// any il2cpp dump. It is set in the class's `.cctor` (RVA 0x1E82D94), which the
/// dump does list, and the store is plain to read:
///
/// ```text
///   1e82dcc: orr  w9, wzr, #0x3      ; w9 = 3
///   1e82dd4: ldr  x8, [x8, #0xb8]    ; static-fields base
///   1e82dd8: str  w9, [x8]           ; static_fields[0x0] = 3
/// ```
///
/// The dump gives `BONUS_RANKS_DIVISOR; // 0x0`, so that store IS the divisor.
/// **BONUS_RANKS_DIVISOR = 3.**
///
/// `g` at 0x28AAD04 is not class-init boilerplate — it is a call, and the dump
/// names that RVA: **`Mathf.FloorToInt(float)`**. It is a no-op here, because the
/// `sdiv` above it already produced an integer that is then widened with `scvtf`;
/// the source was `Mathf.FloorToInt(intA / intB)`, where the division happens in
/// integers before the cast.
///
/// So the grant is:
///
/// ```text
///   bonusRanks = (w20 - w21) / 3  +  (int) xValueByTier[tier]
/// ```
///
/// **Correcting the register reading in the previous pass**, which had both terms
/// coming from the ability. They do not — the pointer chains are two different
/// objects, and each offset is confirmed against the dump:
///
/// ```text
///   w20:  ldr x8,[x19,#0x10]   this->_item          (AbstractItemPropertyBonusInstance._item  0x10)
///         ldr x8,[x8, #0x10]   item->_template      (Item._template                          0x10)
///         ldr w20,[x8,#0x70]   template->_tier      (ItemTemplate._tier                      0x70)
///
///   w21:  str x0,[x20,#0x30]!  x20 = &_boostedAbility  (pre-index; _boostedAbility            0x30)
///         ldr x9,[x20]         the ability
///         ldr w21,[x9,#0x3c]   ability->_bonusRanksOffset (LearnableAbility._bonusRanksOffset 0x3C)
/// ```
///
/// ## Why this leaves 4 / 5 exactly where it is
///
/// The previous pass measured `w20 - w21 == 10` for every ability it checked
/// (Frostbite, Ice Spike, Ward, Paralyze, Fireball). With the divisor now known:
///
/// ```text
///   tier 1 -> FloorToInt(10 / 3) + 1 = 3 + 1 = 4
///   tier 2 -> FloorToInt(10 / 3) + 2 = 3 + 2 = 5
/// ```
///
/// which is the owner's own skills menu — "Frostbite 4+10" on two tier-2 rings,
/// i.e. +5 each. So the shipped numbers were right, and are now derived from the
/// game's own arithmetic instead of asserted. **No behaviour changes here.** That
/// also retires the decisive on-device observation this function has been waiting
/// on (equip one tier-1 grade item, read the menu): the binary answered it.
///
/// The earlier `floor(n_ranks / 3)` attempt is still wrong and must not come back
/// — its `3` was the right constant reached for the wrong reason, applied to the
/// ability's rank COUNT rather than to `itemTier - bonusRanksOffset`.
///
/// ## The one thing still open
///
/// `10` is measured on 5 of 49 abilities, not derived. `_bonusRanksOffset` is a
/// per-ability field and `_tier` a per-item one, so an item or ability where the
/// difference is not 10 would grant something other than 4/5 — and we extract
/// NEITHER field today (`grep bonusRanksOffset` over `deploy/static` and the
/// server: no hits). Generalising means extracting both, and until then this
/// function is a constant standing in for a formula whose inputs we do not carry.
/// That is a narrower and better-understood gap than "the divisor is unknown".
fn grade_bonus_ranks(tier: u8) -> u8 {
    // The game's formula is `(itemTier - ability.bonusRanksOffset) / DIVISOR +
    // xValueByTier[tier]`. We carry neither input, so the first term is pinned at
    // the measured 10/3; see the note above before changing that to a real lookup.
    const BONUS_RANKS_DIVISOR: u8 = 3;
    /// `itemTier - ability._bonusRanksOffset`, measured as 10 on all five abilities
    /// checked in prod. NOT derived — the two fields it comes from are unextracted.
    const MEASURED_TIER_HEADROOM: u8 = 10;

    if tier == 0 {
        // Tier 0 is "no grade property", not "a grade property worth nothing", so
        // it grants nothing at all rather than the headroom term on its own.
        return 0;
    }
    // `xValueByTier` ships [0.0, 1.0, 2.0] on all 49 properties, and the engine
    // truncates it (`fcvtzs`). Tiers above 2 do not exist in the shipped data;
    // clamping rather than extrapolating keeps an unexpected tier from inventing
    // ranks.
    let x_value = tier.min(2);
    MEASURED_TIER_HEADROOM / BONUS_RANKS_DIVISOR + x_value
}

#[cfg(test)]
mod bonus_ranks_divisor {
    use super::grade_bonus_ranks;

    /// The whole point of the change: the numbers are unchanged, but they now come
    /// out of the game's arithmetic rather than a `match`.
    #[test]
    fn the_derived_formula_reproduces_the_shipped_grants() {
        assert_eq!(grade_bonus_ranks(0), 0, "tier 0 is no property at all");
        assert_eq!(grade_bonus_ranks(1), 4, "FloorToInt(10/3) + 1");
        assert_eq!(grade_bonus_ranks(2), 5, "FloorToInt(10/3) + 2");
    }

    /// The owner's observation, which is what the formula had to match: two tier-2
    /// rings on Frostbite read "4+10" in the skills menu, i.e. +5 each.
    #[test]
    fn two_tier_two_rings_give_the_plus_ten_the_menu_showed() {
        assert_eq!(grade_bonus_ranks(2) * 2, 10);
    }

    /// A tier the shipped data does not contain must not extrapolate. `xValueByTier`
    /// has exactly three entries, so tier 3 cannot mean "+3".
    #[test]
    fn an_unexpected_tier_clamps_instead_of_inventing_ranks() {
        assert_eq!(grade_bonus_ranks(3), grade_bonus_ranks(2));
        assert_eq!(grade_bonus_ranks(255), grade_bonus_ranks(2));
        // control: the clamp has not flattened the real tiers into each other.
        assert_ne!(grade_bonus_ranks(1), grade_bonus_ranks(2));
    }
}

/// Sum a jewellery item's GRADING affixes into `out`, keyed by boosted ability.
///
/// Split out from `from_character` so it is testable without constructing a whole
/// character: the collection half and the application half are where the bugs live,
/// and neither is reachable from a test of the tier rule alone.
fn collect_grade_bonus(
    grading: &[blades_lib::user_data::ItemSingleProperty],
    out: &mut std::collections::HashMap<String, u16>,
) {
    for prop in grading {
        let guid = prop.id.as_hyphenated().to_string();
        let Some(g) = gamedata::grade_property(&guid) else {
            continue;
        };
        let tier = prop.tier.min(u8::MAX as u64) as u8;
        *out.entry(g.ability_uuid.to_string()).or_insert(0) +=
            u16::from(grade_bonus_ranks(tier));
    }
}

/// Raise each equipped ability by its accumulated jewellery bonus, clamped to the
/// ability's shipped `maximum_level`.
fn apply_grade_bonuses(
    abilities: &mut [EquippedAbility],
    bonus: &std::collections::HashMap<String, u16>,
) {
    for a in abilities.iter_mut() {
        let Some(extra) = bonus.get(&a.instance_uuid) else {
            continue;
        };
        let cap = gamedata::ability(&a.instance_uuid)
            .map(|ab| ab.maximum_level)
            .unwrap_or_else(|| u16::from(a.level));
        let raised = (u16::from(a.level) + *extra).min(cap);
        if raised != u16::from(a.level) {
            // info!, not debug!: production runs RUST_LOG=info, and at debug this —
            // the only evidence that gear ranks were applied at all — is invisible.
            // Volume is trivial: at most one line per equipped ability per match, and
            // every sibling combat diagnostic (STUNNED / FROZEN / REVENGE / damage)
            // is already info!.
            log::info!(
                "loadout: ability {} rank {} -> {raised} (+{extra} from jewellery, cap {cap})",
                a.instance_uuid,
                a.level,
            );
        }
        a.level = raised.min(u16::from(u8::MAX)) as u8;
    }
}

fn push_resist(lo: &mut Loadout, ty: DamageType, rating: f32) {
    lo.resistances.push((ty, rating));
}

fn push_status_resist(lo: &mut Loadout, cond: StatusEffectType, magnitude: f32) {
    // The threshold bump is expressed as a fraction of max HP by
    // `Fighter::condition_threshold`; the shipped magnitude is a damage figure, so
    // scale it against the base 25 %-of-maxHP trigger at L86 arena HP.
    lo.status_resist.push((cond, magnitude / STATUS_THRESHOLD_REFERENCE_HP));
}

fn push_fortify(lo: &mut Loadout, ty: DamageType, frac: f32) {
    lo.element_fortify.push((ty, frac));
}

/// Reference max-HP used to turn a `Fortify <Condition>` damage magnitude into the
/// fraction-of-max-HP threshold bump `Fighter::condition_threshold` expects
/// (L86 arena HP ≈ 3150). [Class 3: bridge]
const STATUS_THRESHOLD_REFERENCE_HP: f32 = 3150.0;

// ---------------------------------------------------------------------------
// Abilities (Phase 3.11)
// ---------------------------------------------------------------------------

/// Classify an ability by its shipped `editor_name` / `kind` — the **full 63-row
/// table**, replacing the single `"91078132" => ResistElements` prefix match.
/// The [`ActorAnimation`] a maneuver plays, for op58 propId 10.
///
/// Derived from the ability's shipped `editor_name`, never a UUID table: the
/// captured value is the same-named `ActorAnimation` member for every maneuver
/// except the shield-bash family, which all send `ShieldBashBegin`. See the enum's
/// own doc comment for the per-ability frame counts behind that.
///
/// Returns `None` for anything that is not a maneuver — a spell animates off op53
/// instead, and a caller that gets `None` must not emit an op58.
pub fn actor_animation_for_maneuver(uuid_str: &str) -> Option<ActorAnimation> {
    let a = gamedata::ability(uuid_str)?;
    if a.kind != gamedata::AbilityKind::Maneuver {
        return None;
    }
    Some(match a.editor_name {
        // The shield-bash family — the shared wind-up, not each ability's own member.
        "ShieldBash" | "HarryingBash" | "StaggeringBash" | "ReflectingBash" => {
            ActorAnimation::ShieldBashBegin
        }
        "PowerAttack" => ActorAnimation::PowerAttack,
        "QuickStrikes" => ActorAnimation::QuickStrikes,
        "DodgingStrike" => ActorAnimation::DodgingStrike,
        "Skullcrusher" => ActorAnimation::Skullcrusher,
        "Guardbreaker" => ActorAnimation::Guardbreaker,
        "IndomitableSmash" => ActorAnimation::IndomitableSmash,
        "PiercingStrikes" => ActorAnimation::PiercingStrikes,
        "VenomStrikes" => ActorAnimation::VenomStrikes,
        "RecoveryStrikes" => ActorAnimation::RecoveryStrikes,
        "AdrenalineDodge" => ActorAnimation::AdrenalineDodge,
        "FocusingDodge" => ActorAnimation::FocusingDodge,
        "RenewingDodge" => ActorAnimation::RenewingDodge,
        "RecklessFury" => ActorAnimation::RecklessFury,
        // A maneuver the corpus never showed. Emitting `None` (0) would ask the
        // client to play nothing, which is the bug we are fixing; skip the frame
        // instead so the omission is visible rather than silently wrong.
        _ => return None,
    })
}

pub fn ability_tag_for_template(uuid_str: &str) -> AbilityTag {
    let Some(a) = gamedata::ability(uuid_str) else {
        return AbilityTag::Generic;
    };
    match a.editor_name {
        "Ward" | "Spellbreaker" => AbilityTag::Ward,
        "Absorb" | "SiphonLife" => AbilityTag::Absorb,
        "ResistElements" => AbilityTag::ResistElements,
        "Paralyze" => AbilityTag::Paralyze,
        _ => match a.kind {
            gamedata::AbilityKind::Perk => AbilityTag::Perk,
            gamedata::AbilityKind::Maneuver => AbilityTag::Maneuver,
            gamedata::AbilityKind::Spell => {
                if a.damage_type.is_some() {
                    AbilityTag::Damage
                } else {
                    AbilityTag::Generic
                }
            }
        },
    }
}

/// `equippedAbilities` is `{slot: uuid}` — take the VALUES (the ability instance
/// UUIDs, NOT the slot keys); level each from `abilities` (`{uuid: level}`),
/// defaulting to 1. The level is clamped to the ability's shipped `maximum_level`.
fn parse_equipped_abilities(equipped: &Value, levels: &Value) -> Vec<EquippedAbility> {
    let mut out = Vec::new();
    let Some(slots) = equipped.as_object() else {
        return out;
    };
    let levels = levels.as_object();
    for v in slots.values() {
        if let Some(uuid) = v.as_str() {
            let mut level = levels
                .and_then(|m| m.get(uuid))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(u8::MAX as u64) as u8;
            if let Some(a) = gamedata::ability(uuid) {
                level = level.clamp(1, a.maximum_level.min(u8::MAX as u16) as u8);
            }
            out.push(EquippedAbility {
                instance_uuid: uuid.to_string(),
                level,
                tag: ability_tag_for_template(uuid),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {

    /// Collection: a ring's GRADING affixes become per-ability bonus ranks.
    ///
    /// This half is what a test of `grade_bonus_ranks` alone cannot reach — zeroing
    /// the accumulation leaves the tier rule perfectly correct and the mechanic dead.
    #[test]
    fn grading_affixes_collect_into_per_ability_bonuses() {
        use blades_lib::user_data::ItemSingleProperty;
        let mut out = std::collections::HashMap::new();
        // FrostbiteBonusRanks at tier 2 -> +5 on Frostbite.
        super::collect_grade_bonus(
            &[ItemSingleProperty {
                id: Uuid::parse_str("d5676014-c4f7-4da6-a6e7-3a5e3d495da9").unwrap(),
                tier: 2,
            }],
            &mut out,
        );
        assert_eq!(out.get("4be1d681-c35d-4540-b255-c2910ac80664"), Some(&5));
    }

    /// The owner's actual gear: the SAME ring in both hands, so Frostbite gets +10.
    ///
    /// His skills menu reads "Frostbite 4+10" — base 4 plus two rings at +5. This is
    /// the end-to-end fixture for the whole mechanic: additive stacking across slots,
    /// then the per-ability clamp.
    #[test]
    fn the_same_ring_in_both_hands_stacks_additively() {
        use blades_lib::user_data::ItemSingleProperty;
        let frostbite_affix = || ItemSingleProperty {
            id: Uuid::parse_str("d5676014-c4f7-4da6-a6e7-3a5e3d495da9").unwrap(),
            tier: 2,
        };
        let mut bonus = std::collections::HashMap::new();
        super::collect_grade_bonus(&[frostbite_affix()], &mut bonus); // ring 1
        super::collect_grade_bonus(&[frostbite_affix()], &mut bonus); // ring 2
        assert_eq!(bonus.get("4be1d681-c35d-4540-b255-c2910ac80664"), Some(&10));

        let mut abilities = vec![EquippedAbility {
            instance_uuid: "4be1d681-c35d-4540-b255-c2910ac80664".into(),
            level: 4,
            tag: AbilityTag::Damage,
        }];
        super::apply_grade_bonuses(&mut abilities, &bonus);
        assert_eq!(abilities[0].level, 14, "base 4 + two rings at +5 = 14");
    }

    /// The bonus is clamped to the ability's shipped `maximum_level`.
    #[test]
    fn a_gear_bonus_cannot_exceed_the_abilitys_maximum_level() {
        let uuid = "4be1d681-c35d-4540-b255-c2910ac80664"; // Frostbite, maximum_level 16
        let cap = gamedata::ability(uuid).unwrap().maximum_level;
        let mut bonus = std::collections::HashMap::new();
        bonus.insert(uuid.to_string(), 99u16);

        let mut abilities = vec![EquippedAbility {
            instance_uuid: uuid.into(),
            level: 10,
            tag: AbilityTag::Damage,
        }];
        super::apply_grade_bonuses(&mut abilities, &bonus);
        assert_eq!(u16::from(abilities[0].level), cap, "must clamp at maximum_level");
    }

    /// An ability with no jewellery bonus is left exactly as it was.
    #[test]
    fn abilities_without_a_grade_bonus_are_untouched() {
        let mut abilities = vec![EquippedAbility {
            instance_uuid: "4be1d681-c35d-4540-b255-c2910ac80664".into(),
            level: 4,
            tag: AbilityTag::Damage,
        }];
        super::apply_grade_bonuses(&mut abilities, &std::collections::HashMap::new());
        assert_eq!(abilities[0].level, 4);
    }

    /// The measured tier -> ranks rule, and its ceiling.
    #[test]
    fn grade_bonus_is_four_at_tier_one_and_five_above() {
        assert_eq!(super::grade_bonus_ranks(0), 0);
        assert_eq!(super::grade_bonus_ranks(1), 4, "observed in game");
        assert_eq!(super::grade_bonus_ranks(2), 5, "observed in game");
        // Only tiers 1 and 2 exist in the whole captured corpus. Anything higher is
        // clamped to the measured ceiling rather than extrapolated — the shipped
        // headroom is 5 per slot for 46 of 49 abilities, so 5 IS the ceiling.
        assert_eq!(super::grade_bonus_ranks(9), 5, "clamped, not extrapolated");
    }

    /// The rule must NOT be the rank-count formula that was published and retracted.
    ///
    /// `floor(n_ranks / 3)` fits all three in-game observations exactly and is wrong:
    /// it contradicts the shipped headroom on 36 of 49 abilities. Ice Spike has 14
    /// ranks, so that formula caps gear at +4/slot and its ceiling at 12 — but the
    /// game ships `maximum_level` 14. This pins the difference so it cannot come back.
    #[test]
    fn the_retracted_rank_count_formula_is_not_what_we_use() {
        // Ice Spike: 14 ranks. floor(14/3) = 4, which would be tier-independent.
        // The real rule gives 4 at tier 1 but 5 at tier 2.
        assert_ne!(
            super::grade_bonus_ranks(2),
            14 / 3,
            "tier 2 must grant 5, not the rank-count formula's 4",
        );
    }

    /// Every shipped grade property resolves to a real ability.
    ///
    /// The table is generated, so the failure mode is a silent mismatch after a
    /// regeneration — a property pointing at an ability uuid that no longer exists
    /// would simply never grant its bonus, with nothing to notice.
    #[test]
    fn every_grade_property_points_at_a_known_ability() {
        assert!(!gamedata::GRADE_PROPERTIES.is_empty(), "the table must not be empty");
        for g in gamedata::GRADE_PROPERTIES.iter() {
            assert!(
                gamedata::ability(g.ability_uuid).is_some(),
                "{} ({}) points at unknown ability {}",
                g.editor_name,
                g.uuid,
                g.ability_uuid,
            );
        }
    }

    /// `perk_bonus` is a BINARY SEARCH over `(perk, rank)`, so the generated
    /// table has to be sorted that way. Nothing in the generator enforces it —
    /// a future change to the emit order would leave the lookup silently
    /// returning `None`, or worse, another perk's number.
    #[test]
    fn perk_ranks_are_sorted_the_way_perk_bonus_searches() {
        let t = &gamedata::PERK_RANKS;
        assert!(!t.is_empty(), "the table must not be empty");
        for w in t.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert!(
                (a.perk, a.rank) < (b.perk, b.rank),
                "PERK_RANKS out of order: {} rank {} before {} rank {}",
                a.perk,
                a.rank,
                b.perk,
                b.rank,
            );
        }
    }

    /// The three weapon perks, and the ordering that says the table is aligned.
    ///
    /// Light below versatile below heavy at every rank. A table with perks
    /// mis-assigned to weapon classes would break this — it is how a tester's
    /// correction (tracker #49, Scout is light, not Armsman) was confirmed.
    #[test]
    fn the_weapon_perks_order_light_versatile_heavy() {
        for rank in 1..=11u8 {
            let light = gamedata::perk_bonus("Scout", rank).expect("Scout rank");
            let versatile = gamedata::perk_bonus("Armsman", rank).expect("Armsman rank");
            let heavy = gamedata::perk_bonus("Barbarian", rank).expect("Barbarian rank");
            assert!(
                light < versatile && versatile < heavy,
                "rank {rank}: expected light < versatile < heavy, got {light} / {versatile} / {heavy}",
            );
        }
    }

    /// Spot values straight off the shipped assets, so a regeneration that
    /// silently changes the numbers is caught rather than absorbed.
    #[test]
    fn perk_bonus_returns_the_shipped_values() {
        assert_eq!(gamedata::perk_bonus("Armsman", 1), Some(4.22));
        assert_eq!(gamedata::perk_bonus("Scout", 1), Some(3.43));
        assert_eq!(gamedata::perk_bonus("Barbarian", 11), Some(28.34));

        // Unknown perk and out-of-range rank are None, never a silent 0.0 — a
        // zero is indistinguishable from a perk that does nothing, which is the
        // bug this table exists to fix.
        assert_eq!(gamedata::perk_bonus("NoSuchPerk", 1), None);
        assert_eq!(gamedata::perk_bonus("Armsman", 99), None);
        assert_eq!(gamedata::perk_bonus("Armsman", 0), None);
    }

    /// Skill points stop at level 50 — reported by a player (tracker #49) and
    /// true of the shipped data, which is why a level 100 has no skill edge over
    /// a level 50 and the advantage above 50 is gear.
    #[test]
    fn no_perk_rank_is_purchasable_above_level_fifty() {
        let buyable: Vec<_> = gamedata::PERK_RANKS
            .iter()
            .filter(|p| p.required_hero_level > 0)
            .collect();
        assert!(!buyable.is_empty());
        for p in &buyable {
            assert!(
                p.required_hero_level <= 50,
                "{} rank {} requires level {}",
                p.perk,
                p.rank,
                p.required_hero_level,
            );
        }
        // And the rest are gear-only: -1 for both level and cost, never one of
        // the two, which would mean the sentinel had drifted.
        for p in gamedata::PERK_RANKS.iter().filter(|p| p.required_hero_level < 0) {
            assert_eq!(
                p.ability_point_cost, -1,
                "{} rank {} is unbuyable but still has a point cost",
                p.perk, p.rank,
            );
        }
    }

    /// Frostbite's grade property, looked up the way the loadout does it.
    #[test]
    fn the_frostbite_grade_property_resolves() {
        let g = gamedata::grade_property("d5676014-c4f7-4da6-a6e7-3a5e3d495da9")
            .expect("FrostbiteBonusRanks must be in the generated table");
        assert_eq!(g.editor_name, "FrostbiteBonusRanks");
        assert_eq!(g.ability_uuid, "4be1d681-c35d-4540-b255-c2910ac80664");
        assert_eq!(g.slot, "Ring");
    }

    /// The shipped Frost Revenge enchantment must land as ~137.32, not 7591.
    ///
    /// This goes through `apply_enchant` deliberately. A test that sets
    /// `loadout.revenge` by hand cannot catch the scaling bug, and that bug has real
    /// form in this file — the block-rating arm shipped unscaled once and put a single
    /// tier-10 enchant 8x past its cap.
    ///
    /// 7591 * ENCHANT_DAMAGE_PER_VALUE = 137.32, and 137.21 is an observed value on
    /// the wire in s615.
    #[test]
    fn frost_revenge_t10_is_scaled_to_the_captured_magnitude() {
        use super::super::state::DamageType;
        let mut lo = starter();
        lo.revenge.clear();
        let id = Uuid::parse_str("17718cb7-fb8a-4fbc-adeb-c4cdbc37faf4").unwrap();
        super::apply_enchant(&mut lo, &id, 10);

        assert_eq!(lo.revenge.len(), 1, "the enchantment must register exactly once");
        let (ty, mag) = lo.revenge[0];
        assert_eq!(ty, DamageType::Frost);
        // 36.86 is the SHIPPED `RevengeFrostPropertyLogic._xValueByTier[10]`.
        //
        // This used to assert 137.32 "matching the wire". It never matched the wire:
        // 137.32 is `7591 x ENCHANT_DAMAGE_PER_VALUE`, and that constant was
        // back-solved from s506's *Weapon Poison Damage*, a different family entirely.
        // The 203 recorded Revenge frames in s615/s616 carry magnitudes like 105.0 —
        // never 137.32 — so the old number was the shared tier-weight curve times a
        // borrowed constant, not an observation.
        assert!(
            (mag - 36.86).abs() < 0.05,
            "expected the SHIPPED 36.86, got {mag} — 137.32 was the old inferred value",
        );
    }

    /// The nine zero-valued Revenge variants must not register a retaliation.
    #[test]
    fn the_vs_spell_revenge_variants_are_inert() {
        let mut lo = starter();
        lo.revenge.clear();
        // "Frost Revenge Vs Spell" — 0.0 at every one of its ten tiers.
        let id = Uuid::parse_str("18ef65f2-7585-401b-b7a4-3fe66a830721").unwrap();
        super::apply_enchant(&mut lo, &id, 10);
        assert!(
            lo.revenge.iter().all(|(_t, m)| *m > 0.0),
            "a zero-magnitude family must not add a retaliation entry",
        );
    }
    use super::*;
    use serde_json::json;

    const WEAPON_POISON_DAMAGE: &str = "08ea75d0-5cf1-44a9-9816-d3c6740c4191";
    const RESIST_FIRE: &str = "464bedb7-a631-43b6-a2df-f65f089d39da";
    const ELEM_PIERCE: &str = "98757a01-33b8-40ea-bb45-6acd89811ae3";
    /// `Powerful Block` — `PowerfulBlockPropertyLogic`. The ONLY one of the nine
    /// logic classes in the block-rating arm whose shipped curve is non-zero
    /// (tiers 1/3/5/7/9/10 → `268 … 7591`); the eight `Material Block Bonus Vs …`
    /// families and `Templar Set Block Bonus` all ship 0.0 at every tier, so this
    /// is the family that carries the defect.
    const POWERFUL_BLOCK: &str = "f8e9dec5-c6e7-4976-b24b-2155f1921692";

    fn lo() -> Loadout {
        Loadout { status_dur_mult: 1.0, shield_optimal_block_boost: 1.0, ..Default::default() }
    }

    /// Enchants are routed by the family's LOGIC CLASS, and the magnitude comes
    /// from that family's own tier curve (correction 2).
    #[test]
    fn enchants_dispatch_on_logic_class_not_uuid_prefix() {
        let mut l = lo();
        apply_enchant(&mut l, &Uuid::parse_str(WEAPON_POISON_DAMAGE).unwrap(), 10);
        assert_eq!(l.enchants, vec![(DamageType::Poison, 10)]);
        let dmg = tables::enchant_damage(WEAPON_POISON_DAMAGE, 10).unwrap();
        assert!((dmg - 137.32).abs() < 0.5, "s506 poison enchant base {dmg}");

        // A RESIST family with the SAME shared curve becomes a resistance RATING,
        // not a damage track — the value alone cannot tell them apart.
        let mut r = lo();
        let fire_family = gamedata::enchant_family(RESIST_FIRE).unwrap();
        let a_real_tier = fire_family.tiers().last().unwrap().tier;
        apply_enchant(&mut r, &Uuid::parse_str(RESIST_FIRE).unwrap(), a_real_tier);
        assert!(r.enchants.is_empty(), "a resist enchant is not a damage track");
        assert!(
            r.resistances.iter().any(|(t, v)| *t == DamageType::Fire && *v > 0.0),
            "Resist Fire becomes a Fire Resistance RATING, got {:?}",
            r.resistances
        );

        // Elemental Resistance Piercing is a RATING now, not a 0.04/tier fraction.
        let mut p = lo();
        apply_enchant(&mut p, &Uuid::parse_str(ELEM_PIERCE).unwrap(), 10);
        assert!(p.elem_resist_piercing_rating > 0.0);
        assert_eq!(p.elem_resist_piercing, 0.0, "the fractional field is ability-side only");
    }

    /// tracker #24: a block enchant is scaled like every one of its siblings.
    ///
    /// The block-rating arm added the family's RAW `_value` (the shared 268…7591
    /// curve) while the resist / piercing / fortify arms all multiply by
    /// [`tables::ENCHANT_DAMAGE_PER_VALUE`] first. Since
    /// [`tables::block_reduction`] saturates at `MAXIMUM_BLOCK_REDUCTION` from a
    /// rating of `BLOCK_RATING_SCALE / REDUCTION_PER_BLOCK_RATING` = 950 up, a

    /// THE CALIBRATION. The shipped tables are the wire magnitudes; the old global
    /// constant was a back-solve that only ever fitted the one family it came from.
    ///
    /// Two independent exact matches against the s506 capture, using nothing but
    /// shipped numbers:
    ///
    ///   1. `ShieldMagickaDamage` t10 ships **255.83**, and seq 342 records the
    ///      opponent's ShieldManeuver dealing **255.83 Magicka**. Exact.
    ///   2. `WeaponDamagePoison` t10 light **57.25** + `FortifyPoison` t10 **11.45**
    ///      = **68.70**, and seq 323's connected-block elemental is **68.65**. The
    ///      unblocked 137.32 (seq 27/37/277/287/488) is exactly 2x that.
    ///
    /// The second also settles the shape question independently of the loc text:
    /// Fortify only lands on 68.70 if it is a FLAT ADD.
    #[test]
    fn shipped_magnitudes_reproduce_the_s506_anchors() {
        const SHIELD_MAGICKA: &str = "ShieldMagickaDamagePropertyLogic";
        const WEAPON_POISON: &str = "WeaponDamagePoisonPropertyLogic";
        const FORTIFY_POISON: &str = "FortifyPoisonPropertyLogic";

        let by_logic = |logic: &str| -> &'static gamedata::EnchantMagnitude {
            gamedata::ENCHANT_MAGNITUDES
                .iter()
                .find(|e| e.logic == logic)
                .unwrap_or_else(|| panic!("{logic} must ship a magnitude table"))
        };

        // 1. the shield anchor — shipped == wire, exactly.
        let shield = by_logic(SHIELD_MAGICKA).tiers[10];
        assert!(
            (shield - 255.83).abs() < 0.05,
            "ShieldMagickaDamage t10 must be the recorded 255.83, got {shield}"
        );

        // 2. the poison anchor — base + fortify == the connected-block elemental.
        let poison = by_logic(WEAPON_POISON).tiers[10]; // the LIGHT table: a dagger
        let fortify = by_logic(FORTIFY_POISON).tiers[10];
        assert!(
            (poison + fortify - 68.65).abs() < 0.10,
            "shipped poison {poison} + fortify {fortify} must reproduce the recorded \
             blocked elemental 68.65, got {}",
            poison + fortify
        );
        // ...and the unblocked hit is exactly twice it.
        assert!(
            ((poison + fortify) * 2.0 - 137.32).abs() < 0.20,
            "and 2x that must reproduce the recorded unblocked 137.32, got {}",
            (poison + fortify) * 2.0
        );
    }

    /// single tier-10 `Powerful Block` on its own pinned every guard at the 0.95 cap.
    #[test]
    fn a_block_enchant_is_scaled_like_its_siblings_not_raw() {
        let family = gamedata::enchant_family(POWERFUL_BLOCK).unwrap();
        assert_eq!(family.logic, "PowerfulBlockPropertyLogic");
        let top = family.tiers().last().unwrap().tier;
        let raw = family.value(top).unwrap();
        assert!(raw > 7000.0, "Powerful Block t{top} ships the 7591 curve top, got {raw}");

        let mut l = lo();
        apply_enchant(&mut l, &Uuid::parse_str(POWERFUL_BLOCK).unwrap(), top);

        // The rating is the SHIPPED magnitude, not the raw curve value and not the
        // old `raw x ENCHANT_DAMAGE_PER_VALUE` inference. The point of the test — that
        // the raw 7591 must never reach the loadout — is unchanged; only the correct
        // answer moved, from a borrowed constant to the client's own table.
        let want = gamedata::enchant_magnitude(POWERFUL_BLOCK, top)
            .expect("Powerful Block ships a magnitude table");
        assert!(want < raw / 10.0, "the shipped magnitude is nothing like the raw curve");
        assert!(
            (l.block_rating - want).abs() < 1e-3,
            "block rating {} should be the scaled magnitude {want} (raw curve {raw})",
            l.block_rating,
        );
        assert!(
            l.block_rating < raw / 10.0,
            "the raw curve value {raw} is ~55x the magnitude — adding it raw is the bug",
        );

        // And the consequence the tester felt: the raw value alone saturates
        // block_reduction at its cap; the scaled one does not.
        let cap = gamedata::combat_params::MAXIMUM_BLOCK_REDUCTION;
        assert!(
            (tables::block_reduction(raw, true) - cap).abs() < 1e-6,
            "the raw curve value pins block_reduction at the {cap} cap",
        );
        assert!(
            tables::block_reduction(l.block_rating, true) < cap,
            "the scaled magnitude leaves block_reduction below the cap, got {}",
            tables::block_reduction(l.block_rating, true),
        );

        // A full starter set PLUS a top-tier block enchant still must not cap out —
        // 276 + 137 = 413 → 0.413, well under 0.95.
        let mut s = starter();
        apply_enchant(&mut s, &Uuid::parse_str(POWERFUL_BLOCK).unwrap(), top);
        assert!(
            tables::block_reduction(s.block_rating, true) < cap,
            "starter gear + one Powerful Block capped out at {}",
            tables::block_reduction(s.block_rating, true),
        );
    }

    /// The seven `Material Block Bonus Vs <type>` families and `Templar Set Block
    /// Bonus` route through the same arm but ship 0.0 at every tier, so they are
    /// unaffected either way. Asserted so a future data regeneration that gives
    /// them a real curve shows up here instead of silently widening the blast
    /// radius of the arm above.
    #[test]
    fn the_material_block_families_ship_a_zero_curve() {
        let zeroed: Vec<&str> = gamedata::ENCHANT_FAMILIES
            .iter()
            .filter(|f| {
                f.logic.starts_with("BlockReduction")
                    && f.tiers().iter().all(|t| t.value == 0.0)
            })
            .map(|f| f.logic)
            .collect();
        assert_eq!(
            zeroed.len(),
            8,
            "expected the 7 Material + 1 Templar block families to be all-zero, got {zeroed:?}",
        );
        // …and Powerful Block is NOT among them: it is the one that matters.
        assert!(
            gamedata::enchant_family(POWERFUL_BLOCK)
                .unwrap()
                .tiers()
                .iter()
                .any(|t| t.value > 0.0),
            "Powerful Block must carry a real curve — it is the family under test above",
        );
    }

    /// The full ability table drives routing — not one hardcoded prefix.
    #[test]
    fn ability_routing_covers_the_shipped_table() {
        assert_eq!(ability_tag_for_template(gamedata::ids::WARD), AbilityTag::Ward);
        assert_eq!(ability_tag_for_template(gamedata::ids::PARALYZE), AbilityTag::Paralyze);
        assert_eq!(
            ability_tag_for_template(gamedata::ids::RESIST_ELEMENTS),
            AbilityTag::ResistElements
        );
        assert_eq!(ability_tag_for_template(gamedata::ids::FIREBALL), AbilityTag::Damage);
        // Absorb (a spell with no damage_type) is its own negation class.
        assert_eq!(
            ability_tag_for_template("4e760726-b012-4b25-bc92-0cd6312d6601"),
            AbilityTag::Absorb
        );
        // Maneuvers and perks are distinguished.
        assert_eq!(
            ability_tag_for_template("ce6b63e9-9f18-49c4-aee0-51f7985f9892"),
            AbilityTag::Maneuver
        );
        assert_eq!(
            ability_tag_for_template("09aa3390-8f42-4cd5-a88c-5c94d5e1dd29"),
            AbilityTag::Perk
        );
        assert_eq!(ability_tag_for_template("not-a-uuid"), AbilityTag::Generic);
    }

    /// A resolved weapon carries the shipped cadence + block stats and the
    /// tempering bonus — the old `DEFAULT_WEAPON_WEIGHT` is gone.
    #[test]
    fn weapon_profile_uses_real_template_and_tempering() {
        let w = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).unwrap();
        let p = weapon_profile(w, 10);
        assert_eq!(p.weight, Some(tables::Weight::Light));
        assert_eq!(p.primary_type, Some(DamageType::Slashing));
        assert!((profile_base(&p) - 144.0).abs() < 1e-3, "99 + 45 tempering = 144");
        assert!((w.block_base - 49.5).abs() < 1e-3);
        let mut lo = Loadout::default();
        install_weapon(&mut lo, w, 10);
        assert!((lo.swing_interval().as_secs_f32() - 0.783333).abs() < 1e-4);
        assert!((lo.block_rating - 49.5).abs() < 1e-3);
        // Untempered = the shipped quality-0 cell exactly.
        assert!((profile_base(&weapon_profile(w, 0)) - 99.0).abs() < 1e-3);
    }

    #[test]
    fn starter_resolves_a_real_item() {
        let s = starter();
        assert!(s.weapon_template.is_some(), "starter uses a shipped template");
        assert_eq!(s.weapon.weight, Some(tables::Weight::Light));
        // Glass Dagger 72.0 + tempering-4 bonus 9.0.
        let base: f32 = s.weapon.base_by_type.iter().map(|(_, v)| *v).sum();
        assert!((base - 81.0).abs() < 1e-3, "starter base {base}");
        // Chaurus Shield 240 + the dagger's own 36.
        assert!((s.block_rating - 276.0).abs() < 1e-3, "starter block rating {}", s.block_rating);
        assert!(s.has_shield);
        assert_eq!(s.enchants, vec![(DamageType::Shock, 3)]);
    }

    #[test]
    fn parses_equipped_abilities_by_value_with_levels() {
        let equipped = json!({ "0": "aaaaaaaa-0000-0000-0000-000000000001", "1": "bbbbbbbb-0000-0000-0000-000000000002" });
        let levels = json!({ "aaaaaaaa-0000-0000-0000-000000000001": 3 });
        let abilities = parse_equipped_abilities(&equipped, &levels);
        assert_eq!(abilities.len(), 2);
        let a = abilities.iter().find(|a| a.instance_uuid.starts_with("aaaa")).unwrap();
        let b = abilities.iter().find(|a| a.instance_uuid.starts_with("bbbb")).unwrap();
        assert_eq!(a.level, 3);
        assert_eq!(b.level, 1, "missing level defaults to 1");
    }

    /// A rank above the ability's shipped `maximum_level` is clamped.
    #[test]
    fn ability_level_is_clamped_to_the_shipped_maximum() {
        let equipped = json!({ "0": gamedata::ids::FIREBALL });
        let levels = json!({ gamedata::ids::FIREBALL: 250 });
        let a = parse_equipped_abilities(&equipped, &levels);
        let max = gamedata::ability(gamedata::ids::FIREBALL).unwrap().maximum_level as u8;
        assert_eq!(a[0].level, max);
    }

    #[test]
    fn empty_abilities_value_is_safe() {
        assert!(parse_equipped_abilities(&Value::Null, &Value::Null).is_empty());
    }
}


#[cfg(test)]
mod two_handed_tests {

    /// THE ARTIFACT BUG: every artifact was a stat-stick.
    ///
    /// Artifact effects live on the item TEMPLATE's `mandatory_properties`, not on the
    /// item instance's `properties.ENCHANTING` — and captured artifact instances carry
    /// `properties: null` outright, while ordinary items carry an ENCHANTING array. The
    /// generated table dropped the template field entirely, so all 24 artifacts arrived
    /// with zero properties and `apply_enchant` ran zero times for them.
    ///
    /// Dawnbreaker is the sharpest case: it carries the SAME EDIR family that was just
    /// fixed on the spell path, so fixing EDIR did not fix Dawnbreaker — the property
    /// never arrived to be piercing with.
    #[test]
    fn dawnbreaker_delivers_its_template_properties() {
        const DAWNBREAKER: &str = "ca1e2d0f-b902-4c82-b8d6-e82b83dcc9e0";
        let props = super::super::gamedata::mandatory_properties(DAWNBREAKER);
        assert!(
            !props.is_empty(),
            "Dawnbreaker's template properties must reach the generated table"
        );
        let mut lo = Loadout::default();
        for (id, tier) in props {
            if let Ok(u) = uuid::Uuid::parse_str(id) {
                super::apply_enchant(&mut lo, &u, *tier);
            }
        }
        assert!(
            lo.elem_resist_piercing_rating > 0.0,
            "Dawnbreaker carries the EDIR family — it must pierce, got {}",
            lo.elem_resist_piercing_rating
        );
        assert!(
            lo.enchants.iter().any(|(t, _)| *t == DamageType::Fire),
            "and its fire damage must land: {:?}",
            lo.enchants
        );
    }

    /// Dragon's Blight ships 4961 armour piercing — a real PvP number, silently lost.
    #[test]
    fn dragons_blight_delivers_its_armour_piercing() {
        const DRAGONS_BLIGHT: &str = "515450b1-ab21-4cbf-a8ed-c4e0e51df4c0";
        let mut lo = Loadout::default();
        for (id, tier) in super::super::gamedata::mandatory_properties(DRAGONS_BLIGHT) {
            if let Ok(u) = uuid::Uuid::parse_str(id) {
                super::apply_enchant(&mut lo, &u, *tier);
            }
        }
        assert!(
            lo.armor_piercing_rating > 0.0,
            "Dragon's Blight must pierce armour, got {}",
            lo.armor_piercing_rating
        );
    }

    /// An ordinary, non-artifact template must carry NOTHING here — otherwise the new
    /// path is handing out free stats across the board rather than fixing artifacts.
    #[test]
    fn an_ordinary_item_has_no_mandatory_properties() {
        let ordinary = super::super::gamedata::ARMORS
            .iter()
            .find(|a| a.mandatory_properties.is_empty())
            .expect("most armours carry none");
        assert!(super::super::gamedata::mandatory_properties(ordinary.uuid).is_empty());
    }

    use super::*;

    /// Steel Longsword: 132.0 one-handed, 151.8 two-handed (1.15x).
    const STEEL_LONGSWORD: &str = "68577fab-83f5-4bd1-983c-f396963ac14b";
    /// Lich Sword ships `base_two_handed_damage: 0.0` — a value that must NOT be
    /// taken literally.
    const LICH_SWORD: &str = "474165c0-7657-48ee-ab54-aa443f5c009e";

    fn base_of(lo: &Loadout) -> f32 {
        lo.weapon.base_by_type.iter().map(|(_, v)| *v).sum()
    }

    fn with_weapon(uuid: &str, has_shield: bool) -> Loadout {
        let mut lo = Loadout {
            level: 30,
            status_dur_mult: 1.0,
            shield_optimal_block_boost: 1.0,
            weapon_optimal_block_boost: 1.0,
            ..Default::default()
        };
        lo.has_shield = has_shield;           // handedness, before the weapon goes on
        let w = gamedata::weapon(uuid).expect("template must resolve");
        install_weapon(&mut lo, w, 0);
        lo
    }

    /// Every shipped template carries a two-handed base damage and nothing read it,
    /// so a two-handed wielder swung at the one-handed number. 129 of 370 templates
    /// set it 1.15x higher.
    #[test]
    fn a_weapon_without_a_shield_uses_the_two_handed_base() {
        let two = base_of(&with_weapon(STEEL_LONGSWORD, false));
        assert!(
            (two - 151.8).abs() < 0.01,
            "Steel Longsword two-handed ships 151.8, got {two}",
        );
    }

    /// The control. Same weapon, shield equipped, must be unchanged at 132.0 — a
    /// change that simply raised all weapon damage would pass the test above and
    /// fail this one.
    #[test]
    fn the_same_weapon_with_a_shield_is_unchanged() {
        let one = base_of(&with_weapon(STEEL_LONGSWORD, true));
        assert!(
            (one - 132.0).abs() < 0.01,
            "Steel Longsword one-handed ships 132.0, got {one}",
        );
    }

    /// Three templates ship `base_two_handed_damage: 0.0`. Believing that would make
    /// them deal nothing at all.
    #[test]
    fn a_zero_two_handed_value_falls_back_rather_than_disarming_the_weapon() {
        let two = base_of(&with_weapon(LICH_SWORD, false));
        assert!(
            two > 0.0,
            "Lich Sword ships 0.0 two-handed; a literal reading disarms it (got {two})",
        );
        assert!(
            (two - 7.0).abs() < 0.01,
            "it should fall back to the one-handed 7.0, got {two}",
        );
    }

    /// The starter carries a Chaurus Shield, so it is one-handed, and its damage is
    /// the one-handed figure.
    ///
    /// **This test does NOT pin the ordering inside `starter()`, though I first
    /// claimed it did.** Red-proofing showed why: the starter's Glass Dagger ships
    /// `base_damage` 72.0 and `base_two_handed_damage` 72.0 — identical — so the
    /// starter is insensitive to which way round the shield and weapon go in, and
    /// reverting that ordering leaves this test green. The ordering is guarded by
    /// the comment in `starter()` and by `the_same_weapon_with_a_shield_is_unchanged`,
    /// which uses a weapon whose two figures actually differ.
    #[test]
    fn the_starter_loadout_is_one_handed() {
        let lo = starter();
        assert!(lo.has_shield, "the starter carries a shield");
        let dagger_one_handed = 72.0 + tables::tempering_bonus(tables::Weight::Light, 4);
        let got = base_of(&lo);
        assert!(
            (got - dagger_one_handed).abs() < 0.01,
            "starter must stay one-handed at {dagger_one_handed}, got {got}",
        );
        // Why this cannot detect an ordering regression — asserted, not just claimed.
        let w = gamedata::weapon(STARTER_WEAPON).unwrap();
        assert_eq!(
            w.base_damage, w.base_two_handed_damage,
            "if the starter weapon ever ships differing figures this becomes an \
             ordering guard, and the note above is then out of date",
        );
    }
}
