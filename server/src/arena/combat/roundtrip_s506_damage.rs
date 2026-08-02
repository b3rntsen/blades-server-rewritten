//! Offline reproduction-**differential** test for the arena DAMAGE model against the
//! recorded retail match (prod `arena_udp_frames` **session_id = 506**).
//!
//! # Phase 3: the fixture is DERIVED, not hardcoded
//!
//! The attacker side is now built entirely from shipped game data:
//!
//! | quantity | source |
//! |---|---|
//! | weapon base 99.0 | `gamedata::weapon(ids::DRAGONBONE_DAGGER).base_damage` |
//! | +45 tempering | `tables::tempering_bonus(Light, 10)` — §3 "weapon tempering 10" |
//! | Slashing / Light / Dagger | the same template's `damage_type` / `weapon_class` |
//! | cadence 0.7833 s | `attack_delay 0.2333 + recovery_time 0.55` |
//! | poison 137.32 | `Weapon Poison Damage` tier 10 (`value 7591`) × [`tables::ENCHANT_DAMAGE_PER_VALUE`] |
//! | block base 49.5 | the template's `block_base` |
//!
//! The **defender** side is the honest weak point and is called out as such in
//! [`blank`]: Blank's gear was never captured (we only capture our own characters),
//! so its Armor Rating and Block Rating are *solved from the anchors* and then
//! expressed through real shipped templates. See the doc comment there.
//!
//! ## What is compared
//!   - the **physical Slashing** ramp 113.82 / 165.07 / 469.30;
//!   - the **Poison enchant** track 137.32 fresh → 205.36 fully conditioned;
//!   - the **connected optimal block**: physical ≈ 0, elemental ≈ 68.65;
//!   - the **paralyse threshold** (now the shipped ABSOLUTE 32.7, not 0.45·maxHP);
//!   - no 25 %-of-maxHP clamp;
//!   - the **sum invariant** (`totalDamage == Σ health types`, drains excluded).

use std::time::Instant;

use super::damage::{flags, is_health_type, DamageModel, RetailDamageModel, ELEMENT_AMP_MAX};
use super::gamedata;
use super::loadout;
use super::state::{
    health_for_level, paralyze_damage_threshold, ActiveSide, ActorStateType, DamageSource,
    DamageType, Fighter, Loadout, ARENA_HEALTH_MULTIPLIER,
};
use super::tables::{combo_factor, Weight};

// ---------------------------------------------------------------------------
// s506 ground truth (docs/arena-combat-reproduction-spec.md §2a/§3/§4).
// ---------------------------------------------------------------------------

/// The recorded combo-0, unblocked, Right Slashing (post-armor) — seq 27/277/488.
const S506_SLASH_BASE: f32 = 113.82;
/// The recorded fresh Weapon-Poison enchant base @ tier 10 — seq 27/37/277/…
const S506_POISON_BASE: f32 = 137.32;
/// Flappety is L86 Nord (§1).
const S506_LEVEL: u16 = 86;

/// The `Weapon Poison Damage` enchant family (§3, Flappety's weapon suffix).
const WEAPON_POISON_DAMAGE: &str = "08ea75d0-5cf1-44a9-9816-d3c6740c4191";

/// Flappety's weapon **tempering level** (§3: "weapon tempering 10" = Mythical).
const S506_TEMPERING: u64 = 10;

/// The §2a recorded normal-swing combo ramp, `(combo_count, recorded_slashing)`.
const S506_COMBO_RAMP: &[(u32, f32)] = &[
    (0, 113.82), // seq 27/277/488 — fresh (combo reset) ×1.00
    (1, 165.07), // seq 37/287 — first chained alternating swing ×1.45
    (2, S506_SLASH_BASE * 1.50),
    (3, 301.79), // seq 436 — deep combo ×2.65
    (4, 469.30), // seq 452 — deeper ×4.12 (ceiling)
];

/// The §2a recorded `Middle` WeaponManeuver Slashing band (seq 88/106/337).
const S506_MANEUVER_SLASH: &[f32] = &[201.37, 274.51, 186.98];

