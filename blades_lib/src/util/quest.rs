use std::collections::HashMap;

use thiserror::Error;
use uuid::Uuid;

use crate::{
    game_data::GameData,
    static_data::QuestLevelScaling,
    user_data::{
        DungeonGeneratedData,
        ObjectiveStatus, Quest, QuestStatus, QuestType,
    },
};

#[derive(Error, Debug, Clone)]
pub enum GenerateQuestDataError {
    #[error("quest {0} does not exist")]
    QuestNotFound(Uuid),
    #[error("dungeon {0} does not exist")]
    DungeonNotFound(Uuid),
}

/// Generate a quest's body + dungeon data, scaled to the player's level.
///
/// `player_level` + `scaling` (from `quests_daily.json.levelScaling`) drive the enemy /
/// difficulty level and per-enemy XP — replacing the old hard-coded `level 1 / 1000 XP`
/// stub that spawned level-1 enemies for everyone. With an empty `scaling` the enemy
/// level degrades to the player's own level (still never the flat 1).
///
/// A NIL-dungeon quest (`dungeon_uuid == 00000000-...`, the 6 dialogue-only "daily-job"
/// quests) short-circuits to a body with NO dungeon data instead of erroring — the old
/// `.ok_or(DungeonNotFound)` crashed those on accept.
pub fn generate_quest_data(
    game_data: &GameData,
    quest_id: Uuid,
    player_level: i64,
    scaling: &QuestLevelScaling,
) -> Result<(Quest, Option<DungeonGeneratedData>), GenerateQuestDataError> {
    let quest_data = game_data
        .quests
        .get(&quest_id)
        .ok_or(GenerateQuestDataError::QuestNotFound(quest_id))?;

    // A quest without a `dungeon_info` block carries no objectives/dungeon; treat it as a
    // dialogue quest (no dungeon data) rather than panicking on `.unwrap()`.
    let Some(dungeon_info) = quest_data.dungeon_info.as_ref() else {
        return Ok((
            dialogue_quest(quest_id, 0, HashMap::new(), player_level, scaling),
            None,
        ));
    };

    let difficulty_level = scaling.enemy_level(player_level);
    let objective_statuses: HashMap<Uuid, ObjectiveStatus> = dungeon_info
        .objectives
        .iter()
        .map(|(id, _o)| {
            (
                *id,
                ObjectiveStatus {
                    completed: false,
                    progress: 0.0,
                    status: QuestStatus::Active,
                },
            )
        })
        .collect();

    let quest = Quest {
        completed: false,
        difficulty_level,
        gld_quest_id: quest_id,
        seed: 1234.into(),
        r#type: QuestType::Normal,
        version: dungeon_info.version,
        objective_statuses: objective_statuses.clone(),
        // An ordinary quest carries none of the event fields; the event path in
        // `server::quest::event_quests` fills them in after calling this.
        game_event_quest_data: None,
        rewards: None,
        final_reward: None,
        // Not a town job: jobs are built by `server::quest::jobs_gen`, never here.
        job_reward: None,
    };

    // Nil-dungeon (dialogue-only) quests have no dungeon to generate — short-circuit to a
    // no-dungeon completion so /accept doesn't error (they were the DungeonNotFound crash).
    if dungeon_info.dungeon_uuid.is_nil() {
        return Ok((quest, None));
    }

    let enemy_level = scaling.enemy_level(player_level);
    let given_xp = scaling.given_xp(enemy_level);

    // Shared with the Abyss — see `util::dungeon`. The Abyss used to have its
    // own hard-coded copy of this shape, which served floor 1's spawn groups on
    // every floor and hung every deeper run.
    let generated_dungeon_data =
        generate_for_quest_dungeon(game_data, &dungeon_info.dungeon_uuid, enemy_level, given_xp)
            .ok_or(GenerateQuestDataError::DungeonNotFound(
                dungeon_info.dungeon_uuid,
            ))?;

    Ok((quest, Some(generated_dungeon_data)))
}

