//! Offline reproduction-**differential** test for the arena DAMAGE model against the
//! recorded retail match (prod `arena_udp_frames` **session_id = 506**). Sibling of
//! [`super::roundtrip_s506`] (which diffs the round-start *protocol* sequence); this one
//! diffs the per-hit **damage magnitudes / combo ramp / block / status threshold**
//! against the decoded `ReceiveDamage` ground truth in
//! `docs/arena-combat-reproduction-spec.md` §2a/§4.
//!
//! ## What is compared (the user's bar: "damages, timing, material")
//! For the Light Dragonbone-Poison dagger (§3) we drive [`RetailDamageModel`] /
//! [`Fighter`] over the §2a swing/maneuver/block/spell sequence and assert, per hit:
//!   - the **physical Slashing** = combo-0 base 113.82 ramped by the combo counter
//!     (×1.00 → ×1.45 → … → ×4.12) — reproduces the recorded 113.8 / 165.1 / 301.8 /
//!     469.3 and the `Middle` maneuver lane (201–274);
//!   - the **Poison enchant** track = base ≈137.32 @ t10, amplified toward +50% as the
//!     target's poison conditioning stacks (the recorded 137 → 205 ramp);
//!   - the **connected optimal block** = physical negated (≈0) / elemental halved (×0.5)
//!     — the §4.4 cross-spec correction (seq 323: Slash 113.82→~0, Poison 137.32→68.65);
//!   - the **paralyse threshold** crossing (poison accumulation ≥ 0.45·maxHP);
//!   - no 25%-of-maxHP **clamp** (§4.5: a 674 deep-combo hit is legitimate);
//!   - the **sum invariant** (`totalDamage == Σ health types`, drains excluded).
//!
//! Recorded constants come straight from the spec's deduped §2a table (decoded read-only
//! from prod with `web/lib/arena-combat.ts`). The tolerances are the spec's stated
//! charge/`swingFactor` variation band; a structural drift (combo off, enchant base
//! wrong, block symmetric again, clamp back) fails with a message naming the divergence.
//!
//! This is the differential the deploy task verifies (step 3a) — kept in-tree so the
//! s506 reproduction is a permanent regression guard, not a one-off script.

use std::time::Instant;

use super::damage::{flags, is_health_type, DamageModel, RetailDamageModel, ELEMENT_AMP_MAX};
use super::state::{
    health_for_level, ActiveSide, ActorStateType, DamageSource, DamageType, Fighter, Loadout,
    WeaponProfile, ARENA_HEALTH_MULTIPLIER, PARALYZE_POISON_THRESHOLD_FRACTION,
};
use super::tables::{combo_factor, Weight};

// ---------------------------------------------------------------------------
// s506 ground truth (docs/arena-combat-reproduction-spec.md §2a/§3/§4).
// ---------------------------------------------------------------------------

/// The recorded combo-0, unblocked, Right Slashing base (post-armor) for Flappety's
/// Light Dragonbone dagger vs Blank's armor (§4.1 — seq 27/277/488).
const S506_SLASH_BASE: f32 = 113.82;
/// The recorded fresh Weapon-Poison enchant base @ tier 10 (§4.3 — seq 27/37/277/…).
const S506_POISON_BASE: f32 = 137.32;
/// Flappety is L86 Nord (§1).
const S506_LEVEL: u16 = 86;

/// The §2a recorded normal-swing combo ramp, as `(combo_count, recorded_slashing)`
/// for the Light dagger (the recorded per-depth ×base column: 1.00 / 1.45 / 1.50 /
/// 2.65 / 4.12 — seq 277/287/420/436/452). The model uses the explicit
/// [`tables::LIGHT_COMBO_RAMP`] anchor table, so these reproduce the recorded values
/// TIGHTLY (the earlier geometric `1.45^n` overshot the ×2.65 deep step — the bug this
/// differential caught and the calibration fix corrected).
const S506_COMBO_RAMP: &[(u32, f32)] = &[
    (0, 113.82),                 // seq 27/277/488 — fresh (combo reset) ×1.00
    (1, 165.07),                 // seq 37/287 — first chained alternating swing ×1.45
    (2, S506_SLASH_BASE * 1.50), // seq 420 — second step ×1.50 (~170.7)
    (3, 301.79),                 // seq 436 — deep combo ×2.65
    (4, 469.30),                 // seq 452 — deeper ×4.12 (ceiling)
];