// ---------------------------------------------------------------------------
// Fixtures — DERIVED from the shipped templates.
// ---------------------------------------------------------------------------

/// Flappety's Light Dragonbone-Poison dagger (§3), loaded from the real
/// `WeaponTemplateList` row plus the item's tempering level and the real
/// `Weapon Poison Damage` tier-10 curve value.
fn flappety_dagger() -> Loadout {
    let w = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).expect("Dragonbone Dagger");
    let mut lo = Loadout {
        level: S506_LEVEL,
        status_dur_mult: 1.0,
        shield_optimal_block_boost: 1.0,
        ..Default::default()
    };
    lo.weapon = loadout::weapon_profile(w, S506_TEMPERING);
    lo.weapon_template = Some(w);
    lo.weapon_optimal_block_boost = w.optimal_block_boost.max(1.0);
    lo.block_rating = w.block_base;
    lo.enchants = vec![(DamageType::Poison, 10)];
    lo
}

/// **Blank's Armor Rating, solved from the anchor — the one un-observable input.**
///
/// The recorded combo-0 Slashing is 113.82 and the derived weapon base is
/// `99.0 + tempering_bonus(Light, 10) = 144.0`, so the physical cut is 30.18, i.e.
/// an Armor Rating of `30.18 / reductionPerArmorRating(0.1) = 301.8`.
///
/// Blank's gear is **not in our data** — the capture platform only stores our own
/// characters (§1: "Opponent gear is not in our DB"), and prod is unreachable from
/// this agent. So this value cannot be *predicted*; it is *inverted* from the
/// anchor. What CAN be checked, and is asserted in
/// [`blank_armor_is_realisable_from_shipped_templates`], is that 301.8 is exactly
/// realisable from two shipped armor templates — i.e. it sits inside the game's
/// real design space rather than being an out-of-range fudge factor.
///
/// [Class 3: authored/unverifiable — flagged.]
const BLANK_HELMET: &str = "0c39d0f3-79c8-4e58-b435-3622a42e4d3d"; // Paladin's Helmet, AR 230.4
const BLANK_GAUNTLETS: &str = "a2c629d2-65a9-445d-9594-ca992aed624b"; // Quicksilver Gauntlets, AR 71.4
/// Blank's shield — the video shows "poison dagger + shield" (§1). Ebony Shield
/// `blockBase 330`; with the Dragonbone Dagger's own 49.5 that is a Block Rating of
/// 379.5, which at the optimal (×2) weight reproduces the recorded ÷2 elemental.
/// [Class 3: the *shield model* is inverted from the block anchor, as above.]
const BLANK_SHIELD: &str = "1d248608-7347-4122-8b42-840b6304c203"; // Ebony Shield, blockBase 330

fn blank_armor_rating() -> f32 {
    gamedata::armor_rating(BLANK_HELMET).expect("helmet")
        + gamedata::armor_rating(BLANK_GAUNTLETS).expect("gauntlets")
}

fn blank_block_rating() -> f32 {
    gamedata::block_base(BLANK_SHIELD).expect("shield")
        + gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).unwrap().block_base
}

/// Blank, the opponent (#125): a L86 fighter at arena ×3 HP wearing the armor +
/// shield above.
fn blank() -> Fighter {
    let lo = Loadout {
        level: S506_LEVEL,
        armor_rating: blank_armor_rating(),
        block_rating: blank_block_rating(),
        shield_optimal_block_boost: 1.0,
        status_dur_mult: 1.0,
        ..Default::default()
    };
    Fighter::new(1, 125, lo, Instant::now())
}

/// Blank with no gear — for isolating the raw weapon output.
fn naked_blank() -> Fighter {
    Fighter::new(1, 125, Loadout { level: S506_LEVEL, ..Default::default() }, Instant::now())
}

fn slash_of(rd: &super::damage::ResolvedDamage) -> f32 {
    rd.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum()
}
fn poison_of(rd: &super::damage::ResolvedDamage) -> f32 {
    rd.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum()
}

// ---------------------------------------------------------------------------
// (0) The fixture really is derived from shipped data.
// ---------------------------------------------------------------------------

