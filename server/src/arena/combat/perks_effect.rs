//! End-to-end proof that each perk CHANGES A FIGHT.
//!
//! `perks.rs`'s own tests prove the resolver reads the shipped rank tables. They
//! would all still pass if every application site were deleted — the bonuses would
//! resolve perfectly and be applied nowhere, which is exactly the state the engine
//! was in before this module's production code landed.
//!
//! So every test here is a **differential**: run the same situation twice, once
//! with the perk and once without, and assert the outcome moved in the direction
//! and (where the arithmetic is pinned) by the amount the shipped value implies.
//! The unperked run is the control, so a test cannot pass by accident on a fixture
//! that happens to produce the expected number for another reason.
//!
//! Each test names, in a comment, the production line whose removal turns it red.

use std::time::{Duration, Instant};

use super::damage::{DamageModel, ResolvedDamage, RetailDamageModel};
use super::perks::{CasterPerks, PerkBonuses};
use super::state::{
    AbilityTag, ActiveEffect, ActiveSide, ActorStateType, DamageSource, DamageType,
    EquippedAbility, Fighter, Loadout, MatchCombat, StatusEffectType, WeaponProfile,
};
use super::tables::Weight;

// Perk uuids, from the shipped ability table.
const AUGMENTED_FLAMES: &str = "ed235f8d-0648-4aee-b955-a951562f549d";
const ELEMENTAL_PROTECTION: &str = "788aa75e-4796-4d57-bbab-b1b901623f16";
const BARBARIAN: &str = "64a6a981-0dc8-4fc1-b043-a75d052b00f5";
const MAXIMUM_POWER: &str = "83784ade-533e-4965-a540-05bfd4f056d8";
const HEALING_SURGE: &str = "09aa3390-8f42-4cd5-a88c-5c94d5e1dd29";
const COMBAT_FOCUS: &str = "e0b549c8-a686-49d4-a800-4661fff73e1d";
// Resist Elements — the ability whose shipped `statuses_to_remove` is [4,5,6,7,8].
const RESIST_ELEMENTS: &str = "91078132-ef5c-492a-97f2-ac69be5140a8";

fn perk(uuid: &str, level: u8) -> EquippedAbility {
    EquippedAbility { instance_uuid: uuid.to_string(), level, tag: AbilityTag::Perk }
}

fn perks(list: &[EquippedAbility]) -> PerkBonuses {
    PerkBonuses::resolve(list, false)
}

fn plain_target() -> Fighter {
    Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, Instant::now())
}

fn comp(rd: &ResolvedDamage, ty: DamageType) -> f32 {
    rd.components.iter().filter(|(t, _)| *t == ty).map(|(_, v)| *v).sum()
}