/// A quest dungeon's generated data: every stage of its variant family (#260).
pub fn generate_for_quest_dungeon(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    enemy_level: i64,
    given_xp: u64,
) -> Option<DungeonGeneratedData> {
    generate_for_family(game_data, dungeon_uuid, |id| {
        crate::util::dungeon::generate_for_dungeon(game_data, id, enemy_level, given_xp)
    })
}

/// `EQ23_SQ103_DungeonSettings_A`, "The Web Mother's Trap": an event dungeon whose
/// named `_A` is the WHOLE run, although a `_B` shares its handle prefix (#329).
///
/// The event reuses the cave of story quest SQ103, whose `_B` is that story's second
/// stage. Retail's EQ23 data covered `_A` alone: 9 of 9 captured generated-data
/// objects (6 distinct, difficulty 18 to 72) carry `_A`'s 15 groups and none of
/// `_B`'s 10, and 243 of 243 captured update actions in this dungeon name an `_A`
/// group. Serving `_B` too (#465) ended every run of it with its Wispmothers and the
/// spider boss already dead before the player reached them.
pub const EQ23_SINGLE_STAGE: Uuid = Uuid::from_u128(0x401ffa22_79ba_4c1c_aaa2_d67730d76aad);

/// Event dungeons whose generated data is the named dungeon alone, never its family.
const SINGLE_STAGE_EVENT_DUNGEONS: &[Uuid] = &[EQ23_SINGLE_STAGE];

/// Every stage an EVENT run covers: its dungeon's whole variant family (EQ22, EQ24:
/// #323), except for an event that retail ran on the named dungeon alone (#329).
pub fn event_dungeon_stage_ids(game_data: &GameData, dungeon_uuid: &Uuid) -> Option<Vec<Uuid>> {
    if SINGLE_STAGE_EVENT_DUNGEONS.contains(dungeon_uuid) {
        return game_data.dungeons.contains_key(dungeon_uuid).then(|| vec![*dungeon_uuid]);
    }
    quest_dungeon_family_ids(game_data, dungeon_uuid)
}

/// The dungeons sharing an event dungeon's handle family that its run does NOT
/// cover — `EQ23_SQ103_DungeonSettings_B` for EQ23, nothing for any other event.
pub fn event_dungeon_foreign_stage_ids(game_data: &GameData, dungeon_uuid: &Uuid) -> Vec<Uuid> {
    let stages = event_dungeon_stage_ids(game_data, dungeon_uuid).unwrap_or_default();
    quest_dungeon_family_ids(game_data, dungeon_uuid)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !stages.contains(id))
        .collect()
}

/// An event dungeon's generated data: [`event_dungeon_stage_ids`], unseeded.
///
/// Enemies stand at `enemy_level` plus their spawn group's APK level delta, each
/// worth the XP of its own level, as retail generated every event dungeon (see
/// [`crate::util::dungeon::spawn_group_level_delta`]).
pub fn generate_for_event_dungeon(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    enemy_level: i64,
    scaling: &QuestLevelScaling,
) -> Option<DungeonGeneratedData> {
    let levels = crate::util::dungeon::EnemyLevels::WithDeltas {
        base: enemy_level,
        scaling,
    };
    generate_for_ids(event_dungeon_stage_ids(game_data, dungeon_uuid)?, |id| {
        crate::util::dungeon::generate_for_dungeon_levelled(game_data, id, levels)
    })
}

/// An event attempt's generated data, with a per-run loot seed.
///
/// An event quest names its first stage (`EQ24_SQ104_DungeonSettings_A`) exactly as
/// a story quest does, and retail's generated data for it covered every stage: all
/// 8 distinct captured EQ24 objects carry both `_A` and `_B` (20 enemies, 2 chests),
/// all 5 EQ22 objects `_A`, `_B` and `_C`. Generating only the named dungeon left every
/// later stage without data, so its kills showed no experience, dropped nothing,
/// and its containers were empty (report #323). EQ23 is the exception: see
/// [`EQ23_SINGLE_STAGE`].
pub fn generate_for_event_dungeon_with_seed(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    run_seed: u64,
    enemy_level: i64,
    scaling: &QuestLevelScaling,
) -> Option<DungeonGeneratedData> {
    let levels = crate::util::dungeon::EnemyLevels::WithDeltas {
        base: enemy_level,
        scaling,
    };
    generate_for_ids(event_dungeon_stage_ids(game_data, dungeon_uuid)?, |id| {
        crate::util::dungeon::generate_for_dungeon_levelled_with_seed(
            game_data, id, run_seed, levels,
        )
    })
}