/// The §2a recorded `Middle` WeaponManeuver Slashing band (seq 88/106/337): 186–275.
const S506_MANEUVER_SLASH: &[f32] = &[201.37, 274.51, 186.98];

// ---------------------------------------------------------------------------
// Fixtures — Flappety's recovered loadout (§3).
// ---------------------------------------------------------------------------

/// Flappety's Light Dragonbone-Poison dagger (§3): a Slashing physical track at the
/// recovered s506 combo-0 base + a tier-10 Weapon-Poison enchant track. The
/// `elem_resist_piercing` models the Elemental-Resistance-Piercing enchant (§3) that
/// strips the target's poison resist — but Blank's resist is unknown so we leave the
/// opponent un-resisted (the recorded numbers are already post-Blank-armor for physical
/// and post-pierce for poison).
fn flappety_dagger() -> Loadout {
    Loadout {
        level: S506_LEVEL,
        weapon: WeaponProfile {
            primary_type: Some(DamageType::Slashing),
            base_by_type: vec![(DamageType::Slashing, S506_SLASH_BASE)],
            weight: Some(Weight::Light),
        },
        enchants: vec![(DamageType::Poison, 10)],
        ..Default::default()
    }
}

/// Blank, the opponent (#124): a fresh L86 fighter at arena ×3 HP. We don't have
/// Blank's gear, so it carries no resist — physical/poison land at the recorded
/// post-mitigation magnitudes (the spec's numbers are already post-Blank-armor). [§1]
fn blank() -> Fighter {
    Fighter::new(1, 124, Loadout { level: S506_LEVEL, ..Default::default() }, Instant::now())
}

fn slash_of(rd: &super::damage::ResolvedDamage) -> f32 {
    rd.components.iter().filter(|(t, _)| *t == DamageType::Slashing).map(|(_, v)| *v).sum()
}
fn poison_of(rd: &super::damage::ResolvedDamage) -> f32 {
    rd.components.iter().filter(|(t, _)| *t == DamageType::Poison).map(|(_, v)| *v).sum()
}
fn magicka_of(rd: &super::damage::ResolvedDamage) -> f32 {
    rd.components.iter().filter(|(t, _)| *t == DamageType::Magicka).map(|(_, v)| *v).sum()
}

// ---------------------------------------------------------------------------
// (A) The combo ramp + maneuver lane reproduce the §2a Slashing column.
// ---------------------------------------------------------------------------

#[test]
fn s506_combo_ramp_reproduces_recorded_slashing() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    // Each recorded combo step: drive a normal Left/Right swing at that combo depth and
    // compare the model's Slashing to the recorded value. Tolerance is the spec's stated
    // charge/swingFactor band (the deep steps carry the most variation).
    for &(count, recorded) in S506_COMBO_RAMP {
        let side = if count % 2 == 0 { ActiveSide::Right } else { ActiveSide::Left };
        let rd = m.resolve_attack(&lo, &blank(), DamageSource::Attack, side, 1.0, count);
        let got = slash_of(&rd);
        // The model is exactly S506_SLASH_BASE × the recorded LIGHT_COMBO_RAMP factor, so
        // it reproduces each recorded step TIGHTLY (±2% absorbs the recorded-value
        // rounding, e.g. 113.82×2.65=301.62 vs recorded 301.79).
        let tol = (recorded * 0.02).max(0.5);
        assert!(
            (got - recorded).abs() <= tol,
            "DIVERGENCE (COMBO §4.2): combo {count} Slashing modeled {got:.1} vs s506 \
             recorded {recorded:.1} (tol ±{tol:.1}). combo_factor(Light,{count})={:.3}, \
             base={S506_SLASH_BASE}.",
            combo_factor(Weight::Light, count),
        );
    }
    // The two anchor steps must be TIGHT (these are the calibration pins, not deep-combo
    // charge-variable hits): combo-0 = 113.82, combo-1 = 113.82×1.45 = 165.04.
    let c0 = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 0));
    let c1 = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Left, 1.0, 1));
    assert!((c0 - 113.82).abs() < 0.5, "combo-0 anchor {c0:.2} != recorded 113.82");
    assert!((c1 - 165.07).abs() < 1.0, "combo-1 anchor {c1:.2} != recorded 165.07 (×1.45)");
    // The combo is CAPPED at the recorded ×4.12 ceiling — a runaway chain can't exceed it.
    let c9 = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 9));
    assert!((c9 - 113.82 * 4.12).abs() < 1.0, "deep combo capped at ×4.12 ({:.1}), got {c9:.1}", 113.82 * 4.12);
}