#[test]
fn s506_fixture_is_derived_from_shipped_item_data() {
    let w = gamedata::weapon(gamedata::ids::DRAGONBONE_DAGGER).expect("Dragonbone Dagger");
    assert_eq!(w.base_damage, 99.0);
    assert_eq!(w.damage_type, gamedata::DamageType::Slashing);
    assert_eq!(w.weapon_class, gamedata::WeaponClass::Light);
    assert_eq!(w.weapon_type, gamedata::WeaponType::Dagger);
    assert!((w.attack_delay - 0.233333).abs() < 1e-5);
    assert!((w.recovery_time - 0.55).abs() < 1e-5);
    assert!((w.block_base - 49.5).abs() < 1e-5);

    let lo = flappety_dagger();
    let base: f32 = lo.weapon.base_by_type.iter().map(|(_, v)| *v).sum();
    assert!(
        (base - 144.0).abs() < 1e-3,
        "DIVERGENCE: tempered base {base} != 99.0 + tempering_bonus(Light, 10) 45.0",
    );
    assert_eq!(lo.weapon.weight, Some(Weight::Light));
    assert!((lo.swing_interval().as_secs_f32() - 0.783333).abs() < 1e-4);
    // The poison magnitude comes from the family curve, not a literal.
    let poison = super::tables::enchant_damage(WEAPON_POISON_DAMAGE, 10).expect("poison t10");
    assert!(
        (poison - S506_POISON_BASE).abs() < 0.5,
        "DIVERGENCE: `Weapon Poison Damage` tier 10 → {poison}, recorded {S506_POISON_BASE}",
    );
}

/// The armor rating the anchor implies is exactly realisable from shipped templates.
#[test]
fn blank_armor_is_realisable_from_shipped_templates() {
    let ar = blank_armor_rating();
    assert!(
        (ar - 301.8).abs() < 0.05,
        "Blank's modelled Armor Rating {ar} should be the 301.8 the 113.82 anchor implies",
    );
    // 144.0 tempered base − 30.18 armor = 113.82.
    let cut = super::tables::armor_reduction(144.0, ar);
    assert!((144.0 - cut - S506_SLASH_BASE).abs() < 0.05, "144 − {cut} != {S506_SLASH_BASE}");
}

// ---------------------------------------------------------------------------
// (A) The combo ramp + maneuver lane reproduce the §2a Slashing column.
// ---------------------------------------------------------------------------

#[test]
fn s506_combo_ramp_reproduces_recorded_slashing() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    for &(count, recorded) in S506_COMBO_RAMP {
        let side = if count % 2 == 0 { ActiveSide::Right } else { ActiveSide::Left };
        let rd = m.resolve_attack(&lo, &blank(), DamageSource::Attack, side, 1.0, count, Instant::now());
        let got = slash_of(&rd);
        let tol = (recorded * 0.02).max(0.5);
        assert!(
            (got - recorded).abs() <= tol,
            "DIVERGENCE (COMBO §4.2): combo {count} Slashing modeled {got:.2} vs s506 recorded \
             {recorded:.2} (tol ±{tol:.2}). combo_factor(Light,{count})={:.3}, tempered base 144.0, \
             armor cut {:.2}.",
            combo_factor(Weight::Light, count),
            super::tables::armor_reduction(144.0, blank_armor_rating()),
        );
    }
    let now = Instant::now();
    let c0 = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now));
    let c1 = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Left, 1.0, 1, now));
    assert!((c0 - 113.82).abs() < 0.05, "combo-0 anchor {c0:.2} != recorded 113.82");
    assert!((c1 - 165.07).abs() < 1.0, "combo-1 anchor {c1:.2} != recorded 165.07 (×1.45)");
    let c9 = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 9, now));
    assert!((c9 - 113.82 * 4.12).abs() < 1.0, "deep combo capped at ×4.12, got {c9:.1}");
    // The ramp is exactly proportional to the post-armor base — the reason armor is
    // applied BEFORE the swing factor (see `damage.rs` module doc).
    assert!((c1 / c0 - 1.45).abs() < 1e-3, "ratio {} != 1.45", c1 / c0);
}