fn generate_for_family(
    game_data: &GameData,
    dungeon_uuid: &Uuid,
    generate: impl Fn(&Uuid) -> Option<DungeonGeneratedData>,
) -> Option<DungeonGeneratedData> {
    generate_for_ids(quest_dungeon_family_ids(game_data, dungeon_uuid)?, generate)
}

fn generate_for_ids(
    mut ids: Vec<Uuid>,
    generate: impl Fn(&Uuid) -> Option<DungeonGeneratedData>,
) -> Option<DungeonGeneratedData> {
    if ids.is_empty() {
        return None;
    }
    let first = ids.remove(0);
    let mut out = generate(&first)?;

    for id in ids {
        merge_dungeon_generated_data(&mut out, generate(&id)?);
    }

    Some(out)
}

/// The first stage of the variant family `dungeon_uuid` belongs to, from ANY stage:
/// `EQ24_SQ104_DungeonSettings_B` gives `..._A`. A dungeon outside a family is its
/// own entrypoint. [`quest_dungeon_family_ids`] only expands from an entrypoint, so
/// a caller holding a later stage resolves it here first.
pub fn variant_family_entrypoint(game_data: &GameData, dungeon_uuid: &Uuid) -> Uuid {
    let Some(prefix) = game_data
        .dungeons
        .get(dungeon_uuid)
        .and_then(|d| variant_family_prefix(&d.handle))
    else {
        return *dungeon_uuid;
    };
    game_data
        .dungeons
        .iter()
        .find(|(_, d)| first_variant_family_prefix(&d.handle) == Some(prefix))
        .map_or(*dungeon_uuid, |(id, _)| *id)
}

/// Every dungeon a quest's generated data covers: the dungeon itself, or its whole
/// variant family (`*_A`/`*_1` entrypoints) in handle order (#260).
pub fn quest_dungeon_family_ids(game_data: &GameData, dungeon_uuid: &Uuid) -> Option<Vec<Uuid>> {
    let dungeon = game_data.dungeons.get(dungeon_uuid)?;
    let Some(prefix) = first_variant_family_prefix(&dungeon.handle) else {
        return Some(vec![*dungeon_uuid]);
    };

    let mut variants: Vec<(String, Uuid)> = game_data
        .dungeons
        .iter()
        .filter_map(|(id, candidate)| {
            (variant_family_prefix(&candidate.handle) == Some(prefix))
                .then(|| (candidate.handle.clone(), *id))
        })
        .collect();
    variants.sort_by(|a, b| a.0.cmp(&b.0));

    Some(variants.into_iter().map(|(_, id)| id).collect())
}

fn first_variant_family_prefix(handle: &str) -> Option<&str> {
    let prefix = handle
        .strip_suffix("_A")
        .or_else(|| handle.strip_suffix("_1"))?;
    prefix.ends_with("DungeonSettings").then_some(prefix)
}

fn variant_family_prefix(handle: &str) -> Option<&str> {
    ["_A", "_B", "_C", "_D", "_1", "_2", "_3", "_4"]
        .iter()
        .find_map(|suffix| handle.strip_suffix(suffix))
        .filter(|prefix| prefix.ends_with("DungeonSettings"))
}

/// Add `extra`'s spawn groups to `out`; a group `out` already has keeps its rolls.
pub fn merge_dungeon_generated_data(out: &mut DungeonGeneratedData, extra: DungeonGeneratedData) {
    for (id, data) in extra.enemy_generated_data {
        out.enemy_generated_data.entry(id).or_insert(data);
    }
    for (id, data) in extra.item_generated_data {
        out.item_generated_data.entry(id).or_insert(data);
    }
    for (id, data) in extra.chest_generated_data {
        out.chest_generated_data.entry(id).or_insert(data);
    }
}