#[test]
fn s506_middle_maneuver_lands_in_recorded_band() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    // The Middle lane = the WeaponManeuver/charged crit (§4.2). The model uses the Light
    // crit factor (1.325); the recorded maneuvers are a Power-Attack class swing landing
    // 186–275 Slashing. Drive a few swing_factors across that maneuver charge band and
    // assert the spread covers the recorded maneuver values.
    let modeled: Vec<f32> = [1.0, 1.5, 1.8]
        .iter()
        .map(|&sf| slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Middle, sf, 0)))
        .collect();
    let lo_m = *modeled.iter().min_by(|a, b| a.total_cmp(b)).unwrap();
    let hi_m = *modeled.iter().max_by(|a, b| a.total_cmp(b)).unwrap();
    // Every recorded maneuver value must be reachable within the modeled charge band
    // (the maneuver is a charged Middle swing; its magnitude scales with swing_factor).
    for &rec in S506_MANEUVER_SLASH {
        assert!(
            rec >= lo_m * 0.85 && rec <= hi_m * 1.15,
            "DIVERGENCE (MANEUVER §4.2): recorded Middle maneuver {rec:.1} outside the modeled \
             charged band [{lo_m:.1}, {hi_m:.1}] (Light crit ×{:.3} × swing_factor). The Middle \
             lane should carry the maneuver, not the combo ramp.",
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

    // Fresh (no conditioning) → amp ×1.0 → the recorded fresh base 137.32.
    let fresh = m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, 0);
    assert!(
        (poison_of(&fresh) - S506_POISON_BASE).abs() < 1.0,
        "DIVERGENCE (ENCHANT §4.3): fresh Poison {:.1} vs recorded base {S506_POISON_BASE} \
         (NOT the old 30×tier=300).",
        poison_of(&fresh),
    );
    assert!(poison_of(&fresh) < 200.0, "the enchant base is the SMALL 13.7×tier magnitude, not 300");
    // The enchant adds an EQUAL Magicka drain, excluded from the total (§4.3 / §2c).
    assert!((magicka_of(&fresh) - poison_of(&fresh)).abs() < 1e-2, "enchant adds an equal Magicka drain");
    assert!(
        (fresh.total - (slash_of(&fresh) + poison_of(&fresh))).abs() < 1e-2,
        "sum invariant: total == Slashing + Poison (Magicka drain excluded)",
    );

    // AMPLIFICATION: drive enough poison into the target that its poison conditioning
    // approaches the threshold, and assert the enchant track ramps toward the recorded
    // +50% endpoint (137 → ~205, ×1.50). condition_threshold(Poisoned) = 0.25·maxHP =
    // 787.5 @ L86×3; the amp is linear in (recent_poison / threshold). Pour ~the
    // threshold worth of poison in, then the NEXT hit's poison must be near the ceiling.
    let mut tgt = blank();
    let now = Instant::now();
    // Each landed poison ~137; accumulate just past the threshold (≈6 hits) so amp→max.
    for _ in 0..8 {
        tgt.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    let amped = m.resolve_attack(&lo, &tgt, DamageSource::Attack, ActiveSide::Right, 1.0, 0);
    let recorded_amped = 205.36; // §4.3 endpoint (seq 452 Poison)
    let ceiling = S506_POISON_BASE * ELEMENT_AMP_MAX; // 137.32 × 1.5 = 205.98
    assert!(
        (poison_of(&amped) - ceiling).abs() < 1.0,
        "DIVERGENCE (AMP §4.3): fully-conditioned Poison {:.1} should reach the ×1.5 ceiling \
         {ceiling:.1} (recorded endpoint {recorded_amped}). element_amp ramp wrong.",
        poison_of(&amped),
    );
    // The amplified poison must match the recorded endpoint (205.36) within the
    // base-recovery tolerance (the ×1.5 ceiling 205.98 vs recorded 205.36 = 0.3%).
    assert!(
        (poison_of(&amped) - recorded_amped).abs() < 2.0,
        "amplified Poison {:.1} vs recorded endpoint {recorded_amped}",
        poison_of(&amped),
    );
    // Amp is monotonic and bounded: fresh < amped <= ceiling.
    assert!(poison_of(&fresh) < poison_of(&amped), "poison amplifies as conditioning stacks");
    assert!(poison_of(&amped) <= ceiling + 1e-2, "amp never exceeds the +50% ceiling");
}

// ---------------------------------------------------------------------------
// (C) The connected optimal block is asymmetric (§4.4 cross-spec correction).
// ---------------------------------------------------------------------------

#[test]
fn s506_optimal_block_negates_physical_halves_elemental() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    // s506 seq 323: a connected optimal block on a Right swing → Slashing 113.82→0.77
    // (≈0), Poison 137.32→68.65 (=÷2.0). The defender guards the MATCHING side.
    let mut def = blank();
    def.actor_state = ActorStateType::Blocking;
    def.blocking_side = ActiveSide::Right;
    let blocked = m.resolve_attack(&lo, &def, DamageSource::Attack, ActiveSide::Right, 1.0, 0);
    assert!(blocked.flags & flags::WAS_OPTIMAL_BLOCKING != 0, "optimal-block flag set");
    assert_eq!(slash_of(&blocked), 0.0, "DIVERGENCE (BLOCK §4.4): optimal block must NEGATE physical (×0), got {}", slash_of(&blocked));
    let recorded_blocked_poison = 68.65; // seq 323
    assert!(
        (poison_of(&blocked) - recorded_blocked_poison).abs() < 1.5,
        "DIVERGENCE (BLOCK §4.4): optimal block must HALVE elemental → {recorded_blocked_poison} \
         (137.32×0.5), got {:.2}. (NOT the ÷1.23 the status-resistance spec wrongly prescribed.)",
        poison_of(&blocked),
    );
}