/// A heavy weapon dealing a single physical type, for the weight-class perks.
fn heavy_weapon_loadout() -> Loadout {
    Loadout {
        level: 100,
        weapon: WeaponProfile {
            primary_type: Some(DamageType::Slashing),
            base_by_type: vec![(DamageType::Slashing, 200.0)],
            weight: Some(Weight::Heavy),
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Damage-side perks
// ---------------------------------------------------------------------------

/// BARBARIAN — "+{0} Damage with heavy weapons".
///
/// Red when `physical_base_after_armor` stops adding `perks.weapon_bonus`.
#[test]
fn barbarian_adds_its_shipped_damage_to_a_heavy_weapon() {
    let m = RetailDamageModel;
    let now = Instant::now();

    let mut perked = heavy_weapon_loadout();
    perked.perks = perks(&[perk(BARBARIAN, 11)]);
    let control = heavy_weapon_loadout();

    let hit = |lo: &Loadout| {
        m.resolve_attack(lo, &plain_target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now)
    };
    let with = comp(&hit(&perked), DamageType::Slashing);
    let without = comp(&hit(&control), DamageType::Slashing);

    // Rank 11 ships 28.34, and the target is unarmoured, so it arrives intact.
    assert!(
        (with - without - 28.34).abs() < 0.01,
        "expected +28.34 from Barbarian r11, got {with} vs {without}"
    );
}

/// BARBARIAN must not pay out for a weapon of the wrong class. Without this, a
/// single `weapon_bonus` lookup that ignored the weight would still pass the test
/// above.
#[test]
fn barbarian_pays_nothing_for_a_light_weapon() {
    let m = RetailDamageModel;
    let now = Instant::now();

    let mut light = heavy_weapon_loadout();
    light.weapon.weight = Some(Weight::Light);
    let mut perked = light.clone();
    perked.perks = perks(&[perk(BARBARIAN, 11)]);

    let hit = |lo: &Loadout| {
        m.resolve_attack(lo, &plain_target(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now)
    };
    assert_eq!(
        comp(&hit(&perked), DamageType::Slashing),
        comp(&hit(&light), DamageType::Slashing),
    );
}

/// AUGMENTED FLAMES — "Increases fire damage by {0}", on a SPELL.
///
/// This is the test that proves the caster is visible to `resolve_ability` at all.
/// Before the `CasterPerks` parameter existed it passed `&Loadout::default()`, so no
/// perk could ever reach a spell. Red when that parameter stops being used.
#[test]
fn augmented_flames_raises_a_fire_spell() {
    let m = RetailDamageModel;
    let now = Instant::now();
    let fireball = super::gamedata::ids::FIREBALL;

    let bonuses = perks(&[perk(AUGMENTED_FLAMES, 11)]);
    let perked = CasterPerks { perks: &bonuses, magicka_full: false, health_critical: false, elem_resist_piercing: 0.0, elem_resist_piercing_rating: 0.0, element_fortify: &[] };

    let with = m.resolve_ability(fireball, 1, &perked, &plain_target(), ActiveSide::Middle, now);
    let without =
        m.resolve_ability(fireball, 1, &CasterPerks::none(), &plain_target(), ActiveSide::Middle, now);

    let d = comp(&with, DamageType::Fire) - comp(&without, DamageType::Fire);
    assert!((d - 22.5).abs() < 0.01, "expected +22.5 from Augmented Flames r11, got {d}");
}

/// MAXIMUM POWER — "{0}% more effective when cast while Magicka is full".
///
/// The owner remembered it as void when magicka is ravaged. Both halves are
/// asserted: full magicka pays 40%, anything less pays nothing.
#[test]
fn maximum_power_pays_only_at_full_magicka() {
    let m = RetailDamageModel;
    let now = Instant::now();
    let fireball = super::gamedata::ids::FIREBALL;

    let bonuses = perks(&[perk(MAXIMUM_POWER, 6)]);
    let full = CasterPerks { perks: &bonuses, magicka_full: true, health_critical: false, elem_resist_piercing: 0.0, elem_resist_piercing_rating: 0.0, element_fortify: &[] };
    let ravaged = CasterPerks { perks: &bonuses, magicka_full: false, health_critical: false, elem_resist_piercing: 0.0, elem_resist_piercing_rating: 0.0, element_fortify: &[] };

    let a = comp(
        &m.resolve_ability(fireball, 1, &full, &plain_target(), ActiveSide::Middle, now),
        DamageType::Fire,
    );
    let b = comp(
        &m.resolve_ability(fireball, 1, &ravaged, &plain_target(), ActiveSide::Middle, now),
        DamageType::Fire,
    );
    let c = comp(
        &m.resolve_ability(fireball, 1, &CasterPerks::none(), &plain_target(), ActiveSide::Middle, now),
        DamageType::Fire,
    );

    assert!(b > 0.0, "fixture produced no damage at all — the test would be vacuous");
    assert_eq!(b, c, "a ravaged caster must be identical to an unperked one");
    assert!((a / b - 1.4).abs() < 0.01, "expected 40% more at full magicka, got {}x", a / b);
}

/// ELEMENTAL PROTECTION — "+{0} Block Rating against elemental damage while
/// blocking with a shield".
///
/// Red when `block_outcome` stops setting `elem_rating_bonus`, or when `factor_for`
/// stops adding it.
#[test]
fn elemental_protection_reduces_blocked_elemental_damage_only_with_a_shield() {
    let m = RetailDamageModel;
    let now = Instant::now();

    // A defender mid-block, holding a shield.
    let blocking = |shield: bool, perked: bool| {
        let mut lo = Loadout { level: 100, block_rating: 100.0, ..Default::default() };
        lo.has_shield = shield;
        if perked {
            lo.perks = perks(&[perk(ELEMENTAL_PROTECTION, 11)]);
        }
        let mut f = Fighter::new(1, 565, lo, now);
        f.set_actor_state(ActorStateType::Blocking, now);
        f.blocking_side = ActiveSide::Middle;
        f.blocking_until = Some(now + Duration::from_secs(2));
        // `block_phase` keys off this, not off the actor state — without it the
        // fighter reads as not blocking at all and the whole test is vacuous.
        f.block_raised_at = Some(now);
        f
    };

    // A pure fire spell into that block.
    let fire = |target: &Fighter| {
        comp(
            &m.resolve_ability(
                super::gamedata::ids::FIREBALL,
                1,
                &CasterPerks::none(),
                target,
                ActiveSide::Middle,
                now,
            ),
            DamageType::Fire,
        )
    };

    let unperked = fire(&blocking(true, false));
    let perked_with_shield = fire(&blocking(true, true));
    let perked_no_shield = fire(&blocking(false, true));

    assert!(unperked > 0.0, "fixture blocked everything — the test would be vacuous");
    assert!(
        perked_with_shield < unperked,
        "Elemental Protection must reduce blocked fire: {perked_with_shield} vs {unperked}"
    );
    assert_eq!(
        perked_no_shield,
        fire(&blocking(false, false)),
        "the perk is shield-only; a shieldless guard must gain nothing"
    );
}

/// MATCHING SET — the armour bonus reaches the damage model through
/// `Loadout::armor_rating`, so a matched-set defender takes less physical damage.
#[test]
fn matching_set_armor_reduces_incoming_physical_damage() {
    let m = RetailDamageModel;
    let now = Instant::now();
    let attacker = heavy_weapon_loadout();

    let defender = |armor: f32| {
        Fighter::new(1, 565, Loadout { level: 100, armor_rating: armor, ..Default::default() }, now)
    };

    // Matching Set r9 ships 141.0, folded into `armor_rating` at parse time.
    let bare = m.resolve_attack(
        &attacker, &defender(0.0), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now,
    );
    let setted = m.resolve_attack(
        &attacker, &defender(141.0), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now,
    );
    assert!(
        comp(&setted, DamageType::Slashing) < comp(&bare, DamageType::Slashing),
        "matched-set armour must mitigate"
    );
}

// ---------------------------------------------------------------------------
// Engine-side perks
// ---------------------------------------------------------------------------

fn two_fighter_combat(now: Instant) -> MatchCombat {
    let mut c = MatchCombat::new(2, 2, now);
    for slot in 0..2 {
        let obj = c.alloc_net_object_id();
        let mut f = Fighter::new(slot, obj, Loadout { level: 100, ..Default::default() }, now);
        // Casts are gated on the shipped resource cost; a fighter who cannot pay is
        // rejected before any effect runs, which would make these tests vacuous.
        f.max_magicka = 2000;
        f.magicka = 2000;
        f.max_stamina = 2000;
        f.stamina = 2000;
        c.fighters.push(f);
    }
    c.match_net_object_id = c.alloc_net_object_id();
    c
}

/// HEALING SURGE — the only source of health regeneration in a PvP fight.
///
/// Passive health regen is zero on purpose, so this test doubles as the proof that
/// an UNPERKED fighter still regenerates nothing: if someone switched
/// `HEALTH_REGEN_RATE_PER_S` on, the control arm goes red.
#[test]
fn healing_surge_is_the_only_health_regen_and_needs_high_stamina() {
    let now = Instant::now();
    let tick = now + Duration::from_secs(1);

    let run = |perked: bool, stamina_fraction: f32| -> u32 {
        let mut c = two_fighter_combat(now);
        let f = &mut c.fighters[0];
        if perked {
            f.loadout.perks = perks(&[perk(HEALING_SURGE, 8)]);
        }
        f.max_health = 1000;
        f.health = 500;
        f.max_stamina = 100;
        f.stamina = (100.0 * stamina_fraction) as u32;
        super::resolve::apply_regen_tick(&mut c, tick);
        c.fighters[0].health
    };

    assert_eq!(run(false, 1.0), 500, "an UNPERKED fighter must gain no health at all");
    assert_eq!(run(true, 0.25), 500, "Healing Surge must not pay out at low stamina");

    // Rank 8 ships 15.4/s, at a 1 s tick, at full stamina.
    assert_eq!(run(true, 1.0), 515, "expected the full rank-8 rate at full stamina");

    // Halfway up the ramp pays roughly half.
    let mid = run(true, 0.75);
    assert!(mid > 500 && mid < 515, "expected a partial rate at 75% stamina, got {mid}");
}

/// COMBAT FOCUS — "+{0} Resistance to all damage while using an ability".
///
/// Asserts through the resistance the damage model actually reads, and asserts the
/// window EXPIRES — a bonus that never lapses would also pass a "is it applied?"
/// check while being permanently wrong.
#[test]
fn combat_focus_grants_resistance_for_the_cast_window_then_lapses() {
    let now = Instant::now();
    let mut c = two_fighter_combat(now);
    c.fighters[0].loadout.perks = perks(&[perk(COMBAT_FOCUS, 10)]);
    c.fighters[0].loadout.abilities = vec![EquippedAbility {
        instance_uuid: super::gamedata::ids::FIREBALL.to_string(),
        level: 1,
        tag: AbilityTag::Damage,
    }];

    let before = c.fighters[0].transient_resistance_against(DamageType::Fire, now);
    assert_eq!(before, 0.0, "no resistance before the cast");

    super::resolve::resolve_ability_cast(
        &mut c,
        0,
        1,
        &[],
        &super::input::ExecuteAbility {
            sep_offset: 0,
            ability_uuid: super::gamedata::ids::FIREBALL.to_string(),
        },
        now,
    );

    // Rank 10 ships 31.05, against EVERY type — not just the one it was cast with.
    let during = c.fighters[0].transient_resistance_against(DamageType::Fire, now);
    assert!(
        (during - 31.05).abs() < 0.01,
        "expected Combat Focus r10's 31.05 during the cast, got {during}"
    );
    assert!(
        (c.fighters[0].transient_resistance_against(DamageType::Slashing, now) - 31.05).abs() < 0.01,
        "\"to all damage\" must include physical"
    );

    let later = now + Duration::from_secs(30);
    assert_eq!(
        c.fighters[0].transient_resistance_against(DamageType::Fire, later),
        0.0,
        "the bonus must lapse with the cast window"
    );
}

/// RESIST ELEMENTS CURES A CONDITION IN PLACE.
///
/// All 15 RE ranks ship `statuses_to_remove: [4, 5, 6, 7, 8]` and the field was read
/// by nothing, so casting RE while burning left the fire burning. Red when
/// `apply_status_cures` stops being called from `apply_shipped_effects`.
#[test]
fn resist_elements_cures_the_conditions_already_burning() {
    let now = Instant::now();
    let mut c = two_fighter_combat(now);
    c.fighters[0].loadout.abilities = vec![EquippedAbility {
        instance_uuid: RESIST_ELEMENTS.to_string(),
        level: 1,
        tag: AbilityTag::ResistElements,
    }];

    // Set the caster alight, and freeze them, before the cast.
    for effect in [StatusEffectType::Burning, StatusEffectType::Frozen] {
        c.fighters[0].effects.push(ActiveEffect {
            effect,
            damage_type: DamageType::Fire,
            value: 0.0,
            per_tick_damage: 0.0,
            expires_at: now + Duration::from_secs(5),
            last_tick: now,
            is_transient_resist: false,
        });
    }
    assert!(
        c.fighters[0].effects.iter().any(|e| e.effect == StatusEffectType::Burning),
        "fixture failed to apply Burning — the test would be vacuous"
    );

    super::resolve::resolve_ability_cast(
        &mut c,
        0,
        1,
        &[],
        &super::input::ExecuteAbility {
            sep_offset: 0,
            ability_uuid: RESIST_ELEMENTS.to_string(),
        },
        now,
    );

    // RE ships `ChannelDuration = 0.9`, so the cast only QUEUES its effect; the
    // wind-up has to elapse before anything is applied or cured. Asserting on the
    // state right after the cast would test the wrong instant entirely.
    assert!(
        c.fighters[0].effects.iter().any(|e| e.effect == StatusEffectType::Burning),
        "still burning during the wind-up — the cure must not be instant"
    );
    let landed = now + Duration::from_millis(1000);
    super::resolve::land_due_impacts(&mut c, landed);

    assert!(
        !c.fighters[0].effects.iter().any(|e| e.effect == StatusEffectType::Burning),
        "Resist Elements must put out an existing fire"
    );
    assert!(
        !c.fighters[0].effects.iter().any(|e| e.effect == StatusEffectType::Frozen),
        "Resist Elements must clear Frozen too — its shipped list covers 4,5,6,7,8"
    );
}

/// …and it must not announce a cure for something the fighter never had. Emitting
/// an unconditional remove for all five statuses would put traffic on the wire that
/// retail never sent, which the whole engine is built to avoid.
#[test]
fn resist_elements_announces_no_cure_when_there_is_nothing_to_cure() {
    let now = Instant::now();
    let mut c = two_fighter_combat(now);
    c.fighters[0].loadout.abilities = vec![EquippedAbility {
        instance_uuid: RESIST_ELEMENTS.to_string(),
        level: 1,
        tag: AbilityTag::ResistElements,
    }];

    let before = c.fighters[0].transient_resistances.len();
    super::resolve::resolve_ability_cast(
        &mut c,
        0,
        1,
        &[],
        &super::input::ExecuteAbility {
            sep_offset: 0,
            ability_uuid: RESIST_ELEMENTS.to_string(),
        },
        now,
    );
    let out = super::resolve::land_due_impacts(&mut c, now + Duration::from_millis(1000));

    // RE still grants its four protective resistances, so the cast plainly ran —
    // without that check a rejected cast would pass this test trivially. What must
    // be true is that the cure path removed nothing: it only touches statuses the
    // fighter actually held, and this fighter held none.
    assert!(!out.is_empty(), "RE must still emit its protective statuses");
    assert_eq!(
        c.fighters[0].transient_resistances.len(),
        before + 4,
        "RE must still grant its four resistances"
    );
    assert!(
        c.fighters[0].effects.is_empty(),
        "nothing was held, so the cure path must not have touched the effect list"
    );
}