/// A dialogue / no-dungeon quest body (no dungeon data). Used for a quest whose
/// `dungeon_info` is absent — objectives default to whatever is passed (empty for a bare
/// dialogue quest).
fn dialogue_quest(
    quest_id: Uuid,
    version: u64,
    objective_statuses: HashMap<Uuid, ObjectiveStatus>,
    player_level: i64,
    scaling: &QuestLevelScaling,
) -> Quest {
    Quest {
        completed: false,
        difficulty_level: scaling.enemy_level(player_level),
        gld_quest_id: quest_id,
        seed: 1234.into(),
        r#type: QuestType::Normal,
        version,
        objective_statuses,
        game_event_quest_data: None,
        rewards: None,
        final_reward: None,
        job_reward: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::{EnemyLevelScaling, QuestLevelScaling};

    const POOL_OF_DESPAIR: &str = "c178813f-ea7c-4732-aba0-a5c8d8767ec9";
    const SQ201_1: &str = "da0a20c9-bbca-46bf-8f14-fead50f50675";
    const SQ201_2: &str = "0ded6e84-d942-434b-8563-33ef301f6189";
    const SQ201_KEY_HOLDER: &str = "379775a2-8015-4c8c-a503-0691597528a0";
    const DOOR_KEY: &str = "faa3aeb3-9284-4d83-8981-1af00e3a6398";
    const EQ15_QUEST: &str = "e8f3614c-8672-4f77-9dad-4b400676f4b6";
    const EQ15_DUNGEON: &str = "924f1147-fd7f-4736-9e2d-f33fa942dbdd";

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("read parsed.json"))
            .expect("parse game data")
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).expect("uuid")
    }

    /// A scaling table like `quests_daily.json`: default skull (2) → offset 0.
    fn scaling() -> QuestLevelScaling {
        QuestLevelScaling {
            enemy_level_from_player_level: EnemyLevelScaling {
                offset_by_skull: [("2".to_string(), 0i64)].into_iter().collect(),
                default_skull: 2,
            },
            // No measured curve: this fixture exercises the skull-offset fallback.
            measured: Default::default(),
        }
    }

    #[test]
    fn pool_of_despair_generates_both_sq201_stages_and_the_key_holder() {
        let game_data = game_data();
        let (_, generated) = generate_quest_data(&game_data, uuid(POOL_OF_DESPAIR), 13, &scaling())
            .expect("Pool of Despair exists");
        let generated = generated.expect("Pool of Despair has dungeon data");

        let stage_1 = &game_data.dungeons[&uuid(SQ201_1)];
        let stage_2 = &game_data.dungeons[&uuid(SQ201_2)];
        assert_eq!(
            generated.enemy_generated_data.len(),
            stage_1.spawn_info.enemy_spawn_groups.len()
                + stage_2.spawn_info.enemy_spawn_groups.len(),
            "retail sent generated data for both SQ201_DungeonSettings_1 and _2"
        );

        let holder = &generated.enemy_generated_data[&uuid(SQ201_KEY_HOLDER)][0][0];
        assert_eq!(
            holder
                .merged_loot_table()
                .stackable_items
                .get(&uuid(DOOR_KEY))
                .copied(),
            Some(1),
            "SQ201_DungeonSettings_2's key-holder must be present and carry the door key"
        );
    }

    #[test]
    fn non_variant_quest_stays_on_its_single_dungeon() {
        let game_data = game_data();
        assert_eq!(
            game_data.dungeons[&uuid(EQ15_DUNGEON)].handle.as_str(),
            "EQ15_Stone_DungeonSettings"
        );

        let (_, generated) =
            generate_quest_data(&game_data, uuid(EQ15_QUEST), 14, &scaling()).expect("EQ15 exists");
        let generated = generated.expect("EQ15 has dungeon data");

        assert_eq!(
            generated.enemy_generated_data.len(),
            game_data.dungeons[&uuid(EQ15_DUNGEON)]
                .spawn_info
                .enemy_spawn_groups
                .len()
        );
    }

    #[test]
    fn later_variant_is_not_a_family_entrypoint() {
        let game_data = game_data();
        assert_eq!(
            quest_dungeon_family_ids(&game_data, &uuid(SQ201_2)),
            Some(vec![uuid(SQ201_2)])
        );
    }

    #[test]
    fn enemy_level_scales_with_player_not_flat_one() {
        let s = scaling();
        assert_eq!(s.enemy_level(50), 50, "level-50 player → level-50 enemies");
        assert_eq!(s.enemy_level(1), 1, "clamped floor");
        assert_eq!(s.enemy_level(200), 100, "clamped ceiling at 100");
        // XP scales with enemy level, not a flat 1000.
        assert_eq!(s.given_xp(50), 5000);
        assert_ne!(s.given_xp(50), 1000);
    }

    #[test]
    fn empty_scaling_degrades_to_player_level_never_flat_one() {
        let s = QuestLevelScaling::default();
        assert_eq!(s.enemy_level(37), 37, "no table → the player's own level");
        assert_eq!(s.enemy_level(0), 1, "clamped to at least 1");
    }
}

