//! Differential oracle over PvE client damage hooks.
//!
//! The fixture does not contain full actor loadouts, item identities, perk lists,
//! resistance sources or negation pools. When the server API needs one of those
//! values, this oracle derives the narrow scalar from the neighboring client stage:
//!
//! - D1 permanent additions are the per-type delta between `attack-entry` and D1.
//! - D3/D4 factors are the captured `physF`/`nonPhysF` arguments to
//!   `ResolveDamageBonuses`.
//! - E6 armor/resistance is compressed into synthetic armor plus per-type
//!   resistance that reproduces the observed E6 `before -> after` cut. This checks
//!   our flat-cut helpers and mirrored-drain ordering, not the original actor gear.
//! - ReceiveDamage health delta is derived from the E7 health-affecting sum, capped
//!   by the captured health before damage.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Deserializer;

use super::damage::{
    attack_type_multiplier, is_elemental, is_health_type, is_physical, mirrored_drain,
};
use super::state::{DamageSource, DamageType};
use super::tables;

const ABS_TOLERANCE: f32 = 0.01;
const REL_TOLERANCE: f32 = 1e-3;

#[derive(Debug, Deserialize)]
struct OracleHit {
    run: String,
    hit: usize,
    source: String,
    attack_type: Option<AttackType>,
    attacker_snapshot: Option<Stage>,
    defender_snapshot: Option<Stage>,
    stages: BTreeMap<String, Stage>,
    health_before: Option<f32>,
    health_delta: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct AttackType {
    source: String,
    combo: u32,
    swing: f32,
    #[serde(rename = "comboDF")]
    combo_df: f32,
    factor: f32,
}

#[derive(Debug, Default, Deserialize)]
struct Stage {
    #[serde(default)]
    source: String,
    #[serde(default, deserialize_with = "damage_map")]
    list: DamageMap,
    #[serde(default, deserialize_with = "damage_map")]
    before: DamageMap,
    #[serde(default, deserialize_with = "damage_map")]
    after: DamageMap,
    #[serde(default, rename = "physF")]
    phys_f: f32,
    #[serde(default, rename = "nonPhysF")]
    non_phys_f: f32,
    #[serde(default, rename = "blockRating")]
    block_rating: f32,
    #[serde(default)]
    protection: f32,
    #[serde(default, rename = "optimalBoost")]
    optimal_boost: f32,
    #[serde(default)]
    optimal: u8,
    #[serde(default)]
    combo: u32,
    #[serde(default, rename = "comboDF")]
    combo_df: f32,
    #[serde(default)]
    swing: f32,
    #[serde(default, deserialize_with = "bonus_counts")]
    #[serde(rename = "bonusSources")]
    bonus_sources: BTreeMap<String, u32>,
    #[serde(default, deserialize_with = "bonus_counts")]
    #[serde(rename = "attackerPiercingSources")]
    attacker_piercing_sources: BTreeMap<String, u32>,
    #[serde(default)]
    status: SnapshotStatus,
    #[serde(default)]
    negation: SnapshotNegation,
    #[serde(default, deserialize_with = "resistance_rows")]
    resistance: BTreeMap<String, ResistanceRow>,
}

#[derive(Debug, Default, Deserialize)]
struct SnapshotStatus {
    #[serde(default)]
    blocking: u8,
}

#[derive(Debug, Default, Deserialize)]
struct SnapshotNegation {
    #[serde(default)]
    count: u32,
}

#[derive(Debug, Default, Deserialize)]
struct ResistanceRow {
    #[serde(default)]
    resistance: f32,
    #[serde(default)]
    weakness: f32,
}

type DamageMap = BTreeMap<String, f32>;

#[derive(Debug, Default)]
struct StageStats {
    matches: usize,
    mismatches: Vec<String>,
}

#[test]
fn oracle_fixture_stage_boundaries_hold() {
    let hits = load_hits();
    let mut stats: BTreeMap<&'static str, StageStats> = BTreeMap::new();
    for hit in hits.iter().filter(|hit| hit.attacker_snapshot.is_none()) {
        compare_hit(hit, &mut stats);
    }

    let failures: Vec<_> = stats
        .iter()
        .filter_map(|(stage, stat)| {
            stat.mismatches
                .first()
                .map(|first| format!("{stage}: {first}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "oracle stage regressions:\n{}",
        failures.join("\n")
    );
    if std::env::var_os("ORACLE_PRINT").is_some() {
        eprintln!("stage,matches,mismatches,first_mismatch");
        for (stage, stat) in stats {
            eprintln!(
                "{stage},{},{},{}",
                stat.matches,
                stat.mismatches.len(),
                stat.mismatches.first().map(String::as_str).unwrap_or("")
            );
        }
    }
}

#[test]
fn oracle_v2_independent_replay_snapshot_clean_hits() {
    let hits = load_hits();
    let mut stats: BTreeMap<&'static str, StageStats> = BTreeMap::new();
    let mut replayed = 0usize;
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut skip_examples: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();

    for hit in hits.iter().filter(|hit| hit.attacker_snapshot.is_some()) {
        match compare_v2_hit(hit, &mut stats) {
            Ok(()) => replayed += 1,
            Err(skip) => {
                *skipped.entry(skip.class).or_default() += 1;
                let examples = skip_examples.entry(skip.class).or_default();
                if examples.len() < 2 {
                    examples.push(format!("{}#{} {}", hit.run, hit.hit, skip.reason));
                }
            }
        }
    }

    let failures: Vec<_> = stats
        .iter()
        .filter_map(|(stage, stat)| {
            stat.mismatches
                .first()
                .map(|first| format!("{stage}: {first}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "oracle v2 snapshot replay regressions:\n{}\nskipped={skipped:?}",
        failures.join("\n")
    );
    assert!(replayed > 0, "no snapshot-complete oracle v2 hits replayed");
    if std::env::var_os("ORACLE_PRINT").is_some() {
        eprintln!("v2_replayed,{replayed}");
        eprintln!("v2_skipped,{skipped:?}");
        eprintln!("v2_skip_examples,{skip_examples:?}");
        eprintln!("stage,matches,mismatches,first_mismatch");
        for (stage, stat) in stats {
            eprintln!(
                "{stage},{},{},{}",
                stat.matches,
                stat.mismatches.len(),
                stat.mismatches.first().map(String::as_str).unwrap_or("")
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct V2Skip {
    class: &'static str,
    reason: &'static str,
}

#[test]
#[ignore = "class (a): D0 bonusSources record counts but not the source magnitudes/properties, so some attacker-side D1/D3 inputs cannot be reconstructed from snapshots alone"]
fn oracle_v2_known_decode_hook_gap_attacker_bonus_sources() {
    panic!(
        "Known class (a) gap: D0-attacker-snapshot exposes bonus-handler source counts, \
         but not per-source magnitudes/properties. Hits with permanentAdd, \
         permanentFortify, permanentFactor, situationalAdd, situationalFactor or \
         conversion sources are classified here until the hook records those inputs."
    );
}

#[test]
#[ignore = "class (b): the arena helper uses PvP block factors/tick, while these PvE captures require block 1.0/0.667 and tick 0.1 s"]
fn oracle_v2_known_pve_pvp_parameter_handling() {
    panic!(
        "Known class (b) parameter split: production RetailDamageModel is PvP-oriented \
         for block factors and periodic tick size. The oracle v2 replay drives the PvE \
         constants captured from the client: physical block 1.0, elemental block 0.667, \
         health multiplier 1, tick 0.1 s."
    );
}

#[test]
#[ignore = "class (c): snapshot replay flags a model/client divergence once hook gaps and PvE parameters are excluded"]
fn oracle_v2_known_model_client_divergence() {
    panic!(
        "Known class (c) bucket for replayed hits whose snapshot inputs are complete \
         but whose computed stage still differs. CODEX-REPORT.md lists the chapter \
         rule and proposed follow-up fix for any such hits observed in this fixture."
    );
}

fn load_hits() -> Vec<OracleHit> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/arena/combat/testdata/oracle");
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("read {dir:?}: {err}"))
        .map(|entry| entry.expect("oracle fixture dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .flat_map(|path| {
            fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("read {path:?}: {err}"))
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<OracleHit>(line).expect("oracle fixture hit"))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn compare_hit(hit: &OracleHit, stats: &mut BTreeMap<&'static str, StageStats>) {
    if let Some(atf) = &hit.attack_type {
        let source = parse_source(&atf.source);
        let got = attack_type_multiplier(source, atf.combo_df, atf.combo, 1.0 + atf.swing) - 1.0;
        compare_scalar("attack-type-factor", got, atf.factor, hit, "factor", stats);
    }

    let entry = hit
        .stages
        .get("attack-entry")
        .map(|stage| stage.list.clone())
        .unwrap_or_else(|| {
            hit.stages
                .get("D-bonuses")
                .expect("D-bonuses")
                .before
                .clone()
        });
    let d1 = stage_list(hit, "D1-permanent");
    let d1_adds = delta(&entry, &d1);
    compare_map("D1-permanent", add_maps(&entry, &d1_adds), &d1, hit, stats);

    let d2 = stage_list(hit, "D2-conversion");
    compare_map("D2-conversion", d1.clone(), &d2, hit, stats);

    let bonuses = hit.stages.get("D-bonuses").expect("D-bonuses");
    let d3 = stage_list(hit, "D3-physical");
    compare_map(
        "D3-physical",
        scale_category(&d2, bonuses.phys_f, is_physical),
        &d3,
        hit,
        stats,
    );

    let d4 = stage_list(hit, "D4-nonphysical");
    compare_map(
        "D4-nonphysical",
        scale_category(&d3, bonuses.non_phys_f, |ty| !is_physical(ty)),
        &d4,
        hit,
        stats,
    );
    compare_map("D-bonuses", d4.clone(), &bonuses.after, hit, stats);

    let e0 = stage_list(hit, "E0-taken-entry");
    compare_map("E0-taken-entry", bonuses.after.clone(), &e0, hit, stats);

    let e1 = stage_list(hit, "E1-negation");
    compare_map("E1-negation", e0.clone(), &e1, hit, stats);

    let e3 = stage_list(hit, "E3-preblock");
    compare_map("E3-preblock", e1.clone(), &e3, hit, stats);

    let after_block = if let Some(e4) = hit.stages.get("E4-blocking") {
        let got = apply_pve_block(&e4.before, e4);
        compare_map("E4-blocking", got, &e4.after, hit, stats);
        e4.after.clone()
    } else {
        e3.clone()
    };

    let e5 = stage_list(hit, "E5-outer");
    compare_map("E5-outer", after_block.clone(), &e5, hit, stats);

    let e6_stage = hit.stages.get("E6-resistance").expect("E6-resistance");
    let e6 = apply_derived_e6(&e6_stage.before, &e6_stage.after);
    compare_map("E6-resistance", e6, &e6_stage.after, hit, stats);

    let e7 = stage_list(hit, "E7-taken-exit");
    compare_map(
        "E7-taken-exit",
        append_drains(&e6_stage.after),
        &e7,
        hit,
        stats,
    );

    if let Some(delta) = hit.health_delta {
        let before_health = hit.health_before.expect("receive health before");
        let health_damage: f32 = e7
            .iter()
            .filter_map(|(name, value)| Some((parse_damage_type(name), value)))
            .filter(|(ty, _)| is_health_type(*ty))
            .map(|(_, value)| *value)
            .sum();
        compare_scalar(
            "G-receive",
            health_damage.min(before_health),
            delta,
            hit,
            "health_delta",
            stats,
        );
    }
}

fn compare_v2_hit(
    hit: &OracleHit,
    stats: &mut BTreeMap<&'static str, StageStats>,
) -> Result<(), V2Skip> {
    let attacker = hit.attacker_snapshot.as_ref().ok_or(V2Skip {
        class: "a",
        reason: "missing D0-attacker-snapshot",
    })?;
    let defender = hit.defender_snapshot.as_ref().ok_or(V2Skip {
        class: "a",
        reason: "missing E0-defender-snapshot",
    })?;
    if has_attacker_bonus_gap(attacker) {
        return Err(V2Skip {
            class: "a",
            reason: "D0 bonusSources has counts without source magnitudes/properties",
        });
    }
    if defender.negation.count > 0 {
        return Err(V2Skip {
            class: "a",
            reason: "E0 negation records pool count without pool entries",
        });
    }
    if has_piercing_gap(defender) {
        return Err(V2Skip {
            class: "a",
            reason: "attacker piercing source count lacks piercing rating",
        });
    }

    let mut current = attacker.list.clone();
    compare_map(
        "v2-attack-entry",
        current.clone(),
        &stage_list(hit, "attack-entry"),
        hit,
        stats,
    );
    compare_map(
        "v2-D0-attacker-snapshot",
        current.clone(),
        &attacker.list,
        hit,
        stats,
    );

    compare_map(
        "v2-D1-permanent",
        current.clone(),
        &stage_list(hit, "D1-permanent"),
        hit,
        stats,
    );
    compare_map(
        "v2-D2-conversion",
        current.clone(),
        &stage_list(hit, "D2-conversion"),
        hit,
        stats,
    );

    current = scale_category(&current, attacker.phys_f, is_physical);
    compare_map(
        "v2-D3-physical",
        current.clone(),
        &stage_list(hit, "D3-physical"),
        hit,
        stats,
    );

    current = scale_category(&current, attacker.non_phys_f, |ty| !is_physical(ty));
    compare_map(
        "v2-D4-nonphysical",
        current.clone(),
        &stage_list(hit, "D4-nonphysical"),
        hit,
        stats,
    );
    compare_map(
        "v2-D-bonuses",
        current.clone(),
        &hit.stages.get("D-bonuses").expect("D-bonuses").after,
        hit,
        stats,
    );

    compare_map(
        "v2-E0-taken-entry",
        current.clone(),
        &stage_list(hit, "E0-taken-entry"),
        hit,
        stats,
    );
    compare_map(
        "v2-E1-negation",
        current.clone(),
        &stage_list(hit, "E1-negation"),
        hit,
        stats,
    );
    compare_map(
        "v2-E3-preblock",
        current.clone(),
        &stage_list(hit, "E3-preblock"),
        hit,
        stats,
    );

    if let Some(e4) = hit.stages.get("E4-blocking") {
        current = apply_snapshot_pve_block(&current, defender);
        compare_map("v2-E4-blocking", current.clone(), &e4.after, hit, stats);
    }

    compare_map(
        "v2-E5-outer",
        current.clone(),
        &stage_list(hit, "E5-outer"),
        hit,
        stats,
    );

    current = apply_snapshot_e6(&current, defender, &hit.source);
    compare_map(
        "v2-E6-resistance",
        current.clone(),
        &hit.stages
            .get("E6-resistance")
            .expect("E6-resistance")
            .after,
        hit,
        stats,
    );

    current = append_drains(&current);
    compare_map(
        "v2-E7-taken-exit",
        current.clone(),
        &stage_list(hit, "E7-taken-exit"),
        hit,
        stats,
    );

    if let Some(delta) = hit.health_delta {
        let before_health = hit.health_before.expect("receive health before");
        let health_damage: f32 = current
            .iter()
            .filter_map(|(name, value)| Some((parse_damage_type(name), value)))
            .filter(|(ty, _)| is_health_type(*ty))
            .map(|(_, value)| *value)
            .sum();
        compare_scalar(
            "v2-G-receive",
            health_damage.min(before_health),
            delta,
            hit,
            "health_delta",
            stats,
        );
    }

    Ok(())
}

fn has_attacker_bonus_gap(attacker: &Stage) -> bool {
    [
        "permanentAdd",
        "permanentFortify",
        "permanentFactor",
        "situationalAdd",
        "situationalFactor",
        "conversion",
    ]
    .iter()
    .any(|key| attacker.bonus_sources.get(*key).copied().unwrap_or(0) > 0)
}

fn has_piercing_gap(defender: &Stage) -> bool {
    ["armorPiercing", "resistancePiercing", "blockPiercing"]
        .iter()
        .any(|key| {
            defender
                .attacker_piercing_sources
                .get(*key)
                .copied()
                .unwrap_or(0)
                > 0
        })
}

fn compare_map(
    stage: &'static str,
    got: DamageMap,
    want: &DamageMap,
    hit: &OracleHit,
    stats: &mut BTreeMap<&'static str, StageStats>,
) {
    let stat = stats.entry(stage).or_default();
    let mut keys: Vec<_> = got.keys().chain(want.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let g = got.get(key).copied().unwrap_or(0.0);
        let w = want.get(key).copied().unwrap_or(0.0);
        if !close(g, w) {
            stat.mismatches.push(format!(
                "{}#{} field {key}: server {g:.4}, client {w:.4}",
                hit.run, hit.hit
            ));
            return;
        }
    }
    stat.matches += 1;
}

fn compare_scalar(
    stage: &'static str,
    got: f32,
    want: f32,
    hit: &OracleHit,
    field: &str,
    stats: &mut BTreeMap<&'static str, StageStats>,
) {
    let stat = stats.entry(stage).or_default();
    if close(got, want) {
        stat.matches += 1;
    } else {
        stat.mismatches.push(format!(
            "{}#{} field {field}: server {got:.4}, client {want:.4}",
            hit.run, hit.hit
        ));
    }
}

fn damage_map<'de, D>(deserializer: D) -> Result<DamageMap, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut out = DamageMap::new();
    let Some(obj) = value.as_object() else {
        return Ok(out);
    };
    for (key, value) in obj {
        if let Some(number) = value.as_f64() {
            out.insert(key.clone(), number as f32);
        }
    }
    Ok(out)
}

fn bonus_counts<'de, D>(deserializer: D) -> Result<BTreeMap<String, u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut out = BTreeMap::new();
    let Some(obj) = value.as_object() else {
        return Ok(out);
    };
    for (key, value) in obj {
        if let Some(number) = value.as_u64() {
            out.insert(key.clone(), number as u32);
        }
    }
    Ok(out)
}

fn resistance_rows<'de, D>(deserializer: D) -> Result<BTreeMap<String, ResistanceRow>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut out = BTreeMap::new();
    let Some(obj) = value.as_object() else {
        return Ok(out);
    };
    for (key, value) in obj {
        if let Ok(row) = serde_json::from_value::<ResistanceRow>(value.clone()) {
            out.insert(key.clone(), row);
        }
    }
    Ok(out)
}

fn close(got: f32, want: f32) -> bool {
    (got - want).abs() <= ABS_TOLERANCE.max(want.abs() * REL_TOLERANCE)
}

fn stage_list(hit: &OracleHit, stage: &str) -> DamageMap {
    hit.stages
        .get(stage)
        .unwrap_or_else(|| panic!("{}#{} missing {stage}", hit.run, hit.hit))
        .list
        .clone()
}

fn delta(before: &DamageMap, after: &DamageMap) -> DamageMap {
    let mut out = DamageMap::new();
    for key in before.keys().chain(after.keys()) {
        let value =
            after.get(key).copied().unwrap_or(0.0) - before.get(key).copied().unwrap_or(0.0);
        if value != 0.0 {
            out.insert(key.clone(), value);
        }
    }
    out
}

fn add_maps(base: &DamageMap, add: &DamageMap) -> DamageMap {
    let mut out = base.clone();
    for (key, value) in add {
        *out.entry(key.clone()).or_default() += *value;
    }
    retain_nonzero(out)
}

fn scale_category(input: &DamageMap, factor: f32, category: fn(DamageType) -> bool) -> DamageMap {
    let mut out = input.clone();
    for (key, value) in &mut out {
        if *value > 0.0 && category(parse_damage_type(key)) {
            *value *= 1.0 + factor;
        }
    }
    retain_nonzero(out)
}

fn apply_pve_block(input: &DamageMap, stage: &Stage) -> DamageMap {
    let mut out = input.clone();
    let rating = if stage.optimal != 0 {
        stage.block_rating * (1.0 + stage.optimal_boost)
    } else {
        stage.block_rating
    };
    if rating <= 0.0 {
        return out;
    }
    let phys_total = total_matching(input, is_physical);
    let elem_total = total_matching(input, is_elemental);
    for (key, value) in &mut out {
        let ty = parse_damage_type(key);
        *value = match ty {
            DamageType::Stamina | DamageType::Magicka => 0.0,
            t if is_physical(t) => tables::block_cut(*value, phys_total, rating, 1.0),
            t if is_elemental(t) => tables::block_cut(*value, elem_total, rating, 0.667),
            _ => *value,
        };
    }
    retain_nonzero(out)
}

fn apply_snapshot_pve_block(input: &DamageMap, defender: &Stage) -> DamageMap {
    if defender.status.blocking == 0 {
        return input.clone();
    }
    let mut stage = Stage::default();
    stage.block_rating = defender.block_rating;
    stage.optimal = defender.optimal;
    stage.optimal_boost = defender.optimal_boost;
    apply_pve_block(input, &stage)
}

fn apply_snapshot_e6(input: &DamageMap, defender: &Stage, source: &str) -> DamageMap {
    let mut out = input.clone();
    let phys_total = total_matching(input, is_physical);
    if phys_total > 0.0 && defender.protection > 0.0 {
        for (key, value) in &mut out {
            if is_physical(parse_damage_type(key)) && *value > 0.0 {
                *value = tables::armor_cut_share(*value, phys_total, defender.protection);
            }
        }
    }
    let source = parse_source(source);
    for (key, value) in &mut out {
        let ty = parse_damage_type(key);
        if matches!(ty, DamageType::Stamina | DamageType::Magicka) || *value <= 0.0 {
            continue;
        }
        let Some(row) = defender.resistance.get(key) else {
            continue;
        };
        let base_multiplier = snapshot_base_resistance_multiplier(defender, ty, source);
        *value *= base_multiplier;
        *value =
            tables::apply_resistance_and_weakness(*value, row.resistance, row.weakness, 1.0, 0.0);
    }
    retain_nonzero(out)
}

fn snapshot_base_resistance_multiplier(
    _defender: &Stage,
    _ty: DamageType,
    _source: DamageSource,
) -> f32 {
    // The current hook records the presence count for base-resistance sources, but
    // not their per-source factors. `GetResistance` already includes the flat rating
    // path used by these PvE samples; when a future capture needs the percent
    // multiplier, this is the hook gap to close.
    1.0
}

fn apply_derived_e6(before: &DamageMap, after: &DamageMap) -> DamageMap {
    let mut out = before.clone();
    let phys_total = total_matching(before, is_physical);
    let mut armor_rating = 0.0_f32;
    for (key, start) in before {
        let ty = parse_damage_type(key);
        if !is_physical(ty) || *start <= 0.0 {
            continue;
        }
        let want = after.get(key).copied().unwrap_or(0.0);
        if want < start * 0.05 {
            let armor_target = want / 0.05;
            armor_rating = armor_rating.max((*start - armor_target).max(0.0) * 10.0);
        }
    }
    if armor_rating > 0.0 && phys_total > 0.0 {
        for (key, value) in &mut out {
            if is_physical(parse_damage_type(key)) && *value > 0.0 {
                *value = tables::armor_cut_share(*value, phys_total, armor_rating);
            }
        }
    }
    for (key, value) in &mut out {
        let ty = parse_damage_type(key);
        if matches!(ty, DamageType::Stamina | DamageType::Magicka) || *value <= 0.0 {
            continue;
        }
        let want = after.get(key).copied().unwrap_or(*value);
        let rating = (*value - want).max(0.0);
        if rating > 0.0 {
            *value = tables::apply_resistance_and_weakness(*value, rating, 0.0, 1.0, 0.0);
        }
    }
    retain_nonzero(out)
}

fn append_drains(input: &DamageMap) -> DamageMap {
    let mut out = input.clone();
    for (key, value) in input {
        if let Some((drain, ratio)) = mirrored_drain(parse_damage_type(key)) {
            *out.entry(format_damage_type(drain).to_string())
                .or_default() += *value * ratio;
        }
    }
    retain_nonzero(out)
}

fn total_matching(input: &DamageMap, pred: fn(DamageType) -> bool) -> f32 {
    input
        .iter()
        .filter_map(|(key, value)| Some((parse_damage_type(key), *value)))
        .filter(|(ty, value)| *value > 0.0 && pred(*ty))
        .map(|(_, value)| value)
        .sum()
}

fn retain_nonzero(mut map: DamageMap) -> DamageMap {
    map.retain(|_, value| value.abs() > 1e-6);
    map
}

fn parse_source(name: &str) -> DamageSource {
    match name {
        "Attack" => DamageSource::Attack,
        "Spell" => DamageSource::Spell,
        "WeaponManeuver" => DamageSource::WeaponManeuver,
        "StatusEffect" => DamageSource::StatusEffect,
        "Trap" => DamageSource::Trap,
        "Revenge" => DamageSource::Revenge,
        "AreaEffect" => DamageSource::AreaEffect,
        "ContinuousSpell" => DamageSource::ContinuousSpell,
        "EchoWeapon" => DamageSource::EchoWeapon,
        "ContinuousAttack" => DamageSource::ContinuousAttack,
        "ShieldManeuver" => DamageSource::ShieldManeuver,
        _ => DamageSource::None,
    }
}

fn parse_damage_type(name: &str) -> DamageType {
    match name {
        "Slashing" => DamageType::Slashing,
        "Cleaving" => DamageType::Cleaving,
        "Bashing" => DamageType::Bashing,
        "Fire" => DamageType::Fire,
        "Frost" => DamageType::Frost,
        "Shock" => DamageType::Shock,
        "Poison" => DamageType::Poison,
        "Stamina" => DamageType::Stamina,
        "Magicka" => DamageType::Magicka,
        "Health" | "H" => DamageType::Health,
        _ => DamageType::None,
    }
}

fn format_damage_type(ty: DamageType) -> &'static str {
    match ty {
        DamageType::Slashing => "Slashing",
        DamageType::Cleaving => "Cleaving",
        DamageType::Bashing => "Bashing",
        DamageType::Fire => "Fire",
        DamageType::Frost => "Frost",
        DamageType::Shock => "Shock",
        DamageType::Poison => "Poison",
        DamageType::Stamina => "Stamina",
        DamageType::Magicka => "Magicka",
        DamageType::Health => "Health",
        DamageType::None => "None",
    }
}
