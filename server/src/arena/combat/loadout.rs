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
use super::state::{AbilityTag, DamageType, EquippedAbility, Loadout, StatusEffectType, WeaponProfile};
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
    match gamedata::weapon(STARTER_WEAPON) {
        Some(w) => install_weapon(&mut lo, w, STARTER_TEMPERING),
        None => lo.weapon = fallback_weapon_profile(STARTER_LEVEL),
    }
    if let Some(sh) = gamedata::shield(STARTER_SHIELD) {
        lo.has_shield = true;
        lo.block_rating += sh.block_base;
        lo.shield_optimal_block_boost = sh.optimal_block_boost.max(1.0);
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

/// Install a resolved weapon template onto `lo`: the damage profile, the template
/// (for cadence, Phase 3.12) and the weapon's Block Rating contribution.
fn install_weapon(lo: &mut Loadout, w: &'static gamedata::WeaponStats, tempering_level: u64) {
    lo.weapon = weapon_profile(w, tempering_level);
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
        display_name: character.name.clone(),
        status_dur_mult: 1.0,
        shield_optimal_block_boost: 1.0,
        ..Default::default()
    };

    let mut weapon: Option<(&'static gamedata::WeaponStats, u64)> = None;

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
        } else if let Some(s) = gamedata::shield(&template) {
            lo.has_shield = true;
            lo.block_rating += s.block_base;
            lo.shield_optimal_block_boost = lo.shield_optimal_block_boost.max(s.optimal_block_boost);
        }

        // --- jewellery GRADING affixes: +N ranks to a named ability ------------
        // Collected here and applied AFTER `parse_equipped_abilities` below, because
        // the abilities they raise do not exist on the loadout yet at this point.
        collect_grade_bonus(&eq.item.properties.grading, &mut grade_bonus);

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
    let magnitude = value * tables::ENCHANT_DAMAGE_PER_VALUE;

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
        "FortifyFirePropertyLogic" => push_fortify(lo, DamageType::Fire, curve_fraction(family, tier)),
        "FortifyFrostPropertyLogic" => push_fortify(lo, DamageType::Frost, curve_fraction(family, tier)),
        "FortifyShockPropertyLogic" => push_fortify(lo, DamageType::Shock, curve_fraction(family, tier)),
        "FortifyPoisonPropertyLogic" => push_fortify(lo, DamageType::Poison, curve_fraction(family, tier)),

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
/// ## RESOLVED ENOUGH TO STOP CHANGING IT (2026-08-22, second pass)
///
/// The `+1 / +2` reading above is WRONG, and so was my alarm about it. Both it
/// and the contributor's asset dump report `_xValueByTier`, and that field is
/// not the answer — it is one term of a calculation. Disassembled
/// `AbilityBonusRanksBonusInstance::InitializeData` (libil2cpp.so, RVA
/// 0x1E827CC) and the arithmetic that writes `_calculatedBonus` reads:
///
/// ```text
///   w20 = ability[0x70]                 w21 = ability[0x3c]
///   w22 = (int) xValueByTier[tier]      w23 = BONUS_RANKS_DIVISOR (static)
///
///   w8 = (w20 - w21) / w23              ; integer divide
///   w0 = g(w8)                          ; call at 0x28AAD04
///   _calculatedBonus = w0 + w22         ; stored at +0x38
/// ```
///
/// So the grant is `(int)xValue + g(headroom / DIVISOR)` — the tier value ADDED
/// to an ability-dependent term, which is why `GetBonusRanks` takes the ability
/// and why the class holds a DIVISOR at all. `w20 - w21` is 10 for every ability
/// checked (Frostbite, Ice Spike, Ward, Paralyze, Fireball: `maximum_level -
/// maximum_purchaseable_level`), matching the headroom measured off the table.
///
/// **Still unresolved:** the value of `BONUS_RANKS_DIVISOR` (a `static readonly
/// int`, so absent from any dump) and what `g` at 0x28AAD04 does — its entry is
/// il2cpp class-init boilerplate and needs a real decompiler to follow.
///
/// **Therefore leave `4 / 5` where it is.** It is consistent with the owner's
/// own skills menu ("Frostbite 4+10" on two tier-2 rings), and every alternative
/// proposed so far has come from reading one input and calling it the output.
/// This function has now been wrong three times — `floor(n_ranks/3)`, then a
/// flat `4/5` justified by the wrong argument, then `1/2`. The next change to it
/// should come with the divisor's actual value, not another inference.
///
/// ## Left alone deliberately
///
/// Changing this to `1 / 2` would be a 2.5x nerf to every graded ability on every
/// character, shipped on the strength of a field I misread once already in this
/// same function. It needs one decisive observation, named in the PR: equip a
/// SINGLE tier-1 grade item for an ability with no other bonus and read the skills
/// menu. `+1` settles it for the data; `+4` means `_xValueByTier` is not the rank
/// count and something else supplies it.
///
/// A still earlier attempt derived this from the ability's rank COUNT
/// (`floor(n_ranks / 3)`). It fits the observations and is WRONG: it contradicts
/// the shipped headroom on 36 of 49 abilities — under it Ice Spike's ceiling would
/// be 12, but the game ships 14. Do not reintroduce it.
fn grade_bonus_ranks(tier: u8) -> u8 {
    match tier {
        0 => 0,
        1 => 4,
        _ => 5,
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
        assert!(
            (mag - 137.32).abs() < 0.05,
            "expected the scaled 137.32 (matching the wire), got {mag} — \
             an unscaled value would be 7591",
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

        // The rating is the SCALED magnitude, not the raw curve value.
        let want = raw * tables::ENCHANT_DAMAGE_PER_VALUE;
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