/// How retail scaled quest enemies — replayed against 66,994 captured spawns.
///
/// These are the two numbers that decide how fast a character levels: what an
/// enemy is worth, and how hard it is. Both were formulas here and neither
/// matched retail, so they are asserted against the corpus rather than against
/// each other.
#[cfg(test)]
mod how_retail_scaled_enemies {
    use crate::static_data::QuestLevelScaling;
    use serde_json::Value;

    /// The shipped table — the one the server loads at boot.
    fn shipped() -> QuestLevelScaling {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/quests_daily.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let json: Value = serde_json::from_str(&raw).expect("valid quests_daily.json");
        let scaling: QuestLevelScaling =
            serde_json::from_value(json["levelScaling"].clone()).expect("levelScaling parses");
        assert!(
            !scaling.measured.given_xp_by_enemy_level.is_empty()
                && !scaling.measured.enemy_level_by_player_level.is_empty(),
            "the measured curves did not survive deserialization — both tables carry a \
             prose `note` next to their numeric rows and a stricter reader drops the lot"
        );
        scaling
    }

    /// `[[playerLevel, enemyLevel, count], ...]` plus `[[enemyLevel, xp, n], ...]`.
    fn corpus() -> Value {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/retail-journey/spawn_levels.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        serde_json::from_str(&raw).expect("valid spawn_levels.json")
    }

    fn pairs() -> Vec<(i64, i64, u64)> {
        corpus()["playerEnemyPairs"]
            .as_array()
            .expect("playerEnemyPairs")
            .iter()
            .map(|r| {
                (
                    r[0].as_i64().unwrap(),
                    r[1].as_i64().unwrap(),
                    r[2].as_u64().unwrap(),
                )
            })
            .collect()
    }

    /// Share of captured spawns a candidate rule reproduces exactly, and within
    /// two levels.
    fn score(rule: impl Fn(i64) -> i64) -> (f64, f64) {
        let (mut total, mut exact, mut near) = (0u64, 0u64, 0u64);
        for (player, enemy, n) in pairs() {
            total += n;
            let want = rule(player);
            if want == enemy {
                exact += n;
            }
            if (want - enemy).abs() <= 2 {
                near += n;
            }
        }
        assert!(total > 60_000, "only {total} spawns in the fixture");
        (exact as f64 / total as f64, near as f64 / total as f64)
    }