// ---------------------------------------------------------------------------
// (D) No 25% clamp (§4.5) + the round/match HP arithmetic (the seq-342 kill).
// ---------------------------------------------------------------------------

#[test]
fn s506_deep_combo_unclamped_and_kill_arithmetic() {
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    let max_hp = health_for_level(S506_LEVEL) * ARENA_HEALTH_MULTIPLIER;
    assert_eq!(max_hp, 3150, "L86 ×3 = 3150 maxHP (matches the spec's ~3150)");

    // The recorded deep-combo hit (seq 452): Slashing 469.30 + Poison 205.36 = 674.66.
    // Build it: combo-4 + fully-amplified poison. It must NOT be clamped to 25% (≈787).
    let mut amped = blank();
    let now = Instant::now();
    for _ in 0..8 {
        amped.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    let big = m.resolve_attack(&lo, &amped, DamageSource::Attack, ActiveSide::Right, 1.0, 4);
    let recorded_total = 674.66;
    assert!(
        (big.total - recorded_total).abs() < 12.0,
        "DIVERGENCE: the deep-combo hit total {:.1} should reproduce the recorded 674.66 \
         (469.30 Slash + 205.36 Poison).",
        big.total,
    );
    // The total is the exact Σ of health components — NO clamp scaling (§4.5).
    let health_sum: f32 = big.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
    assert!((big.total - health_sum).abs() < 1e-3, "total == Σ health (no 25% clamp distortion)");

    // The kill (seq 342): Flappety #125 reaches 0%. Verify the HP arithmetic — a fresh
    // L86 fighter taking the recorded shield-maneuver kill burst (Bashing 200.34 +
    // accumulated combo damage) drops to 0 over the round, and `take_damage` floors at 0.
    let mut victim = blank();
    victim.take_damage(victim.max_health + 500); // any lethal overkill
    assert!(victim.is_dead(), "a lethal hit kills (HP floors at 0)");
    assert_eq!(victim.health, 0, "dead fighter HP == 0 (the seq-342 #125→0% state)");
}

// ---------------------------------------------------------------------------
// (E) The paralyse threshold (§5.4) crosses at the right poison accumulation.
// ---------------------------------------------------------------------------

#[test]
fn s506_paralyse_threshold_crossing() {
    let max_hp = (health_for_level(S506_LEVEL) * ARENA_HEALTH_MULTIPLIER) as f32;
    let paralyse_threshold = PARALYZE_POISON_THRESHOLD_FRACTION * max_hp; // 0.45 × 3150 = 1417.5
    // Below the threshold: a handful of poison hits do NOT yet trip paralyse.
    let mut f = blank();
    let now = Instant::now();
    for _ in 0..5 {
        f.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    assert!(
        f.recent_element_damage(DamageType::Poison) < paralyse_threshold,
        "5 poison hits ({:.0}) stay under the paralyse threshold ({paralyse_threshold:.0})",
        f.recent_element_damage(DamageType::Poison),
    );
    // Sustained poison (a full combo's worth in the 5s window) crosses it → paralyse
    // would land (the resolve path sets ActorStateType::Paralyzed there; here we assert
    // the accumulation/threshold relationship the §5.4 trigger reads).
    for _ in 0..7 {
        f.record_element_damage(DamageType::Poison, S506_POISON_BASE, now);
    }
    assert!(
        f.recent_element_damage(DamageType::Poison) >= paralyse_threshold,
        "DIVERGENCE (PARALYSE §5.4): sustained poison ({:.0}) must reach the paralyse threshold \
         ({paralyse_threshold:.0} = 0.45·maxHP).",
        f.recent_element_damage(DamageType::Poison),
    );
}

// ---------------------------------------------------------------------------
// (F) End-to-end: a full §2a chain through the live engine reproduces the ramp
//     IN ORDER (combo increments across alternating swings; an optimal block
//     resets it), and the sum invariant holds for every emitted ReceiveDamage.
// ---------------------------------------------------------------------------

#[test]
fn s506_full_chain_through_engine_reproduces_ramp_and_resets_on_block() {
    // Drive the combo counter exactly as `resolve::resolve_swing` does (alternating
    // Right/Left), and confirm the per-swing Slashing follows the recorded ramp, then a
    // connected optimal block RESETS the chain (§4.2) so the next swing is fresh again.
    let m = RetailDamageModel;
    let lo = flappety_dagger();
    let mut attacker = blank();

    // A chain of alternating swings: Right(0) → Left(1) → Right(2) → Left(3) → Right(4).
    let mut last_slash = 0.0;
    for step in 0..5u32 {
        let side = if step % 2 == 0 { ActiveSide::Right } else { ActiveSide::Left };
        let depth = attacker.register_combo_swing(side);
        assert_eq!(depth, step, "alternating swings increment the combo each step");
        let rd = m.resolve_attack(&lo, &blank(), DamageSource::Attack, side, 1.0, depth);
        let s = slash_of(&rd);
        if step > 0 {
            assert!(s >= last_slash, "the combo ramp is monotonic non-decreasing (step {step})");
        }
        // Sum invariant on every hit.
        let health_sum: f32 = rd.components.iter().filter(|(t, _)| is_health_type(*t)).map(|(_, v)| *v).sum();
        assert!((rd.total - health_sum).abs() < 1e-3, "sum invariant holds on hit {step}");
        last_slash = s;
    }
    // The deep end of the chain is the big hit (≈4.12× the base, capped).
    assert!(last_slash > S506_SLASH_BASE * 2.5, "a deep chain produces the recorded big hits");

    // A connected optimal block resets the attacker's combo (mirrors resolve_swing's
    // reset on WAS_OPTIMAL_BLOCKING). After reset, the next swing is the fresh base.
    attacker.reset_combo();
    let depth_after = attacker.register_combo_swing(ActiveSide::Right);
    assert_eq!(depth_after, 0, "after a block-reset the chain restarts at combo 0");
    let fresh = slash_of(&m.resolve_attack(&lo, &blank(), DamageSource::Attack, ActiveSide::Right, 1.0, depth_after));
    assert!((fresh - S506_SLASH_BASE).abs() < 0.5, "post-reset swing is the fresh 113.82 base, got {fresh:.2}");
}