#[test]
fn s506_middle_maneuver_lands_in_recorded_band() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    let modeled: Vec<f32> = [1.0, 1.5, 1.8]
        .iter()
        .map(|&sf| {
            slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Middle, sf, 0, Instant::now()))
        })
        .collect();
    let lo_m = *modeled.iter().min_by(|a, b| a.total_cmp(b)).unwrap();
    let hi_m = *modeled.iter().max_by(|a, b| a.total_cmp(b)).unwrap();
    for &rec in S506_MANEUVER_SLASH {
        assert!(
            rec >= lo_m * 0.85 && rec <= hi_m * 1.15,
            "DIVERGENCE (MANEUVER §4.2): recorded Middle maneuver {rec:.1} outside the modeled \
             charged band [{lo_m:.1}, {hi_m:.1}] (Light crit ×{:.3} × swing_factor).",
            Weight::Light.crit_combo().0,
        );
    }
}

// ---------------------------------------------------------------------------
// (B) The poison enchant base + amplification reproduce the §4.3 ramp.
// ---------------------------------------------------------------------------

#[test]
fn s506_poison_base_and_amplification_ramp() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();

    let now = Instant::now();
    let fresh = m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
    assert!(
        (poison_of(&fresh) - S506_POISON_BASE).abs() < 0.5,
        "DIVERGENCE (ENCHANT §4.3): fresh Poison {:.2} vs recorded base {S506_POISON_BASE}. \
         The family curve `Weapon Poison Damage` t10 = 7591 × {} should give it.",
        poison_of(&fresh),
        super::tables::ENCHANT_DAMAGE_PER_VALUE,
    );
    // Phase 3.6: Poison has NO mirrored stat drain (only Frost→Stamina, Shock→Magicka).
    let magicka: f32 =
        fresh.components.iter().filter(|(t, _)| *t == DamageType::Magicka).map(|(_, v)| *v).sum();
    assert_eq!(magicka, 0.0, "Poison does not drain Magicka (correction: the drain is per-element)");
    assert!(
        (fresh.total - (slash_of(&fresh) + poison_of(&fresh))).abs() < 1e-2,
        "sum invariant: total == Slashing + Poison",
    );

    // AMPLIFICATION toward the recorded +50 % endpoint (137 → ~205).
    let mut tgt = blank();
    for _ in 0..8 {
        tgt.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    let amped = m.resolve_attack(&lo, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
    let recorded_amped = 205.36; // §4.3 endpoint (seq 452 Poison)
    let ceiling = S506_POISON_BASE * ELEMENT_AMP_MAX;
    assert!(
        (poison_of(&amped) - ceiling).abs() < 1.0,
        "DIVERGENCE (AMP §4.3): fully-conditioned Poison {:.2} should reach the ×1.5 ceiling \
         {ceiling:.2} (recorded endpoint {recorded_amped}).",
        poison_of(&amped),
    );
    assert!(
        (poison_of(&amped) - recorded_amped).abs() < 2.0,
        "amplified Poison {:.2} vs recorded endpoint {recorded_amped}",
        poison_of(&amped),
    );
    assert!(poison_of(&fresh) < poison_of(&amped));
}

// ---------------------------------------------------------------------------
// (C) The connected optimal block is asymmetric (§4.4).
// ---------------------------------------------------------------------------

#[test]
fn s506_optimal_block_negates_physical_halves_elemental() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    // s506 seq 323: a connected optimal block on a Right swing → Slashing 113.82→0.77
    // (≈0), Poison 137.32→68.65 (=÷2.0).
    let now = Instant::now();
    let mut def = blank();
    def.set_actor_state(ActorStateType::Blocking, now);
    def.blocking_side = ActiveSide::Right;
    def.block_raised_at = Some(now);
    def.blocking_until = Some(now + std::time::Duration::from_secs(2));
    let blocked = m.resolve_attack(&lo, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
    assert!(blocked.flags & flags::WAS_OPTIMAL_BLOCKING != 0, "optimal-block flag set");
    assert!(
        slash_of(&blocked) <= 1.0,
        "DIVERGENCE (BLOCK §4.4): a connected optimal block must drive physical to ≈0 \
         (recorded 0.77), got {:.2}",
        slash_of(&blocked),
    );
    let recorded_blocked_poison = 68.65; // seq 323
    assert!(
        (poison_of(&blocked) - recorded_blocked_poison).abs() < 1.5,
        "DIVERGENCE (BLOCK §4.4): optimal-block elemental must land near {recorded_blocked_poison} \
         (137.32 × ~0.5), got {:.2}. Block Rating {:.1} → elemental reduction {:.4}.",
        poison_of(&blocked),
        def.block_rating(true),
        super::tables::block_reduction(def.block_rating(true), false),
    );
    // A LATE / wrong-side guard does NOT negate physical.
    let mut late = def.clone();
    late.blocking_side = ActiveSide::Left;
    let l = m.resolve_attack(&lo, &late, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
    assert!(l.flags & flags::WAS_LATE_BLOCKING != 0);
    assert!(slash_of(&l) > 1.0, "a late block only reduces, got {:.2}", slash_of(&l));
}

// ---------------------------------------------------------------------------
// (D) No 25 % clamp (§4.5) + the round/match HP arithmetic (the seq-342 kill).
// ---------------------------------------------------------------------------

#[test]
fn s506_deep_combo_unclamped_and_kill_arithmetic() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    let max_hp = health_for_level(S506_LEVEL) * ARENA_HEALTH_MULTIPLIER;
    assert_eq!(max_hp, 3150, "L86 ×3 = 3150 maxHP");

    let mut amped = blank();
    let now = Instant::now();
    for _ in 0..8 {
        amped.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    let big = m.resolve_attack(&lo, &amped, DamageSource::Attack, ActiveSide::Right, 1.0, 4, now);
    let recorded_total = 674.66; // 469.30 Slash + 205.36 Poison
    assert!(
        (big.total - recorded_total).abs() < 12.0,
        "DIVERGENCE: the deep-combo hit total {:.2} should reproduce the recorded {recorded_total}",
        big.total,
    );
    let health_sum: f32 = big.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
    assert!((big.total - health_sum).abs() < 1e-3, "total == Σ health (no 25 % clamp)");

    let mut victim = blank();
    victim.take_damage(victim.max_health + 500);
    assert!(victim.is_dead());
    assert_eq!(victim.health, 0);
}

// ---------------------------------------------------------------------------
// (E) Paralyse — now the shipped ABSOLUTE threshold (Phase 3.9).
// ---------------------------------------------------------------------------

#[test]
fn s506_paralyse_threshold_is_the_shipped_absolute_value() {
    // `ParalyzeRank1._damageToCauseParalyze` = 32.7, an ABSOLUTE damage figure.
    let r1 = paralyze_damage_threshold(1);
    assert!((r1 - 32.7).abs() < 1e-3, "Paralyze R1 threshold {r1} != 32.7");
    assert!(paralyze_damage_threshold(2) > r1, "the threshold rises with rank");
    assert!(
        (super::state::paralyze_duration_secs(1) - 2.0).abs() < 1e-3,
        "Paralyze R1 duration is 2.0 s, not the invented 3.1",
    );
    // The old model needed 0.45 × 3150 = 1417.5 accumulated poison — 43× more.
    let old_fraction_model = 0.45 * (health_for_level(S506_LEVEL) * ARENA_HEALTH_MULTIPLIER) as f32;
    assert!(
        old_fraction_model / r1 > 40.0,
        "sanity: the deleted fraction model was {old_fraction_model} vs the shipped {r1}",
    );
    // One landed s506 poison hit (137.32) already clears the shipped threshold.
    let mut f = blank();
    f.record_element_damage(DamageType::Poison, S506_POISON_BASE, Instant::now());
    assert!(f.recent_element_damage(DamageType::Poison) >= r1);
}

// ---------------------------------------------------------------------------
// (F) End-to-end chain through the combo counter.
// ---------------------------------------------------------------------------

#[test]
fn s506_full_chain_through_engine_reproduces_ramp_and_resets_on_block() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    let mut attacker = blank();

    let mut last_slash = 0.0;
    for step in 0..5u32 {
        let side = if step % 2 == 0 { ActiveSide::Right } else { ActiveSide::Left };
        let depth = attacker.register_combo_swing(side);
        assert_eq!(depth, step, "alternating swings increment the combo each step");
        let rd = m.resolve_attack(&lo, &blank(), DamageSource::Attack, side, 1.0, depth, Instant::now());
        let s = slash_of(&rd);
        if step > 0 {
            assert!(s >= last_slash, "the combo ramp is monotonic (step {step})");
        }
        let health_sum: f32 = rd.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert!((rd.total - health_sum).abs() < 1e-3, "sum invariant on hit {step}");
        last_slash = s;
    }
    assert!(last_slash > S506_SLASH_BASE * 2.5);

    attacker.reset_combo();
    let depth_after = attacker.register_combo_swing(ActiveSide::Right);
    assert_eq!(depth_after, 0);
    let fresh = slash_of(&m.resolve_attack(
        &lo,
        &blank(),
        DamageSource::Attack,
        ActiveSide::Right,
        1.0,
        depth_after,
        Instant::now(),
    ));
    assert!((fresh - S506_SLASH_BASE).abs() < 0.05, "post-reset swing is 113.82, got {fresh:.2}");
}

/// A single place that prints EVERY derived anchor next to its recorded value, so a
/// reviewer can see the residuals without reading five assertions.
/// `cargo test -p server s506_anchor_report -- --nocapture`
#[test]
fn s506_anchor_report() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    let now = Instant::now();
    let mut rows: Vec<(&str, f32, f32)> = Vec::new();

    let c0 = m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
    rows.push(("combo-0 Slashing", slash_of(&c0), 113.82));
    rows.push(("combo-0 Poison", poison_of(&c0), 137.32));
    let c1 = m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Left, 1.0, 1, now);
    rows.push(("combo-1 Slashing", slash_of(&c1), 165.07));
    let c4 = m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 4, now);
    rows.push(("combo-4 Slashing", slash_of(&c4), 469.30));

    let mut def = blank();
    def.set_actor_state(ActorStateType::Blocking, now);
    def.blocking_side = ActiveSide::Right;
    def.block_raised_at = Some(now);
    def.blocking_until = Some(now + std::time::Duration::from_secs(2));
    let b = m.resolve_attack(&lo, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0, now);
    rows.push(("optimal-block Slashing", slash_of(&b), 0.77));
    rows.push(("optimal-block Poison", poison_of(&b), 68.65));

    let mut amped = blank();
    for _ in 0..8 {
        amped.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    let big = m.resolve_attack(&lo, &amped, DamageSource::Attack, ActiveSide::Right, 1.0, 4, now);
    rows.push(("conditioned Poison", poison_of(&big), 205.36));
    rows.push(("deep-combo total", big.total, 674.66));

    println!("\n  s506 anchor    | emitted  | recorded | delta");
    println!("  ---------------|----------|----------|-------");
    for (name, got, want) in &rows {
        println!("  {name:<14} | {got:>8.2} | {want:>8.2} | {:+.2}", got - want);
    }
    // Every anchor within 1.5 % of the recorded value (the block residual is the widest).
    for (name, got, want) in &rows {
        if *want < 1.0 {
            assert!(*got <= 1.0, "{name}: emitted {got:.2}, recorded {want:.2}");
        } else {
            let err = (got - want).abs() / want;
            assert!(err <= 0.015, "{name}: emitted {got:.2} vs recorded {want:.2} ({:.2} %)", err * 100.0);
        }
    }
}

/// Without the defender's armor the SAME weapon emits its raw tempered base — proof
/// that the 113.82 anchor is `weapon − armor` and not a magic number in the fixture.
#[test]
fn unarmored_target_takes_the_raw_tempered_base() {
    let m = RetailDamageModel;
    let rd = m.resolve_attack(
        &flappety_dagger(),
        &naked_blank(),
        DamageSource::Attack,
        ActiveSide::Right,
        1.0,
        0,
        Instant::now(),
    );
    assert!((slash_of(&rd) - 144.0).abs() < 0.05, "raw tempered base, got {:.2}", slash_of(&rd));
}