    /// THE measurement, with its controls.
    ///
    /// The curve is a median over every spawn group, so it is closer rather than
    /// right — which is exactly why it is scored against alternatives instead of
    /// asserted to be exact. What matters is that it beats what we shipped, and
    /// by how much.
    #[test]
    fn the_measured_curve_beats_serving_enemies_at_the_players_level() {
        let scaling = shipped();
        let (exact, near) = score(|p| scaling.enemy_level(p));
        // What the server did before: enemyLevel = playerLevel.
        let (flat_exact, flat_near) = score(|p| p.clamp(1, 100));
        // A second control, so "anything below the player" cannot be the reason.
        let (minus_ten_exact, _) = score(|p| (p - 10).clamp(1, 100));

        assert!(
            exact > flat_exact * 2.0,
            "the curve reproduces {:.1}% of captured spawns exactly against {:.1}% \
             for enemy=player — it must beat it by a wide margin or it is not \
             worth the table",
            exact * 100.0,
            flat_exact * 100.0
        );
        assert!(
            near > flat_near * 2.0,
            "within two levels: {:.1}% vs {:.1}%",
            near * 100.0,
            flat_near * 100.0
        );
        assert!(
            exact > minus_ten_exact * 2.0,
            "the curve ({:.1}%) must beat a flat player-10 ({:.1}%); otherwise all \
             it has found is 'lower'",
            exact * 100.0,
            minus_ten_exact * 100.0
        );
    }

    /// The shape the fix exists for: retail's enemies track the player early and
    /// fall behind later. Serving `playerLevel` all the way up is what made our
    /// quests harder than retail's at high level.
    #[test]
    fn enemies_track_the_player_early_and_fall_behind_later() {
        let scaling = shipped();
        for p in 1..=20 {
            let gap = scaling.enemy_level(p) - p;
            assert!(
                gap.abs() <= 3,
                "at player level {p} retail's enemies were within 3 levels; ours are {gap:+}"
            );
        }
        assert!(
            scaling.enemy_level(100) <= 85,
            "at player level 100 retail's median enemy was 78; ours is {}",
            scaling.enemy_level(100)
        );
        // Monotone: a level-up must never make the world easier in absolute terms.
        for p in 2..=100 {
            assert!(
                scaling.enemy_level(p) >= scaling.enemy_level(p - 1),
                "enemy level went DOWN from player {} to {p}",
                p - 1
            );
        }
    }

    /// Every enemy level the corpus covers is worth what retail paid for it.
    ///
    /// The canonical value is the maximum observed: the same enemy level carries
    /// two XP populations (14 and 41 both appear at level 12) and the lower one is
    /// the partial/zero-XP case.
    #[test]
    fn an_enemy_is_worth_what_retail_paid_for_it() {
        let scaling = shipped();
        let corpus = corpus();
        let mut checked = 0;
        for row in corpus["givenXpObservations"].as_array().unwrap() {
            let (level, canonical, n) = (
                row[0].as_i64().unwrap(),
                row[1].as_u64().unwrap(),
                row[2].as_u64().unwrap(),
            );
            // The extractor drops thin and zero-XP rows from the shipped table on
            // purpose; skip them here for the same reason.
            if n < 5 || canonical == 0 {
                continue;
            }
            assert_eq!(
                scaling.given_xp(level),
                canonical,
                "an enemy of level {level} was worth {canonical} XP at retail ({n} observations)"
            );
            checked += 1;
        }
        assert!(checked >= 80, "only {checked} enemy levels checked");
    }

    /// THE CONTROL on the XP fix: the old formula was not slightly off, it was an
    /// order of magnitude out, and that is most of why levelling here was fast.
    #[test]
    fn the_old_hundred_times_level_formula_was_an_order_of_magnitude_out() {
        let scaling = shipped();
        assert_eq!(scaling.given_xp(50), 220, "retail's level-50 enemy");
        assert_eq!(100 * 50, 5000, "what we paid for it");
        assert!(
            scaling.given_xp(50) * 20 < 5000,
            "the formula was more than 20x retail at level 50"
        );
        // Both ends stay sane: the table's floor at 1 and its flat top past 90.
        assert_eq!(scaling.given_xp(1), 11);
        assert_eq!(scaling.given_xp(0), 11, "clamped, not zero");
        assert_eq!(
            scaling.given_xp(200),
            scaling.given_xp(90),
            "past the table's top it holds, rather than resuming a formula that \
             would pay 9,100 at level 91 against 284 at 90"
        );
    }

    /// A server booted with no static data must still spawn something playable.
    #[test]
    fn an_empty_table_degrades_instead_of_breaking() {
        let empty = QuestLevelScaling::default();
        assert_eq!(empty.enemy_level(37), 37, "the player's own level");
        assert_eq!(empty.enemy_level(0), 1, "clamped");
        assert_eq!(empty.given_xp(50), 5000, "the old formula, only as a last resort");
    }
}

/// Event dungeons as retail generated them (tracker #353, #358, #365): every
/// container filled, every enemy at its group's level.
///
/// Measured against the capture corpus: retail put the APK's `_quantity` of
/// results on each container spawn (1,283 of 1,285 captured spawns), and stood
/// each event enemy at `difficultyLevel + _levelDeltaSequence` (1,641 of 1,643).
#[cfg(test)]
mod event_dungeons_as_retail_built_them {
    use super::*;
    use crate::static_data::QuestLevelScaling;
    use std::collections::HashMap;

    /// EQ25 "Cave", captured: retail quest 2e3c2eca at difficulty 72.
    const EQ25: &str = "8e2db517-ac60-4db8-b3cd-aefaed596ea5";
    /// EQ40 (Sephoris, 10-05 and 10-06) and EQ30 "Spirit of the Hunt" (HauDrauf,
    /// 10-08): never captured, so no retail count for any of their containers.
    const EQ40: &str = "1806964b-c886-4e69-a494-c7d512698e95";
    const EQ30: &str = "7e840c88-2952-48e4-8a6c-496a86612482";

    fn game_data() -> GameData {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("read parsed.json"))
            .expect("parse game data")
    }

    fn shipped() -> QuestLevelScaling {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/quests_daily.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p).expect("read")).expect("json");
        serde_json::from_value(json["levelScaling"].clone()).expect("levelScaling")
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).expect("uuid")
    }

    /// Container count per spawn, by the spawn's APK name (`Breakable T1` ...).
    fn piles(gd: &GameData, dungeon: &str, data: &DungeonGeneratedData) -> Vec<(String, usize)> {
        let spawns = &gd.dungeons[&uuid(dungeon)].spawn_info.item;
        let mut out: Vec<(String, usize)> = data
            .item_generated_data
            .iter()
            .map(|(id, pile)| (spawns[id].name.clone().unwrap_or_default(), pile.len()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn an_uncaptured_event_dungeon_fills_every_container() {
        let gd = game_data();
        for (dungeon, names) in [
            (EQ40, ["Breakable T1", "Breakable T2", "Breakable T3"]),
            (EQ30, ["Breakable_T1", "Breakable_T2", "Breakable_T3"]),
        ] {
            let data = generate_for_event_dungeon(&gd, &uuid(dungeon), 78, &shipped()).unwrap();
            assert_eq!(
                piles(&gd, dungeon, &data),
                vec![(names[0].into(), 7), (names[1].into(), 3), (names[2].into(), 1)],
                "{dungeon}: the 7 / 3 / 1 containers the client places, not 1 / 1 / 1"
            );
            // And every one of them holds a roll for each of its tables.
            for pile in data.item_generated_data.values() {
                assert!(pile.iter().all(|r| !r.loot_table_loot.is_empty()));
            }
        }
    }

    /// CONTROL: a captured event dungeon keeps exactly retail's piles.
    #[test]
    fn a_captured_event_dungeon_keeps_retails_piles() {
        let gd = game_data();
        let data = generate_for_event_dungeon(&gd, &uuid(EQ25), 72, &shipped()).unwrap();
        let by_id: HashMap<String, usize> = data
            .item_generated_data
            .iter()
            .map(|(k, v)| (k.to_string(), v.len()))
            .collect();
        // Retail quest 2e3c2eca, difficulty 72.
        assert_eq!(by_id["efbafb6d-0424-4310-83d3-d29021f28a20"], 7);
        assert_eq!(by_id["8d207c3c-6b73-4035-b363-181a0cf3ab93"], 3);
        assert_eq!(by_id["fb30ad34-3cb5-42fd-9bb5-e7fc101e0cfc"], 1);
    }

    /// Retail quest 2e3c2eca (EQ25, difficulty 72), every enemy's level and XP.
    #[test]
    fn event_enemies_stand_at_their_groups_level() {
        let gd = game_data();
        let data = generate_for_event_dungeon(&gd, &uuid(EQ25), 72, &shipped()).unwrap();
        let retail: [(&str, i64, u64); 8] = [
            ("08f503be-9dc1-4cb1-811a-df1226983de2", 72, 257),
            ("29535b6a-7a48-4ba6-be65-47d28a754c7d", 72, 257),
            ("38d12d9c-0f0f-421c-ab95-9cc57f2b4818", 69, 252),
            ("70ab652e-692c-45a6-8397-e63352e4719b", 77, 264),
            ("9094d04e-b35a-45c9-bf50-ec7d913f5c54", 72, 257),
            ("9b886c9a-0502-42d3-ab2e-4a3d2ca0b0e1", 69, 252),
            ("bbd417df-fb1c-4b94-902a-d00e2578c763", 78, 266),
            ("d57b0525-f90d-4462-a4aa-7341ce8ea949", 78, 266),
        ];
        assert_eq!(data.enemy_generated_data.len(), retail.len());
        for (group, level, xp) in retail {
            let enemies: Vec<(i64, u64)> = data.enemy_generated_data[&uuid(group)]
                .iter()
                .flatten()
                .map(|e| (e.enemy_level, e.given_xp))
                .collect();
            assert_eq!(enemies, vec![(level, xp)], "group {group}");
        }
    }

    /// HauDrauf's 10-08 event: five bosses, each at the dungeon's level + 6.
    #[test]
    fn the_spirit_of_the_hunt_bosses_stand_six_levels_up() {
        let gd = game_data();
        let data = generate_for_event_dungeon(&gd, &uuid(EQ30), 78, &shipped()).unwrap();
        assert_eq!(data.enemy_generated_data.len(), 5);
        for spawners in data.enemy_generated_data.values() {
            for e in spawners.iter().flatten() {
                assert_eq!(e.enemy_level, 84);
                assert_eq!(e.given_xp, shipped().given_xp(84));
            }
        }
    }

    /// The seeded path a run and the `/quests` row use agrees with the unseeded one
    /// on levels and pile sizes.
    #[test]
    fn the_seeded_run_is_built_the_same_way() {
        let gd = game_data();
        let a = generate_for_event_dungeon(&gd, &uuid(EQ40), 50, &shipped()).unwrap();
        let b = generate_for_event_dungeon_with_seed(&gd, &uuid(EQ40), 0xC0FFEE, 50, &shipped())
            .unwrap();
        for (spawn, pile) in &a.item_generated_data {
            assert_eq!(pile.len(), b.item_generated_data[spawn].len());
        }
        for (group, spawners) in &a.enemy_generated_data {
            let lv = |s: &Vec<Vec<crate::user_data::DungeonEnemyResult>>| {
                s.iter().flatten().map(|e| (e.enemy_level, e.given_xp)).collect::<Vec<_>>()
            };
            assert_eq!(lv(spawners), lv(&b.enemy_generated_data[group]));
        }
        // EQ40's boss and its two +5 groups are above the dungeon's level.
        let max = a.enemy_generated_data.values().flatten().flatten().map(|e| e.enemy_level).max();
        assert_eq!(max, Some(56));
    }

    /// CONTROL: a story quest's enemies stay flat. Retail story rows do not follow
    /// the delta (their `difficultyLevel` is not the enemy level), so only the
    /// event path applies it. SA01's `[-3]` group is the discriminating case.
    #[test]
    fn story_quest_enemies_are_not_levelled_by_group() {
        let gd = game_data();
        let (id, _) = gd
            .dungeons
            .iter()
            .find(|(_, d)| d.handle == "SA01_DungeonSettings")
            .expect("SA01");
        assert!(gd.dungeons[id]
            .spawn_info
            .enemy_spawn_groups
            .keys()
            .any(|g| crate::util::dungeon::spawn_group_level_delta(g, 0) != 0));
        let data = generate_for_quest_dungeon(&gd, id, 40, 123).unwrap();
        for e in data.enemy_generated_data.values().flatten().flatten() {
            assert_eq!((e.enemy_level, e.given_xp), (40, 123));
        }
    }
}
